//! Progress from rayon's parallel iterators, with no adapter: `inspect`
//! counts each item as it passes a point in the chain, on whichever thread
//! has it. Two jobs put it in different places.
//!
//! The first job makes thumbnails in three stages: load, resize and save. An
//! `inspect` after each `map` counts the images through that stage into a
//! child weighted by the stage's cost. Rayon runs the whole chain for an item
//! on one thread, so the stages run at the same time, not one after another,
//! and their three shares fill together.
//!
//! The second job looks for one record with `find_any`, and an `inspect` in
//! front of it counts the records checked. Rayon stops as soon as a thread
//! finds a match, so the count stops well short of the total. Dropping the
//! step's progress finishes its share anyway, so the bar ends at 100%.
//!
//! Each `inc` updates counters every thread shares, so for items that take
//! only microseconds, count per chunk rather than per item.
//!
//! ```sh
//! cargo run --example rayon_pipeline
//! ```

mod support;

use in_progress::{Level, Progress, Tracker, Unit};
use rayon::prelude::*;
use std::{thread, time::Duration};

fn main() {
    // Enough work per thread that each job takes a couple of seconds on any
    // machine.
    let threads = rayon::current_num_threads() as u64;

    let tracker = Tracker::new();
    let job = thread::spawn({
        let tracker = tracker.clone();
        move || thumbnails(40 * threads, &tracker)
    });
    let made = support::watch(&tracker, job);
    println!("Made {made} thumbnails on {threads} threads.");

    let tracker = Tracker::new();
    let records = 2000 * threads;
    let job = thread::spawn({
        let tracker = tracker.clone();
        move || search(records, &tracker)
    });
    match support::watch(&tracker, job) {
        Some(record) => println!("Found record {record} of {records}."),
        None => println!("No record matched."),
    }
}

/// Three stages, each with a share weighted by its cost: resizing takes
/// three times as long as loading or saving.
fn thumbnails(images: u64, p: &dyn Progress) -> usize {
    let loaded = p.child("load", 0.2);
    let resized = p.child("resize", 0.6);
    let saved = p.child("save", 0.2);
    for stage in [&loaded, &resized, &saved] {
        stage.set_total(Some(images), Unit::Count("images"));
    }
    (0..images)
        .into_par_iter()
        .map(|image| work(image, 10))
        .inspect(|_| loaded.inc(1))
        .map(|image| work(image, 30))
        .inspect(|_| resized.inc(1))
        .map(|image| work(image, 10))
        .inspect(|_| saved.inc(1))
        .count()
}

/// Checks records until one matches.
fn search(records: u64, p: &dyn Progress) -> Option<u64> {
    let checking = p.child("check", 1.0);
    checking.set_total(Some(records), Unit::Count("records"));
    let target = records * 3 / 5;
    let found = (0..records)
        .into_par_iter()
        .inspect(|_| checking.inc(1))
        .find_any(|&record| work(record, 1) == target);
    if let Some(record) = found {
        p.message(Level::Info, &format!("found record {record}"));
    }
    found
}

/// A stand-in for real work that takes `ms` milliseconds.
fn work(item: u64, ms: u64) -> u64 {
    thread::sleep(Duration::from_millis(ms));
    item
}
