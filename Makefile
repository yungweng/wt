.PHONY: build check install

build:
	cargo build --release

check:
	cargo fmt --all -- --check
	cargo clippy --all-targets --all-features -- -D warnings
	cargo test --all-targets

install:
	cargo install --path . --force
