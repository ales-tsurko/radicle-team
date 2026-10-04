//! Watcher classification tests.

use std::{collections::BTreeSet, str::FromStr as _};

use radicle::{
    cob::{
        EntryId, Manifest, ObjectId, Op, Reaction, Timestamp, Title, TypeName, Version,
        issue::{Action, State as IssueState, TYPENAME},
        patch::{
            Action as PatchAction, Lifecycle, MergeTarget, ReviewEdit, ReviewId, RevisionId,
            TYPENAME as PATCH_TYPENAME, Verdict,
        },
    },
    crypto::PublicKey,
    identity::{Did, RepoId},
};

use crate::{
    conversation::Triggers,
    manifest::{Limits, TriggerAccess},
    member::Member,
    watcher::{
        Change, Comment, Subject,
        activity::{Activity, Delivery, Kind},
        classification::{ActionEnvelope, Context, NormalizedAction},
    },
};

const MEMBER: &str = "z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7";
const AUTHOR: &str = "z6MkvUJtYD9dHDJfpevWRT98mzDDpdAtmUjwyDSkyqksUr7C";
const DELEGATE: &str = "z6MkeXCES4onVW4up9Qgz1KRnZsKmGufcaZxF6Zpv2w5QwUK";
const REPOSITORY: &str = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc";
const ISSUE: &str = "1111111111111111111111111111111111111111";
const OPERATION: &str = "2222222222222222222222222222222222222222";
const COMMENT: &str = "3333333333333333333333333333333333333333";
const CHANGE: &str = "4444444444444444444444444444444444444444";
const REPLY: &str = "5555555555555555555555555555555555555555";

struct Fixture;

impl Fixture {
    // Returns how many turns a conversation gets when the manifest says nothing about it.
    fn exchanges() -> usize {
        Limits::default().exchanges()
    }

    // Pairs a trigger rule with the members whose turns count against each other.
    fn triggers_for(access: TriggerAccess, delegates: Vec<Did>, members: Vec<Did>) -> Triggers {
        Triggers::new(access, delegates, members)
    }

    // Pairs an open repository with the members whose turns count against each other.
    fn triggers(members: Vec<Did>) -> Triggers {
        Self::triggers_for(TriggerAccess::Anyone, Vec::new(), members)
    }

    fn revision(description: &str) -> PatchAction {
        PatchAction::Revision {
            description: description.to_owned(),
            base: *ObjectId::from_str(ISSUE).expect("base should be valid"),
            oid: *ObjectId::from_str(COMMENT).expect("head should be valid"),
            resolves: BTreeSet::new(),
        }
    }

    fn comment(body: &str) -> Action {
        Action::Comment {
            body: body.to_owned(),
            reply_to: None,
            embeds: Vec::new(),
        }
    }

    fn operation<A>(id: &str, action: A, author: PublicKey, type_name: TypeName) -> Op<A> {
        Op::new(
            EntryId::from_str(id).expect("operation should be valid"),
            (action, Vec::new()),
            author,
            Timestamp::from_secs(0),
            None,
            Manifest::new(type_name, Version::default()),
        )
    }

    fn issue() -> Subject {
        Subject::Issue(ObjectId::from_str(ISSUE).expect("issue should be valid"))
    }

    fn patch() -> Subject {
        Subject::Patch(ObjectId::from_str(ISSUE).expect("patch should be valid"))
    }

    fn classify<A>(
        id: &str,
        action: A,
        author: PublicKey,
        type_name: TypeName,
        subject: Subject,
        context: &mut Context,
        triggers: &Triggers,
    ) -> Vec<Activity>
    where
        for<'a> NormalizedAction<'a>: From<ActionEnvelope<'a, A>>,
    {
        context.classify(
            &Self::operation(id, action, author, type_name),
            Member::Nico,
            PublicKey::from_str(MEMBER).expect("member key should be valid"),
            RepoId::from_str(REPOSITORY).expect("repository should be valid"),
            subject,
            triggers,
        )
    }

    fn classify_with(
        action: Action,
        author: PublicKey,
        context: &mut Context,
        triggers: &Triggers,
    ) -> Vec<Activity> {
        Self::classify(
            OPERATION,
            action,
            author,
            TYPENAME.clone(),
            Self::issue(),
            context,
            triggers,
        )
    }

    fn classify_patch_with(
        id: &str,
        action: PatchAction,
        author: PublicKey,
        context: &mut Context,
        triggers: &Triggers,
    ) -> Vec<Activity> {
        Self::classify(
            id,
            action,
            author,
            PATCH_TYPENAME.clone(),
            Self::patch(),
            context,
            triggers,
        )
    }

    fn classify_issue(action: Action, author: PublicKey, context: &mut Context) -> Vec<Activity> {
        Self::classify_with(action, author, context, &Self::triggers(Vec::new()))
    }

    fn classify_patch(
        id: &str,
        action: PatchAction,
        author: PublicKey,
        context: &mut Context,
    ) -> Vec<Activity> {
        Self::classify_patch_with(id, action, author, context, &Self::triggers(Vec::new()))
    }
}

#[test]
fn suppresses_activities_from_authors_that_may_not_trigger() {
    let member = PublicKey::from_str(MEMBER).expect("member key should be valid");
    let author = PublicKey::from_str(AUTHOR).expect("author key should be valid");
    let delegate = PublicKey::from_str(DELEGATE).expect("delegate key should be valid");
    let assignment = Action::Assign {
        assignees: BTreeSet::from([Did::from(member)]),
    };
    let delegates = vec![Did::from(delegate)];

    let permitted = Fixture::classify_with(
        assignment.clone(),
        author,
        &mut Context::default(),
        &Fixture::triggers_for(TriggerAccess::Anyone, delegates.clone(), Vec::new()),
    );
    assert!(
        !permitted.is_empty(),
        "'anyone' must let a non-delegate trigger work; without this control the suppression \
         below could come from the action rather than the policy"
    );

    let mut context = Context::default();
    let suppressed = Fixture::classify_with(
        assignment.clone(),
        author,
        &mut context,
        &Fixture::triggers_for(TriggerAccess::Delegates, delegates.clone(), Vec::new()),
    );
    assert!(
        suppressed.is_empty(),
        "'delegates' must suppress a non-delegate's assignment"
    );
    assert!(
        context.assigned(),
        "the suppressed assignment must still update state, so that later delegate activity on \
         this subject classifies against the repository's real history"
    );

    assert!(
        !Fixture::classify_with(
            assignment,
            delegate,
            &mut Context::default(),
            &Fixture::triggers_for(TriggerAccess::Delegates, delegates, Vec::new()),
        )
        .is_empty(),
        "'delegates' must let a delegate trigger work"
    );
}

#[test]
fn listed_trigger_dids_keep_delegate_access() {
    let member = PublicKey::from_str(MEMBER).expect("member key should be valid");
    let author = PublicKey::from_str(AUTHOR).expect("author key should be valid");
    let delegate = PublicKey::from_str(DELEGATE).expect("delegate key should be valid");
    let assignment = Action::Assign {
        assignees: BTreeSet::from([Did::from(member)]),
    };
    let triggers = Fixture::triggers_for(
        TriggerAccess::Dids(BTreeSet::from([Did::from(author)])),
        vec![Did::from(delegate)],
        Vec::new(),
    );

    assert!(
        !Fixture::classify_with(
            assignment.clone(),
            author,
            &mut Context::default(),
            &triggers
        )
        .is_empty(),
        "a listed DID must trigger work"
    );
    assert!(
        !Fixture::classify_with(assignment, delegate, &mut Context::default(), &triggers)
            .is_empty(),
        "delegates keep access when explicit DIDs are listed, because they control the manifest"
    );
}

#[test]
fn classifies_assignment_changes() {
    let member = PublicKey::from_str(MEMBER).expect("member key should be valid");
    let author = PublicKey::from_str(AUTHOR).expect("author key should be valid");
    let mut context = Context::default();
    let assignment = Action::Assign {
        assignees: BTreeSet::from([Did::from(member)]),
    };

    assert!(matches!(
        Fixture::classify_issue(assignment.clone(), author, &mut context).as_slice(),
        [Activity {
            kind: Kind::Assigned,
            ..
        }]
    ));
    assert!(context.assigned());
    assert!(Fixture::classify_issue(assignment, author, &mut context).is_empty());
    assert!(matches!(
        Fixture::classify_issue(
            Action::Assign {
                assignees: BTreeSet::new()
            },
            author,
            &mut context
        )
        .as_slice(),
        [Activity {
            kind: Kind::Delivered(Delivery::Changed(Change::Unassigned)),
            ..
        }]
    ));
    assert!(!context.assigned());
}

#[test]
fn classifies_external_mentions_only() {
    let member = PublicKey::from_str(MEMBER).expect("member key should be valid");
    let author = PublicKey::from_str(AUTHOR).expect("author key should be valid");
    let mut context = Context::default();
    let comment = Action::Comment {
        body: "Hey @nico".to_owned(),
        reply_to: None,
        embeds: Vec::new(),
    };

    assert!(matches!(
        Fixture::classify_issue(comment.clone(), author, &mut context).as_slice(),
        [Activity {
            kind: Kind::Delivered(Delivery::Mentioned(_)),
            ..
        }]
    ));
    assert!(Fixture::classify_issue(comment, member, &mut context).is_empty());
    assert!(matches!(
        Fixture::classify_issue(
            Action::CommentEdit {
                id: EntryId::from_str(ISSUE).expect("comment should be valid"),
                body: "@Nico, updated".to_owned(),
                embeds: Vec::new(),
            },
            author,
            &mut context,
        )
        .as_slice(),
        [Activity {
            kind: Kind::Delivered(Delivery::Mentioned(_)),
            ..
        }]
    ));
}

#[test]
fn classifies_issue_replies_and_owned_discussion() {
    let member = PublicKey::from_str(MEMBER).expect("member key should be valid");
    let author = PublicKey::from_str(AUTHOR).expect("author key should be valid");
    let mut context = Context::default();
    let member_comment = Action::Comment {
        body: "I will investigate".to_owned(),
        reply_to: None,
        embeds: Vec::new(),
    };

    assert!(Fixture::classify_issue(member_comment, member, &mut context).is_empty());
    assert!(matches!(
        Fixture::classify(
            COMMENT,
            Action::Comment {
                body: "Here are the details".to_owned(),
                reply_to: Some(EntryId::from_str(OPERATION).expect("comment should be valid")),
                embeds: Vec::new(),
            },
            author,
            TYPENAME.clone(),
            Fixture::issue(),
            &mut context,
            &Fixture::triggers(Vec::new()),
        )
        .as_slice(),
        [Activity {
            kind: Kind::Delivered(Delivery::Replied { .. }),
            ..
        }]
    ));
}

#[test]
fn classifies_patch_assignment_and_review() {
    let member = PublicKey::from_str(MEMBER).expect("member key should be valid");
    let author = PublicKey::from_str(AUTHOR).expect("author key should be valid");
    let mut assignment_context = Context::default();

    assert!(matches!(
        Fixture::classify_patch(
            OPERATION,
            PatchAction::Assign {
                assignees: BTreeSet::from([Did::from(member)])
            },
            author,
            &mut assignment_context,
        )
        .as_slice(),
        [Activity {
            kind: Kind::Assigned,
            ..
        }]
    ));

    let mut owned_context = Context::default();
    assert!(
        Fixture::classify_patch(
            OPERATION,
            Fixture::revision("Initial revision"),
            member,
            &mut owned_context,
        )
        .is_empty()
    );
    assert!(matches!(
        Fixture::classify_patch(
            COMMENT,
            PatchAction::Review {
                revision: RevisionId::from(
                    EntryId::from_str(OPERATION).expect("revision should be valid")
                ),
                summary: Some("Looks good".to_owned()),
                verdict: Some(Verdict::Accept),
                labels: Vec::new(),
            },
            author,
            &mut owned_context,
        )
        .as_slice(),
        [Activity {
            kind: Kind::Delivered(Delivery::Changed(Change::ReviewSubmitted { .. })),
            ..
        }]
    ));
}

#[test]
fn reviews_obey_trigger_policy_and_delegates_need_no_mention() {
    let member = PublicKey::from_str(MEMBER).expect("member key should be valid");
    let person = PublicKey::from_str(AUTHOR).expect("person key should be valid");
    let delegate = PublicKey::from_str(DELEGATE).expect("delegate key should be valid");
    let revision = RevisionId::from(EntryId::from_str(ISSUE).expect("revision should be valid"));

    for (access, person_allowed) in [
        (TriggerAccess::Delegates, false),
        (TriggerAccess::Dids(BTreeSet::new()), false),
        (
            TriggerAccess::Dids(BTreeSet::from([Did::from(person)])),
            true,
        ),
        (TriggerAccess::Anyone, true),
    ] {
        let triggers = Fixture::triggers_for(access, vec![Did::from(delegate)], Vec::new());
        let mut context = Context::default();
        assert!(
            Fixture::classify_patch_with(
                ISSUE,
                Fixture::revision("Initial revision"),
                member,
                &mut context,
                &triggers,
            )
            .is_empty()
        );
        let unlisted_review = Fixture::classify_patch_with(
            COMMENT,
            PatchAction::Review {
                revision,
                summary: Some("@nico please fix this".to_owned()),
                verdict: Some(Verdict::Reject),
                labels: Vec::new(),
            },
            person,
            &mut context,
            &triggers,
        );
        assert_eq!(
            !unlisted_review.is_empty(),
            person_allowed,
            "a review or mention must not bypass the repository's trigger policy"
        );
        let delegate_review = Fixture::classify_patch_with(
            CHANGE,
            PatchAction::Review {
                revision,
                summary: None,
                verdict: Some(Verdict::Reject),
                labels: Vec::new(),
            },
            delegate,
            &mut context,
            &triggers,
        );
        assert!(
            matches!(
                delegate_review.as_slice(),
                [Activity {
                    kind: Kind::Delivered(Delivery::Changed(Change::ReviewSubmitted { .. })),
                    ..
                }]
            ),
            "a delegate's review must reach the patch owner without a mention or summary"
        );
    }
}

#[test]
fn classifies_patch_mentions_and_replies() {
    let member = PublicKey::from_str(MEMBER).expect("member key should be valid");
    let author = PublicKey::from_str(AUTHOR).expect("author key should be valid");
    let revision = RevisionId::from(EntryId::from_str(ISSUE).expect("revision should be valid"));
    let review = ReviewId::from(EntryId::from_str(ISSUE).expect("review should be valid"));
    let mut context = Context::default();

    assert!(
        Fixture::classify_patch(
            OPERATION,
            PatchAction::RevisionComment {
                revision,
                location: None,
                body: "My comment".to_owned(),
                reply_to: None,
                embeds: Vec::new(),
            },
            member,
            &mut context,
        )
        .is_empty()
    );
    assert!(matches!(
        Fixture::classify_patch(
            COMMENT,
            PatchAction::RevisionComment {
                revision,
                location: None,
                body: "A reply".to_owned(),
                reply_to: Some(EntryId::from_str(OPERATION).expect("comment should be valid")),
                embeds: Vec::new(),
            },
            author,
            &mut context,
        )
        .as_slice(),
        [Activity {
            kind: Kind::Delivered(Delivery::Replied {
                comment: Comment::Revision { .. },
                ..
            }),
            ..
        }]
    ));
    assert!(matches!(
        Fixture::classify_patch(
            COMMENT,
            PatchAction::ReviewComment {
                review,
                body: "Please check this, @nico".to_owned(),
                location: None,
                reply_to: None,
                embeds: Vec::new(),
            },
            author,
            &mut context,
        )
        .as_slice(),
        [Activity {
            kind: Kind::Delivered(Delivery::Mentioned(_)),
            ..
        }]
    ));
}

#[test]
fn classifies_every_remaining_issue_interaction() {
    let member = PublicKey::from_str(MEMBER).expect("member key should be valid");
    let author = PublicKey::from_str(AUTHOR).expect("author key should be valid");
    let mut context = Context::default();
    assert!(
        Fixture::classify_issue(
            Action::Comment {
                body: "Owned discussion".to_owned(),
                reply_to: None,
                embeds: Vec::new(),
            },
            member,
            &mut context,
        )
        .is_empty()
    );

    let comment = EntryId::from_str(OPERATION).expect("comment should be valid");
    let reaction = Reaction::new('👍').expect("reaction should be valid");
    let actions = [
        Action::Edit {
            title: Title::new("Updated issue").expect("title should be valid"),
        },
        Action::Lifecycle {
            state: IssueState::Open,
        },
        Action::Label {
            labels: BTreeSet::new(),
        },
        Action::Comment {
            body: "Additional context".to_owned(),
            reply_to: None,
            embeds: Vec::new(),
        },
        Action::CommentEdit {
            id: comment,
            body: "Edited context".to_owned(),
            embeds: Vec::new(),
        },
        Action::CommentRedact { id: comment },
    ];

    for action in actions {
        assert!(!Fixture::classify_issue(action, author, &mut context).is_empty());
    }

    assert!(
        Fixture::classify_issue(
            Action::CommentReact {
                id: comment,
                reaction,
                active: true,
            },
            author,
            &mut context,
        )
        .is_empty(),
        "a reaction must not start work, because approving an idea and asking for it to be \
         carried out are indistinguishable"
    );
}

#[test]
fn classifies_every_remaining_patch_interaction() {
    let member = PublicKey::from_str(MEMBER).expect("member key should be valid");
    let author = PublicKey::from_str(AUTHOR).expect("author key should be valid");
    let revision =
        RevisionId::from(EntryId::from_str(OPERATION).expect("revision should be valid"));
    let review = ReviewId::from(EntryId::from_str(COMMENT).expect("review should be valid"));
    let review_comment = EntryId::from_str(CHANGE).expect("comment should be valid");
    let revision_comment = EntryId::from_str(REPLY).expect("comment should be valid");
    let reaction = Reaction::new('👍').expect("reaction should be valid");
    let mut context = Context::default();

    let member_actions = [
        (OPERATION, Fixture::revision("Initial revision")),
        (
            COMMENT,
            PatchAction::Review {
                revision,
                summary: Some("My review".to_owned()),
                verdict: Some(Verdict::Accept),
                labels: Vec::new(),
            },
        ),
        (
            CHANGE,
            PatchAction::ReviewComment {
                review,
                body: "Review comment".to_owned(),
                location: None,
                reply_to: None,
                embeds: Vec::new(),
            },
        ),
        (
            REPLY,
            PatchAction::RevisionComment {
                revision,
                location: None,
                body: "Revision comment".to_owned(),
                reply_to: None,
                embeds: Vec::new(),
            },
        ),
    ];
    for (id, action) in member_actions {
        assert!(Fixture::classify_patch(id, action, member, &mut context).is_empty());
    }

    let actions = [
        PatchAction::Edit {
            title: Title::new("Updated patch").expect("title should be valid"),
            target: MergeTarget::Delegates,
        },
        PatchAction::Label {
            labels: BTreeSet::new(),
        },
        PatchAction::Lifecycle {
            state: Lifecycle::Archived,
        },
        PatchAction::Merge {
            revision,
            commit: *ObjectId::from_str(COMMENT).expect("commit should be valid"),
        },
        PatchAction::ReviewEdit(ReviewEdit::new(
            review,
            "Updated review".to_owned(),
            Some(Verdict::Reject),
            Vec::new(),
            Vec::new(),
        )),
        PatchAction::ReviewRedact { review },
        PatchAction::ReviewCommentEdit {
            review,
            comment: review_comment,
            body: "Edited review comment".to_owned(),
            embeds: Vec::new(),
        },
        PatchAction::ReviewCommentRedact {
            review,
            comment: review_comment,
        },
        PatchAction::ReviewCommentResolve {
            review,
            comment: review_comment,
        },
        PatchAction::ReviewCommentUnresolve {
            review,
            comment: review_comment,
        },
        PatchAction::RevisionEdit {
            revision,
            description: "Edited revision".to_owned(),
            embeds: Vec::new(),
        },
        PatchAction::RevisionRedact { revision },
        PatchAction::RevisionCommentEdit {
            revision,
            comment: revision_comment,
            body: "Edited revision comment".to_owned(),
            embeds: Vec::new(),
        },
        PatchAction::RevisionCommentRedact {
            revision,
            comment: revision_comment,
        },
    ];

    for action in actions {
        assert!(!Fixture::classify_patch(ISSUE, action, author, &mut context).is_empty());
    }

    let reactions = [
        PatchAction::ReviewReact {
            review,
            reaction,
            active: true,
        },
        PatchAction::ReviewCommentReact {
            review,
            comment: review_comment,
            reaction,
            active: true,
        },
        PatchAction::RevisionReact {
            revision,
            location: None,
            reaction,
            active: true,
        },
        PatchAction::RevisionCommentReact {
            revision,
            comment: revision_comment,
            reaction,
            active: true,
        },
    ];

    for action in reactions {
        assert!(
            Fixture::classify_patch(ISSUE, action, author, &mut context).is_empty(),
            "a reaction must not start work, because approving an idea and asking for it to be \
             carried out are indistinguishable"
        );
    }
}

#[test]
fn allows_members_to_trigger_each_other_whatever_the_manifest_says() {
    let member = PublicKey::from_str(MEMBER).expect("member key should be valid");
    let peer = PublicKey::from_str(AUTHOR).expect("peer key should be valid");
    let triggers =
        Fixture::triggers_for(TriggerAccess::Delegates, Vec::new(), vec![Did::from(peer)]);

    assert!(
        !Fixture::classify_with(
            Action::Assign {
                assignees: BTreeSet::from([Did::from(member)]),
            },
            peer,
            &mut Context::default(),
            &triggers,
        )
        .is_empty(),
        "one member must be able to delegate to another without being listed in `triggered_by`"
    );
}

#[test]
fn refuses_team_mentions_between_members() {
    let peer = PublicKey::from_str(AUTHOR).expect("peer key should be valid");
    let outsider = PublicKey::from_str(DELEGATE).expect("outsider key should be valid");
    let triggers = Fixture::triggers(vec![Did::from(peer)]);

    assert!(
        Fixture::classify_with(
            Fixture::comment("@team what do you think?"),
            peer,
            &mut Context::default(),
            &triggers,
        )
        .is_empty(),
        "no member may summon the whole team, because that fans one message out to everyone"
    );
    assert!(
        !Fixture::classify_with(
            Fixture::comment("@nico could you look at this?"),
            peer,
            &mut Context::default(),
            &triggers,
        )
        .is_empty(),
        "a member may still address another by name"
    );
    assert!(
        !Fixture::classify_with(
            Fixture::comment("@team what do you think?"),
            outsider,
            &mut Context::default(),
            &triggers,
        )
        .is_empty(),
        "`@team` remains available to everyone outside the team"
    );
}

#[test]
fn refuses_ambient_changes_between_members() {
    let member = PublicKey::from_str(MEMBER).expect("member key should be valid");
    let peer = PublicKey::from_str(AUTHOR).expect("peer key should be valid");
    let outsider = PublicKey::from_str(DELEGATE).expect("outsider key should be valid");
    let triggers = Fixture::triggers(vec![Did::from(peer)]);
    let mut context = Context::default();

    assert!(
        !Fixture::classify_with(
            Action::Assign {
                assignees: BTreeSet::from([Did::from(member)]),
            },
            outsider,
            &mut context,
            &triggers,
        )
        .is_empty(),
        "the assignment itself should reach the member"
    );

    let edit = || Action::Edit {
        title: Title::new("Updated issue").expect("title should be valid"),
    };
    assert!(
        Fixture::classify_with(edit(), peer, &mut context, &triggers).is_empty(),
        "a member editing the subject must not wake another, or a thread could sustain itself"
    );
    assert!(
        !Fixture::classify_with(edit(), outsider, &mut context, &triggers).is_empty(),
        "the same change from outside the team still reaches the member"
    );
}

#[test]
fn stops_a_conversation_that_has_run_its_turns_until_a_person_joins_it() {
    let member = PublicKey::from_str(MEMBER).expect("member key should be valid");
    let peer = PublicKey::from_str(AUTHOR).expect("peer key should be valid");
    let outsider = PublicKey::from_str(DELEGATE).expect("outsider key should be valid");
    let triggers = Fixture::triggers(vec![Did::from(member), Did::from(peer)]);
    let mut context = Context::default();
    let asks = |author, context: &mut Context| {
        Fixture::classify_with(
            Fixture::comment("@nico thoughts?"),
            author,
            context,
            &triggers,
        )
    };

    for turn in 0..Fixture::exchanges() {
        assert!(
            !asks(peer, &mut context).is_empty(),
            "turn {turn} is still within the allowance"
        );
        let _own = Fixture::classify_with(
            Fixture::comment("here they are"),
            member,
            &mut context,
            &triggers,
        );
    }
    assert!(
        asks(peer, &mut context).is_empty(),
        "a conversation between two members must stop once it has run its turns"
    );

    let _aside = Fixture::classify_with(
        Fixture::comment("unrelated note"),
        outsider,
        &mut context,
        &triggers,
    );
    assert!(
        asks(peer, &mut context).is_empty(),
        "a person passing through without addressing the member must not re-arm the conversation, \
         or an unrelated label or remark would restart it by accident"
    );

    let _asked = Fixture::classify_with(
        Fixture::comment("@nico what do you think?"),
        outsider,
        &mut context,
        &triggers,
    );
    assert!(
        !asks(peer, &mut context).is_empty(),
        "a person addressing the member restores it"
    );
}

#[test]
fn counts_a_run_of_operations_by_one_author_as_a_single_turn() {
    let member = PublicKey::from_str(MEMBER).expect("member key should be valid");
    let peer = PublicKey::from_str(AUTHOR).expect("peer key should be valid");
    let triggers = Fixture::triggers(vec![Did::from(member), Did::from(peer)]);
    let mut context = Context::default();

    for remark in 0..Fixture::exchanges() * 3 {
        assert!(
            !Fixture::classify_with(
                Action::Comment {
                    body: format!("@nico point {remark}"),
                    reply_to: None,
                    embeds: Vec::new(),
                },
                peer,
                &mut context,
                &triggers,
            )
            .is_empty(),
            "remark {remark} is part of one uninterrupted turn, so it costs what one remark costs"
        );
    }
}

#[test]
fn counts_turns_against_the_member_that_took_them_rather_than_the_subject() {
    let member = PublicKey::from_str(MEMBER).expect("member key should be valid");
    let first = PublicKey::from_str(AUTHOR).expect("first peer key should be valid");
    let second = PublicKey::from_str(DELEGATE).expect("second peer key should be valid");
    let triggers = Fixture::triggers(vec![Did::from(member), Did::from(first), Did::from(second)]);
    let mut context = Context::default();

    for _turn in 0..Fixture::exchanges() {
        let _asked = Fixture::classify_with(
            Fixture::comment("@nico thoughts?"),
            first,
            &mut context,
            &triggers,
        );
        let _own = Fixture::classify_with(
            Fixture::comment("here they are"),
            member,
            &mut context,
            &triggers,
        );
    }
    assert!(
        Fixture::classify_with(
            Fixture::comment("@nico thoughts?"),
            first,
            &mut context,
            &triggers
        )
        .is_empty(),
        "the first conversation has run its turns"
    );
    assert!(
        !Fixture::classify_with(
            Fixture::comment("@nico thoughts?"),
            second,
            &mut context,
            &triggers
        )
        .is_empty(),
        "a second colleague has said nothing yet, so a patch with several reviewers gives each of \
         them a whole conversation instead of a share of one"
    );
}

#[test]
fn delivers_peer_reviews_to_owners_and_revision_followups_to_rejecting_reviewers() {
    let member = PublicKey::from_str(MEMBER).expect("member key should be valid");
    let peer = PublicKey::from_str(AUTHOR).expect("peer key should be valid");
    let triggers = Fixture::triggers(vec![Did::from(peer)]);
    let revision = RevisionId::from(EntryId::from_str(COMMENT).expect("revision should be valid"));
    let mut context = Context::default();

    let opened = Fixture::classify_patch_with(
        ISSUE,
        Fixture::revision("Initial revision"),
        member,
        &mut context,
        &triggers,
    );
    assert!(
        opened.is_empty(),
        "a member's own revision must not come back to it"
    );

    let reviewed = Fixture::classify_patch_with(
        OPERATION,
        PatchAction::Review {
            revision,
            summary: None,
            verdict: Some(Verdict::Reject),
            labels: Vec::new(),
        },
        peer,
        &mut context,
        &triggers,
    );
    assert!(
        !reviewed.is_empty(),
        "a review by another member must reach the patch owner without a mention, or delegated \
         work would stall once it was reviewed"
    );

    let mut reviewer = Context::default();
    assert!(
        Fixture::classify_patch_with(
            ISSUE,
            Fixture::revision("Initial revision"),
            peer,
            &mut reviewer,
            &triggers,
        )
        .is_empty(),
        "a peer's revision must not summon an uninvolved member"
    );
    let _own_review = Fixture::classify_patch_with(
        OPERATION,
        PatchAction::Review {
            revision,
            summary: None,
            verdict: Some(Verdict::Reject),
            labels: Vec::new(),
        },
        member,
        &mut reviewer,
        &triggers,
    );
    assert!(
        matches!(
            Fixture::classify_patch_with(
                CHANGE,
                Fixture::revision("Addressed the feedback"),
                peer,
                &mut reviewer,
                &triggers,
            )
            .as_slice(),
            [Activity {
                kind: Kind::Delivered(Delivery::Changed(Change::ReviewFollowUp {
                    review,
                    revision: updated,
                })),
                ..
            }] if *review == ReviewId::from(EntryId::from_str(OPERATION).expect("review should parse"))
                && *updated == RevisionId::from(EntryId::from_str(CHANGE).expect("revision should parse"))
        ),
        "a new revision must identify the rejecting member's own review"
    );
}

#[test]
fn accepted_review_does_not_follow_another_reviewers_feedback() {
    let member = PublicKey::from_str(MEMBER).expect("member key should be valid");
    let author = PublicKey::from_str(AUTHOR).expect("author key should be valid");
    let delegate = PublicKey::from_str(DELEGATE).expect("delegate key should be valid");
    let revision = RevisionId::from(EntryId::from_str(ISSUE).expect("revision should parse"));
    let own_review = ReviewId::from(EntryId::from_str(OPERATION).expect("review should parse"));
    let other_review = ReviewId::from(EntryId::from_str(CHANGE).expect("review should parse"));
    let triggers = Fixture::triggers(vec![Did::from(author)]);
    let mut context = Context::default();
    let _opened = Fixture::classify_patch(
        ISSUE,
        Fixture::revision("Initial revision"),
        author,
        &mut context,
    );
    let _accepted = Fixture::classify_patch(
        OPERATION,
        PatchAction::Review {
            revision,
            summary: Some("Ready; two optional suggestions".to_owned()),
            verdict: Some(Verdict::Accept),
            labels: Vec::new(),
        },
        member,
        &mut context,
    );

    for action in [
        PatchAction::Review {
            revision,
            summary: Some("Required changes".to_owned()),
            verdict: Some(Verdict::Reject),
            labels: Vec::new(),
        },
        PatchAction::ReviewComment {
            review: other_review,
            body: "Please shorten the README".to_owned(),
            reply_to: None,
            location: None,
            embeds: Vec::new(),
        },
        Fixture::revision("Addressed the delegate's review"),
        PatchAction::Lifecycle {
            state: Lifecycle::Open,
        },
    ] {
        assert!(
            Fixture::classify_patch(CHANGE, action, delegate, &mut context).is_empty(),
            "another review and its resulting revisions must not reopen an accepted review"
        );
    }

    assert!(
        !Fixture::classify_patch_with(
            REPLY,
            PatchAction::ReviewComment {
                review: own_review,
                body: "I addressed your optional suggestion".to_owned(),
                reply_to: None,
                location: None,
                embeds: Vec::new(),
            },
            author,
            &mut context,
            &triggers,
        )
        .is_empty(),
        "a peer's direct response on the member's own accepted review still needs attention"
    );
    assert!(
        matches!(
            Fixture::classify_patch(
                COMMENT,
                PatchAction::ReviewComment {
                    review: other_review,
                    body: "@nico please check the terminal requirement".to_owned(),
                    reply_to: None,
                    location: None,
                    embeds: Vec::new(),
                },
                delegate,
                &mut context,
            )
            .as_slice(),
            [Activity {
                kind: Kind::Delivered(Delivery::Mentioned(_)),
                ..
            }]
        ),
        "an explicit mention can involve a reviewer in another review"
    );
}

#[test]
fn clearing_or_accepting_or_redacting_the_own_review_stops_revision_followups() {
    let member = PublicKey::from_str(MEMBER).expect("member key should be valid");
    let author = PublicKey::from_str(AUTHOR).expect("author key should be valid");
    let revision = RevisionId::from(EntryId::from_str(ISSUE).expect("revision should parse"));
    let review = ReviewId::from(EntryId::from_str(OPERATION).expect("review should parse"));
    for close in [
        PatchAction::ReviewEdit(ReviewEdit::new(
            review,
            "The findings are satisfied".to_owned(),
            Some(Verdict::Accept),
            Vec::new(),
            Vec::new(),
        )),
        PatchAction::ReviewEdit(ReviewEdit::new(
            review,
            "The finding is only a suggestion".to_owned(),
            None,
            Vec::new(),
            Vec::new(),
        )),
        PatchAction::ReviewRedact { review },
    ] {
        let mut context = Context::default();
        let _opened = Fixture::classify_patch(
            ISSUE,
            Fixture::revision("Initial revision"),
            author,
            &mut context,
        );
        let _rejected = Fixture::classify_patch(
            OPERATION,
            PatchAction::Review {
                revision,
                summary: Some("Required fixes".to_owned()),
                verdict: Some(Verdict::Reject),
                labels: Vec::new(),
            },
            member,
            &mut context,
        );
        let _closed = Fixture::classify_patch(REPLY, close, member, &mut context);
        assert!(
            Fixture::classify_patch(
                CHANGE,
                Fixture::revision("Another revision"),
                author,
                &mut context,
            )
            .is_empty(),
            "a completed review must not follow later revisions"
        );
    }
}

#[test]
fn opening_a_draft_identifies_the_rejected_review_and_the_current_revision() {
    let member = PublicKey::from_str(MEMBER).expect("member key should be valid");
    let author = PublicKey::from_str(AUTHOR).expect("author key should be valid");
    let revision = RevisionId::from(EntryId::from_str(ISSUE).expect("revision should parse"));
    let review = ReviewId::from(EntryId::from_str(OPERATION).expect("review should parse"));
    let updated = RevisionId::from(EntryId::from_str(CHANGE).expect("revision should parse"));
    let mut context = Context::default();
    let _opened = Fixture::classify_patch(
        ISSUE,
        Fixture::revision("Initial revision"),
        author,
        &mut context,
    );
    let _rejected = Fixture::classify_patch(
        OPERATION,
        PatchAction::Review {
            revision,
            summary: Some("Required fixes".to_owned()),
            verdict: Some(Verdict::Reject),
            labels: Vec::new(),
        },
        member,
        &mut context,
    );
    let _draft = Fixture::classify_patch(
        REPLY,
        PatchAction::Lifecycle {
            state: Lifecycle::Draft,
        },
        author,
        &mut context,
    );
    assert!(
        Fixture::classify_patch(
            CHANGE,
            Fixture::revision("Addressed the findings"),
            author,
            &mut context,
        )
        .is_empty()
    );
    assert!(matches!(
        Fixture::classify_patch(
            COMMENT,
            PatchAction::Lifecycle {
                state: Lifecycle::Open,
            },
            author,
            &mut context,
        )
        .as_slice(),
        [Activity {
            kind: Kind::Delivered(Delivery::Changed(Change::ReviewFollowUp {
                review: own,
                revision: current,
            })),
            ..
        }] if *own == review && *current == updated
    ));
    assert!(
        Fixture::classify_patch(
            REPLY,
            PatchAction::RevisionEdit {
                revision: updated,
                description: "Clarified the description".to_owned(),
                embeds: Vec::new(),
            },
            author,
            &mut context,
        )
        .is_empty(),
        "editing a description must not request another review of unchanged code"
    );
}

#[test]
fn draft_revisions_wait_for_readiness_but_explicit_messages_and_assignments_arrive() {
    let member = PublicKey::from_str(MEMBER).expect("member key should be valid");
    let peer = PublicKey::from_str(AUTHOR).expect("peer key should be valid");
    let triggers = Fixture::triggers(vec![Did::from(peer)]);
    let revision = RevisionId::from(EntryId::from_str(ISSUE).expect("revision should parse"));
    let mut reviewer = Context::default();
    let _initial = Fixture::classify_patch_with(
        ISSUE,
        Fixture::revision("Initial"),
        peer,
        &mut reviewer,
        &triggers,
    );
    let _review = Fixture::classify_patch_with(
        OPERATION,
        PatchAction::Review {
            revision,
            summary: Some("Fix the parser".to_owned()),
            verdict: Some(Verdict::Reject),
            labels: Vec::new(),
        },
        member,
        &mut reviewer,
        &triggers,
    );
    let transaction = Op::new(
        EntryId::from_str(CHANGE).expect("operation should parse"),
        (
            Fixture::revision("Work started"),
            vec![PatchAction::Lifecycle {
                state: Lifecycle::Draft,
            }],
        ),
        peer,
        Timestamp::from_secs(0),
        None,
        Manifest::new(PATCH_TYPENAME.clone(), Version::default()),
    );
    assert!(
        reviewer
            .classify(
                &transaction,
                Member::Nico,
                member,
                RepoId::from_str(REPOSITORY).expect("repository should parse"),
                Fixture::patch(),
                &triggers,
            )
            .is_empty(),
        "a revision and draft state in one transaction must not request review"
    );

    for action in [
        Fixture::revision("Still implementing"),
        PatchAction::RevisionEdit {
            revision,
            description: "Progress".to_owned(),
            embeds: Vec::new(),
        },
    ] {
        assert!(
            Fixture::classify_patch_with(CHANGE, action, peer, &mut reviewer, &triggers,)
                .is_empty(),
            "intermediate draft changes must not start another review"
        );
    }
    assert!(
        matches!(
            Fixture::classify_patch_with(
                COMMENT,
                Fixture::revision("@nico which input did you test?"),
                peer,
                &mut reviewer,
                &triggers,
            )
            .as_slice(),
            [Activity {
                kind: Kind::Delivered(Delivery::Mentioned(_)),
                ..
            }]
        ),
        "a draft must still support explicit questions"
    );
    assert!(
        matches!(
            Fixture::classify_patch_with(
                REPLY,
                PatchAction::Assign {
                    assignees: [Did::from(member)].into()
                },
                peer,
                &mut reviewer,
                &triggers,
            )
            .as_slice(),
            [Activity {
                kind: Kind::Assigned,
                ..
            }]
        ),
        "an assignment to a draft still queues work"
    );
    assert!(
        matches!(
            Fixture::classify_patch_with(
                REPLY,
                PatchAction::Lifecycle {
                    state: Lifecycle::Open
                },
                peer,
                &mut reviewer,
                &triggers,
            )
            .as_slice(),
            [Activity {
                kind: Kind::Delivered(Delivery::Changed(Change::PatchReady)),
                ..
            }]
        ),
        "opening the finished patch must notify an assignee without a mention"
    );

    let mut uninvolved = Context::default();
    assert!(
        Fixture::classify_patch_with(
            ISSUE,
            PatchAction::Lifecycle {
                state: Lifecycle::Open
            },
            peer,
            &mut uninvolved,
            &triggers,
        )
        .is_empty(),
        "readiness must not summon members who never owned or reviewed this patch"
    );
}

#[test]
fn a_member_s_own_activity_never_spends_its_own_allowance() {
    let member = PublicKey::from_str(MEMBER).expect("member key should be valid");
    let peer = PublicKey::from_str(AUTHOR).expect("peer key should be valid");
    let triggers = Fixture::triggers(vec![Did::from(member), Did::from(peer)]);
    let mut context = Context::default();

    for _ in 0..4 {
        let _own = Fixture::classify_patch_with(
            ISSUE,
            Fixture::revision("Another revision"),
            member,
            &mut context,
            &triggers,
        );
    }

    assert!(
        !Fixture::classify_patch_with(
            OPERATION,
            Fixture::revision("@nico could you rebase this?"),
            peer,
            &mut context,
            &triggers,
        )
        .is_empty(),
        "an allowance counts what a colleague has said to the member, not what the member has \
         been doing, so a day spent building must not make it deaf to its own patch"
    );
}
#[test]
fn caps_the_review_and_revision_cycle_between_two_members() {
    let nico = PublicKey::from_str(MEMBER).expect("member key should be valid");
    let vera = PublicKey::from_str(AUTHOR).expect("reviewer key should be valid");
    let triggers = Fixture::triggers(vec![Did::from(nico), Did::from(vera)]);
    let revision = RevisionId::from(EntryId::from_str(ISSUE).expect("revision should be valid"));
    let mut context = Context::default();
    let revise = || Fixture::revision("addressed the comments");
    let review = || PatchAction::Review {
        revision,
        summary: Some("still not right".to_owned()),
        verdict: Some(Verdict::Reject),
        labels: Vec::new(),
    };

    // Nico opens the patch, so the context records him as its owner.
    let _opened = Fixture::classify_patch_with(REPLY, revise(), nico, &mut context, &triggers);

    for round in 0..Fixture::exchanges() {
        assert!(
            !Fixture::classify_patch_with(CHANGE, review(), vera, &mut context, &triggers)
                .is_empty(),
            "round {round} of the review still reaches the author"
        );
        let _addressed =
            Fixture::classify_patch_with(OPERATION, revise(), nico, &mut context, &triggers);
    }

    assert!(
        Fixture::classify_patch_with(CHANGE, review(), vera, &mut context, &triggers).is_empty(),
        "reviewing and revising cost nobody anything to repeat, so without this the two would \
         carry on without end and no person would be asked"
    );
}

#[test]
fn reactions_do_not_split_a_peers_turn_or_spend_an_exchange() {
    let peer: PublicKey = AUTHOR.parse().expect("peer");
    let member: PublicKey = MEMBER.parse().expect("member");
    let triggers = Fixture::triggers(vec![peer.into()]);
    let mut context = Context::default();
    for _ in 0..(Fixture::exchanges() + 2) {
        assert_eq!(
            Fixture::classify_with(
                Fixture::comment("@nico look at this"),
                peer,
                &mut context,
                &triggers
            )
            .len(),
            1
        );
        assert!(
            Fixture::classify_with(
                Action::CommentReact {
                    id: OPERATION.parse().expect("comment"),
                    reaction: Reaction::new('👍').expect("reaction"),
                    active: true
                },
                member,
                &mut context,
                &triggers
            )
            .is_empty()
        );
    }
    assert!(
        context.take_ignored().is_empty(),
        "acknowledgement alone cannot exhaust the quota"
    );
    for turn in 1..=Fixture::exchanges() {
        Fixture::classify_with(
            Fixture::comment("my reply"),
            member,
            &mut context,
            &triggers,
        );
        let next = Fixture::classify_with(
            Fixture::comment("@nico next point"),
            peer,
            &mut context,
            &triggers,
        );
        assert_eq!(next.len(), usize::from(turn < Fixture::exchanges()));
    }
    assert_eq!(
        context.take_ignored().len(),
        1,
        "real member exchanges remain bounded"
    );
}
