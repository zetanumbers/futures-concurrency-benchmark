use std::{
    cell::UnsafeCell,
    future::Future,
    marker::PhantomData,
    mem,
    pin::Pin,
    ptr,
    sync::atomic::{self, AtomicPtr, AtomicUsize},
    task,
};

pub mod raw;

pub struct Join<F>
where
    F: Future,
{
    base: ptr::NonNull<erased::JoinImpl>,
    _marker: PhantomData<ptr::NonNull<mem::ManuallyDrop<F>>>,
}

// TODO: impl Sync, Send for Join

// TODO: use `F: IntoFuture`
// TODO: FromIterator
impl<F> Join<F>
where
    F: Future,
{
    pub fn from_vec(v: Vec<F>) -> Self {
        let entry_count = v.len();
        let base = erased::alloc_join_impl::<F>(entry_count);

        let header = erased::header(base.as_ptr());
        let entries = unsafe { erased::entries::<F>(base.as_ptr()) };
        unsafe {
            let end_entry = entries.add(entry_count - 1);
            header.write(JoinHeader {
                allocation_rc: AtomicUsize::new(1),
                entry_count,
                pending_entry_count: entry_count,
                extern_waker: UnsafeCell::new(mem::MaybeUninit::uninit()),
                last_to_poll: AtomicPtr::new(erased::erase_entry(end_entry)),
                root_entry: EntrySubHeader {
                    next_scheduled: AtomicPtr::new(erased::erase_entry(entries)),
                },
            })
        };

        for (i, future) in v.into_iter().enumerate() {
            unsafe {
                entries.add(i).write(Entry {
                    header: EntryHeader {
                        inner: EntrySubHeader {
                            next_scheduled: AtomicPtr::new(erased::erase_entry(entries.add(i + 1))),
                        },
                        base,
                    },
                    future: mem::ManuallyDrop::new(future),
                })
            }
        }

        Join {
            base,
            _marker: PhantomData,
        }
    }
}

impl<F> Drop for Join<F>
where
    F: Future,
{
    fn drop(&mut self) {
        let entry_count = self.len();
        let entries = self.entries();

        // Dropping futures
        for i in 0..entry_count {
            unsafe { mem::ManuallyDrop::drop(&mut (*entries.add(i)).future) }
        }

        unsafe { dec_rc::<F>(self.base.as_ptr()) };
    }
}

impl<F> Join<F>
where
    F: Future,
{
    #[inline(always)]
    fn header(&self) -> *mut JoinHeader {
        erased::header(self.base.as_ptr())
    }

    #[inline(always)]
    fn entries(&self) -> *mut Entry<F> {
        unsafe { erased::entries(self.base.as_ptr()) }
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        unsafe { (*self.header()).entry_count }
    }
}

impl<F> Future for Join<F>
where
    F: Future,
{
    // TODO: Output should be something that satisfies `IntoIterator<Item = F::Output>`
    // TODO: Collect output within the entry in place of the finished future
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> task::Poll<Self::Output> {
        unsafe {
            let new_waker = cx.waker();
            let header = self.header();
            let entry_count = (*header).entry_count;
            let entries = self.entries();
            let end_entry = erased::erase_entry(entries.add(entry_count));
            let root_entry = erased::erase_entry_header(ptr::addr_of_mut!((*header).root_entry));
            let buffer_entry =
                erased::erase_entry_header(ptr::addr_of_mut!((*header).buffer_entry));

            // Schedule buffer entry if empty
            let mut last = (*header).last_to_poll.load(atomic::Ordering::Relaxed);

            let mut buffer_used = false;

            if last == root_entry {
                match (*header).last_to_poll.compare_exchange(
                    last,
                    buffer_entry,
                    // There's no need for synchronization on success
                    // because `&mut self` is synchronized already.
                    atomic::Ordering::Relaxed,
                    atomic::Ordering::Relaxed,
                ) {
                    Err(new_last) => {
                        last = new_last;
                        // Found another first entry, extern_waker was
                        // uninitialized by waking but now it's place is
                        // no longer accessible to anyone except for us.
                        (*header).extern_waker.write(new_waker.clone());
                    }
                    Ok(_) => {
                        buffer_used = true;
                        // Pushed buffered entry and acquired ownership
                        // of the old extern_waker.
                        let old_waker = (*header).extern_waker.assume_init_mut();
                        if !old_waker.will_wake(new_waker) {
                            old_waker.clone_from(new_waker);
                        }
                    }
                }
            }

            // Root entry is accessable exclusivelly by us
            let first = mem::replace((*header).root_entry.next_scheduled.get_mut(), end_entry);
            (*header)
                .last_to_poll
                .swap(root_entry, atomic::Ordering::AcqRel);

            // Remove the buffer entry
            if buffer_used {
                first
            }

            todo!()
        }
    }
}

struct JoinHeader {
    allocation_rc: AtomicUsize,
    // TODO: consider atomic contention
    entry_count: usize,
    pending_entry_count: usize,
    extern_waker: mem::MaybeUninit<task::Waker>,
    last_to_poll: AtomicPtr<erased::Entry>,
    root_entry: EntrySubHeader,
    buffer_entry: EntrySubHeader,
}

struct EntrySubHeader {
    next_scheduled: AtomicPtr<erased::Entry>,
}

/// Used in wakers
#[repr(C)] // We assume casting erased::Entry pointer to EntryHeader pointer is valid to access entry header
struct EntryHeader {
    inner: EntrySubHeader,
    base: ptr::NonNull<erased::JoinImpl>,
}

#[repr(C)] // We assume casting erased::Entry pointer to EntryHeader pointer is valid to access entry header
struct Entry<T> {
    header: EntryHeader,
    future: mem::ManuallyDrop<T>,
}

unsafe fn inc_rc(base: *mut erased::JoinImpl) {
    unsafe {
        (*erased::header(base))
            .allocation_rc
            .fetch_add(1, atomic::Ordering::Acquire);
    }
}

unsafe fn dec_rc<T>(base: *mut erased::JoinImpl) {
    let header = erased::header(base);
    let old_alloc_rc = unsafe {
        (*header)
            .allocation_rc
            .fetch_sub(1, atomic::Ordering::Release)
    };
    if old_alloc_rc == 1 {
        atomic::fence(atomic::Ordering::Acquire);
        unsafe { erased::dealloc_join_impl::<T>(base, (*header).entry_count) }
    }
}

mod erased {
    use std::{
        alloc::{self, alloc, dealloc},
        marker::PhantomPinned,
        mem, ptr,
    };

    pub struct JoinImpl {
        _data: u8,
        _pinned: PhantomPinned,
    }

    pub struct Entry {
        _data: u8,
        _pinned: PhantomPinned,
    }

    pub const fn header_entry(entry: *mut Entry) -> *mut super::EntrySubHeader {
        entry.cast()
    }

    pub const fn erase_entry_header(header: *mut super::EntrySubHeader) -> *mut Entry {
        header.cast()
    }

    pub const fn erase_entry<T>(entry: *mut super::Entry<T>) -> *mut Entry {
        entry.cast()
    }

    #[inline(always)]
    pub const fn header(base: *mut JoinImpl) -> *mut super::JoinHeader {
        base.cast()
    }

    #[inline]
    pub const unsafe fn entries<T>(base: *mut JoinImpl) -> *mut super::Entry<T> {
        base.add(entries_offset::<T>()).cast()
    }

    pub fn alloc_join_impl<T>(entry_count: usize) -> ptr::NonNull<JoinImpl> {
        let layout = join_impl_layout::<T>(entry_count);
        // SAFETY: size is always non zero because of header data
        let Some(base) = ptr::NonNull::new(unsafe { alloc(layout) }) else {
            alloc::handle_alloc_error(layout)
        };
        base.cast()
    }

    pub unsafe fn dealloc_join_impl<T>(base: *mut JoinImpl, entry_count: usize) {
        let layout = join_impl_layout::<T>(entry_count);
        unsafe { dealloc(base.cast(), layout) }
    }

    pub const fn join_impl_layout<T>(entry_count: usize) -> alloc::Layout {
        let entries_size = entry_count * mem::size_of::<super::Entry<T>>();

        unsafe {
            alloc::Layout::from_size_align_unchecked(
                const { entries_offset::<T>() } + entries_size,
                const {
                    max(
                        checked_align_of::<super::Entry<T>>(),
                        checked_align_of::<super::JoinHeader>(),
                    )
                },
            )
        }
    }

    const fn entries_offset<T>() -> usize {
        let entry_align = checked_align_of::<super::Entry<T>>();
        let header_size = mem::size_of::<super::JoinHeader>();

        header_size.next_multiple_of(entry_align)
    }

    const fn max(a: usize, b: usize) -> usize {
        if a < b {
            b
        } else {
            a
        }
    }

    const fn checked_align_of<T>() -> usize {
        let align = mem::align_of::<T>();

        if !align.is_power_of_two() {
            panic!();
        }

        align
    }
}
