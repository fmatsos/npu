# The single entry point of the quality harness. Plain `make` runs everything.
.PHONY: qa fmt lint test audit fix release modules

qa: fmt lint test audit

fmt:
	cargo fmt --all --check

lint:
	cargo clippy --all-targets --all-features -- -D warnings
	RUSTDOCFLAGS="-D warnings -A rustdoc::private_intra_doc_links" cargo doc --no-deps --document-private-items --all-features
	@! grep -rnE 'L[0-9] review|review L[0-9]|[Pp]hase [0-9]|§ ?[0-9]|[Ff]ix [0-9]|shared contract' src tests Cargo.toml \
		|| { echo 'historical reference in a comment: state the rule, not where it came from'; exit 1; }

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
