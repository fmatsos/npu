# Point d'entrée unique du harness qualité. `make` seul lance tout.
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

modules: # diagnostic, jamais un gate : cargo-modules visualise, il n'enforce pas
	cargo modules structure --lib
