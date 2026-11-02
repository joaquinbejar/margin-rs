# Makefile for margin-rs

MSRV := 1.85.0

.PHONY: all
all: pre-push

.PHONY: build
build:
	cargo build --all-targets --all-features

.PHONY: test
test:
	cargo test --all-targets --all-features
	cargo test --doc --all-features

.PHONY: fmt
fmt:
	cargo +stable fmt --all

.PHONY: fmt-check
fmt-check:
	cargo +stable fmt --all --check

.PHONY: lint
lint:
	cargo clippy --all-targets --all-features --workspace -- -D warnings

.PHONY: doc
doc:
	RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features

.PHONY: msrv
msrv:
	cargo +$(MSRV) check --locked --all-targets --all-features

# Check-only gate: it never rewrites files. Run `make fmt` to format.
.PHONY: pre-push
pre-push: fmt-check lint test doc msrv

.PHONY: clean
clean:
	cargo clean
