//! Progress and cancellation for long-running operations.
//!
//! Ops take `&dyn Progress`, report how far they are, and call
//! `p.check()?` once per page or chunk so a cancel stops them cleanly.
//! [`Tracker`] is the shared implementation: the GUI polls its
//! [`Snapshot`] each frame and the CLI draws it as a bar; both cancel it
//! from another thread. [`NoProgress`] is for callers that don't care.
//!
//! A [`Progress::child`] covers a weighted share of its parent's span, so
//! a job made of steps ("render, then OCR") can hand each step its own
//! progress without the step knowing where it sits in the whole.
//!
//! How a [`Tracker`] adds up:
//!
//! - A progress's own count fills its span from the start. A child's share
//!   starts after that count and after the earlier children's shares.
//! - Children add up, so siblings working at once on other threads each
//!   move the bar by their share.
//! - A progress that counts and has children too shows whichever is
//!   further: its count, or its children plus what its count had covered
//!   before them. Mixing the two never moves the bar back.
//! - Dropping a child finishes its share, so a step that ends early or
//!   counts nothing still moves the bar on; a cancelled one doesn't.
//! - Cancelling a tracker stops it and its children, not its parent.

use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU64, AtomicUsize};
use std::sync::{Arc, Mutex, MutexGuard};

/// What a count counts. A closed set, so it can cross into plugins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Unit {
    #[default]
    Items,
    Pages,
    Bytes,
}

impl Unit {
    /// The plural noun, for "3 of 10 pages".
    pub fn noun(self) -> &'static str {
        match self {
            Unit::Items => "items",
            Unit::Pages => "pages",
            Unit::Bytes => "bytes",
        }
    }

    fn from_u8(v: u8) -> Unit {
        match v {
            1 => Unit::Pages,
            2 => Unit::Bytes,
            _ => Unit::Items,
        }
    }

    fn to_u8(self) -> u8 {
        match self {
            Unit::Items => 0,
            Unit::Pages => 1,
            Unit::Bytes => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

/// The error an op returns when it stops because it was cancelled.
/// Callers that need to tell a cancel from a failure downcast to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

pub trait Progress: Send + Sync {
    /// How many there are to do; `None` while that isn't known.
    fn set_total(&self, total: Option<u64>, unit: Unit);
    /// How many are done.
    fn set(&self, done: u64);
    fn inc(&self, n: u64);
    fn message(&self, level: Level, msg: &str);
    fn is_cancelled(&self) -> bool;
    /// `Err(Cancelled)` once cancelled, for `p.check()?`.
    fn check(&self) -> Result<(), Cancelled> {
        if self.is_cancelled() {
            Err(Cancelled)
        } else {
            Ok(())
        }
    }
    /// A progress that covers `weight` (0–1, NaN as 0) of this one's span,
    /// starting where this one's count is or where its last child's share
    /// ended, whichever is further. Dropping it finishes its share. It
    /// stops when this one is cancelled.
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

/// What a [`Tracker`] looks like right now.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Snapshot {
    /// Overall, 0–1; `None` until some count is known or some step done.
    pub fraction: Option<f32>,
    /// The step that last reported a count: its name, done, total, unit.
    pub step: String,
    pub done: u64,
    pub total: Option<u64>,
    pub unit: Unit,
    /// The latest message.
    pub message: Option<(Level, String)>,
    /// How many messages there have been, so a poller can tell a new
    /// message from a repeat of the last one.
    pub messages: u64,
    /// The whole job was cancelled.
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
    unit: AtomicU8,
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
            unit: AtomicU8::new(0),
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
    fn refresh(&self) {
        let mut node = self;
        loop {
            let mut old = node.filled.load(Relaxed);
            let new = loop {
                let new = node.progress();
                if new == old {
                    return;
                }
                match node
                    .filled
                    .compare_exchange_weak(old, new, Relaxed, Relaxed)
                {
                    Ok(_) => break new,
                    Err(now) => old = now,
                }
            };
            let Some(parent) = &node.parent else {
                return;
            };
            parent.children.fetch_add(new as i64 - old as i64, Relaxed);
            node = parent;
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
    /// Some count is known or some step done, so the fraction means
    /// something.
    known: AtomicBool,
    /// The node that last reported a count, for the snapshot's step.
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

/// The shared [`Progress`]: cheap to clone, every clone is the same
/// progress. Children share the root's snapshot; each can be cancelled
/// on its own.
#[derive(Clone)]
pub struct Tracker(Arc<Handle>);

/// A tracker and its clones. When the last of them goes, the node's
/// share is done.
struct Handle {
    shared: Arc<Shared>,
    node: Arc<Node>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        let node = &self.node;
        if node.parent.is_some() && !node.is_cancelled() {
            node.finished.store(true, Relaxed);
            self.shared.known.store(true, Relaxed);
            node.refresh();
        }
    }
}

impl Default for Tracker {
    fn default() -> Self {
        Self::new()
    }
}

impl Tracker {
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
        }))
    }

    /// [`Progress::child`] as an owned `Tracker`, for holders that can't
    /// borrow this one (a plugin's handle, say).
    pub fn child_tracker(&self, name: &str, weight: f32) -> Tracker {
        let parent = &self.0.node;
        let (lo, hi) = parent.place(weight);
        Tracker(Arc::new(Handle {
            shared: self.0.shared.clone(),
            node: Arc::new(Node::new(name, Some(parent.clone()), lo, hi)),
        }))
    }

    /// Ask the work to stop: this tracker and its children fail their next
    /// `check()`. Its parent and siblings carry on.
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
            unit: Unit::from_u8(last.unit.load(Relaxed)),
            message,
            messages,
            cancelled: root.cancelled.load(Relaxed),
        }
    }

    /// Publish this node's count: up the tree, and as the job's step.
    fn report(&self) {
        let Handle { shared, node } = &*self.0;
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
        node.total.store(total.unwrap_or(NO_TOTAL), Relaxed);
        node.unit.store(unit.to_u8(), Relaxed);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn frac(t: &Tracker) -> f32 {
        t.snapshot().fraction.unwrap()
    }

    #[test]
    fn counts_and_fraction() {
        let t = Tracker::new();
        assert_eq!(t.snapshot().fraction, None);
        t.set_total(Some(4), Unit::Pages);
        assert_eq!(frac(&t), 0.0);
        t.inc(1);
        t.inc(1);
        assert_eq!(frac(&t), 0.5);
        let s = t.snapshot();
        assert_eq!((s.done, s.total, s.unit), (2, Some(4), Unit::Pages));
        t.set(4);
        assert_eq!(frac(&t), 1.0);
        // Overshoot stays at the end.
        t.inc(3);
        assert_eq!(frac(&t), 1.0);
    }

    #[test]
    fn indeterminate_leaves_the_fraction() {
        let t = Tracker::new();
        t.set_total(None, Unit::Items);
        t.inc(5);
        assert_eq!(t.snapshot().fraction, None);
        assert_eq!(t.snapshot().done, 5);
    }

    #[test]
    fn children_fill_their_share() {
        let t = Tracker::new();
        t.set_total(Some(1), Unit::Items);
        {
            let render = t.child("render", 0.25);
            render.set_total(Some(10), Unit::Pages);
            render.set(10);
        }
        assert_eq!(frac(&t), 0.25);
        let ocr = t.child("ocr", 0.75);
        ocr.set_total(Some(2), Unit::Pages);
        ocr.inc(1);
        assert_eq!(frac(&t), 0.25 + 0.75 / 2.0);
        // Grandchildren nest inside their parent's share.
        let page = ocr.child("page 2", 0.5);
        page.set_total(Some(2), Unit::Items);
        page.set(1);
        assert_eq!(frac(&t), 0.625 + 0.75 * 0.5 * 0.5);
        let s = t.snapshot();
        assert_eq!(s.step, "page 2");
    }

    #[test]
    fn siblings_at_once_add_up() {
        let t = Tracker::new();
        let a = t.child_tracker("a", 0.5);
        let b = t.child_tracker("b", 0.5);
        a.set_total(Some(2), Unit::Items);
        b.set_total(Some(2), Unit::Items);
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
        std::thread::scope(|s| {
            for i in 0..8 {
                let step = t.child_tracker(&format!("worker {i}"), 1.0 / 8.0);
                s.spawn(move || {
                    step.set_total(Some(1000), Unit::Items);
                    for _ in 0..1000 {
                        step.inc(1);
                    }
                });
            }
        });
        assert_eq!(frac(&t), 1.0);
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
        t.set_total(Some(4), Unit::Items);
        {
            let half = t.child("half", 0.5);
            half.set_total(Some(1), Unit::Items);
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
        t.set_total(Some(4), Unit::Items);
        t.set(2);
        let c = t.child("c", 0.25);
        c.set_total(Some(2), Unit::Items);
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
        a.set_total(Some(3), Unit::Pages);
        b.set_total(Some(9), Unit::Bytes);
        b.set(4);
        a.set(2);
        let s = t.snapshot();
        assert_eq!(
            (s.step.as_str(), s.done, s.total, s.unit),
            ("a", 2, Some(3), Unit::Pages)
        );
        b.inc(1);
        assert_eq!(t.snapshot().step, "b");
    }

    #[test]
    fn messages_keep_the_latest() {
        let t = Tracker::new();
        t.child("x", 1.0).message(Level::Warn, "page 3 is blank");
        assert_eq!(
            t.snapshot().message,
            Some((Level::Warn, "page 3 is blank".into()))
        );
        // A repeat still counts as a new message.
        t.message(Level::Warn, "page 3 is blank");
        assert_eq!(t.snapshot().messages, 2);
    }

    #[test]
    fn no_progress_never_cancels() {
        let p = NoProgress;
        p.set_total(Some(3), Unit::Pages);
        assert!(p.child("c", 1.0).check().is_ok());
    }
}
