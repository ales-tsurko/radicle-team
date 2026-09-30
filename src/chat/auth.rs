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
    pub(super) const fn new(did: Did) -> Self {
        Self { did }
    }

    pub(crate) const fn did(self) -> Did {
        self.did
    }

    #[cfg(test)]
    pub(crate) const fn for_test(did: Did) -> Self {
        Self { did }
    }
}
