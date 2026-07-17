use clap::Parser;

use crate::member::Member;

#[derive(Debug, Parser)]
#[command(version, about = "Run an AI contributor on Radicle")]
/// Command-line interface for the Radicle team application.
pub struct Cli {
    /// Team member to run: nico, maya, leon, vera, nora, or theo.
    member: Member,
}

impl Cli {
    /// Parses the command-line arguments.
    #[must_use]
    pub fn parse() -> Self {
        <Self as Parser>::parse()
    }

    /// Returns the selected team member.
    #[must_use]
    pub const fn member(&self) -> Member {
        self.member
    }
}

#[cfg(test)]
mod tests {
    use clap::{Parser as _, error::ErrorKind};

    use super::Cli;
    use crate::member::Member;

    #[test]
    fn parses_every_member() {
        for member in Member::ALL {
            assert!(
                matches!(Cli::try_parse_from(["radicle-team", member.id()]), Ok(cli) if cli.member() == member)
            );
        }
    }

    #[test]
    fn rejects_an_unknown_member() {
        assert!(matches!(
            Cli::try_parse_from(["radicle-team", "unknown"]),
            Err(error) if error.kind() == ErrorKind::ValueValidation
        ));
    }
}
