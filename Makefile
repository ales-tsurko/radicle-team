# The whole workspace, so the tests' Linux sandbox helper is built beside them.
CARGO_PACKAGES := --workspace
MEMBER ?= nico
REPO ?=
COVERAGE_LCOV := target/radicle-team-lcov.info
CRAP_REPORT := target/radicle-team-crap.md
CRAP_EXCLUDES := --exclude '**/build.rs' --exclude '**/benches/**' --exclude '**/examples/**' --exclude '**/tests.rs' --exclude '**/*_tests.rs' --exclude '**/tests/**'
SIMILARITY_ARGS := src --threshold 0.92 --min-lines 12 --min-tokens 80 --fail-on-duplicates

.PHONY: check test-all lint fmt code-health audit

check:
	cargo check $(CARGO_PACKAGES) --all-features --all-targets

test-all:
	cargo nextest run $(CARGO_PACKAGES) --all-features --all-targets

audit:
	# Radicle uses Ed25519 only. See SECURITY.md for the transitive RSA exception.
	cargo audit --deny unsound --ignore RUSTSEC-2023-0071

lint:
	@status=0; \
	cargo clippy $(CARGO_PACKAGES) --all-targets --all-features -- -D warnings || status=$$?; \
	cargo +nightly fmt --check --all || status=$$?; \
	rumdl check . || status=$$?; \
	rumdl fmt --check . || status=$$?; \
	RUSTDOCFLAGS="-D warnings" cargo doc ${CARGO_PACKAGES} --all-features \
		--no-deps --document-private-items || status=$$?; \
	exit $$status

fmt:
	cargo +nightly fmt --all
	rumdl fmt .

code-health:
	similarity-rs $(SIMILARITY_ARGS)
	cargo machete --skip-target-dir
	cargo llvm-cov nextest $(CARGO_PACKAGES) --all-features --all-targets --lcov --output-path $(COVERAGE_LCOV)
	cargo crap --workspace --lcov $(COVERAGE_LCOV) $(CRAP_EXCLUDES) --format markdown --output $(CRAP_REPORT)
	cargo crap --workspace --lcov $(COVERAGE_LCOV) $(CRAP_EXCLUDES) --summary --fail-above
