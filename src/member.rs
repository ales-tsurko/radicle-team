use std::{fmt, str::FromStr};

/// A fixed member of the team.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Member {
    /// Junior developer.
    Nico,
    /// Senior developer.
    Maya,
    /// Software architect.
    Leon,
    /// Security engineer.
    Vera,
    /// UX/UI designer.
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
            },
            Self::Maya => Profile {
                name: "Maya",
                role: "Senior Developer",
                description: "Calm, capable, and difficult to surprise. Turns vague ideas and \
                              messy code into something that actually ships.",
                stats: Stats::new(5, 4, 5, 3, 4),
                special_trait: "Gets It Done",
            },
            Self::Leon => Profile {
                name: "Leon",
                role: "Software Architect",
                description: "Sees the whole system while everyone else is looking at one file. \
                              Thoughtful and patient, but deeply suspicious of shortcuts.",
                stats: Stats::new(5, 2, 5, 4, 5),
                special_trait: "Sees Three Moves Ahead",
            },
            Self::Vera => Profile {
                name: "Vera",
                role: "Security Engineer",
                description: "Sharp, skeptical, and armed with very dry humor. Notices the \
                              unlocked door everyone else has already walked through.",
                stats: Stats::new(5, 3, 4, 3, 5),
                special_trait: "Trust Nothing",
            },
            Self::Nora => Profile {
                name: "Nora",
                role: "UX/UI Designer",
                description: "Empathetic, expressive, and unafraid to challenge the team. Knows \
                              that technically working is not the same as being usable.",
                stats: Stats::new(4, 4, 4, 5, 3),
                special_trait: "Human Perspective",
            },
            Self::Theo => Profile {
                name: "Theo",
                role: "Researcher",
                description: "A patient investigator who happily follows strange clues. Sometimes \
                              disappears down a rabbit hole, but usually returns with the missing \
                              answer.",
                stats: Stats::new(5, 3, 4, 5, 4),
                special_trait: "Leave No Stone Unturned",
            },
        }
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

/// Error returned when a team member name is unknown.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown member '{0}'; expected nico, maya, leon, vera, nora, or theo")]
pub struct UnknownMember(String);

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
}

/// Character statistics rated from 1 to 5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Intelligence.
    pub intelligence: u8,
    /// Speed.
    pub speed: u8,
    /// Experience.
    pub experience: u8,
    /// Creativity.
    pub creativity: u8,
    /// Caution.
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn provides_every_member_profile() {
        let expected = [
            (Member::Nico, "Nico", "Junior Developer"),
            (Member::Maya, "Maya", "Senior Developer"),
            (Member::Leon, "Leon", "Software Architect"),
            (Member::Vera, "Vera", "Security Engineer"),
            (Member::Nora, "Nora", "UX/UI Designer"),
            (Member::Theo, "Theo", "Researcher"),
        ];

        for (member, name, role) in expected {
            let profile = member.profile();
            assert_eq!(profile.name, name);
            assert_eq!(profile.role, role);
        }
    }
}
