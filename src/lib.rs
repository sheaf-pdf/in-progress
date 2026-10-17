//! Progress reporting and cooperative cancellation for long-running
//! operations.
//!
//! An operation takes `&dyn Progress`, reports how far along it is, and
//! calls [`check`](Progress::check) between units of work so a cancel stops
//! it cleanly. Whoever runs it holds a [`Tracker`]: it polls the tracker's
//! [`Snapshot`] from another thread to draw a progress bar or a status line,
//! and can cancel it at any time. Callers that don't care pass
//! [`NoProgress`].
//!
//! Each progress covers a *share* of the whole job. The root's share is all
//! of it, and a [`child`](Progress::child) covers a weighted part of its
//! parent's share, so a job made of steps ("prepare, then process") can hand
//! each step its own progress without the step knowing where it sits in the
//! whole.
//!
//! # Reporting progress
//!
//! ```
//! use in_progress::{Cancelled, Progress, Tracker, Unit};
//!
//! fn process(items: &[u32], p: &dyn Progress) -> Result<u32, Cancelled> {
//!     p.set_total(Some(items.len() as u64), Unit::Count("records"));
//!     let mut sum = 0;
//!     for item in items {
//!         p.check()?;
//!         sum += item;
//!         p.inc(1);
//!     }
//!     Ok(sum)
//! }
//!
//! fn job(items: &[u32], p: &dyn Progress) -> Result<u32, Cancelled> {
//!     process(items, &p.child("prepare", 0.25))?;
//!     process(items, &p.child("process", 0.75))
//! }
//!
//! let tracker = Tracker::new();
//! assert_eq!(job(&[1, 2, 3], &tracker)?, 6);
//! assert_eq!(tracker.snapshot().fraction, Some(1.0));
//! # Ok::<(), Cancelled>(())
//! ```
//!
//! # Polling and cancelling
//!
//! The job runs on another thread with a clone of the tracker, while the
//! owner polls [`Tracker::snapshot`]. [`Tracker::cancel`] asks the job to
//! stop: its next check returns [`Cancelled`].
//!
//! ```
//! use in_progress::{Cancelled, Progress, Tracker, Unit};
//! use std::{thread, time::Duration};
//!
//! fn download(p: &dyn Progress) -> Result<(), Cancelled> {
//!     p.set_total(None, Unit::Bytes); // The length isn't known.
//!     loop {
//!         p.check()?;
//!         thread::sleep(Duration::from_millis(1));
//!         p.inc(4096);
//!     }
//! }
//!
//! let tracker = Tracker::new();
//! let worker = thread::spawn({
//!     let tracker = tracker.clone();
//!     move || download(&tracker)
//! });
//! while !worker.is_finished() {
//!     let s = tracker.snapshot();
//!     println!("{} {}", s.done, s.unit.noun());
//!     if s.done >= 64 * 1024 {
//!         tracker.cancel();
//!     }
//!     thread::sleep(Duration::from_millis(1));
//! }
//! assert_eq!(worker.join().unwrap(), Err(Cancelled));
//! ```
//!
//! # How a tracker adds up
//!
//! - A progress's own count fills its share from the start. A child's share
//!   starts after that count and after the earlier children's shares.
//! - Children add up, so steps running at once on other threads each move
//!   the bar by their share.
//! - A progress that counts and has children too shows whichever is
//!   further: its count, or its children plus what its count had covered
//!   before them. Counting up doesn't undo progress from its children.
//! - Dropping a child finishes its share however the step ended: returning,
//!   failing with an error, or unwinding from a panic. Children made from it
//!   keep it open until they are dropped too. Only cancellation leaves a
//!   share unfinished, where it had got to.
//! - Cancelling a tracker stops it and its children, not its parent or
//!   siblings.
//!
//! The fraction can go down when a count is lowered, a total is raised, or a
//! total becomes unknown.

#![warn(missing_docs, missing_debug_implementations)]

use std::sync::atomic::Ordering::{Relaxed, SeqCst};

// Loom is a dev-dependency, so only the crate's own tests can switch to it.
#[cfg(all(test, loom))]
use loom::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, fence},
};
#[cfg(not(all(test, loom)))]
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, fence},
};

/// What a count counts, for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Unit {
    /// Things counted one at a time, named by a plural noun: `"files"`,
    /// `"pages"`, `"records"`.
    Count(&'static str),
    /// Bytes, which a renderer can show as sizes ("1.2 MiB of 4 MiB").
    Bytes,
}

impl Unit {
    /// Plain items, when nothing more specific names the count. The default.
    pub const ITEMS: Unit = Unit::Count("items");

    /// The plural noun, for "3 of 10 files".
    pub fn noun(self) -> &'static str {
        match self {
            Unit::Count(noun) => noun,
            Unit::Bytes => "bytes",
        }
    }
}

impl Default for Unit {
    fn default() -> Self {
        Unit::ITEMS
    }
}

/// How serious a [`Progress::message`] is. Ordered by severity, so
/// `level >= Level::Warn` picks out warnings and errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    /// Status for the user.
    Info,
    /// Something that may need attention.
    Warn,
    /// Something failed.
    Error,
}

/// The error an operation returns when it stops because it was cancelled.
///
/// [`Progress::check`] returns it. An operation can return it as is or wrap
/// it in its own error type. To tell a cancel from a failure, match the
/// variant that wraps it, or use `downcast_ref::<Cancelled>()` on a boxed
/// error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// Progress reporting and cooperative cancellation for an operation.
///
/// Operations take `&dyn Progress`, report to it, and call
/// [`check`](Self::check) between units of work; nothing else stops them.
/// Calls may come from several threads at once, and an operation may make
/// one per unit of work, so they should be cheap. Implementations may ignore
/// reports, as [`NoProgress`] does.
pub trait Progress: Send + Sync {
    /// How many there are to do, in `unit`; `None` while that isn't known.
    /// `Some(0)` counts as all done. Calling it again changes the total and
    /// unit but keeps the count. For an open-ended count, pass `None` with
    /// the unit: `set_total(None, Unit::Bytes)`.
    fn set_total(&self, total: Option<u64>, unit: Unit);
    /// How many are done. It may exceed the total.
    fn set(&self, done: u64);
    /// Adds `n` to how many are done.
    fn inc(&self, n: u64);
    /// Reports a message for display. It doesn't stop the operation, even
    /// at [`Level::Error`].
    fn message(&self, level: Level, msg: &str);
    /// Whether the operation has been asked to stop. Once true, it stays
    /// true.
    fn is_cancelled(&self) -> bool;
    /// `Err(Cancelled)` once cancelled, for `p.check()?`.
    fn check(&self) -> Result<(), Cancelled> {
        if self.is_cancelled() {
            Err(Cancelled)
        } else {
            Ok(())
        }
    }
    /// A progress named `name` that covers `weight` (0–1) of this one's
    /// whole share, after this one's count and its earlier children's
    /// shares. It is cancelled whenever this one is. Dropping it finishes its
    /// share, unless it was cancelled.
    ///
    /// The box borrows this progress but is `Send`, so an operation can move
    /// children into [`std::thread::scope`] threads. A caller that owns a
    /// [`Tracker`] and needs a `'static` handle uses
    /// [`Tracker::child_tracker`].
    ///
    /// ```
    /// use in_progress::{Progress, Tracker, Unit};
    ///
    /// let tracker = Tracker::new();
    /// let prepare = tracker.child("prepare", 0.25);
    /// prepare.set_total(Some(2), Unit::ITEMS);
    /// prepare.inc(1);
    /// assert_eq!(tracker.snapshot().fraction, Some(0.125)); // Half of a quarter.
    /// drop(prepare);
    /// assert_eq!(tracker.snapshot().fraction, Some(0.25));
    /// let process = tracker.child("process", 0.75); // Starts at a quarter.
    /// process.set_total(Some(3), Unit::ITEMS);
    /// process.inc(1);
    /// assert_eq!(tracker.snapshot().fraction, Some(0.5));
    /// ```
    #[must_use = "dropping a child immediately finishes its share"]
    fn child(&self, name: &str, weight: f32) -> Box<dyn Progress + '_>;
}

/// Reports nowhere and never cancels.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoProgress;

impl Progress for NoProgress {
    fn set_total(&self, _: Option<u64>, _: Unit) {}
    fn set(&self, _: u64) {}
    fn inc(&self, _: u64) {}
    fn message(&self, _: Level, _: &str) {}
    fn is_cancelled(&self) -> bool {
        false
    }
    fn child(&self, _: &str, _: f32) -> Box<dyn Progress + '_> {
        Box::new(NoProgress)
    }
}

/// Forwards to the boxed progress, so `&p.child(..)` can be passed where a
/// `&dyn Progress` is expected.
impl<P: Progress + ?Sized> Progress for Box<P> {
    fn set_total(&self, total: Option<u64>, unit: Unit) {
        (**self).set_total(total, unit);
    }
    fn set(&self, done: u64) {
        (**self).set(done);
    }
    fn inc(&self, n: u64) {
        (**self).inc(n);
    }
    fn message(&self, level: Level, msg: &str) {
        (**self).message(level, msg);
    }
    fn is_cancelled(&self) -> bool {
        (**self).is_cancelled()
    }
    fn check(&self) -> Result<(), Cancelled> {
        (**self).check()
    }
    fn child(&self, name: &str, weight: f32) -> Box<dyn Progress + '_> {
        (**self).child(name, weight)
    }
}

/// The state of a whole job, from [`Tracker::snapshot`].
///
/// The fields are read one at a time while the job may be reporting, so they
/// can mix moments: they are for display, not for synchronizing with the
/// work. A full bar doesn't mean the job is over or that it succeeded: a
/// count can fill it while steps still run, and a step that fails still
/// finishes its share. The job's result says how it went.
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct Snapshot {
    /// Overall, 0–1; `None` until some progress sets a known total or a
    /// child's share finishes. Once `Some`, it stays `Some`.
    pub fraction: Option<f32>,
    /// The step: the progress that most recently set its total or count. Its
    /// own name, not a path; empty for the root. When the step is dropped,
    /// its parent becomes the step. Messages don't change it.
    pub step: String,
    /// The step's count. It can exceed the total.
    pub done: u64,
    /// The step's total; `None` while it isn't known.
    pub total: Option<u64>,
    /// What the step counts.
    pub unit: Unit,
    /// The latest message from anywhere in the job.
    pub message: Option<(Level, String)>,
    /// How many messages there have been, so a poller can tell a new
    /// message from a repeat of the last one.
    pub messages: u64,
    /// Whether the whole job was cancelled. Cancelling only a child doesn't
    /// set it.
    pub cancelled: bool,
}

/// The whole job's span, in fixed point: shares are integers, so
/// children working at once add up with atomic adds.
const ONE: u64 = 1 << 48;
const NO_TOTAL: u64 = u64::MAX;

/// One progress in the tree. Spans are absolute, in `ONE`ths of the job.
struct Node {
    name: String,
    parent: Option<Arc<Node>>,
    lo: u64,
    hi: u64,
    total: AtomicU64,
    done: AtomicU64,
    /// Only for display, so it can sit behind a lock.
    unit: Mutex<Unit>,
    cancelled: AtomicBool,
    /// Dropped without being cancelled: its whole span is done.
    finished: AtomicBool,
    /// Where the next child's share can start: the end of the last one's.
    next: AtomicU64,
    /// How much of the span the count had covered before children were
    /// placed after it.
    skipped: AtomicU64,
    /// The sum of the children's `filled`. Signed, since one child's
    /// updates can land out of order and dip below zero for a moment.
    children: AtomicI64,
    /// How much of the span is done, as last added to the parent's
    /// `children`.
    filled: AtomicU64,
}

impl Node {
    fn new(name: &str, parent: Option<Arc<Node>>, lo: u64, hi: u64) -> Node {
        Node {
            name: name.to_string(),
            parent,
            lo,
            hi,
            total: AtomicU64::new(NO_TOTAL),
            done: AtomicU64::new(0),
            unit: Mutex::new(Unit::ITEMS),
            cancelled: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            next: AtomicU64::new(lo),
            skipped: AtomicU64::new(0),
            children: AtomicI64::new(0),
            filled: AtomicU64::new(0),
        }
    }

    fn span(&self) -> u64 {
        self.hi - self.lo
    }

    fn total(&self) -> Option<u64> {
        match self.total.load(Relaxed) {
            NO_TOTAL => None,
            t => Some(t),
        }
    }

    /// How much of the span the count covers.
    fn counted(&self) -> u64 {
        let span = self.span();
        match self.total() {
            None => 0,
            Some(0) => span,
            Some(t) => {
                let done = self.done.load(Relaxed).min(t);
                (u128::from(span) * u128::from(done) / u128::from(t)) as u64
            }
        }
    }

    /// How much of the span is done: the count, or the children plus what
    /// the count had covered before them, whichever is further.
    fn progress(&self) -> u64 {
        let span = self.span();
        if self.finished.load(Relaxed) {
            return span;
        }
        let children = self.children.load(Relaxed).clamp(0, span as i64) as u64;
        let by_children = self.skipped.load(Relaxed) + children;
        self.counted().max(by_children).min(span)
    }

    /// Carry a change in this node's progress up the tree, as far as it
    /// changes anything.
    ///
    /// Threads can recompute a node at once from inputs they see at
    /// different moments, and only one result sticks. To make it the right
    /// one:
    ///
    /// - A `SeqCst` fence sits between writing inputs and reading them back,
    ///   here and after each write to `filled`. Of two threads that change
    ///   inputs at once, the fences make at least one see both changes.
    /// - Whoever writes `filled` recomputes after its fence and goes again if
    ///   the result changed: its value may be stale, written over one that a
    ///   thread which saw everything had found right and left alone.
    ///
    /// The fences order everything else, so the other accesses are relaxed.
    /// The loom tests at the end of this file check each of these steps.
    fn refresh(&self) {
        let mut node = self;
        fence(SeqCst);
        loop {
            let mut old = node.filled.load(Relaxed);
            let mut moved = false;
            loop {
                let new = node.progress();
                if new == old {
                    break;
                }
                match node
                    .filled
                    .compare_exchange_weak(old, new, Relaxed, Relaxed)
                {
                    Ok(_) => {
                        if let Some(parent) = &node.parent {
                            parent.children.fetch_add(new as i64 - old as i64, Relaxed);
                        }
                        moved = true;
                        old = new;
                        fence(SeqCst);
                    }
                    Err(now) => old = now,
                }
            }
            // Whoever moved `filled` carries the change to the parent.
            match &node.parent {
                Some(parent) if moved => node = parent,
                _ => return,
            }
        }
    }

    /// Reserve the next `weight` of this span for a child: after the count
    /// and after the earlier children, never overlapping one made at the
    /// same moment on another thread.
    fn place(&self, weight: f32) -> (u64, u64) {
        let weight = if weight.is_nan() {
            0.0
        } else {
            weight.clamp(0.0, 1.0)
        };
        let len = (f64::from(weight) * self.span() as f64) as u64;
        let mut next = self.next.load(Relaxed);
        loop {
            let lo = (self.lo + self.counted()).max(next).min(self.hi);
            let hi = (lo + len).min(self.hi);
            match self.next.compare_exchange_weak(next, hi, Relaxed, Relaxed) {
                Ok(_) => {
                    // The count already covers the gap, so this doesn't
                    // move the bar; it keeps the gap done if the count
                    // falls behind the children.
                    self.skipped.fetch_add(lo - next, Relaxed);
                    if lo > next {
                        // A count lowered meanwhile may have missed the gap.
                        self.refresh();
                    }
                    return (lo, hi);
                }
                Err(now) => next = now,
            }
        }
    }

    fn is_cancelled(&self) -> bool {
        let mut node = Some(self);
        while let Some(n) = node {
            if n.cancelled.load(Relaxed) {
                return true;
            }
            node = n.parent.as_deref();
        }
        false
    }
}

struct Shared {
    root: Arc<Node>,
    /// Some total is known or some step done, so the fraction means
    /// something.
    known: AtomicBool,
    /// The snapshot's step: the node that last reported a count, or the
    /// parent of one that was dropped since.
    last: Mutex<Arc<Node>>,
    /// `last`'s address, written under its lock, so a node reporting again
    /// can see it's still the last without taking the lock. `last` holds
    /// the node, so no other node can reuse the address meanwhile.
    last_id: AtomicUsize,
    /// The latest message, and how many there have been.
    message: Mutex<(u64, Option<(Level, String)>)>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while holding the lock leaves plain data; keep going.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The shared [`Progress`]: cheap to clone, and every clone is the same
/// progress. All the trackers in a job share its [`Snapshot`], but a child
/// can be cancelled without its parent.
///
/// A child's share is finished once its last clone, and every tracker made
/// from it, has been dropped, unless it was cancelled. Weights are clamped to
/// 0–1, NaN counting as 0, and a share that would run past the end of its
/// parent's is cut short.
#[derive(Clone)]
pub struct Tracker(Arc<Handle>);

/// A tracker and its clones. When the last of them goes, the node's
/// share is done.
struct Handle {
    shared: Arc<Shared>,
    node: Arc<Node>,
    /// Children hold their parent's handle, so a share isn't finished while
    /// trackers made from it are still reporting.
    _parent: Option<Arc<Handle>>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        let Handle { shared, node, .. } = self;
        let Some(parent) = &node.parent else {
            return;
        };
        if !node.is_cancelled() {
            node.finished.store(true, Relaxed);
            shared.known.store(true, Relaxed);
            node.refresh();
        }
        // A step that's over hands the snapshot's step back to its parent.
        if shared.last_id.load(Relaxed) == Arc::as_ptr(node) as usize {
            let mut last = lock(&shared.last);
            if Arc::ptr_eq(&last, node) {
                *last = parent.clone();
                shared.last_id.store(Arc::as_ptr(parent) as usize, Relaxed);
            }
        }
    }
}

impl std::fmt::Debug for Tracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tracker")
            .field("name", &self.0.node.name)
            .field("cancelled", &self.is_cancelled())
            .finish_non_exhaustive()
    }
}

impl Default for Tracker {
    fn default() -> Self {
        Self::new()
    }
}

impl Tracker {
    /// A new job's root, with nothing reported yet.
    pub fn new() -> Tracker {
        let root = Arc::new(Node::new("", None, 0, ONE));
        Tracker(Arc::new(Handle {
            shared: Arc::new(Shared {
                root: root.clone(),
                known: AtomicBool::new(false),
                last_id: AtomicUsize::new(Arc::as_ptr(&root) as usize),
                last: Mutex::new(root.clone()),
                message: Mutex::new((0, None)),
            }),
            node: root,
            _parent: None,
        }))
    }

    /// [`Progress::child`] as an owned `Tracker`, for a holder that can't
    /// borrow this one, such as a thread started with [`std::thread::spawn`].
    ///
    /// ```
    /// use in_progress::{Progress, Tracker, Unit};
    ///
    /// let tracker = Tracker::new();
    /// let worker = tracker.child_tracker("worker", 1.0);
    /// let handle = worker.clone();
    /// std::thread::spawn(move || {
    ///     handle.set_total(Some(4), Unit::ITEMS);
    ///     handle.inc(2);
    /// })
    /// .join()
    /// .unwrap();
    /// assert_eq!(tracker.snapshot().fraction, Some(0.5));
    /// drop(worker); // The last clone, so its share is finished.
    /// assert_eq!(tracker.snapshot().fraction, Some(1.0));
    /// ```
    #[must_use = "dropping a child immediately finishes its share"]
    pub fn child_tracker(&self, name: &str, weight: f32) -> Tracker {
        let parent = &self.0.node;
        let (lo, hi) = parent.place(weight);
        Tracker(Arc::new(Handle {
            shared: self.0.shared.clone(),
            node: Arc::new(Node::new(name, Some(parent.clone()), lo, hi)),
            _parent: Some(self.0.clone()),
        }))
    }

    /// Ask the work to stop: this tracker and its children fail their next
    /// [`Progress::check`]. Its parent and siblings carry on. Nothing is
    /// interrupted; the work stops when it next checks.
    ///
    /// ```
    /// use in_progress::{Cancelled, Progress, Tracker};
    ///
    /// let tracker = Tracker::new();
    /// let handle = tracker.clone();
    /// std::thread::spawn(move || handle.cancel()).join().unwrap();
    /// assert_eq!(tracker.check(), Err(Cancelled));
    /// assert!(tracker.snapshot().cancelled);
    /// ```
    pub fn cancel(&self) {
        self.0.node.cancelled.store(true, Relaxed);
    }

    /// The whole job, whichever tracker in it this is called on.
    pub fn snapshot(&self) -> Snapshot {
        let shared = &self.0.shared;
        let root = &shared.root;
        let last = lock(&shared.last).clone();
        let (messages, message) = lock(&shared.message).clone();
        Snapshot {
            fraction: shared
                .known
                .load(Relaxed)
                .then(|| (root.filled.load(Relaxed) as f64 / ONE as f64) as f32),
            step: last.name.clone(),
            done: last.done.load(Relaxed),
            total: last.total(),
            unit: *lock(&last.unit),
            message,
            messages,
            cancelled: root.cancelled.load(Relaxed),
        }
    }

    /// Publish this node's count: up the tree, and as the job's step.
    fn report(&self) {
        let Handle { shared, node, .. } = &*self.0;
        node.refresh();
        let id = Arc::as_ptr(node) as usize;
        if shared.last_id.load(Relaxed) != id {
            let mut last = lock(&shared.last);
            *last = node.clone();
            shared.last_id.store(id, Relaxed);
        }
    }
}

impl Progress for Tracker {
    fn set_total(&self, total: Option<u64>, unit: Unit) {
        let node = &self.0.node;
        // `NO_TOTAL` stands for an unknown total, so a known one stops short.
        let stored = total.map_or(NO_TOTAL, |t| t.min(NO_TOTAL - 1));
        node.total.store(stored, Relaxed);
        *lock(&node.unit) = unit;
        if total.is_some() {
            self.0.shared.known.store(true, Relaxed);
        }
        self.report();
    }

    fn set(&self, done: u64) {
        self.0.node.done.store(done, Relaxed);
        self.report();
    }

    fn inc(&self, n: u64) {
        self.0.node.done.fetch_add(n, Relaxed);
        self.report();
    }

    fn message(&self, level: Level, msg: &str) {
        let mut m = lock(&self.0.shared.message);
        m.0 += 1;
        m.1 = Some((level, msg.to_string()));
    }

    fn is_cancelled(&self) -> bool {
        self.0.node.is_cancelled()
    }

    fn child(&self, name: &str, weight: f32) -> Box<dyn Progress + '_> {
        Box::new(self.child_tracker(name, weight))
    }
}

/// The README's example, run as a doctest.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    fn frac(t: &Tracker) -> f32 {
        t.snapshot().fraction.unwrap()
    }

    #[test]
    fn counts_and_fraction() {
        let t = Tracker::new();
        assert_eq!(t.snapshot().fraction, None);
        t.set_total(Some(4), Unit::Count("files"));
        assert_eq!(frac(&t), 0.0);
        t.inc(1);
        t.inc(1);
        assert_eq!(frac(&t), 0.5);
        let s = t.snapshot();
        assert_eq!(
            (s.done, s.total, s.unit),
            (2, Some(4), Unit::Count("files"))
        );
        t.set(4);
        assert_eq!(frac(&t), 1.0);
        // Overshoot stays at the end, even far past the total.
        t.inc(3);
        assert_eq!(frac(&t), 1.0);
        t.set(4 << 20);
        assert_eq!(frac(&t), 1.0);
    }

    #[test]
    fn a_total_of_zero_is_done() {
        let t = Tracker::new();
        let empty = t.child("empty", 0.5);
        empty.set_total(Some(0), Unit::ITEMS);
        assert_eq!(frac(&t), 0.5);
    }

    #[test]
    fn a_count_alone_leaves_the_fraction_unknown() {
        let t = Tracker::new();
        t.set_total(None, Unit::Bytes);
        t.inc(5);
        assert_eq!(t.snapshot().fraction, None);
        assert_eq!(t.snapshot().done, 5);
        t.set_total(Some(10), Unit::Bytes);
        assert_eq!(frac(&t), 0.5);
        // Once known, it stays known.
        t.set_total(None, Unit::Bytes);
        assert_eq!(t.snapshot().fraction, Some(0.0));
    }

    #[test]
    fn a_total_of_u64_max_is_known() {
        let t = Tracker::new();
        t.set_total(Some(u64::MAX), Unit::Bytes);
        t.set(u64::MAX / 2);
        let s = t.snapshot();
        assert_eq!((s.total, s.fraction), (Some(u64::MAX - 1), Some(0.5)));
    }

    #[test]
    fn children_fill_their_share() {
        let t = Tracker::new();
        t.set_total(Some(1), Unit::ITEMS);
        {
            let prepare = t.child("prepare", 0.25);
            prepare.set_total(Some(10), Unit::Count("files"));
            prepare.set(10);
        }
        assert_eq!(frac(&t), 0.25);
        let process = t.child("process", 0.75);
        process.set_total(Some(2), Unit::Count("files"));
        process.inc(1);
        assert_eq!(frac(&t), 0.25 + 0.75 / 2.0);
        // Grandchildren nest inside their parent's share.
        let item = process.child("file 2", 0.5);
        item.set_total(Some(2), Unit::Bytes);
        item.set(1);
        assert_eq!(frac(&t), 0.625 + 0.75 * 0.5 * 0.5);
        assert_eq!(t.snapshot().step, "file 2");
    }

    #[test]
    fn a_boxed_child_is_a_progress() {
        let t = Tracker::new();
        let p: &dyn Progress = &t;
        let child = p.child("a", 0.5);
        let boxed: &dyn Progress = &child;
        boxed.set_total(Some(2), Unit::ITEMS);
        boxed.inc(1);
        assert_eq!(frac(&t), 0.25);
    }

    #[test]
    fn siblings_at_once_add_up() {
        let t = Tracker::new();
        let a = t.child_tracker("a", 0.5);
        let b = t.child_tracker("b", 0.5);
        a.set_total(Some(2), Unit::ITEMS);
        b.set_total(Some(2), Unit::ITEMS);
        a.set(1);
        assert_eq!(frac(&t), 0.25);
        b.set(1);
        assert_eq!(frac(&t), 0.5);
        a.set(2);
        assert_eq!(frac(&t), 0.75);
        b.set(2);
        assert_eq!(frac(&t), 1.0);
    }

    #[test]
    fn siblings_on_threads_add_up() {
        let t = Tracker::new();
        let steps: Vec<Tracker> = (0..8)
            .map(|i| t.child_tracker(&format!("worker {i}"), 1.0 / 8.0))
            .collect();
        std::thread::scope(|s| {
            for step in &steps {
                s.spawn(move || {
                    step.set_total(Some(1000), Unit::ITEMS);
                    for _ in 0..1000 {
                        step.inc(1);
                    }
                });
            }
        });
        // The steps are still alive, so only their counts can fill the bar.
        assert_eq!(frac(&t), 1.0);
    }

    #[test]
    fn clones_on_threads_add_up() {
        let t = Tracker::new();
        t.set_total(Some(8000), Unit::ITEMS);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    for _ in 0..1000 {
                        t.inc(1);
                    }
                });
            }
        });
        assert_eq!((t.snapshot().done, frac(&t)), (8000, 1.0));
    }

    #[test]
    fn children_made_at_once_dont_overlap() {
        let t = Tracker::new();
        let spans = std::sync::Mutex::new(Vec::new());
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    for _ in 0..100 {
                        let c = t.child_tracker("c", 1.0 / 800.0);
                        lock(&spans).push((c.0.node.lo, c.0.node.hi));
                        c.cancel(); // keep its share open
                    }
                });
            }
        });
        let mut spans = spans.into_inner().unwrap();
        spans.sort();
        for w in spans.windows(2) {
            assert_eq!(w[0].1, w[1].0, "{:?} then {:?}", w[0], w[1]);
        }
    }

    #[test]
    fn counting_after_a_child_never_goes_back() {
        let t = Tracker::new();
        t.set_total(Some(4), Unit::ITEMS);
        {
            let half = t.child("half", 0.5);
            half.set_total(Some(1), Unit::ITEMS);
            half.set(1);
        }
        assert_eq!(frac(&t), 0.5);
        t.inc(1);
        assert_eq!(frac(&t), 0.5);
        t.set(3);
        assert_eq!(frac(&t), 0.75);
    }

    #[test]
    fn a_child_after_the_count_adds_to_it() {
        let t = Tracker::new();
        t.set_total(Some(4), Unit::ITEMS);
        t.set(2);
        let c = t.child("c", 0.25);
        c.set_total(Some(2), Unit::ITEMS);
        c.set(1);
        assert_eq!(frac(&t), 0.625);
        drop(c);
        assert_eq!(frac(&t), 0.75);
    }

    #[test]
    fn dropping_a_child_finishes_its_share() {
        let t = Tracker::new();
        drop(t.child("nothing to count", 0.4));
        assert_eq!(t.snapshot().fraction, Some(0.4));
        let c = t.child_tracker("stopped", 0.2);
        c.cancel();
        drop(c);
        assert_eq!(t.snapshot().fraction, Some(0.4));
    }

    #[test]
    fn a_child_of_a_cancelled_step_stays_open() {
        let t = Tracker::new();
        let step = t.child_tracker("step", 0.5);
        let item = step.child("item", 1.0);
        step.cancel();
        drop(item);
        drop(step);
        assert_eq!(t.snapshot().fraction, None);
    }

    #[test]
    fn a_step_that_panics_still_finishes() {
        let t = Tracker::new();
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let step = t.child("step", 0.5);
            step.set_total(Some(10), Unit::ITEMS);
            step.inc(3);
            panic!("the step failed");
        }));
        assert!(unwound.is_err());
        assert_eq!(frac(&t), 0.5);
    }

    #[test]
    fn a_share_waits_for_its_children() {
        let t = Tracker::new();
        let step = t.child_tracker("step", 0.5);
        let worker = step.child_tracker("worker", 1.0);
        worker.set_total(Some(10), Unit::ITEMS);
        drop(step);
        assert_eq!(frac(&t), 0.0);
        worker.set(5);
        assert_eq!(frac(&t), 0.25);
        drop(worker);
        assert_eq!(frac(&t), 0.5);
    }

    #[test]
    fn a_dropped_step_hands_back_to_its_parent() {
        let t = Tracker::new();
        t.set_total(Some(2), Unit::Count("stages"));
        let stage = t.child("stage", 0.5);
        let file = stage.child("file", 0.5);
        file.set_total(Some(4), Unit::Bytes);
        file.set(1);
        assert_eq!(t.snapshot().step, "file");
        drop(file);
        assert_eq!(t.snapshot().step, "stage");
        drop(stage);
        let s = t.snapshot();
        assert_eq!(
            (s.step.as_str(), s.done, s.total, s.unit),
            ("", 0, Some(2), Unit::Count("stages"))
        );
        // Dropping a progress that isn't the step leaves the step alone.
        let a = t.child_tracker("a", 0.25);
        let b = t.child_tracker("b", 0.25);
        a.inc(1);
        b.inc(1);
        drop(a);
        assert_eq!(t.snapshot().step, "b");
    }

    #[test]
    fn cancel_reaches_every_child() {
        let t = Tracker::new();
        let handle = t.clone();
        let child = t.child("a", 0.5);
        let grandchild = child.child("b", 0.5);
        assert_eq!(grandchild.check(), Ok(()));
        std::thread::spawn(move || handle.cancel()).join().unwrap();
        assert_eq!(grandchild.check(), Err(Cancelled));
        assert!(t.snapshot().cancelled);
    }

    #[test]
    fn cancelling_a_child_leaves_the_rest() {
        let t = Tracker::new();
        let a = t.child_tracker("a", 0.5);
        let b = t.child_tracker("b", 0.5);
        let a1 = a.child("a1", 1.0);
        a.cancel();
        assert_eq!(a1.check(), Err(Cancelled));
        assert_eq!(b.check(), Ok(()));
        assert_eq!(t.check(), Ok(()));
        assert!(!t.snapshot().cancelled);
    }

    #[test]
    fn nan_weight_gets_nothing() {
        let t = Tracker::new();
        let nan = t.child_tracker("nan", f32::NAN);
        assert_eq!(nan.0.node.span(), 0);
        let rest = t.child_tracker("rest", 0.5);
        assert_eq!((rest.0.node.lo, rest.0.node.hi), (0, ONE / 2));
    }

    #[test]
    fn the_step_is_the_last_to_report() {
        let t = Tracker::new();
        let a = t.child_tracker("a", 0.5);
        let b = t.child_tracker("b", 0.5);
        a.set_total(Some(3), Unit::Count("files"));
        b.set_total(Some(9), Unit::Bytes);
        b.set(4);
        a.set(2);
        let s = t.snapshot();
        assert_eq!(
            (s.step.as_str(), s.done, s.total, s.unit),
            ("a", 2, Some(3), Unit::Count("files"))
        );
        b.inc(1);
        assert_eq!(t.snapshot().step, "b");
    }

    #[test]
    fn messages_keep_the_latest() {
        let t = Tracker::new();
        t.child("x", 1.0).message(Level::Warn, "skipped 1 file");
        assert_eq!(
            t.snapshot().message,
            Some((Level::Warn, "skipped 1 file".into()))
        );
        // A repeat still counts as a new message.
        t.message(Level::Warn, "skipped 1 file");
        assert_eq!(t.snapshot().messages, 2);
        assert!(Level::Error > Level::Warn && Level::Warn > Level::Info);
    }

    #[test]
    fn no_progress_never_cancels() {
        let p = NoProgress;
        p.set_total(Some(3), Unit::ITEMS);
        assert!(p.child("c", 1.0).check().is_ok());
    }
}

/// Model checks of the lock-free updates, run with `just loom`.
#[cfg(all(test, loom))]
mod loom_tests {
    use super::*;
    use loom::thread::{self, JoinHandle};

    /// Runs `f` on a new thread with a clone of `t`.
    fn spawn_with<R: 'static>(
        t: &Tracker,
        f: impl FnOnce(Tracker) -> R + Send + 'static,
    ) -> JoinHandle<R> {
        let t = t.clone();
        thread::spawn(move || f(t))
    }

    fn fraction(t: &Tracker) -> Option<f32> {
        t.snapshot().fraction
    }

    #[test]
    fn siblings_on_threads_add_up() {
        loom::model(|| {
            let t = Tracker::new();
            let a = t.child_tracker("a", 0.5);
            let b = t.child_tracker("b", 0.5);
            a.set_total(Some(1), Unit::ITEMS);
            b.set_total(Some(1), Unit::ITEMS);
            let other = spawn_with(&a, |a| a.inc(1));
            b.inc(1);
            other.join().unwrap();
            assert_eq!(fraction(&t), Some(1.0));
        });
    }

    #[test]
    fn clones_on_threads_add_up() {
        loom::model(|| {
            let t = Tracker::new();
            t.set_total(Some(2), Unit::ITEMS);
            let other = spawn_with(&t, |t| t.inc(1));
            t.inc(1);
            other.join().unwrap();
            assert_eq!(fraction(&t), Some(1.0));
        });
    }

    #[test]
    fn a_total_and_a_count_set_at_once_both_count() {
        loom::model(|| {
            let t = Tracker::new();
            let other = spawn_with(&t, |t| t.set_total(Some(1), Unit::ITEMS));
            t.inc(1);
            other.join().unwrap();
            assert_eq!(fraction(&t), Some(1.0));
        });
    }

    #[test]
    fn a_total_and_a_count_that_cancel_out() {
        loom::model(|| {
            let t = Tracker::new();
            t.set_total(Some(8), Unit::ITEMS);
            t.set(7);
            let other = spawn_with(&t, |t| t.set_total(Some(16), Unit::ITEMS));
            t.inc(7);
            other.join().unwrap();
            assert_eq!(fraction(&t), Some(0.875));
        });
    }

    #[test]
    fn a_full_bar_whose_total_and_count_grow() {
        loom::model(|| {
            let t = Tracker::new();
            t.set_total(Some(8), Unit::ITEMS);
            t.set(8);
            let other = spawn_with(&t, |t| t.set_total(Some(16), Unit::ITEMS));
            t.inc(8);
            other.join().unwrap();
            assert_eq!(fraction(&t), Some(1.0));
        });
    }

    #[test]
    fn a_parent_total_races_a_child_count() {
        loom::model(|| {
            let t = Tracker::new();
            t.set_total(Some(8), Unit::ITEMS);
            let c = t.child_tracker("c", 0.5);
            c.set_total(Some(1), Unit::ITEMS);
            t.set(7);
            let other = spawn_with(&c, |c| c.inc(1));
            t.set_total(Some(16), Unit::ITEMS);
            other.join().unwrap();
            // The child's half beats the count's 7/16.
            assert_eq!(fraction(&t), Some(0.5));
        });
    }

    #[test]
    fn a_parent_total_races_a_child_finishing() {
        loom::model(|| {
            let t = Tracker::new();
            t.set_total(Some(8), Unit::ITEMS);
            let c = t.child_tracker("c", 0.5);
            t.set(7);
            let other = thread::spawn(move || drop(c));
            t.set_total(Some(16), Unit::ITEMS);
            other.join().unwrap();
            assert_eq!(fraction(&t), Some(0.5));
        });
    }

    #[test]
    fn a_parent_count_lowered_while_a_child_counts() {
        loom::model(|| {
            let t = Tracker::new();
            t.set_total(Some(8), Unit::ITEMS);
            let c = t.child_tracker("c", 0.5);
            c.set_total(Some(1), Unit::ITEMS);
            t.set(7);
            let other = spawn_with(&c, |c| c.inc(1));
            t.set(1);
            other.join().unwrap();
            assert_eq!(fraction(&t), Some(0.5));
        });
    }

    #[test]
    fn siblings_finish_at_once() {
        loom::model(|| {
            let t = Tracker::new();
            let a = t.child_tracker("a", 0.5);
            let b = t.child_tracker("b", 0.5);
            let other = thread::spawn(move || drop(a));
            drop(b);
            other.join().unwrap();
            assert_eq!(fraction(&t), Some(1.0));
        });
    }

    #[test]
    fn a_share_finishes_when_its_last_child_drops_elsewhere() {
        loom::model(|| {
            let t = Tracker::new();
            let step = t.child_tracker("step", 0.5);
            let worker = step.child_tracker("worker", 0.5);
            let other = thread::spawn(move || {
                worker.set_total(Some(1), Unit::ITEMS);
                worker.inc(1);
            });
            drop(step);
            other.join().unwrap();
            let s = t.snapshot();
            assert_eq!((s.step.as_str(), s.fraction), ("", Some(0.5)));
        });
    }

    #[test]
    fn placing_a_child_races_a_lowered_count() {
        loom::model(|| {
            let t = Tracker::new();
            t.set_total(Some(2), Unit::ITEMS);
            t.set(1);
            let other = spawn_with(&t, |t| {
                let c = t.child_tracker("c", 0.5);
                c.cancel(); // Keep its share open.
                c
            });
            t.set(0);
            let c = other.join().unwrap();
            // Placed after the count, the gap before it stays done.
            if c.0.node.lo == ONE / 2 {
                assert_eq!(fraction(&t), Some(0.5));
            }
        });
    }
}
