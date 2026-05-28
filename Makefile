.PHONY: all build test fmt clippy clean

all: fmt clippy build test

build:
	cargo build --workspace

test:
	cargo test --workspace

fmt:
	cargo fmt --all -- --check

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

clean:
	cargo clean
