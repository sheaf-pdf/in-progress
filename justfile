# Development tasks. CI runs these same recipes, so a local `just ci` checks
# what CI checks.

# The oldest Rust the crate supports, from Cargo.toml.
msrv := `grep -m1 '^rust-version' Cargo.toml | cut -d'"' -f2`

# List the recipes.
default:
    @just --list --unsorted

# Everything CI runs.
ci: fmt clippy test doc examples loom loom-lint msrv

# Check formatting; `cargo fmt --all` fixes it.
fmt:
    cargo fmt --all -- --check

# Lint the library, tests and examples.
clippy:
    cargo clippy --locked --all-targets -- -D warnings

# Unit tests and doctests, building the examples too.
test:
    cargo test --locked

# Build the API docs, failing on warnings.
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps

# Stdin is at its end for `cancel`, so it runs to completion rather than
# waiting for Enter.
# Run each example once, with short inputs.
examples:
    cargo run --locked --example merge_sort -- 4096
    cargo run --locked --example cancel < /dev/null
    cargo run --locked --example workers
    cargo run --locked --example custom_progress

# Model-check the concurrent updates with loom.
loom:
    RUSTFLAGS="--cfg loom" cargo test --locked --release --lib

# Projects that model-check their own code build their dependencies with
# --cfg loom too, so the library has to build that way without loom.
# Build and lint with --cfg loom.
loom-lint:
    RUSTFLAGS="--cfg loom" cargo check --locked --lib
    RUSTFLAGS="--cfg loom" cargo clippy --locked --release --all-targets -- -D warnings

# Test on the oldest supported Rust, which must be installed.
msrv:
    cargo +{{ msrv }} test --locked
