//! Evidence that an IRC account passed the repository's attestation chain.

use radicle::identity::Did;

/// Sender identity produced only by chat authorization.
///
/// Its fields and constructor are private so only validated manifest authentication can create
/// authenticated chat context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AuthenticatedSender {
    did: Did,
}

impl AuthenticatedSender {
    #[cfg_attr(
        any(not(test), miri),
        expect(
            dead_code,
            reason = "construction is restricted to successful manifest authentication"
        )
    )]
    pub(super) const fn new(did: Did) -> Self {
        Self { did }
    }

    pub(crate) const fn did(self) -> Did {
        self.did
    }

    #[cfg(all(test, not(miri)))]
    pub(crate) const fn for_test(did: Did) -> Self {
        Self { did }
    }
}
