# The single entry point of the quality harness. Plain `make` runs everything.
.PHONY: qa fmt lint test audit fix release modules

qa: fmt lint test audit

fmt:
	cargo fmt --all --check

lint:
	cargo clippy --all-targets --all-features -- -D warnings
	@# Keeps the hardware-tooling seam a real compile-time boundary rather
	@# than a convention that rots: this must compile and lint clean with
	@# model discover and backend tune not even declared.
	cargo clippy --all-targets --no-default-features -- -D warnings
	RUSTDOCFLAGS="-D warnings -A rustdoc::private_intra_doc_links" cargo doc --no-deps --document-private-items --all-features
	@# Comment lines are joined first, so a reference wrapped across two lines is caught too.
	@perl -0777 -ne 's/\n\s*(?:\/\/[\/!]?|#)\s*/ /g; while (/(L[0-9] review|review L[0-9]|[Pp]hase [0-9]|§ ?[0-9]|[Ff]ix [0-9]|shared contract|[Ss]pec [AB][0-9]|\([AB][0-9]{1,2}[,)]|(?<![\w-])[AB][0-9]{1,2}:)/g) { print "$$ARGV: $$1\n"; $$f = 1 } END { if ($$f) { print "historical reference in a comment: state the rule, not where it came from\n"; exit 1 } }' \
		$$(find src tests -name '*.rs') Cargo.toml
	@# The engine knows protocols and processes, never a vendor: nothing
	@# under these modules may reference vendor::, in any spelling
	@# (crate::vendor, super::vendor, a `use` of it, or the bare path).
	@if grep -rnE '(^|[^A-Za-z0-9_])vendor::' \
		src/backend.rs src/exec.rs src/prompt.rs src/output.rs src/command.rs \
		src/config src/input.rs src/cli src/dispatch.rs src/error.rs src/log.rs \
		src/runtime/docker.rs src/runtime/process.rs src/runtime/state.rs src/runtime/mod.rs \
		2>/dev/null; then \
		echo "the engine must not reference vendor:: (see above)"; exit 1; \
	fi

test:
	cargo test --all-features
	cargo test --no-default-features

audit:
	cargo deny check

fix:
	cargo fmt --all
	cargo clippy --all-targets --all-features --fix --allow-dirty -- -D warnings

release:
	cargo build --release

modules: # diagnostic, never a gate: cargo-modules visualizes, it enforces nothing
	cargo modules structure --lib
