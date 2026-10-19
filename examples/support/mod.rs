//! Draws a job's progress with indicatif while it runs.
//!
//! in-progress tracks a job but doesn't draw it. This is the glue: a loop
//! that polls the tracker's snapshot and copies it into an indicatif bar.

use in_progress::{Snapshot, Tracker, Unit};
use indicatif::{HumanBytes, ProgressBar, ProgressStyle};
use std::{
    thread::{self, JoinHandle},
    time::Duration,
};

/// The bar's length, so the fraction is shown in thousandths.
const LEN: u64 = 1000;

/// Draws `tracker`'s progress until `job` finishes, then returns its result.
///
/// indicatif hides the bar when stderr isn't a terminal, but messages are
/// still printed.
pub fn watch<T>(tracker: &Tracker, job: JoinHandle<T>) -> T {
    let bar = ProgressBar::new(LEN).with_style(style("{spinner} {msg}"));
    bar.enable_steady_tick(Duration::from_millis(100));
    let mut known = false;
    let mut messages = 0;
    loop {
        let finished = job.is_finished();
        let s = tracker.snapshot();
        // A spinner until the fraction is known; once known, it stays known.
        if let Some(fraction) = s.fraction {
            if !known {
                bar.set_style(style("{bar:24} {percent:>3}%  {msg}"));
                known = true;
            }
            bar.set_position((fraction * LEN as f32).round() as u64);
        }
        bar.set_message(status(&s));
        // The snapshot keeps only the latest message, so a jump of more than
        // one means some were replaced between polls.
        if s.message_count > messages {
            if let Some((level, msg)) = &s.message {
                bar.suspend(|| eprintln!("{level:?}: {msg}"));
            }
            messages = s.message_count;
        }
        if finished {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    // Leaves the bar where the job left it: `finish` would fill it, even
    // after a cancelled step.
    bar.abandon();
    job.join().unwrap_or_else(|e| std::panic::resume_unwind(e))
}

fn style(template: &str) -> ProgressStyle {
    ProgressStyle::with_template(template).expect("a valid template")
}

/// The step and its count, such as `copy  1.50 MiB/12.00 MiB` or
/// `scan  12 files`.
fn status(s: &Snapshot) -> String {
    let amount = |n: u64| match s.unit {
        Unit::Count(_) => n.to_string(),
        Unit::Bytes => HumanBytes(n).to_string(),
    };
    let count = match s.total {
        Some(total) => format!("{}/{}", amount(s.done), amount(total)),
        None if s.done > 0 => amount(s.done),
        None => return s.step.clone(),
    };
    let noun = match s.unit {
        Unit::Count(noun) => noun,
        Unit::Bytes => "",
    };
    format!("{}  {count} {noun}", s.step).trim().to_owned()
}
