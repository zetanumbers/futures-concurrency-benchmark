use std::{
    cell::UnsafeCell,
    mem::{self, MaybeUninit},
    pin::Pin,
    ptr,
    sync::atomic::{self, AtomicUsize},
    task::{self, Poll, Waker},
};

const FIRST_ALIVE: usize = 0b01;
const SECOND_ALIVE: usize = 0b10;
const WAKER_UNINIT: usize = 0b10000;

const fn alive_flag(side: usize) -> usize {
    1 << side
}

const fn ready_flag(side: usize) -> usize {
    0b100 << side
}

pub fn handshake() -> [Side; 2] {
    let state = Box::new(State {
        status: AtomicUsize::new(FIRST_ALIVE | SECOND_ALIVE | WAKER_UNINIT),
        waker: UnsafeCell::new(MaybeUninit::uninit()),
    });
    let ptr = Box::into_raw(state);
    unsafe {
        [
            Side {
                ptr: ptr::NonNull::new_unchecked(ptr),
            },
            Side {
                ptr: ptr::NonNull::new_unchecked(ptr.wrapping_byte_add(1)),
            },
        ]
    }
}

// Assert least significant bit of an aligned address to State is always 0
const _: () = {
    if mem::align_of::<State>() % 2 != 0 {
        panic!();
    }
};

struct State {
    status: AtomicUsize,
    waker: UnsafeCell<mem::MaybeUninit<Waker>>,
}

pub struct Side {
    ptr: ptr::NonNull<State>,
}

unsafe impl Send for Side {}
unsafe impl Sync for Side {}

impl Side {
    fn side_and_state(&self) -> (usize, *mut State) {
        // strict provenance tricks
        let ptr = self.ptr.as_ptr();
        let side = unsafe { mem::transmute::<*mut State, usize>(ptr) } & 1;
        let state = ptr.wrapping_byte_sub(side);
        (side, state)
    }

    pub fn ready(&self, cx: &mut task::Context<'_>) -> Poll<()> {
        let (side, state) = self.side_and_state();
        let state = unsafe { &*state };

        let status = state
            .status
            .fetch_or(ready_flag(side) | WAKER_UNINIT, atomic::Ordering::Acquire);
        if status & ready_flag(side ^ 1) != 0 {
            if status & WAKER_UNINIT == 0 {
                unsafe { (*state.waker.get()).assume_init_read().wake() };
            }
            Poll::Ready(())
        } else {
            let new_waker = cx.waker();
            if status & WAKER_UNINIT == 0 {
                let old_waker = unsafe { (*state.waker.get()).assume_init_mut() };
                if !old_waker.will_wake(new_waker) {
                    old_waker.clone_from(new_waker)
                }
            } else {
                unsafe { (*state.waker.get()).write(new_waker.clone()) };
            }
            // TODO: consider fences instead
            let status = state
                .status
                .fetch_and(!WAKER_UNINIT, atomic::Ordering::Release);

            if status & ready_flag(side ^ 1) != 0 {
                // Waker will be dropped in the drop impl
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }
    }
}

impl std::future::Future for Side {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        self.ready(cx)
    }
}

impl Drop for Side {
    fn drop(&mut self) {
        let (side, state_ptr) = self.side_and_state();
        let state = unsafe { &*state_ptr };
        let status = state
            .status
            .fetch_and(!alive_flag(side), atomic::Ordering::Acquire);
        if status & alive_flag(side ^ 1) == 0 {
            if status & WAKER_UNINIT == 0 {
                unsafe { (*state.waker.get()).assume_init_drop() }
            }
            unsafe {
                drop(Box::from_raw(state_ptr));
            }
        }
    }
}
