# in-progress

Progress reporting and cooperative cancellation for long-running operations, in one small Rust crate with no dependencies.

An operation takes a `&dyn Progress`, says how far along it is, and calls `check()?` between units of work so a cancel stops it cleanly. Whoever runs it holds a `Tracker`: it polls a `Snapshot` from another thread to draw a progress bar or a status line, and can cancel at any time. Callers that don't care pass `NoProgress`.

Steps nest. An operation hands each step a weighted child, and the step doesn't need to know where it sits in the whole job.

## Example

```rust
use in_progress::{Cancelled, Progress, Tracker, Unit};
use std::{thread, time::Duration};

// Operations only see `&dyn Progress`.
fn process(items: &[u32], progress: &dyn Progress) -> Result<u32, Cancelled> {
    progress.set_total(Some(items.len() as u64), Unit::Count("records"));
    let mut sum = 0;
    for item in items {
        progress.check()?;
        thread::sleep(Duration::from_millis(5)); // Stand-in for real work.
        sum += item;
        progress.inc(1);
    }
    Ok(sum)
}

// A job made of steps gives each one a weighted share of its own.
fn job(items: &[u32], progress: &dyn Progress) -> Result<u32, Cancelled> {
    process(items, &progress.child("validate", 0.25))?;
    process(items, &progress.child("sum", 0.75))
}

fn main() {
    let tracker = Tracker::new();
    let worker = thread::spawn({
        let tracker = tracker.clone();
        move || job(&[1, 2, 3], &tracker)
    });
    while !worker.is_finished() {
        let s = tracker.snapshot();
        let percent = s.fraction.unwrap_or(0.0) * 100.0;
        println!("{percent:.0}% {}: {} {}", s.step, s.done, s.unit.noun());
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(worker.join().unwrap(), Ok(6));
    assert_eq!(tracker.snapshot().fraction, Some(1.0));
}
```

Calling `tracker.cancel()` from any thread stops the job: its next `check()` returns `Err(Cancelled)`.

The [API documentation](https://docs.rs/in-progress) covers how shares add up, threads, and cancellation.

## Examples

Each one draws a live progress line and takes a few seconds:

- `cargo run --example merge_sort -- [len]`: a merge sort that gives each half of the problem, and the merge that joins them, a weighted child. The top levels sort on scoped threads, so sibling shares fill at once.
- `cargo run --example cancel`: a job of three steps. Press Enter to cancel it, and `Err(Cancelled)` comes back up through each step.
- `cargo run --example workers`: a pool of `thread::spawn` workers, each job with an owned `child_tracker`. One job is cancelled while the rest finish.
- `cargo run --example custom_progress -- [time limit in ms]`: implementing `Progress` yourself, as a log that prints the tree of steps and cancels at a deadline. The same job also runs with `NoProgress`.
- `cargo run --example rayon_pipeline`: progress from rayon's parallel iterators with no adapter, using `inspect`. Thumbnails go through three `map` stages, each counted into a child weighted by its cost. A search counts records checked before `find_any`, which stops early.

## Status

No dependencies beyond `std`; Rust 1.85 or later. The API may change before 1.0.

## Development

The checks are recipes in the [justfile](justfile), run with [just](https://github.com/casey/just):

```sh
just          # list the recipes
just ci       # run everything CI runs
just loom     # or one at a time
```

CI runs the same recipes on pushes and pull requests. The example above runs as a doctest.

## License

Licensed under either of Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE)) or MIT license ([LICENSE-MIT](LICENSE-MIT)) at your option.
