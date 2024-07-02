#![no_std]
#![allow(unexpected_cfgs)]
#![deny(clippy::std_instead_of_core, clippy::alloc_instead_of_core)]

extern crate alloc;

use core::{future::Future, hint, marker::PhantomData, mem, pin::Pin, ptr, task};

#[cfg(not(alt_join_loom))]
use core::sync::atomic::{self, AtomicPtr, AtomicUsize};

#[cfg(alt_join_loom)]
use loom::sync::atomic::{self, AtomicPtr, AtomicUsize};

use fused_future::FusedFuture;
use noop_waker::noop_waker;

mod fused_future;

const MAX_REFCOUNT: usize = isize::MAX as usize;

pub struct Join<F>
where
    F: Future,
{
    base: ptr::NonNull<erased::JoinImpl>,
    _marker: PhantomData<ptr::NonNull<mem::ManuallyDrop<F>>>,
}

unsafe impl<F> Send for Join<F> where F: Future + Send {}
unsafe impl<F> Sync for Join<F> where F: Future + Sync {}

static mut RESERVE_END_ENTRY_ADDR: mem::MaybeUninit<u8> = mem::MaybeUninit::uninit();
#[inline(always)]
fn end_entry() -> *mut erased::Entry {
    unsafe { ptr::addr_of_mut!(RESERVE_END_ENTRY_ADDR).cast() }
}

// TODO: use `F: IntoFuture`
// TODO: FromIterator
// TODO: consider atomic consume ordering
// TODO: Fences aren't supported by the ThreadSanitizer
impl<F> Join<F>
where
    F: Future,
{
    pub fn from_iterable<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = F>,
        I::IntoIter: ExactSizeIterator,
    {
        // TODO: Does this affect performance?
        // Convince the compiler to reserve a unique address
        hint::black_box(end_entry());
        let mut iter = iter.into_iter();
        let entry_count = iter.len();
        let base = erased::alloc_join_impl::<F>(entry_count);

        let header = erased::header(base.as_ptr());
        let entries = unsafe { erased::entries::<F>(base.as_ptr()) };
        let root_entry =
            erased::erase_entry_subheader(unsafe { ptr::addr_of_mut!((*header).root_entry) });
        let last_entry = unsafe {
            entry_count
                .checked_sub(1)
                .map_or(root_entry, |i| erased::erase_entry(entries.add(i)))
        };
        let end_entry = end_entry();
        unsafe {
            header.write(JoinHeader {
                allocation_rc: AtomicUsize::new(1),
                entry_count,
                pending_entry_count: entry_count,
                extern_waker: mem::MaybeUninit::new(noop_waker()),
                last_to_poll: AtomicPtr::new(last_entry),
                root_entry: EntrySubHeader {
                    next_scheduled: AtomicPtr::new(if entry_count != 0 {
                        erased::erase_entry(entries)
                    } else {
                        // TODO: verify
                        // null pointer are easy to calculate instead of one past end pointer
                        // in wakers
                        // TODO: sentinel value
                        end_entry
                    }),
                },
                // TODO: root RCU instead?
                buffer_entry: EntrySubHeader {
                    next_scheduled: AtomicPtr::new(ptr::null_mut()),
                },
            })
        };

        if entry_count > 0 {
            let mut init_entry_next = |i, next_scheduled| {
                let Some(future) = iter.next() else {
                    unsafe {
                        for j in 0..i {
                            mem::ManuallyDrop::drop(&mut (*entries.add(j)).future);
                        }
                        erased::dealloc_join_impl::<F>(base.as_ptr(), entry_count)
                    }
                    panic!("input iterator had faulty `len` method implementation, such that it didn't provide specified number of values")
                };
                unsafe {
                    entries.add(i).write(Entry {
                        header: EntryHeader {
                            inner: EntrySubHeader {
                                next_scheduled: AtomicPtr::new(next_scheduled),
                            },
                            base,
                        },
                        future: mem::ManuallyDrop::new(FusedFuture::new(future)),
                    })
                }
            };

            unsafe {
                for i in 0..entry_count - 1 {
                    init_entry_next(i, erased::erase_entry(entries.add(i + 1)));
                }
                init_entry_next(entry_count - 1, end_entry);

                assert!(iter.next().is_none(), "input iterator had faulty `len` method implementation, such that iterator provided too many values");
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
        unsafe {
            let header = self.header();
            let entries = self.entries();
            let entry_count = (*header).entry_count;

            let buffer_entry = ptr::addr_of_mut!((*header).buffer_entry);
            debug_assert_eq!(
                // buffer_entry is operated only when `&mut self` is present and isn't reborrowed
                (*buffer_entry)
                    .next_scheduled
                    .load(atomic::Ordering::Relaxed),
                ptr::null_mut(),
            );
            let buffer_entry = erased::erase_entry_subheader(buffer_entry);
            schedule_entry(buffer_entry, self.base.as_ptr());

            // Dropping futures
            for i in 0..entry_count {
                mem::ManuallyDrop::drop(&mut (*entries.add(i)).future)
            }

            dec_rc::<F>(self.base.as_ptr());
            // TODO: Does this affect performance?
            // Convince the compiler to reserve a unique address
            hint::black_box(end_entry());
        }
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
    pub fn count(&self) -> usize {
        unsafe { (*self.header()).entry_count }
    }

    #[inline(always)]
    pub fn pending_count(&self) -> usize {
        unsafe { (*self.header()).pending_entry_count }
    }

    #[inline(always)]
    pub fn is_complete(&self) -> bool {
        self.pending_count() == 0
    }
}

impl<F> Future for Join<F>
where
    F: Future,
{
    // TODO: Output should be something that satisfies `IntoIterator<Item = F::Output>`
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> task::Poll<Self::Output> {
        unsafe {
            let base = self.base.as_ptr();
            let new_waker = cx.waker();
            let header = self.header();
            let root_entry = erased::erase_entry_subheader(ptr::addr_of_mut!((*header).root_entry));
            let buffer_entry =
                erased::erase_entry_subheader(ptr::addr_of_mut!((*header).buffer_entry));

            // buffer_entry is synchronized with `&mut self`
            #[cfg(not(alt_join_loom))]
            debug_assert_eq!(
                *(*header).buffer_entry.next_scheduled.get_mut(),
                ptr::null_mut()
            );

            #[cfg(alt_join_loom)]
            debug_assert_eq!(
                (*header)
                    .buffer_entry
                    .next_scheduled
                    .load(atomic::Ordering::Relaxed),
                ptr::null_mut()
            );

            if (*header).pending_entry_count == 0 {
                panic!("Polled the already completed `Join`")
            }

            let intermediate = schedule_entry(buffer_entry, base).unwrap();

            // TODO: consider to not "stuff" if anything is already scheduled
            // We have "stuffed" the buffer entry to dissallow wakers to access the extern_waker.

            if intermediate == root_entry {
                let extern_waker = (*header).extern_waker.assume_init_mut();
                if !extern_waker.will_wake(new_waker) {
                    extern_waker.clone_from(new_waker);
                }
            } else {
                (*header).extern_waker.write(new_waker.clone());
            }
            // Release extern_waker
            atomic::fence(atomic::Ordering::Release);

            // Any waker that accesses root will schedule its future in the next batch.
            let mut first = (*header)
                .root_entry
                .next_scheduled
                .swap(ptr::null_mut(), atomic::Ordering::Relaxed);

            schedule_entry(root_entry, base);

            // Root entry is now at the top, no need to update last_to_poll
            let next_intermediate = (*erased::entry_subheader(buffer_entry))
                .next_scheduled
                .swap(ptr::null_mut(), atomic::Ordering::Relaxed);
            if intermediate != next_intermediate {
                (*erased::entry_subheader(intermediate))
                    .next_scheduled
                    .store(next_intermediate, atomic::Ordering::Relaxed);
            }
            if first == buffer_entry {
                first = next_intermediate;
            }

            let mut current = first;
            while current != root_entry {
                let current_entry = erased::entry::<F>(current);
                let next = (*current_entry)
                    .header
                    .inner
                    .next_scheduled
                    .swap(ptr::null_mut(), atomic::Ordering::Relaxed);

                if !(*current_entry).future.is_ready() {
                    let waker = weak_waker::<F>(current);
                    let mut cx = task::Context::from_waker(&waker);
                    // TODO: Do no ignore the output:
                    if Pin::new_unchecked(&mut *(*current_entry).future)
                        .poll(&mut cx)
                        .is_ready()
                    {
                        (*header).pending_entry_count -= 1;
                    }
                }

                current = next;
            }

            if (*header).pending_entry_count == 0 {
                task::Poll::Ready(())
            } else {
                task::Poll::Pending
            }
        }
    }
}

// TODO: consider erasing type param to reduce code size
/// Weak waker itself does not increment allocation reference counter, but it's clone would.
unsafe fn weak_waker<F>(current: *mut erased::Entry) -> mem::ManuallyDrop<task::Waker>
where
    F: Future,
{
    unsafe fn clone<T: Future>(entry: *const ()) -> task::RawWaker {
        inc_rc((*erased::entry_header(entry as _)).base.as_ptr());
        task::RawWaker::new(entry, const { &vtable::<T>() })
    }

    unsafe fn wake<T: Future>(entry: *const ()) {
        wake_by_ref(entry);
        drop_waker::<T>(entry);
    }

    // TODO: consider the possiblity that adding future's generic type might help with performance
    // because there would be many monomorphized copies of this function performing only on their
    // designated type, thus, perhaps, helping the branch predictor.
    unsafe fn wake_by_ref(entry: *const ()) {
        let entry = entry as _;
        let entry_header = erased::entry_header(entry);

        let base = (*entry_header).base.as_ptr();
        let header = erased::header(base);

        let Some(last) = schedule_entry(entry, base) else {
            return;
        };

        let root = erased::erase_entry_subheader(ptr::addr_of_mut!((*header).root_entry));
        if last == root {
            // Scheduled first, so we are in our right to wake the external waker
            atomic::fence(atomic::Ordering::Acquire);
            (*header).extern_waker.assume_init_read().wake()
        }
    }

    unsafe fn drop_waker<T: Future>(entry: *const ()) {
        dec_rc::<T>((*erased::entry_header(entry as _)).base.as_ptr());
    }

    const fn vtable<F: Future>() -> task::RawWakerVTable {
        task::RawWakerVTable::new(clone::<F>, wake::<F>, wake_by_ref, drop_waker::<F>)
    }

    let raw = task::RawWaker::new(current.cast::<()>(), const { &vtable::<F>() });
    mem::ManuallyDrop::new(task::Waker::from_raw(raw))
}

unsafe fn schedule_entry(
    entry: *mut erased::Entry,
    base: *mut erased::JoinImpl,
) -> Option<*mut erased::Entry> {
    let header = erased::header(base);
    let end_entry = end_entry();

    if (*erased::entry_subheader(entry))
        .next_scheduled
        .compare_exchange(
            ptr::null_mut(),
            end_entry,
            atomic::Ordering::Relaxed,
            atomic::Ordering::Relaxed,
        )
        .is_err()
    {
        // Current future is already scheduled
        return None;
    }

    let mut last;
    'reload_last: loop {
        // "Acquire" the last.next_scheduled writes
        last = (*header).last_to_poll.load(atomic::Ordering::Acquire);
        while let Err(new_last) = (*erased::entry_subheader(last))
            .next_scheduled
            .compare_exchange_weak(
                end_entry,
                entry,
                atomic::Ordering::Relaxed,
                atomic::Ordering::Relaxed,
            )
        {
            if !new_last.is_null() {
                // We are racing against join's poll. We ensured our future is not scheduled
                // and then marked it as such, but not yet scheduled it. But join's poll
                // already got to current "last" scheduled future, so no way to schedule in
                // this batch. We need to reload last pointer and try the next batch.
                continue 'reload_last;
            }
            last = new_last;
        }
        break 'reload_last;
    }

    while (*header)
        .last_to_poll
        .compare_exchange_weak(
            last,
            entry,
            // "Release" the last.next_scheduled and entry.next_scheduled entry writes
            atomic::Ordering::Release,
            atomic::Ordering::Relaxed,
        )
        .is_err()
    {
        // Someone who scheduled some previous entry is currently "releases"
        // memory effects, they will eventually exchange last_to_poll
        // pointer to one we expect.
        // TODO: consider std::thread::yield_now
        // TODO: consider crossbeam::utils::Backoff
        core::hint::spin_loop()
    }

    Some(last)
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
// We assume casting erased::Entry pointer to EntrySubHeader pointer is valid to access entry header
#[repr(C)]
struct EntryHeader {
    inner: EntrySubHeader,
    // TODO: This pointer can be made relative and reduced in size
    base: ptr::NonNull<erased::JoinImpl>,
}

// TODO: allow user to access this type through slice from JoinOutput, but be carefull with moves
// We assume casting erased::Entry pointer to EntryHeader pointer is valid to access entry header
#[repr(C)]
struct Entry<T>
where
    T: Future,
{
    header: EntryHeader,
    future: mem::ManuallyDrop<FusedFuture<T>>,
}

unsafe fn inc_rc(base: *mut erased::JoinImpl) {
    // FIXME?: Commented out formally sound code, which replaces practically sound code
    let old_alloc_rc = unsafe {
        (*erased::header(base))
            .allocation_rc
            .fetch_add(1, atomic::Ordering::Relaxed)
        // (*erased::header(base))
        //     .allocation_rc
        //     .load(atomic::Ordering::Relaxed)
    };
    if old_alloc_rc > MAX_REFCOUNT {
        // https://github.com/rust-lang/rust/issues/67952
        struct ForceAbort;
        impl Drop for ForceAbort {
            fn drop(&mut self) {
                panic!("Aborting");
            }
        }
        let _abort = ForceAbort;
        panic!("Too many wakers, as some wakers got leaked");
    }
    // while (*erased::header(base))
    //     .allocation_rc
    //     .compare_exchange_weak(
    //         old_alloc_rc,
    //         old_alloc_rc + 1,
    //         atomic::Ordering::Relaxed,
    //         atomic::Ordering::Relaxed,
    //     )
    //     .is_err()
    // {
    //     hint::spin_loop()
    // }
}

unsafe fn dec_rc<T: Future>(base: *mut erased::JoinImpl) {
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
    use alloc::alloc::{alloc, dealloc, handle_alloc_error};
    use core::{alloc::Layout, future::Future, marker::PhantomPinned, mem, ptr};

    #[repr(transparent)]
    pub struct JoinImpl {
        _header: super::JoinHeader,
        _pinned: PhantomPinned,
    }

    #[repr(transparent)]
    pub struct Entry {
        _header: super::EntryHeader,
        _pinned: PhantomPinned,
    }

    pub const fn entry_subheader(entry: *mut Entry) -> *mut super::EntrySubHeader {
        entry.cast()
    }

    pub const fn entry_header(entry: *mut Entry) -> *mut super::EntryHeader {
        entry.cast()
    }

    pub const fn entry<T: Future>(entry: *mut Entry) -> *mut super::Entry<T> {
        entry.cast()
    }

    pub const fn erase_entry_subheader(header: *mut super::EntrySubHeader) -> *mut Entry {
        header.cast()
    }

    pub const fn erase_entry<T: Future>(entry: *mut super::Entry<T>) -> *mut Entry {
        entry.cast()
    }

    #[inline(always)]
    pub const fn header(base: *mut JoinImpl) -> *mut super::JoinHeader {
        base.cast()
    }

    #[inline]
    pub const unsafe fn entries<T: Future>(base: *mut JoinImpl) -> *mut super::Entry<T> {
        base.byte_add(entries_offset::<T>()).cast()
    }

    pub fn alloc_join_impl<T: Future>(entry_count: usize) -> ptr::NonNull<JoinImpl> {
        let layout = join_impl_layout::<T>(entry_count);
        // SAFETY: size is always non zero because of header data
        let Some(base) = ptr::NonNull::new(unsafe { alloc(layout) }) else {
            handle_alloc_error(layout)
        };
        base.cast()
    }

    pub unsafe fn dealloc_join_impl<T: Future>(base: *mut JoinImpl, entry_count: usize) {
        let layout = join_impl_layout::<T>(entry_count);
        unsafe { dealloc(base.cast(), layout) }
    }

    pub const fn join_impl_layout<T: Future>(entry_count: usize) -> Layout {
        let entries_size = entry_count * mem::size_of::<super::Entry<T>>();

        unsafe {
            Layout::from_size_align_unchecked(
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

    const fn entries_offset<T: Future>() -> usize {
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
