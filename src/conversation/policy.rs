//! Authorization and exchange quotas shared by repository and chat conversations.

use std::collections::BTreeMap;

use radicle::identity::Did;

use crate::{
    manifest::{Limits, TriggerAccess},
    member::Member,
};

/// Repository trigger policy resolved against the current delegate set.
#[derive(Debug, Clone)]
pub(crate) struct Triggers {
    access: TriggerAccess,
    delegates: Vec<Did>,
    members: Vec<Did>,
}

impl Triggers {
    /// Creates a trigger classifier from validated repository policy.
    pub(crate) const fn new(access: TriggerAccess, delegates: Vec<Did>, members: Vec<Did>) -> Self {
        Self {
            access,
            delegates,
            members,
        }
    }

    /// Returns who `author` is to the member handling the conversation.
    pub(crate) fn author(&self, author: Did, member: Did) -> Author {
        if author == member {
            Author::Member
        } else if self.members.contains(&author) {
            Author::Peer
        } else if self.access.allows(author, &self.delegates) {
            Author::Person
        } else {
            Author::Stranger
        }
    }

    /// Replaces repository delegates after the repository identity changes.
    pub(crate) fn update_delegates(&mut self, delegates: Vec<Did>) {
        self.delegates = delegates;
    }
}

/// Who an author is to the member handling a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Author {
    /// The member itself.
    Member,
    /// Another member of the same team.
    Peer,
    /// Somebody outside the team who may put its members to work.
    Person,
    /// Somebody the repository does not let put members to work.
    Stranger,
}

impl Author {
    /// Checks the addressing form allowed for this kind of author.
    pub(crate) fn addresses(self, member: Member, body: &str) -> bool {
        if self == Self::Peer {
            member.is_named_in(body)
        } else {
            member.is_mentioned_in(body)
        }
    }
}

/// How far one conversation has run since a person last joined it.
#[derive(Debug, Default)]
pub(crate) struct Turn {
    charged: Option<Did>,
    exchanges: BTreeMap<Did, usize>,
    limits: Limits,
}

impl Turn {
    /// Starts a conversation under the repository's limits.
    pub(crate) fn new(limits: Limits) -> Self {
        Self {
            limits,
            ..Self::default()
        }
    }

    /// Ends the turn in progress when the next message has another author.
    pub(crate) fn begin(&mut self, author: Did) {
        if self.charged != Some(author) {
            self.charged = None;
        }
    }

    /// Charges an addressed author for waking the member.
    pub(crate) fn admit(&mut self, author: Did, origin: Author) -> Result<(), AdmissionError> {
        match origin {
            Author::Stranger => return Err(AdmissionError::Unauthorized),
            Author::Member | Author::Person => {
                self.exchanges.clear();
                return Ok(());
            }
            Author::Peer => {}
        }
        let spent = self.exchanges.entry(author).or_default();
        if self.charged != Some(author) {
            self.charged = Some(author);
            *spent += 1;
        }
        if *spent <= self.limits.exchanges() {
            Ok(())
        } else {
            Err(AdmissionError::Limit)
        }
    }
}

/// Reason an addressed author cannot continue a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum AdmissionError {
    /// Repository trigger policy does not authorize the author.
    #[error("its author may not trigger artificial contributors")]
    Unauthorized,
    /// The conversation has exhausted its autonomous turn allowance.
    #[error("this conversation has run its allowed number of turns since a person last joined it")]
    Limit,
}
