use core::{slice, task};
use std::{
    alloc,
    marker::PhantomData,
    mem, ptr,
    sync::atomic::{self, AtomicPtr, AtomicUsize},
};

use atomic_waker::AtomicWaker;

struct RawSharedHandle<T> {
    state: ptr::NonNull<u8>,
    entry_count: usize,
    _marker: PhantomData<T>,
}

impl<T> Clone for RawSharedHandle<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for RawSharedHandle<T> {}

impl<T> RawSharedHandle<T> {
    fn alloc(entry_count: usize) -> Self {
        let (layout, _) = Self::layout_and_entries_offset(entry_count).unwrap();
        // SAFETY: Header contains data so layout cannot be zero sized
        let Some(state) = ptr::NonNull::new(unsafe { alloc::alloc(layout) }) else {
            alloc::handle_alloc_error(layout)
        };
        RawSharedHandle {
            state,
            entry_count,
            _marker: PhantomData,
        }
    }

    unsafe fn inc_rc(self) {
        (*self.header()).inc_rc()
    }

    unsafe fn dec_rc(self) {
        let old_rc = (*self.header())
            .allocation_rc
            .fetch_sub(1, atomic::Ordering::Release);
        if old_rc == 1 {
            atomic::fence(atomic::Ordering::Acquire);
            self.dealloc()
        }
    }

    unsafe fn dealloc(self) {
        let (layout, _) = Self::layout_and_entries_offset(self.entry_count).unwrap();
        unsafe { alloc::dealloc(self.state.as_ptr(), layout) };
    }

    unsafe fn drop_futures(self) {
        let entries = self.entries();
        for i in 0..self.entry_count {
            Entry::drop_future(entries.add(i))
        }
    }

    unsafe fn force_poll(self) {
        let entries = self.entries();
        for i in 0..self.entry_count {
            todo!()
        }
    }

    fn header(self) -> *const Header<T> {
        self.state.cast().as_ptr()
    }

    fn entries(self) -> *mut Entry<T> {
        let (_, offset) = Self::layout_and_entries_offset(self.entry_count).unwrap();
        let ptr = unsafe { self.state.as_ptr().add(offset).cast::<Entry<T>>() };
        ptr
    }

    #[inline]
    fn layout_and_entries_offset(
        entry_count: usize,
    ) -> Result<(alloc::Layout, usize), alloc::LayoutError> {
        alloc::Layout::new::<Header<T>>().extend(alloc::Layout::array::<Entry<T>>(entry_count)?)
    }
}

struct Header<T> {
    entry_count: usize,
    // TODO: Consider adding per waker rc to entry to reduce contention
    allocation_rc: AtomicUsize,
    extern_waker: AtomicWaker,
    first_to_poll: AtomicPtr<Entry<T>>,
    last_to_poll: AtomicPtr<Entry<T>>,
}

impl<T> Header<T> {
    fn new(entry_count: usize) -> Self {
        Header {
            entry_count,
            allocation_rc: AtomicUsize::new(1),
            extern_waker: AtomicWaker::new(),
            first_to_poll: AtomicPtr::new(ptr::null_mut()),
            last_to_poll: AtomicPtr::new(ptr::null_mut()),
        }
    }

    // NOTE: unsafe to avoid overflows which would mess up things
    unsafe fn inc_rc(&self) {
        (*self)
            .allocation_rc
            .fetch_add(1, atomic::Ordering::Acquire);
    }

    unsafe fn handle(this: *mut u8) -> RawSharedHandle<T> {
        RawSharedHandle {
            state: ptr::NonNull::new_unchecked(this),
            entry_count: (*this.cast::<Self>()).entry_count,
            _marker: PhantomData,
        }
    }

    unsafe fn schedule_to_poll(&self, entry: *mut Entry<T>) {
        // Ensure we schedule entry once
        let is_scheduled = (*entry)
            .is_scheduled_and_next
            .compare_exchange(
                ptr::null_mut(),
                scheduled_no_next(),
                atomic::Ordering::Relaxed,
                atomic::Ordering::Relaxed,
            )
            .is_err();
        if is_scheduled {
            return;
        }

        while self
            .last_to_poll
            .fetch_update(
                atomic::Ordering::Release,
                atomic::Ordering::Acquire,
                |prev_entry| {
                    let res = if prev_entry.is_null() {
                        self.first_to_poll.compare_exchange(
                            ptr::null_mut(),
                            entry,
                            atomic::Ordering::Relaxed,
                            atomic::Ordering::Relaxed,
                        )
                    } else {
                        (*prev_entry).is_scheduled_and_next.compare_exchange(
                            scheduled_no_next(),
                            entry,
                            atomic::Ordering::Relaxed,
                            atomic::Ordering::Relaxed,
                        )
                    };
                    match res {
                        Ok(_) => Some(entry),
                        Err(_) => None,
                    }
                },
            )
            .is_err()
        {}
    }
}

struct Entry<T> {
    header: ptr::NonNull<u8>,
    is_scheduled_and_next: AtomicPtr<Entry<T>>,
    future: mem::MaybeUninit<T>,
}

/// Get sentinel value
#[inline]
fn scheduled_no_next<T>() -> *mut Entry<T> {
    unsafe { ptr::addr_of_mut!(RESERVE_SCHEDULED_NO_NEXT_ADDR).cast() }
}

static mut RESERVE_SCHEDULED_NO_NEXT_ADDR: mem::MaybeUninit<u8> = mem::MaybeUninit::uninit();

impl<T> Entry<T> {
    const WAKER_VTABLE: task::RawWakerVTable = task::RawWakerVTable::new({
        unsafe fn clone<T>(entry: *const ()) -> task::RawWaker {
            (*Entry::header(entry.cast::<Entry<T>>())).inc_rc();
            task::RawWaker {
                data: entry,
                vtable: Entry<T>::WAKER_VTABLE,
            }
        }
        clone::<T>
    });

    unsafe fn header(this: *const Self) -> *const Header<T> {
        (*this).header.cast().as_ptr()
    }

    unsafe fn write_future(this: *mut Self, future: T) {
        (*this).future.write(future);
    }

    unsafe fn drop_future(this: *mut Self) {
        (*this).future.assume_init_drop();
    }

    unsafe fn weak_waker(this: *const Self) -> mem::ManuallyDrop<task::Waker> {
        // TODO: Do not inc_rc just forget the waker
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts() {
        assert_eq!(mem::size_of::<()>(), 0);
        assert_ne!(mem::size_of::<Header<()>>(), 0);
    }

    #[test]
    fn needs_drop() {
        assert!(mem::needs_drop::<String>());
        assert!(!mem::needs_drop::<Entry<String>>());
        assert!(!mem::needs_drop::<Header<String>>());
    }
}
