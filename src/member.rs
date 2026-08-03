//! Fixed team members and their public profiles.

use std::{fmt, str::FromStr};

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
                role: "Junior Developer",
                tuning: ModelTuning::new(3, 5, 1, 4, 2),
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
                role: "Senior Developer & Team Lead",
                tuning: ModelTuning::new(5, 3, 5, 4, 4),
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
                role: "Software Architect",
                tuning: ModelTuning::new(5, 2, 5, 4, 5),
                mcp_allow: &["read", "chat", "comments", "issues", "reactions", "reviews"],
            },
            Self::Vera => Profile {
                member: self,
                name: "Vera",
                role: "Security Engineer",
                tuning: ModelTuning::new(5, 2, 5, 3, 5),
                mcp_allow: &["read", "chat", "comments", "issues", "reactions", "reviews"],
            },
            Self::Nora => Profile {
                member: self,
                name: "Nora",
                role: "UX/UI Designer & Frontend Developer",
                tuning: ModelTuning::new(4, 4, 4, 5, 3),
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
                role: "Researcher",
                tuning: ModelTuning::new(5, 1, 5, 4, 5),
                mcp_allow: &["read", "chat", "comments", "issues", "reactions", "reviews"],
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
    /// Team role.
    pub role: &'static str,
    /// Model behavior preferences interpreted by the agent adapter.
    pub tuning: ModelTuning,
    /// MCP permission groups used when setup creates this member's binding.
    pub mcp_allow: &'static [&'static str],
}

/// Provider-neutral model behavior preferences rated from 1 to 5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelTuning {
    /// Preferred model capability and reasoning quality.
    pub intelligence: u8,
    /// Preference for response and execution speed.
    pub speed: u8,
    /// Preferred engineering autonomy and verification depth.
    pub experience: u8,
    /// Preferred breadth of solution exploration.
    pub creativity: u8,
    /// Preferred deliberation and risk awareness.
    pub caution: u8,
}

impl ModelTuning {
    const fn new(intelligence: u8, speed: u8, experience: u8, creativity: u8, caution: u8) -> Self {
        Self {
            intelligence,
            speed,
            experience,
            creativity,
            caution,
        }
    }
}

/// Error returned when a team member name is unknown.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown member '{0}'; expected nico, maya, leon, vera, nora, or theo")]
pub struct UnknownMember(String);

#[cfg(test)]
mod tests {
    use crate::member::{Member, UnknownMember};

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

        for (member, name, role) in expected {
            let profile = member.profile();
            assert_eq!(profile.member, member);
            assert_eq!(profile.name, name);
            assert_eq!(profile.role, role);
            for value in [
                profile.tuning.intelligence,
                profile.tuning.speed,
                profile.tuning.experience,
                profile.tuning.creativity,
                profile.tuning.caution,
            ] {
                assert!((1..=5).contains(&value));
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
