//! An unbounded set of futures.
//!
//! This module is only available when the `std` or `alloc` feature of this
//! library is activated, and it is activated by default.

use crate::task::AtomicWaker;
use alloc::sync::{Arc, Weak};
use core::cell::UnsafeCell;
use core::cmp;
use core::fmt::{self, Debug};
use core::iter::FromIterator;
use core::marker::PhantomData;
use core::mem;
use core::pin::Pin;
use core::ptr;
use core::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release, SeqCst};
use core::sync::atomic::{AtomicBool, AtomicPtr};
use futures_core::future::Future;
use futures_core::stream::{FusedStream, Stream};
use futures_core::task::{Context, Poll};
use futures_task::{FutureObj, LocalFutureObj, LocalSpawn, Spawn, SpawnError};

mod abort;

mod iter;
pub use self::iter::{IntoIter, Iter, IterMut, IterPinMut, IterPinRef};

mod task;
use self::task::Task;

mod ready_to_run_queue;
use self::ready_to_run_queue::{Dequeue, ReadyToRunQueue};

/// Constant used for a `FuturesUnordered` to determine how many times it is
/// allowed to poll underlying futures without yielding.
///
/// A single call to `poll_next` may potentially do a lot of work before
/// yielding. This happens in particular if the underlying futures are awoken
/// frequently but continue to return `Pending`. This is problematic if other
/// tasks are waiting on the executor, since they do not get to run. This value
/// caps the number of calls to `poll` on underlying futures a single call to
/// `poll_next` is allowed to make.
///
/// The value itself is chosen somewhat arbitrarily. It needs to be high enough
/// that amortize wakeup and scheduling costs, but low enough that we do not
/// starve other tasks for long.
///
/// See also https://github.com/rust-lang/futures-rs/issues/2047.
///
/// Note that using the length of the `FuturesUnordered` instead of this value
/// may cause problems if the number of futures is large.
/// See also https://github.com/rust-lang/futures-rs/pull/2527.
///
/// Additionally, polling the same future twice per iteration may cause another
/// problem. So, when using this value, it is necessary to limit the max value
/// based on the length of the `FuturesUnordered`.
/// (e.g., `cmp::min(self.len(), YIELD_EVERY)`)
/// See also https://github.com/rust-lang/futures-rs/pull/2333.
const YIELD_EVERY: usize = 32;

/// A set of futures which may complete in any order.
///
/// This structure is optimized to manage a large number of futures.
/// Futures managed by [`FuturesUnordered`] will only be polled when they
/// generate wake-up notifications. This reduces the required amount of work
/// needed to poll large numbers of futures.
///
/// [`FuturesUnordered`] can be filled by [`collect`](Iterator::collect)ing an
/// iterator of futures into a [`FuturesUnordered`], or by
/// [`push`](FuturesUnordered::push)ing futures onto an existing
/// [`FuturesUnordered`]. When new futures are added,
/// [`poll_next`](Stream::poll_next) must be called in order to begin receiving
/// wake-ups for new futures.
///
/// Note that you can create a ready-made [`FuturesUnordered`] via the
/// [`collect`](Iterator::collect) method, or you can start with an empty set
/// with the [`FuturesUnordered::new`] constructor.
///
/// This type is only available when the `std` or `alloc` feature of this
/// library is activated, and it is activated by default.
#[must_use = "streams do nothing unless polled"]
pub struct FuturesUnordered<Fut> {
    ready_to_run_queue: Arc<ReadyToRunQueue<Fut>>,
    head_all: AtomicPtr<Task<Fut>>,
    is_terminated: AtomicBool,
}

unsafe impl<Fut: Send> Send for FuturesUnordered<Fut> {}
unsafe impl<Fut: Sync> Sync for FuturesUnordered<Fut> {}
impl<Fut> Unpin for FuturesUnordered<Fut> {}

impl Spawn for FuturesUnordered<FutureObj<'_, ()>> {
    fn spawn_obj(&self, future_obj: FutureObj<'static, ()>) -> Result<(), SpawnError> {
        self.push(future_obj);
        Ok(())
    }
}

impl LocalSpawn for FuturesUnordered<LocalFutureObj<'_, ()>> {
    fn spawn_local_obj(&self, future_obj: LocalFutureObj<'static, ()>) -> Result<(), SpawnError> {
        self.push(future_obj);
        Ok(())
    }
}

// FuturesUnordered is implemented using two linked lists. One which links all
// futures managed by a `FuturesUnordered` and one that tracks futures that have
// been scheduled for polling. The first linked list allows for thread safe
// insertion of nodes at the head as well as forward iteration, but is otherwise
// not thread safe and is only accessed by the thread that owns the
// `FuturesUnordered` value for any other operations. The second linked list is
// an implementation of the intrusive MPSC queue algorithm described by
// 1024cores.net.
//
// When a future is submitted to the set, a task is allocated and inserted in
// both linked lists. The next call to `poll_next` will (eventually) see this
// task and call `poll` on the future.
//
// Before a managed future is polled, the current context's waker is replaced
// with one that is aware of the specific future being run. This ensures that
// wake-up notifications generated by that specific future are visible to
// `FuturesUnordered`. When a wake-up notification is received, the task is
// inserted into the ready to run queue, so that its future can be polled later.
//
// Each task is wrapped in an `Arc` and thereby atomically reference counted.
// Also, each task contains an `AtomicBool` which acts as a flag that indicates
// whether the task is currently inserted in the atomic queue. When a wake-up
// notification is received, the task will only be inserted into the ready to
// run queue if it isn't inserted already.

impl<Fut> Default for FuturesUnordered<Fut> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Fut> FuturesUnordered<Fut> {
    /// Constructs a new, empty [`FuturesUnordered`].
    ///
    /// The returned [`FuturesUnordered`] does not contain any futures.
    /// In this state, [`FuturesUnordered::poll_next`](Stream::poll_next) will
    /// return [`Poll::Ready(None)`](Poll::Ready).
    pub fn new() -> Self {
        let stub = Arc::new(Task {
            future: UnsafeCell::new(None),
            next_all: AtomicPtr::new(ptr::null_mut()),
            prev_all: UnsafeCell::new(ptr::null()),
            len_all: UnsafeCell::new(0),
            next_ready_to_run: AtomicPtr::new(ptr::null_mut()),
            queued: AtomicBool::new(true),
            ready_to_run_queue: Weak::new(),
        });
        let stub_ptr = &*stub as *const Task<Fut>;
        let ready_to_run_queue = Arc::new(ReadyToRunQueue {
            waker: AtomicWaker::new(),
            head: AtomicPtr::new(stub_ptr as *mut _),
            tail: UnsafeCell::new(stub_ptr),
            stub,
        });

        Self {
            head_all: AtomicPtr::new(ptr::null_mut()),
            ready_to_run_queue,
            is_terminated: AtomicBool::new(false),
        }
    }

    /// Returns the number of futures contained in the set.
    ///
    /// This represents the total number of in-flight futures.
    pub fn len(&self) -> usize {
        let (_, len) = self.atomic_load_head_and_len_all();
        len
    }

    /// Returns `true` if the set contains no futures.
    pub fn is_empty(&self) -> bool {
        // Relaxed ordering can be used here since we don't need to read from
        // the head pointer, only check whether it is null.
        self.head_all.load(Relaxed).is_null()
    }

    /// Push a future into the set.
    ///
    /// This method adds the given future to the set. This method will not
    /// call [`poll`](core::future::Future::poll) on the submitted future. The caller must
    /// ensure that [`FuturesUnordered::poll_next`](Stream::poll_next) is called
    /// in order to receive wake-up notifications for the given future.
    pub fn push(&self, future: Fut) {
        let task = Arc::new(Task {
            future: UnsafeCell::new(Some(future)),
            next_all: AtomicPtr::new(self.pending_next_all()),
            prev_all: UnsafeCell::new(ptr::null_mut()),
            len_all: UnsafeCell::new(0),
            next_ready_to_run: AtomicPtr::new(ptr::null_mut()),
            queued: AtomicBool::new(true),
            ready_to_run_queue: Arc::downgrade(&self.ready_to_run_queue),
        });

        // Reset the `is_terminated` flag if we've previously marked ourselves
        // as terminated.
        self.is_terminated.store(false, Relaxed);

        // Right now our task has a strong reference count of 1. We transfer
        // ownership of this reference count to our internal linked list
        // and we'll reclaim ownership through the `unlink` method below.
        let ptr = self.link(task);

        // We'll need to get the future "into the system" to start tracking it,
        // e.g. getting its wake-up notifications going to us tracking which
        // futures are ready. To do that we unconditionally enqueue it for
        // polling here.
        self.ready_to_run_queue.enqueue(ptr);
    }

    /// Returns an iterator that allows inspecting each future in the set.
    pub fn iter(&self) -> Iter<'_, Fut>
    where
        Fut: Unpin,
    {
        Iter(Pin::new(self).iter_pin_ref())
    }

    /// Returns an iterator that allows inspecting each future in the set.
    pub fn iter_pin_ref(self: Pin<&Self>) -> IterPinRef<'_, Fut> {
        let (task, len) = self.atomic_load_head_and_len_all();
        let pending_next_all = self.pending_next_all();

        IterPinRef { task, len, pending_next_all, _marker: PhantomData }
    }

    /// Returns an iterator that allows modifying each future in the set.
    pub fn iter_mut(&mut self) -> IterMut<'_, Fut>
    where
        Fut: Unpin,
    {
        IterMut(Pin::new(self).iter_pin_mut())
    }

    /// Returns an iterator that allows modifying each future in the set.
    pub fn iter_pin_mut(mut self: Pin<&mut Self>) -> IterPinMut<'_, Fut> {
        // `head_all` can be accessed directly and we don't need to spin on
        // `Task::next_all` since we have exclusive access to the set.
        let task = *self.head_all.get_mut();
        let len = if task.is_null() { 0 } else { unsafe { *(*task).len_all.get() } };

        IterPinMut { task, len, _marker: PhantomData }
    }

    /// Returns the current head node and number of futures in the list of all
    /// futures within a context where access is shared with other threads
    /// (mostly for use with the `len` and `iter_pin_ref` methods).
    fn atomic_load_head_and_len_all(&self) -> (*const Task<Fut>, usize) {
        let task = self.head_all.load(Acquire);
        let len = if task.is_null() {
            0
        } else {
            unsafe {
                (*task).spin_next_all(self.pending_next_all(), Acquire);
                *(*task).len_all.get()
            }
        };

        (task, len)
    }

    /// Releases the task. It destroys the future inside and either drops
    /// the `Arc<Task>` or transfers ownership to the ready to run queue.
    /// The task this method is called on must have been unlinked before.
    fn release_task(&mut self, task: Arc<Task<Fut>>) {
        // `release_task` must only be called on unlinked tasks
        debug_assert_eq!(task.next_all.load(Relaxed), self.pending_next_all());
        unsafe {
            debug_assert!((*task.prev_all.get()).is_null());
        }

        // The future is done, try to reset the queued flag. This will prevent
        // `wake` from doing any work in the future
        let prev = task.queued.swap(true, SeqCst);

        // Drop the future, even if it hasn't finished yet. This is safe
        // because we're dropping the future on the thread that owns
        // `FuturesUnordered`, which correctly tracks `Fut`'s lifetimes and
        // such.
        unsafe {
            // Set to `None` rather than `take()`ing to prevent moving the
            // future.
            *task.future.get() = None;
        }

        // If the queued flag was previously set, then it means that this task
        // is still in our internal ready to run queue. We then transfer
        // ownership of our reference count to the ready to run queue, and it'll
        // come along and free it later, noticing that the future is `None`.
        //
        // If, however, the queued flag was *not* set then we're safe to
        // release our reference count on the task. The queued flag was set
        // above so all future `enqueue` operations will not actually
        // enqueue the task, so our task will never see the ready to run queue
        // again. The task itself will be deallocated once all reference counts
        // have been dropped elsewhere by the various wakers that contain it.
        if prev {
            mem::forget(task);
        }
    }

    /// Insert a new task into the internal linked list.
    fn link(&self, task: Arc<Task<Fut>>) -> *const Task<Fut> {
        // `next_all` should already be reset to the pending state before this
        // function is called.
        debug_assert_eq!(task.next_all.load(Relaxed), self.pending_next_all());
        let ptr = Arc::into_raw(task);

        // Atomically swap out the old head node to get the node that should be
        // assigned to `next_all`.
        let next = self.head_all.swap(ptr as *mut _, AcqRel);

        unsafe {
            // Store the new list length in the new node.
            let new_len = if next.is_null() {
                1
            } else {
                // Make sure `next_all` has been written to signal that it is
                // safe to read `len_all`.
                (*next).spin_next_all(self.pending_next_all(), Acquire);
                *(*next).len_all.get() + 1
            };
            *(*ptr).len_all.get() = new_len;

            // Write the old head as the next node pointer, signaling to other
            // threads that `len_all` and `next_all` are ready to read.
            (*ptr).next_all.store(next, Release);

            // `prev_all` updates don't need to be synchronized, as the field is
            // only ever used after exclusive access has been acquired.
            if !next.is_null() {
                *(*next).prev_all.get() = ptr;
            }
        }

        ptr
    }

    /// Remove the task from the linked list tracking all tasks currently
    /// managed by `FuturesUnordered`.
    /// This method is unsafe because it has be guaranteed that `task` is a
    /// valid pointer.
    unsafe fn unlink(&mut self, task: *const Task<Fut>) -> Arc<Task<Fut>> {
        // Compute the new list length now in case we're removing the head node
        // and won't be able to retrieve the correct length later.
        let head = *self.head_all.get_mut();
        debug_assert!(!head.is_null());
        let new_len = *(*head).len_all.get() - 1;

        let task = Arc::from_raw(task);
        let next = task.next_all.load(Relaxed);
        let prev = *task.prev_all.get();
        task.next_all.store(self.pending_next_all(), Relaxed);
        *task.prev_all.get() = ptr::null_mut();

        if !next.is_null() {
            *(*next).prev_all.get() = prev;
        }

        if !prev.is_null() {
            (*prev).next_all.store(next, Relaxed);
        } else {
            *self.head_all.get_mut() = next;
        }

        // Store the new list length in the head node.
        let head = *self.head_all.get_mut();
        if !head.is_null() {
            *(*head).len_all.get() = new_len;
        }

        task
    }

    /// Returns the reserved value for `Task::next_all` to indicate a pending
    /// assignment from the thread that inserted the task.
    ///
    /// `FuturesUnordered::link` needs to update `Task` pointers in an order
    /// that ensures any iterators created on other threads can correctly
    /// traverse the entire `Task` list using the chain of `next_all` pointers.
    /// This could be solved with a compare-exchange loop that stores the
    /// current `head_all` in `next_all` and swaps out `head_all` with the new
    /// `Task` pointer if the head hasn't already changed. Under heavy thread
    /// contention, this compare-exchange loop could become costly.
    ///
    /// An alternative is to initialize `next_all` to a reserved pending state
    /// first, perform an atomic swap on `head_all`, and finally update
    /// `next_all` with the old head node. Iterators will then either see the
    /// pending state value or the correct next node pointer, and can reload
    /// `next_all` as needed until the correct value is loaded. The number of
    /// retries needed (if any) would be small and will always be finite, so
    /// this should generally perform better than the compare-exchange loop.
    ///
    /// A valid `Task` pointer in the `head_all` list is guaranteed to never be
    /// this value, so it is safe to use as a reserved value until the correct
    /// value can be written.
    fn pending_next_all(&self) -> *mut Task<Fut> {
        // The `ReadyToRunQueue` stub is never inserted into the `head_all`
        // list, and its pointer value will remain valid for the lifetime of
        // this `FuturesUnordered`, so we can make use of its value here.
        &*self.ready_to_run_queue.stub as *const _ as *mut _
    }
}

impl<Fut: Future> Stream for FuturesUnordered<Fut> {
    type Item = Fut::Output;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // See YIELD_EVERY docs　for more.
        let yield_every = cmp::min(self.len(), YIELD_EVERY);

        // Keep track of how many child futures we have polled,
        // in case we want to forcibly yield.
        let mut polled = 0;

        // Ensure `parent` is correctly set.
        self.ready_to_run_queue.waker.register(cx.waker());

        loop {
            // Safety: &mut self guarantees the mutual exclusion `dequeue`
            // expects
            let task = match unsafe { self.ready_to_run_queue.dequeue() } {
                Dequeue::Empty => {
                    if self.is_empty() {
                        // We can only consider ourselves terminated once we
                        // have yielded a `None`
                        *self.is_terminated.get_mut() = true;
                        return Poll::Ready(None);
                    } else {
                        return Poll::Pending;
                    }
                }
                Dequeue::Inconsistent => {
                    // At this point, it may be worth yielding the thread &
                    // spinning a few times... but for now, just yield using the
                    // task system.
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                Dequeue::Data(task) => task,
            };

            debug_assert!(task != self.ready_to_run_queue.stub());

            // Safety:
            // - `task` is a valid pointer.
            // - We are the only thread that accesses the `UnsafeCell` that
            //   contains the future
            let future = match unsafe { &mut *(*task).future.get() } {
                Some(future) => future,

                // If the future has already gone away then we're just
                // cleaning out this task. See the comment in
                // `release_task` for more information, but we're basically
                // just taking ownership of our reference count here.
                None => {
                    // This case only happens when `release_task` was called
                    // for this task before and couldn't drop the task
                    // because it was already enqueued in the ready to run
                    // queue.

                    // Safety: `task` is a valid pointer
                    let task = unsafe { Arc::from_raw(task) };

                    // Double check that the call to `release_task` really
                    // happened. Calling it required the task to be unlinked.
                    debug_assert_eq!(task.next_all.load(Relaxed), self.pending_next_all());
                    unsafe {
                        debug_assert!((*task.prev_all.get()).is_null());
                    }
                    continue;
                }
            };

            // Safety: `task` is a valid pointer
            let task = unsafe { self.unlink(task) };

            // Unset queued flag: This must be done before polling to ensure
            // that the future's task gets rescheduled if it sends a wake-up
            // notification **during** the call to `poll`.
            let prev = task.queued.swap(false, SeqCst);
            assert!(prev);

            // We're going to need to be very careful if the `poll`
            // method below panics. We need to (a) not leak memory and
            // (b) ensure that we still don't have any use-after-frees. To
            // manage this we do a few things:
            //
            // * A "bomb" is created which if dropped abnormally will call
            //   `release_task`. That way we'll be sure the memory management
            //   of the `task` is managed correctly. In particular
            //   `release_task` will drop the future. This ensures that it is
            //   dropped on this thread and not accidentally on a different
            //   thread (bad).
            // * We unlink the task from our internal queue to preemptively
            //   assume it'll panic, in which case we'll want to discard it
            //   regardless.
            struct Bomb<'a, Fut> {
                queue: &'a mut FuturesUnordered<Fut>,
                task: Option<Arc<Task<Fut>>>,
            }

            impl<Fut> Drop for Bomb<'_, Fut> {
                fn drop(&mut self) {
                    if let Some(task) = self.task.take() {
                        self.queue.release_task(task);
                    }
                }
            }

            let mut bomb = Bomb { task: Some(task), queue: &mut *self };

            // Poll the underlying future with the appropriate waker
            // implementation. This is where a large bit of the unsafety
            // starts to stem from internally. The waker is basically just
            // our `Arc<Task<Fut>>` and can schedule the future for polling by
            // enqueuing itself in the ready to run queue.
            //
            // Critically though `Task<Fut>` won't actually access `Fut`, the
            // future, while it's floating around inside of wakers.
            // These structs will basically just use `Fut` to size
            // the internal allocation, appropriately accessing fields and
            // deallocating the task if need be.
            let res = {
                let waker = Task::waker_ref(bomb.task.as_ref().unwrap());
                let mut cx = Context::from_waker(&waker);

                // Safety: We won't move the future ever again
                let future = unsafe { Pin::new_unchecked(future) };

                future.poll(&mut cx)
            };
            polled += 1;

            match res {
                Poll::Pending => {
                    let task = bomb.task.take().unwrap();
                    bomb.queue.link(task);

                    if polled == yield_every {
                        // We have polled a large number of futures in a row without yielding.
                        // To ensure we do not starve other tasks waiting on the executor,
                        // we yield here, but immediately wake ourselves up to continue.
                        cx.waker().wake_by_ref();
                        return Poll::Pending;
                    }
                    continue;
                }
                Poll::Ready(output) => return Poll::Ready(Some(output)),
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.len();
        (len, Some(len))
    }
}

impl<Fut> Debug for FuturesUnordered<Fut> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FuturesUnordered {{ ... }}")
    }
}

impl<Fut> FuturesUnordered<Fut> {
    /// Clears the set, removing all futures.
    pub fn clear(&mut self) {
        self.clear_head_all();

        // we just cleared all the tasks, and we have &mut self, so this is safe.
        unsafe { self.ready_to_run_queue.clear() };

        self.is_terminated.store(false, Relaxed);
    }

    fn clear_head_all(&mut self) {
        while !self.head_all.get_mut().is_null() {
            let head = *self.head_all.get_mut();
            let task = unsafe { self.unlink(head) };
            self.release_task(task);
        }
    }
}

impl<Fut> Drop for FuturesUnordered<Fut> {
    fn drop(&mut self) {
        // When a `FuturesUnordered` is dropped we want to drop all futures
        // associated with it. At the same time though there may be tons of
        // wakers flying around which contain `Task<Fut>` references
        // inside them. We'll let those naturally get deallocated.
        self.clear_head_all();

        // Note that at this point we could still have a bunch of tasks in the
        // ready to run queue. None of those tasks, however, have futures
        // associated with them so they're safe to destroy on any thread. At
        // this point the `FuturesUnordered` struct, the owner of the one strong
        // reference to the ready to run queue will drop the strong reference.
        // At that point whichever thread releases the strong refcount last (be
        // it this thread or some other thread as part of an `upgrade`) will
        // clear out the ready to run queue and free all remaining tasks.
        //
        // While that freeing operation isn't guaranteed to happen here, it's
        // guaranteed to happen "promptly" as no more "blocking work" will
        // happen while there's a strong refcount held.
    }
}

impl<'a, Fut: Unpin> IntoIterator for &'a FuturesUnordered<Fut> {
    type Item = &'a Fut;
    type IntoIter = Iter<'a, Fut>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a, Fut: Unpin> IntoIterator for &'a mut FuturesUnordered<Fut> {
    type Item = &'a mut Fut;
    type IntoIter = IterMut<'a, Fut>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

impl<Fut: Unpin> IntoIterator for FuturesUnordered<Fut> {
    type Item = Fut;
    type IntoIter = IntoIter<Fut>;

    fn into_iter(mut self) -> Self::IntoIter {
        // `head_all` can be accessed directly and we don't need to spin on
        // `Task::next_all` since we have exclusive access to the set.
        let task = *self.head_all.get_mut();
        let len = if task.is_null() { 0 } else { unsafe { *(*task).len_all.get() } };

        IntoIter { len, inner: self }
    }
}

impl<Fut> FromIterator<Fut> for FuturesUnordered<Fut> {
    fn from_iter<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = Fut>,
    {
        let acc = Self::new();
        iter.into_iter().fold(acc, |acc, item| {
            acc.push(item);
            acc
        })
    }
}

impl<Fut: Future> FusedStream for FuturesUnordered<Fut> {
    fn is_terminated(&self) -> bool {
        self.is_terminated.load(Relaxed)
    }
}

impl<Fut> Extend<Fut> for FuturesUnordered<Fut> {
    fn extend<I>(&mut self, iter: I)
    where
        I: IntoIterator<Item = Fut>,
    {
        for item in iter {
            self.push(item);
        }
    }
}