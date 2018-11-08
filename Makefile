.PHONY: watch
watch:
	cargo watch -x check -x "test --all-features" --clear

.PHONY: run
run: debug
	RUST_LOG=INFO ./target/debug/examples/basic

.PHONY: doc
doc:
	cargo doc --open

.PHONY: build
build:
	cargo build --release

.PHONY: debug
debug:
	cargo build

.PHONY: test
test:
	cargo test --all-features

.PHONY: clippy
clippy:
	cargo +nightly clippy

.PHONY: fuzz
fuzz:
	cargo +nightly fuzz run parse_command
