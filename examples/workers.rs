//! A pool of worker threads, each job with a tracker of its own, and one job
//! cancelled while the rest carry on.
//!
//! `Progress::child` borrows its parent, which suits scoped threads. Threads
//! started with `std::thread::spawn` need handles they own, so each job gets
//! a `Tracker::child_tracker`. They're made up front, so the shares sit in
//! job order whichever worker picks a job up. Partway through, a supervisor
//! cancels the biggest job. Only that job stops, and its share is left
//! unfinished, so the bar ends short of 100%.
//!
//! ```sh
//! cargo run --example workers
//! ```

mod support;

use in_progress::{Cancelled, Level, Progress, Tracker, Unit};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        mpsc::{self, Sender},
    },
    thread,
    time::Duration,
};

const WORKERS: usize = 4;
const MIB: u64 = 1 << 20;

struct Job {
    name: &'static str,
    size: u64,
    progress: Tracker,
}

fn main() {
    let files = [
        ("disk.iso", 300 * MIB),
        ("photos.tar", 90 * MIB),
        ("music.zip", 70 * MIB),
        ("videos.tar", 120 * MIB),
        ("mail.mbox", 25 * MIB),
        ("code.tar", 40 * MIB),
        ("notes.zip", 10 * MIB),
        ("db.dump", 80 * MIB),
        ("logs.tar", 55 * MIB),
        ("docs.zip", 30 * MIB),
        ("backup.img", 65 * MIB),
        ("config.tar", 15 * MIB),
    ];
    let total: u64 = files.iter().map(|&(_, size)| size).sum();
    let tracker = Tracker::new();
    let queue: VecDeque<Job> = files
        .iter()
        .map(|&(name, size)| Job {
            name,
            size,
            progress: tracker.child_tracker(name, size as f32 / total as f32),
        })
        .collect();

    // Something outside the work decides that one job should stop, like a
    // time limit, or a user clicking the job's cancel button.
    let disk = queue[0].progress.clone();
    let supervisor = thread::spawn(move || {
        thread::sleep(Duration::from_millis(1500));
        disk.message(Level::Warn, "disk.iso is taking too long; cancelling it");
        disk.cancel();
    });

    let queue = Arc::new(Mutex::new(queue));
    let job = thread::spawn(move || {
        let (ended, results) = mpsc::channel();
        let workers: Vec<_> = (0..WORKERS)
            .map(|_| {
                let (queue, ended) = (queue.clone(), ended.clone());
                thread::spawn(move || work(&queue, &ended))
            })
            .collect();
        drop(ended);
        let results: Vec<_> = results.iter().collect();
        for worker in workers {
            worker.join().unwrap();
        }
        results
    });
    let results = support::watch(&tracker, job);
    supervisor.join().unwrap();

    println!("In the order they ended:");
    for (name, result) in &results {
        let how = if result.is_ok() { "done" } else { "cancelled" };
        println!("  {name:<12} {how}");
    }
    let s = tracker.snapshot();
    assert!(!s.cancelled, "cancelling one job leaves the rest running");
    let done = results.iter().filter(|(_, result)| result.is_ok()).count();
    let percent = s.fraction.unwrap_or(0.0) * 100.0;
    println!(
        "{done} of {} jobs done. The bar stopped at {percent:.0}%: the cancelled job's share stays unfinished.",
        results.len()
    );
}

/// Runs jobs from `queue` until it's empty, sending each result as it ends.
fn work(queue: &Mutex<VecDeque<Job>>, ended: &Sender<(&'static str, Result<(), Cancelled>)>) {
    loop {
        let Some(job) = queue.lock().unwrap().pop_front() else {
            return;
        };
        let result = copy(job.size, &job.progress);
        ended.send((job.name, result)).unwrap();
        // Dropping the job drops its tracker, which finishes its share
        // unless the job was cancelled.
    }
}

fn copy(size: u64, p: &dyn Progress) -> Result<(), Cancelled> {
    p.set_total(Some(size), Unit::Bytes);
    for _ in 0..size / MIB {
        p.check()?;
        thread::sleep(Duration::from_millis(20));
        p.inc(MIB);
    }
    Ok(())
}
