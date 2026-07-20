CARGO_PACKAGES := -p radicle-team
MEMBER ?= nico
REPO ?=
MIRI_FLAGS := -Zmiri-disable-isolation -Zmiri-tree-borrows
MIRI_PACKAGES := radicle-team
MIRI_SYSROOT := $(CURDIR)/target/miri-sysroot
COVERAGE_LCOV := target/radicle-team-lcov.info
CRAP_REPORT := target/radicle-team-crap.md
CRAP_EXCLUDES := --exclude '**/build.rs' --exclude '**/benches/**' --exclude '**/examples/**' --exclude '**/tests.rs' --exclude '**/*_tests.rs' --exclude '**/tests/**'
SIMILARITY_ARGS := src --threshold 0.92 --min-lines 12 --min-tokens 80 --fail-on-duplicates

.PHONY: check test-all lint fmt code-health

check:
	cargo check $(CARGO_PACKAGES) --all-features --all-targets

test-all:
	cargo nextest run $(CARGO_PACKAGES) --all-features --all-targets

lint:
	@status=0; \
	cargo clippy $(CARGO_PACKAGES) --all-targets --all-features -- -D warnings || status=$$?; \
	cargo +nightly fmt --check $(CARGO_PACKAGES) || status=$$?; \
	exit $$status

fmt:
	cargo +nightly fmt $(CARGO_PACKAGES)

code-health:
	similarity-rs $(SIMILARITY_ARGS)
	cargo machete --skip-target-dir
	cargo llvm-cov nextest $(CARGO_PACKAGES) --all-features --all-targets --lcov --output-path $(COVERAGE_LCOV)
	cargo crap --workspace --lcov $(COVERAGE_LCOV) $(CRAP_EXCLUDES) --format markdown --output $(CRAP_REPORT)
	cargo crap --workspace --lcov $(COVERAGE_LCOV) $(CRAP_EXCLUDES) --summary --fail-above
	MIRI_SYSROOT="$(MIRI_SYSROOT)" cargo +nightly miri setup
	@set -e; \
	for package in $(MIRI_PACKAGES); do \
		MIRI_SYSROOT="$(MIRI_SYSROOT)" MIRIFLAGS="$(MIRI_FLAGS)" cargo +nightly miri test -p "$$package" --all-features; \
	done
