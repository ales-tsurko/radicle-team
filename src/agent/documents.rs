//! The documents an adapter gives its agent as standing context.
//!
//! Standing context: what the team shares, the world the members come from, what one role of a
//! member does, and the three which make this member that member rather than a competent stranger.
//! They are compiled in, because the binary ships without the repository they were written in.

use std::{fs, io, path::PathBuf};

use crate::{agent::Role, home::MemberHome, member::Member};

const TEAM: Document = Document::new("TEAM.md", include_str!("../../members/TEAM.md"));
const UNIVERSE: Document = Document::new("UNIVERSE.md", include_str!("../../members/UNIVERSE.md"));
const TALKER: Document = Document::new("TALKER.md", include_str!("../../members/TALKER.md"));
const CHATTER: Document = Document::new("CHATTER.md", include_str!("../../members/CHATTER.md"));
const CODER: Document = Document::new("CODER.md", include_str!("../../members/CODER.md"));

/// One document, under the name it is written out as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Document {
    name: &'static str,
    body: &'static str,
}

impl Document {
    const fn new(name: &'static str, body: &'static str) -> Self {
        Self { name, body }
    }

    /// Returns the file name this document is written out as.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// Returns the document itself.
    #[must_use]
    pub const fn body(&self) -> &'static str {
        self.body
    }
}

/// Returns everything `role` tells `member`'s agent, in the order it should be read.
///
/// Shared rules first, then the setting, then what this role does, then who is doing it. A
/// member document contradicting a shared rule is a mistake in the member document, and the order
/// says so.
#[must_use]
pub fn for_member(member: Member, role: Role) -> Vec<Document> {
    let [character, expertise, examples] = personal(member);
    let mut documents = vec![TEAM, UNIVERSE];
    if role == Role::Chatter {
        documents.push(TALKER);
    }
    documents.extend([role.into(), character, expertise, examples]);

    documents
}

impl From<Role> for Document {
    fn from(role: Role) -> Self {
        match role {
            Role::Talker => TALKER,
            Role::Chatter => CHATTER,
            Role::Coder => CODER,
        }
    }
}

/// Writes the documents for `member` and `role` into that role's instructions directory.
///
/// The directory is emptied first, so a document removed from a later build does not linger and
/// keep instructing the agent. Which directory that is comes from [`MemberHome`], which owns every
/// other path in a member's home -- emptying a directory named by the caller is how a mistyped
/// argument becomes a deleted harness.
///
/// # Errors
///
/// Returns an error when the directory or any document cannot be written.
pub fn install(home: &MemberHome, member: Member, role: Role) -> Result<Vec<PathBuf>, io::Error> {
    let directory = home.instructions(role);
    // The files are cleared, not the directory, so the mode `MemberHome::create` established stays
    // on it. Recreating the directory would fall back to the process umask instead, leaving one
    // directory in a private home world-readable.
    for entry in fs::read_dir(&directory)? {
        fs::remove_file(entry?.path())?;
    }
    let mut written = Vec::new();
    for document in for_member(member, role) {
        let path = directory.join(document.name());
        fs::write(&path, document.body())?;
        written.push(path);
    }

    Ok(written)
}

// Returns the three documents which belong to one member, as character, expertise, examples.
fn personal(member: Member) -> [Document; 3] {
    macro_rules! member_documents {
        ($directory:literal) => {
            [
                Document::new(
                    "CHARACTER.md",
                    include_str!(concat!("../../members/", $directory, "/CHARACTER.md")),
                ),
                Document::new(
                    "EXPERTISE.md",
                    include_str!(concat!("../../members/", $directory, "/EXPERTISE.md")),
                ),
                Document::new(
                    "EXAMPLES.md",
                    include_str!(concat!("../../members/", $directory, "/EXAMPLES.md")),
                ),
            ]
        };
    }

    match member {
        Member::Nico => member_documents!("nico"),
        Member::Maya => member_documents!("maya"),
        Member::Leon => member_documents!("leon"),
        Member::Vera => member_documents!("vera"),
        Member::Nora => member_documents!("nora"),
        Member::Theo => member_documents!("theo"),
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        agent::{Role, documents, documents::Document},
        member::Member,
    };

    #[test]
    fn gives_every_member_and_role_a_complete_set() {
        for member in Member::ALL {
            for role in Role::ALL {
                for document in documents::for_member(member, role) {
                    assert!(
                        !document.body().trim().is_empty(),
                        "{member} as {role} was given an empty {}",
                        document.name()
                    );
                }
            }
        }
    }

    // A member is its character documents. Handing one member's to another would put a different
    // person behind the same name and key, which nothing downstream could detect.
    #[test]
    fn never_gives_one_member_another_members_character() {
        let mine = documents::for_member(Member::Nico, Role::Talker);
        let theirs = documents::for_member(Member::Maya, Role::Talker);

        for (mine, theirs) in mine.into_iter().zip(theirs) {
            assert_eq!(mine.name(), theirs.name());
            if matches!(mine.name(), "CHARACTER.md" | "EXPERTISE.md" | "EXAMPLES.md") {
                assert_ne!(
                    mine.body(),
                    theirs.body(),
                    "{} must differ between members",
                    mine.name()
                );
            }
        }
    }

    // Talking and building roles are one person, so only their role document differs.
    #[test]
    fn changes_only_the_role_document_between_one_members_roles() {
        let talking = documents::for_member(Member::Nico, Role::Talker);
        let building = documents::for_member(Member::Nico, Role::Coder);
        let differing = talking
            .into_iter()
            .zip(building)
            .filter(|(talking, building)| talking.body() != building.body())
            .count();

        assert_eq!(
            differing, 1,
            "only the role document may differ, or the roles stop being one person"
        );
        assert_eq!(
            Document::from(Role::Talker).name(),
            "TALKER.md",
            "and it must be the role document which differs"
        );
    }

    #[test]
    fn installs_the_talking_rules_chatter_refers_to() {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let home = crate::home::MemberHome::at(root.path().join("member"));
        home.create().expect("member directory should be created");
        let written = documents::install(&home, Member::Nico, Role::Chatter)
            .expect("chat instructions should be installed");

        assert!(
            written
                .windows(2)
                .any(|pair| { pair[0].ends_with("TALKER.md") && pair[1].ends_with("CHATTER.md") }),
            "the talking rules must be loaded before the chat-specific rules"
        );
        assert_eq!(
            std::fs::read_to_string(home.instructions(Role::Chatter).join("TALKER.md"))
                .expect("the rules CHATTER.md refers to must be available"),
            Document::from(Role::Talker).body()
        );
    }

    #[test]
    fn writes_a_fresh_set_over_a_stale_one() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("agent-documents");
        let _removed = std::fs::remove_dir_all(&root);
        let home = crate::home::MemberHome::at(&root);
        home.create().expect("member directory should be created");
        let directory = home.instructions(Role::Coder);
        let stale = directory.join("REMOVED.md");
        std::fs::write(&stale, "an instruction from an older build")
            .expect("stale document should be written");

        let written = documents::install(&home, Member::Nico, Role::Coder)
            .expect("documents should be installed");

        assert_eq!(written.len(), 6);
        for path in &written {
            assert!(
                path.is_file(),
                "{} should have been written",
                path.display()
            );
        }
        assert!(
            !stale.exists(),
            "a document from an older build must not go on instructing the agent"
        );
        // Clearing the files rather than the directory is what keeps this. Recreating the
        // directory would take the process umask instead.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            let mode = std::fs::metadata(&directory)
                .expect("instructions directory should be there")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(
                mode, 0o700,
                "installing must not loosen the directory holding a member's instructions"
            );
        }
    }
}
