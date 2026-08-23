default:
    @just --list

build:
    cargo build --locked

check:
    cargo fmt --all -- --check
    cargo clippy --all-targets --locked -- -D warnings
    cargo test --all-targets --locked
    python3 -m unittest -v tests/test_hook_example.py

format:
    cargo fmt --all

test:
    cargo test --all-targets --locked
    python3 -m unittest -v tests/test_hook_example.py

release:
    cargo build --release --locked
