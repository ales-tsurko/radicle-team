//! Serves as Zerobox's Linux sandbox helper for radicle-team's tests.
//!
//! Confining a command on Linux re-executes a helper before it parses its arguments. libtest owns
//! `main` in a unit-test binary and never dispatches, so the tests point Zerobox at this binary
//! instead of the one they are running in.

fn main() {
    radicle_team::sandbox::dispatch_linux_helper();
}
