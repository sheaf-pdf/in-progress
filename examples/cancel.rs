//! Cancelling a job from another thread: press Enter to stop it.
//!
//! The job is a make-believe backup in three steps. A thread reading stdin
//! cancels the tracker when you press Enter. The job's next `check()` fails,
//! and `Err(Cancelled)` comes back up through each step. The bar stays where
//! the job got to, since a cancelled step's share is left unfinished.
//!
//! ```sh
//! cargo run --example cancel
//! ```

mod support;

use in_progress::{Cancelled, Level, Progress, Tracker, Unit};
use std::{io, thread, time::Duration};

const BLOCK: u64 = 256 << 10;

struct File {
    name: String,
    size: u64,
}

fn main() {
    let tracker = Tracker::new();
    thread::spawn({
        let tracker = tracker.clone();
        move || {
            let mut line = String::new();
            // The end of input, as when stdin isn't a terminal, isn't a
            // request to stop.
            if io::stdin().read_line(&mut line).is_ok_and(|n| n > 0) {
                tracker.cancel();
            }
        }
    });
    println!("Backing up. Press Enter to cancel.");
    let job = thread::spawn({
        let tracker = tracker.clone();
        move || backup(&tracker)
    });
    match support::watch(&tracker, job) {
        Ok(files) => println!("Backed up {files} files."),
        Err(Cancelled) => {
            let s = tracker.snapshot();
            assert!(s.cancelled);
            let percent = s.fraction.unwrap_or(0.0) * 100.0;
            println!("Cancelled at {percent:.0}%. The steps it didn't finish stay unfinished.");
        }
    }
}

fn backup(p: &dyn Progress) -> Result<usize, Cancelled> {
    let files = scan(&p.child("scan", 0.1))?;
    p.message(Level::Info, &format!("found {} files", files.len()));
    copy(&files, &p.child("copy", 0.8))?;
    verify(&files, &p.child("verify", 0.1))?;
    Ok(files.len())
}

/// Finds the files to back up. How many there are isn't known until it's
/// done.
fn scan(p: &dyn Progress) -> Result<Vec<File>, Cancelled> {
    p.set_total(None, Unit::Count("files"));
    let mut files = Vec::new();
    for i in 0..40 {
        p.check()?;
        thread::sleep(Duration::from_millis(15));
        files.push(File {
            name: format!("photo-{i:03}.jpg"),
            size: (1 + i * 7 % 11) * BLOCK,
        });
        p.inc(1);
    }
    Ok(files)
}

fn copy(files: &[File], p: &dyn Progress) -> Result<(), Cancelled> {
    p.set_total(Some(files.iter().map(|f| f.size).sum()), Unit::Bytes);
    for file in files {
        for _ in 0..file.size / BLOCK {
            // A real backup would delete the half-written copy here.
            p.check().inspect_err(|_| {
                let msg = format!("removed the partial copy of {}", file.name);
                p.message(Level::Warn, &msg);
            })?;
            thread::sleep(Duration::from_millis(20));
            p.inc(BLOCK);
        }
    }
    Ok(())
}

fn verify(files: &[File], p: &dyn Progress) -> Result<(), Cancelled> {
    p.set_total(Some(files.len() as u64), Unit::Count("files"));
    for _ in files {
        p.check()?;
        thread::sleep(Duration::from_millis(15));
        p.inc(1);
    }
    Ok(())
}
