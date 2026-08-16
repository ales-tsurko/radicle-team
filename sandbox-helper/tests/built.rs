//! Proves the helper is built as a runnable binary, which is what puts it where the tests look.
//!
//! radicle-team's own tests point Zerobox at this binary rather than the one they run in. Cargo
//! builds a package's binaries for its integration tests, so this test existing is what makes the
//! binary exist; `cargo test --all-targets` otherwise builds it with libtest's `main`, which cannot
//! dispatch.

use std::process::Command;

#[test]
fn exits_when_it_was_not_re_entered_as_the_helper() {
    let status = Command::new(env!("CARGO_BIN_EXE_sandbox-helper"))
        .status()
        .expect("the helper binary should be built and runnable");

    assert!(
        status.success(),
        "a helper started under its own name has nothing to dispatch and must simply exit"
    );
}
