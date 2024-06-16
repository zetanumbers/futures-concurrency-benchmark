use std::{
    alloc::{alloc, dealloc, Layout},
    future::Future,
    pin::Pin,
    ptr,
    sync::atomic::{self, AtomicUsize},
    task::{self, Poll},
};

use atomic_waker::AtomicWaker;

mod raw;

pub struct JoinVec<T> {
    partially_shared: ptr::NonNull<JoinVecState<T>>,
}

impl<T> JoinVec<T> {
    // TODO: impl FromIterator
    pub fn from_vec(v: Vec<T>) -> Self {}
}

struct JoinVecState<T> {
    // TODO: Aquire memory fence (load) before deallocation
    allocation_rc: AtomicUsize,
    extern_waker: AtomicWaker,
    last_to_poll: Atomic
    // TODO: store in place
    elems: ptr::NonNull<[JoinVecElem<T>]>,
}

struct JoinVecElem<T> {
    future: T,
    waker_rc: AtomicUsize,
    parent_state: ptr::NonNull<JoinVecState<T>>,
    next_to_poll: Option<ptr::NonNull<JoinVecElem<T>>>,
}

impl<T: Future> Future for JoinVec<T> {
    // TODO: Custom IntoIterator type
    type Output = Vec<T::Output>;

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        todo!()
    }
}

impl<T> Drop for JoinVec<T> {
    fn drop(&mut self) {
        unsafe {
            let state = self.partially_shared.as_ptr();

            (*state).extern_waker.take();

            let rc = (*state)
                .allocation_rc
                .fetch_sub(1, atomic::Ordering::Release);
            if rc == 1 {
                JoinVecState::dealloc(self.partially_shared.as_ptr())
            }
        }
    }
}

impl<T> JoinVecState<T> {
    fn alloc() -> *mut JoinVecState<T> {
        unsafe { alloc(Layout::new::<Self>()).cast() }
    }

    unsafe fn dealloc(ptr: *mut JoinVecState<T>) {
        dealloc(ptr.cast(), Layout::new::<Self>())
    }
}
