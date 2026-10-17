//! The progress line the examples draw while their job runs.

use in_progress::{Snapshot, Tracker, Unit};
use std::{
    io::{IsTerminal, Write},
    thread::{self, JoinHandle},
    time::Duration,
};

const WIDTH: usize = 24;

/// Draws `tracker`'s progress until `job` finishes, then returns its result.
///
/// On a terminal the line is redrawn in place; when the output is piped, a
/// plain line is printed every half second instead.
pub fn watch<T>(tracker: &Tracker, job: JoinHandle<T>) -> T {
    let tty = std::io::stdout().is_terminal();
    let mut polls = 0;
    while !job.is_finished() {
        if tty || polls % 10 == 0 {
            draw(&tracker.snapshot(), tty);
        }
        polls += 1;
        thread::sleep(Duration::from_millis(50));
    }
    draw(&tracker.snapshot(), tty);
    if tty {
        println!();
    }
    job.join().unwrap_or_else(|e| std::panic::resume_unwind(e))
}

fn draw(s: &Snapshot, tty: bool) {
    let fraction = s.fraction.unwrap_or(0.0);
    let filled = (fraction * WIDTH as f32).round() as usize;
    let mut line = format!(
        "[{}{}] {:>3.0}%",
        "#".repeat(filled),
        "·".repeat(WIDTH - filled),
        fraction * 100.0
    );
    if !s.step.is_empty() {
        line += &format!("  {}", s.step);
    }
    let noun = match s.unit {
        Unit::Count(noun) => format!(" {noun}"),
        Unit::Bytes => String::new(),
    };
    let done = amount(s.done, s.unit);
    match s.total {
        Some(total) => line += &format!("  {done}/{}{noun}", amount(total, s.unit)),
        None if s.done > 0 => line += &format!("  {done}{noun}"),
        None => {}
    }
    if let Some((level, msg)) = &s.message {
        line += &format!("  ({level:?}: {msg})");
    }
    let mut out = std::io::stdout().lock();
    if tty {
        // Back to the start of the line and clear it.
        write!(out, "\r\x1b[2K{line}").unwrap();
    } else {
        writeln!(out, "{line}").unwrap();
    }
    out.flush().unwrap();
}

/// `n` in `unit`: a plain number for counts, a size for bytes.
fn amount(n: u64, unit: Unit) -> String {
    match unit {
        Unit::Count(_) => n.to_string(),
        Unit::Bytes => {
            const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
            let mut size = n as f64;
            let mut i = 0;
            while size >= 1024.0 && i < UNITS.len() - 1 {
                size /= 1024.0;
                i += 1;
            }
            if i == 0 {
                format!("{n} B")
            } else {
                format!("{size:.1} {}", UNITS[i])
            }
        }
    }
}
