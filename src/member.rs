//! Fixed team members and their public profiles.

use std::{fmt, str::FromStr};

/// A fixed member of the team.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Member {
    /// Junior developer.
    Nico,
    /// Senior developer.
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
                name: "Nico",
                role: "Junior Developer",
                description: "A little silly, endlessly curious, and always eager to try \
                              something. Works fast, occasionally breaks things, and learns even \
                              faster.",
                stats: Stats::new(3, 5, 1, 4, 2),
                special_trait: "Beginner's Luck",
                mcp_allow: &["read", "comments", "patches", "reviews"],
            },
            Self::Maya => Profile {
                name: "Maya",
                role: "Senior Developer",
                description: "Calm, capable, and difficult to surprise. Turns vague ideas and \
                              messy code into something that actually ships.",
                stats: Stats::new(5, 3, 5, 4, 4),
                special_trait: "Gets It Done",
                mcp_allow: &["read", "comments", "issues", "patches", "reviews"],
            },
            Self::Leon => Profile {
                name: "Leon",
                role: "Software Architect",
                description: "Sees the whole system while everyone else is looking at one file. \
                              Thoughtful and patient, but deeply suspicious of shortcuts.",
                stats: Stats::new(5, 2, 5, 4, 5),
                special_trait: "Sees Three Moves Ahead",
                mcp_allow: &["read", "comments", "issues", "reviews"],
            },
            Self::Vera => Profile {
                name: "Vera",
                role: "Security Engineer",
                description: "Sharp, skeptical, and armed with very dry humor. Notices the \
                              unlocked door everyone else has already walked through.",
                stats: Stats::new(5, 2, 5, 3, 5),
                special_trait: "Trust Nothing",
                mcp_allow: &["read", "comments", "issues", "reviews"],
            },
            Self::Nora => Profile {
                name: "Nora",
                role: "UX/UI Designer & Frontend Developer",
                description: "Empathetic, expressive, and unafraid to challenge the team. Knows \
                              that technically working is not the same as being usable.",
                stats: Stats::new(4, 4, 4, 5, 3),
                special_trait: "Human Perspective",
                mcp_allow: &["read", "comments", "issues", "patches", "reviews"],
            },
            Self::Theo => Profile {
                name: "Theo",
                role: "Researcher",
                description: "A patient investigator who happily follows strange clues. Sometimes \
                              disappears down a rabbit hole, but usually returns with the missing \
                              answer.",
                stats: Stats::new(5, 1, 5, 4, 5),
                special_trait: "Leave No Stone Unturned",
                mcp_allow: &["read", "comments", "issues", "reviews"],
            },
        }
    }

    pub(crate) fn is_mentioned_in(self, body: &str) -> bool {
        body.split(|character: char| {
            !(character.is_ascii_alphanumeric() || matches!(character, '@' | '_' | '-'))
        })
        .any(|word| {
            word.strip_prefix('@')
                .is_some_and(|name| name.eq_ignore_ascii_case(self.id()))
        })
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

/// A member's fixed character profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    /// Display name.
    pub name: &'static str,
    /// Team role.
    pub role: &'static str,
    /// Short character description.
    pub description: &'static str,
    /// Character statistics.
    pub stats: Stats,
    /// Unique character trait.
    pub special_trait: &'static str,
    /// MCP permission groups used when setup creates this member's binding.
    pub mcp_allow: &'static [&'static str],
}

/// Character statistics rated from 1 to 5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Model capability and reasoning quality.
    pub intelligence: u8,
    /// Response and execution speed.
    pub speed: u8,
    /// Engineering judgment and autonomy.
    pub experience: u8,
    /// Breadth of solution exploration.
    pub creativity: u8,
    /// Deliberation and risk awareness.
    pub caution: u8,
}

impl Stats {
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
    fn provides_every_member_profile() {
        let expected = [
            (Member::Nico, "Nico", "Junior Developer"),
            (Member::Maya, "Maya", "Senior Developer"),
            (Member::Leon, "Leon", "Software Architect"),
            (Member::Vera, "Vera", "Security Engineer"),
            (Member::Nora, "Nora", "UX/UI Designer & Frontend Developer"),
            (Member::Theo, "Theo", "Researcher"),
        ];

        for (member, name, role) in expected {
            let profile = member.profile();
            assert_eq!(profile.name, name);
            assert_eq!(profile.role, role);
        }
    }

    #[test]
    fn provides_setup_mcp_defaults_as_profile_data() {
        assert_eq!(
            Member::Nico.profile().mcp_allow,
            ["read", "comments", "patches", "reviews"]
        );
        assert_eq!(
            Member::Maya.profile().mcp_allow,
            ["read", "comments", "issues", "patches", "reviews"]
        );
        assert!(
            !Member::ALL
                .iter()
                .any(|member| member.profile().mcp_allow.contains(&"merge_patch"))
        );
    }
}
