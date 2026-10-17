# in-progress

Progress reporting and cancellation for long-running operations, in one small, dependency-free Rust crate.

An operation takes a `&dyn Progress`, says how far along it is, and calls `p.check()?` once per page or chunk so a cancel stops it cleanly. The caller decides what to do with the reports: a GUI can poll a snapshot every frame, a CLI can draw a bar, and a caller that doesn't care can pass `NoProgress`.

## Status

Early (`0.1.0`), not yet published. The whole library is a single file, `src/lib.rs`, with unit tests. It has no dependencies and uses only `std`. The API may change.

## Usage

```rust
use in_progress::{Cancelled, Level, NoProgress, Progress, Tracker, Unit};

// An op only knows about `&dyn Progress`.
fn ocr(pages: &[Page], p: &dyn Progress) -> Result<(), Cancelled> {
    p.set_total(Some(pages.len() as u64), Unit::Pages);
    for page in pages {
        p.check()?; // stops here if someone cancelled
        // ... do the work ...
        p.inc(1);
    }
    Ok(())
}

// A job made of steps hands each step a weighted child. The step doesn't
// need to know where it sits in the whole.
fn job(pages: &[Page], p: &dyn Progress) -> Result<(), Cancelled> {
    render(pages, &*p.child("render", 0.25))?;
    ocr(pages, &*p.child("ocr", 0.75))
}

let tracker = Tracker::new();
let handle = tracker.clone(); // clones share the same progress
std::thread::spawn(move || job(&pages, &handle));

// From a UI thread:
let snap = tracker.snapshot(); // fraction, step, done/total/unit, message, cancelled
tracker.cancel();              // from any thread
```

Callers that don't care about progress pass `&NoProgress`.

## Concepts

- **`Progress`** is the trait ops depend on: `set_total`, `set`, `inc`, `message`, `is_cancelled`, `check` and `child`.
- **`Tracker`** is the shared implementation. It is cheap to clone, and every clone is the same progress. `snapshot()` gives the state of the whole job, from whichever tracker in it you call it on.
- **`NoProgress`** reports nowhere and never cancels.
- **`Snapshot`** is a plain value for pollers. It has the overall `fraction` (`None` until something is known), the current `step` name with its `done`, `total` and `unit`, the latest `message`, a `messages` counter so a poller can tell a new message from a repeat, and `cancelled`.
- **`Unit`** is `Items`, `Pages` or `Bytes`. It is a closed set so it can cross plugin boundaries. `Unit::noun()` gives the plural for "3 of 10 pages".
- **`Cancelled`** is the error ops return when they stop. Callers downcast to it to tell a cancel from a failure.
- **`Level`** is a message's severity: `Info`, `Warn` or `Error`.

### How a tracker adds up

- A progress's own count fills its span from the start. A child's share starts after that count and after the earlier children's shares.
- Children add up, so siblings working at once on other threads each move the bar by their share. Use `Tracker::child_tracker` when a child must be owned, for example by a worker thread or a plugin handle.
- A progress that counts and has children shows whichever is further: its count, or its children plus what its count had covered before them. The bar never moves backwards.
- Dropping a child finishes its share, so a step that ends early or counts nothing still moves the bar on. A cancelled child doesn't.
- Cancelling a tracker stops it and its children, not its parent or siblings.

### Implementation notes

The whole job's span is fixed point (`1 << 48`), so concurrent children add up with atomic integer adds rather than locks. Only the "last step" and "latest message" fields take a mutex. A panic while holding one is tolerated, because the data behind it is plain.

## Development

```sh
cargo test
```

The tests cover fractions and indeterminate totals, nested children, siblings on threads, non-overlapping concurrent child placement, monotonic bars, drop and cancel semantics, NaN weights, step selection, and messages.

## License

Licensed under either of Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE)) or MIT license ([LICENSE-MIT](LICENSE-MIT)) at your option.
