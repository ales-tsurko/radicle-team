//! Evidence that an IRC account passed the repository's attestation chain.

#![cfg_attr(
    any(not(test), miri),
    expect(
        dead_code,
        reason = "manifest attestation validation will produce this evidence in PR 4"
    )
)]

use radicle::identity::Did;

/// Sender identity produced only by chat authorization.
///
/// Its fields are private to this module. The manifest authorization work will add the production
/// constructor; until then no production path can create authenticated chat context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AuthenticatedSender {
    did: Did,
}

impl AuthenticatedSender {
    pub(crate) const fn did(self) -> Did {
        self.did
    }

    #[cfg(all(test, not(miri)))]
    pub(crate) const fn for_test(did: Did) -> Self {
        Self { did }
    }
}
