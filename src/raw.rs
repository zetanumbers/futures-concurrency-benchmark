use std::{
    alloc,
    future::Future,
    marker::PhantomData,
    mem,
    pin::Pin,
    ptr,
    sync::atomic::{self, AtomicPtr, AtomicUsize},
    task,
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

    unsafe fn force_poll(self)
    where
        T: Future,
    {
        let entries = self.entries();
        for i in 0..self.entry_count {
            let entry = entries.add(i);
            let waker = Entry::weak_waker(entry);
            let mut cx = task::Context::from_waker(&waker);
            // TODO: react to output
            Pin::new_unchecked((*entry).future.assume_init_mut()).poll(&mut cx);
        }
    }

    unsafe fn poll_scheduled(self)
    where
        T: Future,
    {
        // TODO: both taking scheduled entries (by one) and polling them are
        // easily paralellizable
        let header = self.header();
        let first_entry = header
            .first_to_poll
            .swap(ptr::null_mut(), atomic::Ordering::Relaxed);

        if first_entry.is_null() {
            return;
        }

        let _last_entry = header
            .last_to_poll
            .swap(ptr::null_mut(), atomic::Ordering::AcqRel);
        debug_assert!(!_last_entry.is_null());
        debug_assert_eq!(
            (*_last_entry)
                .is_scheduled_and_next
                .load(atomic::Ordering::Relaxed),
            scheduled_no_next()
        );

        let mut current_entry = first_entry;
        while current_entry != scheduled_no_next() {
            debug_assert!(!current_entry.is_null());

            let waker = Entry::weak_waker(current_entry);
            let mut cx = task::Context::from_waker(&waker);
            // TODO: react to output
            Pin::new_unchecked((*current_entry).future.assume_init_mut()).poll(&mut cx);

            current_entry = (*current_entry)
                .is_scheduled_and_next
                .load(atomic::Ordering::Relaxed);
        }
    }

    unsafe fn header<'a>(self) -> &'a Header<T> {
        self.state.cast().as_ref()
    }

    unsafe fn entries(self) -> *mut Entry<T> {
        let (_, offset) = Self::layout_and_entries_offset(self.entry_count).unwrap();
        let ptr = self.state.as_ptr().add(offset).cast::<Entry<T>>();
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
                    // TODO: can use root entry instead of null
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
    const WAKER_VTABLE: task::RawWakerVTable = {
        // TODO: Cannot yet erase T since it could influence struct layouts
        unsafe fn clone<T>(entry: *const ()) -> task::RawWaker {
            (*Entry::header(entry.cast::<Entry<T>>())).inc_rc();
            task::RawWaker::new(entry, &Entry::<T>::WAKER_VTABLE)
        }

        unsafe fn wake<T>(entry: *const ()) {
            wake_by_ref::<T>(entry);
            drop::<T>(entry);
        }

        unsafe fn wake_by_ref<T>(entry: *const ()) {
            let entry = entry as *mut Entry<T>;
            let header = &Entry::header(entry);
            header.schedule_to_poll(entry);
            header.extern_waker.wake();
        }

        unsafe fn drop<T>(entry: *const ()) {
            Entry::handle(entry.cast::<Entry<T>>()).dec_rc();
        }

        task::RawWakerVTable::new(clone::<T>, wake::<T>, wake_by_ref::<T>, drop::<T>)
    };

    unsafe fn header<'a>(this: *const Self) -> &'a Header<T> {
        (*this).header.cast().as_ref()
    }

    unsafe fn handle(this: *const Self) -> RawSharedHandle<T> {
        Header::handle((*this).header.as_ptr())
    }

    unsafe fn write_future(this: *mut Self, future: T) {
        (*this).future.write(future);
    }

    unsafe fn drop_future(this: *mut Self) {
        (*this).future.assume_init_drop();
    }

    unsafe fn strong_waker(this: *mut Self) -> task::Waker {
        (*Entry::header(this)).inc_rc();
        let waker = Self::weak_waker(this);
        mem::ManuallyDrop::into_inner(waker)
    }

    unsafe fn weak_waker(this: *mut Self) -> mem::ManuallyDrop<task::Waker> {
        mem::ManuallyDrop::new(task::Waker::from_raw(task::RawWaker::new(
            this.cast(),
            &Self::WAKER_VTABLE,
        )))
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
    }
}
