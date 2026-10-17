//! Implementing `Progress` yourself, for reporting this crate doesn't do.
//!
//! `Log` prints each step as it starts and ends, indented by depth, which
//! suits a log file or CI output better than a bar. Its `is_cancelled` is a
//! deadline, so the provided `check()` stops the job once time is up. The
//! same job then runs with `NoProgress`, for callers that don't want
//! reports.
//!
//! ```sh
//! cargo run --example custom_progress -- [time limit in ms]
//! ```

use in_progress::{Cancelled, Level, NoProgress, Progress, Unit};
use std::{
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
    thread,
    time::{Duration, Instant},
};

fn main() {
    let limit = std::env::args().nth(1).map_or(10_000, |arg| {
        arg.parse()
            .expect("usage: custom_progress [time limit in ms]")
    });
    let log = Log::new(0, Instant::now() + Duration::from_millis(limit));
    let logged = build(&log);
    println!("With Log: {logged:?}");
    let quiet = build(&NoProgress);
    println!("With NoProgress: {quiet:?}");
}

/// Prints what an operation reports instead of tracking it.
struct Log {
    depth: usize,
    deadline: Instant,
    /// The total and unit from `set_total`.
    total: Mutex<(Option<u64>, Unit)>,
    done: AtomicU64,
}

impl Log {
    fn new(depth: usize, deadline: Instant) -> Log {
        Log {
            depth,
            deadline,
            total: Mutex::new((None, Unit::ITEMS)),
            done: AtomicU64::new(0),
        }
    }

    fn print(&self, line: &str) {
        println!("{:indent$}{line}", "", indent = 2 * self.depth);
    }
}

impl Progress for Log {
    fn set_total(&self, total: Option<u64>, unit: Unit) {
        *self.total.lock().unwrap() = (total, unit);
        match total {
            Some(total) => self.print(&format!("{total} {} to do", unit.noun())),
            None => self.print(&format!("counting {}", unit.noun())),
        }
    }

    fn set(&self, done: u64) {
        self.done.store(done, Relaxed);
    }

    fn inc(&self, n: u64) {
        self.done.fetch_add(n, Relaxed);
    }

    fn message(&self, level: Level, msg: &str) {
        self.print(&format!("{level:?}: {msg}"));
    }

    fn is_cancelled(&self) -> bool {
        Instant::now() >= self.deadline
    }

    fn child(&self, name: &str, weight: f32) -> Box<dyn Progress + '_> {
        self.print(&format!("{name} ({:.0}%)", weight * 100.0));
        Box::new(Log::new(self.depth + 1, self.deadline))
    }
}

/// A step ends when its progress is dropped, however it ended.
impl Drop for Log {
    fn drop(&mut self) {
        let (total, unit) = *self.total.get_mut().unwrap();
        let Some(total) = total else {
            return; // It didn't count anything.
        };
        let done = *self.done.get_mut();
        let how = if self.is_cancelled() {
            "stopped at"
        } else {
            "finished"
        };
        self.print(&format!("{how} {done} of {total} {}", unit.noun()));
    }
}

const CRATES: [(&str, u64); 4] = [("serde", 40), ("regex", 30), ("tokio", 60), ("app", 20)];

/// A make-believe build: an operation that only sees `&dyn Progress`.
fn build(p: &dyn Progress) -> Result<(), Cancelled> {
    steps(&p.child("resolve", 0.1), 12, "packages")?;
    let compile = p.child("compile", 0.8);
    let units: u64 = CRATES.iter().map(|&(_, n)| n).sum();
    for (name, n) in CRATES {
        steps(&compile.child(name, n as f32 / units as f32), n, "units")?;
    }
    drop(compile);
    steps(&p.child("link", 0.1), 5, "objects")?;
    p.message(Level::Info, &format!("built {} crates", CRATES.len()));
    Ok(())
}

/// `n` units of make-believe work.
fn steps(p: &dyn Progress, n: u64, noun: &'static str) -> Result<(), Cancelled> {
    p.set_total(Some(n), Unit::Count(noun));
    for _ in 0..n {
        p.check()?;
        thread::sleep(Duration::from_millis(5));
        p.inc(1);
    }
    Ok(())
}
