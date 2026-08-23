default:
    @just --list

build:
    cargo build --locked

check:
    cargo fmt --all -- --check
    cargo clippy --all-targets --locked -- -D warnings
    cargo test --all-targets --locked

format:
    cargo fmt --all

test:
    cargo test --all-targets --locked

release:
    cargo build --release --locked

