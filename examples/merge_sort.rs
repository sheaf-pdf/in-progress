//! Merge sort that tracks the problem as it divides it.
//!
//! Each call hands its two halves, and then the merge that joins them, a
//! child progress weighted by how long it takes. No call knows where it sits
//! in the whole sort, yet the bar moves evenly, and the step shown is the
//! range being worked on. The top levels sort their halves on two threads at
//! once, so sibling shares fill together.
//!
//! ```sh
//! cargo run --example merge_sort -- [len]
//! ```

mod support;

use in_progress::{Cancelled, Progress, Tracker, Unit};
use std::{
    thread,
    time::{Duration, Instant},
};

/// Runs this short are sorted directly.
const LEAF: usize = 64;
/// Levels that sort their halves on two threads at once: up to 2³ threads.
const PARALLEL_LEVELS: u32 = 3;
/// Elements merged between checks.
const CHUNK: usize = 256;
/// A stand-in for an expensive comparison, so the sort is slow enough to
/// watch.
const PER_COMPARISON: Duration = Duration::from_micros(20);

fn main() {
    let len = std::env::args()
        .nth(1)
        .map_or(1 << 16, |arg| arg.parse().expect("usage: merge_sort [len]"));
    println!(
        "Sorting {len} numbers, on up to {} threads.",
        1 << PARALLEL_LEVELS
    );
    let tracker = Tracker::new();
    let started = Instant::now();
    let job = thread::spawn({
        let tracker = tracker.clone();
        move || {
            let mut v = random(len);
            let mut buf = vec![0; len];
            merge_sort(&mut v, &mut buf, 0, 0, &tracker).map(|()| v)
        }
    });
    let v = support::watch(&tracker, job).expect("nothing cancels this sort");
    assert!(v.is_sorted());
    assert_eq!(tracker.snapshot().fraction, Some(1.0));
    println!("Sorted in {:.1?}.", started.elapsed());
}

/// Sorts `v`, using `buf`, of the same length, as scratch space. `start` is
/// where `v` begins in the whole slice, for naming steps, and `level` is how
/// deep the recursion is.
fn merge_sort(
    v: &mut [u64],
    buf: &mut [u64],
    start: usize,
    level: u32,
    p: &dyn Progress,
) -> Result<(), Cancelled> {
    p.check()?;
    let n = v.len();
    if n <= LEAF {
        p.set_total(Some(n as u64), Unit::Count("numbers"));
        pause(comparisons(n));
        v.sort_unstable();
        p.set(n as u64);
        return Ok(());
    }

    // A half's share is the part of this call's time it takes. Halves that
    // run one after the other each take their own time. Halves that run at
    // once move the bar together, so each takes half of that time.
    let mid = n / 2;
    let at_once = level < PARALLEL_LEVELS;
    let whole = time(n, level);
    let share = |len| ((if at_once { 0.5 } else { 1.0 }) * time(len, level + 1) / whole) as f32;
    let left = p.child(&format!("sort {start}..{}", start + mid), share(mid));
    let right = p.child(
        &format!("sort {}..{}", start + mid, start + n),
        share(n - mid),
    );
    {
        let (v_left, v_right) = v.split_at_mut(mid);
        let (buf_left, buf_right) = buf.split_at_mut(mid);
        if at_once {
            // The children are `Send`, so one can go to another thread.
            thread::scope(|s| {
                let other = s.spawn(|| merge_sort(v_left, buf_left, start, level + 1, &left));
                let here = merge_sort(v_right, buf_right, start + mid, level + 1, &right);
                let other = other
                    .join()
                    .unwrap_or_else(|e| std::panic::resume_unwind(e));
                other.and(here)
            })?;
        } else {
            merge_sort(v_left, buf_left, start, level + 1, &left)?;
            merge_sort(v_right, buf_right, start + mid, level + 1, &right)?;
        }
    }
    // Both halves are sorted, and dropping them finishes their shares.
    drop((left, right));

    // The merge takes the rest of this call's share: a weight that runs past
    // the end of the share is cut short. Taking all of what's left, rather
    // than a computed weight, leaves no rounding gap, so the bar ends at
    // exactly 100%.
    let merging = p.child(&format!("merge {start}..{}", start + n), 1.0);
    merging.set_total(Some(n as u64), Unit::Count("numbers"));
    merge(v, buf, mid, &merging)
}

/// Merges the sorted runs `v[..mid]` and `v[mid..]` through `buf`.
fn merge(v: &mut [u64], buf: &mut [u64], mid: usize, p: &dyn Progress) -> Result<(), Cancelled> {
    let (mut i, mut j) = (0, mid);
    for chunk in buf.chunks_mut(CHUNK) {
        p.check()?;
        pause(chunk.len() as f64);
        for slot in chunk.iter_mut() {
            if j == v.len() || (i < mid && v[i] <= v[j]) {
                *slot = v[i];
                i += 1;
            } else {
                *slot = v[j];
                j += 1;
            }
        }
        p.inc(chunk.len() as u64);
    }
    v.copy_from_slice(buf);
    Ok(())
}

/// About how many comparisons sorting `n` numbers takes: n·log₂(n).
fn comparisons(n: usize) -> f64 {
    let n = n as f64;
    n * n.log2().max(1.0)
}

/// About how long sorting `n` numbers at `level` takes, in comparisons made
/// one after another. Halves sorted at once take as long as one of them.
fn time(n: usize, level: u32) -> f64 {
    if level < PARALLEL_LEVELS && n > LEAF {
        time(n - n / 2, level + 1) + n as f64
    } else {
        comparisons(n)
    }
}

fn pause(comparisons: f64) {
    thread::sleep(PER_COMPARISON.mul_f64(comparisons));
}

/// `len` pseudo-random numbers, the same each run.
fn random(len: usize) -> Vec<u64> {
    let mut x = 0x2545_f491_4f6c_dd1d_u64;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        })
        .collect()
}
