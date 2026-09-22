# The single entry point of the quality harness. Plain `make` runs everything.
.PHONY: qa fmt lint test audit fix release modules

qa: fmt lint test audit

fmt:
	cargo fmt --all --check

lint:
	cargo clippy --all-targets --all-features -- -D warnings

test:
	cargo test --all-features

audit:
	cargo deny check

fix:
	cargo fmt --all
	cargo clippy --all-targets --all-features --fix --allow-dirty -- -D warnings

release:
	cargo build --release

modules: # diagnostic, never a gate: cargo-modules visualizes, it enforces nothing
	cargo modules structure --lib
