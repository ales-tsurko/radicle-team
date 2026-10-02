//! Fixed team members and their public profiles.

use std::{fmt, str::FromStr};

use serde::Deserialize;

// Handle which mentions every member at once, for work with no obvious owner.
const TEAM_MENTION: &str = "team";

/// A fixed member of the team.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Member {
    /// Junior developer.
    Nico,
    /// Senior developer and team lead.
    Maya,
    /// Software architect.
    Leon,
    /// Security engineer.
    Vera,
    /// UX/UI designer and frontend developer.
    Nora,
    /// Researcher.
    Theo,
}

impl Member {
    /// Every available team member.
    pub const ALL: [Self; 6] = [
        Self::Nico,
        Self::Maya,
        Self::Leon,
        Self::Vera,
        Self::Nora,
        Self::Theo,
    ];

    /// Returns the member's stable command-line name.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Nico => "nico",
            Self::Maya => "maya",
            Self::Leon => "leon",
            Self::Vera => "vera",
            Self::Nora => "nora",
            Self::Theo => "theo",
        }
    }

    /// Returns the member's fixed profile.
    #[must_use]
    pub const fn profile(self) -> Profile {
        match self {
            Self::Nico => Profile {
                member: self,
                name: "Nico",
                title: "Junior Developer",
                tuning: ModelTuning::new(3, 2),
                mcp_allow: &[
                    "read",
                    "chat",
                    "comments",
                    "patches",
                    "reactions",
                    "reviews",
                ],
            },
            Self::Maya => Profile {
                member: self,
                name: "Maya",
                title: "Senior Developer & Team Lead",
                tuning: ModelTuning::new(4, 4),
                mcp_allow: &[
                    "read",
                    "chat",
                    "comments",
                    "issues",
                    "patches",
                    "reactions",
                    "reviews",
                ],
            },
            Self::Leon => Profile {
                member: self,
                name: "Leon",
                title: "Software Architect",
                tuning: ModelTuning::new(5, 5),
                mcp_allow: &[
                    "read",
                    "chat",
                    "comments",
                    "issues",
                    "patches",
                    "reactions",
                    "reviews",
                ],
            },
            Self::Vera => Profile {
                member: self,
                name: "Vera",
                title: "Security Engineer",
                tuning: ModelTuning::new(5, 5),
                mcp_allow: &[
                    "read",
                    "chat",
                    "comments",
                    "issues",
                    "patches",
                    "reactions",
                    "reviews",
                ],
            },
            Self::Nora => Profile {
                member: self,
                name: "Nora",
                title: "UX/UI Designer & Frontend Developer",
                tuning: ModelTuning::new(4, 3),
                mcp_allow: &[
                    "read",
                    "chat",
                    "comments",
                    "issues",
                    "patches",
                    "reactions",
                    "reviews",
                ],
            },
            Self::Theo => Profile {
                member: self,
                name: "Theo",
                title: "Researcher",
                tuning: ModelTuning::new(5, 5),
                mcp_allow: &[
                    "read",
                    "chat",
                    "comments",
                    "issues",
                    "patches",
                    "reactions",
                    "reviews",
                ],
            },
        }
    }

    // Returns whether `body` addresses this member by name or through [`TEAM_MENTION`].
    pub(crate) fn is_mentioned_in(self, body: &str) -> bool {
        Self::addresses(body, |name| {
            name.eq_ignore_ascii_case(self.id()) || name.eq_ignore_ascii_case(TEAM_MENTION)
        })
    }

    // Returns whether `body` addresses this member by name alone.
    pub(crate) fn is_named_in(self, body: &str) -> bool {
        Self::addresses(body, |name| name.eq_ignore_ascii_case(self.id()))
    }

    fn addresses(body: &str, is_match: impl Fn(&str) -> bool) -> bool {
        body.split(|character: char| {
            !(character.is_ascii_alphanumeric() || matches!(character, '@' | '_' | '-'))
        })
        .any(|word| word.strip_prefix('@').is_some_and(&is_match))
    }
}

impl fmt::Display for Member {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.id())
    }
}

impl FromStr for Member {
    type Err = UnknownMember;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "nico" => Ok(Self::Nico),
            "maya" => Ok(Self::Maya),
            "leon" => Ok(Self::Leon),
            "vera" => Ok(Self::Vera),
            "nora" => Ok(Self::Nora),
            "theo" => Ok(Self::Theo),
            _ => Err(UnknownMember(value.to_owned())),
        }
    }
}

/// Provider-neutral configuration for one fixed team member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    /// Stable team member identity.
    pub member: Member,
    /// Display name.
    pub name: &'static str,
    /// Team title.
    pub title: &'static str,
    /// Model behavior preferences interpreted by the agent adapter.
    pub tuning: ModelTuning,
    /// MCP permission groups used when setup creates this member's binding.
    pub mcp_allow: &'static [&'static str],
}

/// One of the scores a provider file sets a control for.
///
/// The names are the ones the roster shows, so a reader comparing `docs/members.md` against a
/// provider configuration is reading one vocabulary rather than two. The roster carries more
/// scores than these two; the others come from the member's documents rather than from a model
/// control, so no provider file has a section for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
pub enum Stat {
    /// How sharp the member is.
    #[serde(rename = "INT")]
    Intelligence,
    /// How closely the member sticks to the obvious answer.
    #[serde(rename = "ACC")]
    Accuracy,
}

impl Stat {
    /// Every score, in the order the roster lists them.
    pub const ALL: [Self; 2] = [Self::Intelligence, Self::Accuracy];

    /// Returns the three-letter name this score is written as.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Intelligence => "INT",
            Self::Accuracy => "ACC",
        }
    }
}

impl fmt::Display for Stat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.id())
    }
}

/// The part of a member's character sheet a provider file can set, rated 1 to 5.
///
/// A character sheet: these are read to decide who a task suits, not set by whoever runs one.
///
/// Two, because two is what a model control can carry: which model runs, and how far it ranges
/// before answering. The rest of the sheet is no less real -- how much a member says, what it cares
/// about, when it pushes back -- but it comes from the documents under `members/`, which give the
/// model rules and examples rather than a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelTuning {
    /// How sharp the member is.
    ///
    /// Picks the model, so it carries speed with it: the sharper models are the slower ones.
    pub intelligence: u8,
    /// How closely the member sticks to the obvious answer.
    ///
    /// One score for two controls which never appear together: a model which reasons takes a
    /// reasoning effort and rejects a temperature, and a model which does not is the other way
    /// round. High means deliberate and on-track either way.
    pub accuracy: u8,
}

impl ModelTuning {
    /// Highest a score goes. Every score runs from 1 to this.
    pub const HIGHEST: u8 = 5;

    const fn new(intelligence: u8, accuracy: u8) -> Self {
        Self {
            intelligence,
            accuracy,
        }
    }

    /// Returns this member's score for `stat`.
    #[must_use]
    pub const fn score(self, stat: Stat) -> u8 {
        match stat {
            Stat::Intelligence => self.intelligence,
            Stat::Accuracy => self.accuracy,
        }
    }
}

/// Error returned when a team member name is unknown.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown member '{0}'; expected nico, maya, leon, vera, nora, or theo")]
pub struct UnknownMember(String);

#[cfg(test)]
mod tests {
    use crate::member::{Member, Stat, UnknownMember};

    #[test]
    fn parses_every_member_id() {
        for member in Member::ALL {
            assert_eq!(member.id().parse(), Ok(member));
        }
    }

    #[test]
    fn rejects_an_unknown_member() {
        assert_eq!(
            "unknown".parse::<Member>(),
            Err(UnknownMember("unknown".to_owned()))
        );
    }

    #[test]
    fn detects_member_mentions_without_partial_matches() {
        assert!(Member::Nico.is_mentioned_in("Could you check this, @Nico?"));
        assert!(Member::Nico.is_mentioned_in("(@nico) please investigate"));
        assert!(!Member::Nico.is_mentioned_in("Ask @nicole"));
        assert!(!Member::Nico.is_mentioned_in("mail@example@nico"));
        assert!(!Member::Nico.is_mentioned_in("nico"));
    }

    #[test]
    fn detects_team_mentions_for_every_member() {
        for member in Member::ALL {
            assert!(member.is_mentioned_in("@team what do you think?"));
            assert!(member.is_mentioned_in("(@Team) please weigh in"));
            assert!(!member.is_mentioned_in("Ask @teams"));
            assert!(!member.is_mentioned_in("team"));
        }
    }

    #[test]
    fn provides_every_member_profile() {
        let expected = [
            (Member::Nico, "Nico", "Junior Developer"),
            (Member::Maya, "Maya", "Senior Developer & Team Lead"),
            (Member::Leon, "Leon", "Software Architect"),
            (Member::Vera, "Vera", "Security Engineer"),
            (Member::Nora, "Nora", "UX/UI Designer & Frontend Developer"),
            (Member::Theo, "Theo", "Researcher"),
        ];

        for (member, name, title) in expected {
            let profile = member.profile();
            assert_eq!(profile.member, member);
            assert_eq!(profile.name, name);
            assert_eq!(profile.title, title);
            for stat in Stat::ALL {
                assert!((1..=5).contains(&profile.tuning.score(stat)));
            }
        }
    }

    #[test]
    fn provides_setup_mcp_defaults_as_profile_data() {
        assert_eq!(
            Member::Nico.profile().mcp_allow,
            [
                "read",
                "chat",
                "comments",
                "patches",
                "reactions",
                "reviews"
            ]
        );
        assert_eq!(
            Member::Maya.profile().mcp_allow,
            [
                "read",
                "chat",
                "comments",
                "issues",
                "patches",
                "reactions",
                "reviews"
            ]
        );
        assert!(
            !Member::ALL
                .iter()
                .any(|member| member.profile().mcp_allow.contains(&"merge_patch"))
        );
    }
}
