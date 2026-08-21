# trpr development tasks — `just --list` to see these

# Everything CI would run
default: check

check: fmt-check lint test

fmt:
    cargo fmt

fmt-check:
    cargo fmt --check

# Clippy with warnings promoted to errors, over all targets (tests included)
lint:
    cargo clippy --all-targets -- -D warnings

test:
    cargo test

build:
    cargo build --release
