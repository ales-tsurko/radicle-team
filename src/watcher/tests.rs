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
    manifest::TriggerAccess,
    member::Member,
    watcher::{
        Change, Comment, Subject,
        activity::{Activity, Delivery, Kind},
        classification::{ActionEnvelope, Context, NormalizedAction, Triggers},
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

    fn classify<A>(
        id: &str,
        action: A,
        author: PublicKey,
        type_name: TypeName,
        subject: Subject,
        context: &mut Context,
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
            &Triggers::new(TriggerAccess::Anyone, Vec::new(), Vec::new()),
        )
    }

    fn classify_with(
        action: Action,
        author: PublicKey,
        context: &mut Context,
        triggers: &Triggers,
    ) -> Vec<Activity> {
        context.classify(
            &Self::operation(OPERATION, action, author, TYPENAME.clone()),
            Member::Nico,
            PublicKey::from_str(MEMBER).expect("member key should be valid"),
            RepoId::from_str(REPOSITORY).expect("repository should be valid"),
            Subject::Issue(ObjectId::from_str(ISSUE).expect("issue should be valid")),
            triggers,
        )
    }

    fn classify_issue(action: Action, author: PublicKey, context: &mut Context) -> Vec<Activity> {
        Self::classify(
            OPERATION,
            action,
            author,
            TYPENAME.clone(),
            Subject::Issue(ObjectId::from_str(ISSUE).expect("issue should be valid")),
            context,
        )
    }

    fn classify_patch(
        id: &str,
        action: PatchAction,
        author: PublicKey,
        context: &mut Context,
    ) -> Vec<Activity> {
        Self::classify(
            id,
            action,
            author,
            PATCH_TYPENAME.clone(),
            Subject::Patch(ObjectId::from_str(ISSUE).expect("patch should be valid")),
            context,
        )
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
        &Triggers::new(TriggerAccess::Anyone, delegates.clone(), Vec::new()),
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
        &Triggers::new(TriggerAccess::Delegates, delegates.clone(), Vec::new()),
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
            &Triggers::new(TriggerAccess::Delegates, delegates, Vec::new()),
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
    let triggers = Triggers::new(
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
            Subject::Issue(ObjectId::from_str(ISSUE).expect("issue should be valid")),
            &mut context,
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
            PatchAction::Revision {
                description: "Initial revision".to_owned(),
                base: *ObjectId::from_str(ISSUE).expect("base should be valid"),
                oid: *ObjectId::from_str(COMMENT).expect("head should be valid"),
                resolves: BTreeSet::new(),
            },
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
        (
            OPERATION,
            PatchAction::Revision {
                description: "Initial revision".to_owned(),
                base: *ObjectId::from_str(ISSUE).expect("base should be valid"),
                oid: *ObjectId::from_str(COMMENT).expect("head should be valid"),
                resolves: BTreeSet::new(),
            },
        ),
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
    let triggers = Triggers::new(TriggerAccess::Delegates, Vec::new(), vec![Did::from(peer)]);

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
    let triggers = Triggers::new(TriggerAccess::Anyone, Vec::new(), vec![Did::from(peer)]);
    let comment = |body: &str| Action::Comment {
        body: body.to_owned(),
        reply_to: None,
        embeds: Vec::new(),
    };

    assert!(
        Fixture::classify_with(
            comment("@team what do you think?"),
            peer,
            &mut Context::default(),
            &triggers,
        )
        .is_empty(),
        "no member may summon the whole team, because that fans one message out to everyone"
    );
    assert!(
        !Fixture::classify_with(
            comment("@nico could you look at this?"),
            peer,
            &mut Context::default(),
            &triggers,
        )
        .is_empty(),
        "a member may still address another by name"
    );
    assert!(
        !Fixture::classify_with(
            comment("@team what do you think?"),
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
    let triggers = Triggers::new(TriggerAccess::Anyone, Vec::new(), vec![Did::from(peer)]);
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
fn stops_answering_once_a_member_reaches_its_response_limit() {
    let member = PublicKey::from_str(MEMBER).expect("member key should be valid");
    let peer = PublicKey::from_str(AUTHOR).expect("peer key should be valid");
    let outsider = PublicKey::from_str(DELEGATE).expect("outsider key should be valid");
    let triggers = Triggers::new(
        TriggerAccess::Anyone,
        Vec::new(),
        vec![Did::from(member), Did::from(peer)],
    );
    let comment = |body: &str| Action::Comment {
        body: body.to_owned(),
        reply_to: None,
        embeds: Vec::new(),
    };
    let mut context = Context::default();

    for answer in 0..3 {
        assert!(
            !Fixture::classify_with(comment("@nico thoughts?"), peer, &mut context, &triggers)
                .is_empty(),
            "answer {answer} is still within the limit"
        );
        let _own =
            Fixture::classify_with(comment("here they are"), member, &mut context, &triggers);
    }

    assert!(
        Fixture::classify_with(comment("@nico thoughts?"), peer, &mut context, &triggers)
            .is_empty(),
        "a member must stop answering a subject once it has spent its allowance"
    );

    let _human =
        Fixture::classify_with(comment("let us move on"), outsider, &mut context, &triggers);
    assert!(
        !Fixture::classify_with(comment("@nico thoughts?"), peer, &mut context, &triggers)
            .is_empty(),
        "a non-member acting on the subject must restore the allowance"
    );
}

#[test]
fn delivers_a_peer_review_to_the_patch_owner() {
    let member = PublicKey::from_str(MEMBER).expect("member key should be valid");
    let peer = PublicKey::from_str(AUTHOR).expect("peer key should be valid");
    let triggers = Triggers::new(TriggerAccess::Anyone, Vec::new(), vec![Did::from(peer)]);
    let revision = RevisionId::from(EntryId::from_str(COMMENT).expect("revision should be valid"));
    let mut context = Context::default();

    let opened = context.classify(
        &Fixture::operation(
            ISSUE,
            PatchAction::Revision {
                description: "Initial revision".to_owned(),
                base: *ObjectId::from_str(ISSUE).expect("base should be valid"),
                oid: *ObjectId::from_str(COMMENT).expect("head should be valid"),
                resolves: BTreeSet::new(),
            },
            member,
            PATCH_TYPENAME.clone(),
        ),
        Member::Nico,
        member,
        RepoId::from_str(REPOSITORY).expect("repository should be valid"),
        Subject::Patch(ObjectId::from_str(ISSUE).expect("patch should be valid")),
        &triggers,
    );
    assert!(
        opened.is_empty(),
        "a member's own revision must not come back to it"
    );

    let reviewed = context.classify(
        &Fixture::operation(
            OPERATION,
            PatchAction::Review {
                revision,
                summary: None,
                verdict: Some(Verdict::Reject),
                labels: Vec::new(),
            },
            peer,
            PATCH_TYPENAME.clone(),
        ),
        Member::Nico,
        member,
        RepoId::from_str(REPOSITORY).expect("repository should be valid"),
        Subject::Patch(ObjectId::from_str(ISSUE).expect("patch should be valid")),
        &triggers,
    );
    assert!(
        !reviewed.is_empty(),
        "a review by another member must reach the patch owner without a mention, or delegated \
         work would stall once it was reviewed"
    );
}

#[test]
fn building_does_not_spend_a_member_s_answer_allowance() {
    let member = PublicKey::from_str(MEMBER).expect("member key should be valid");
    let peer = PublicKey::from_str(AUTHOR).expect("peer key should be valid");
    let triggers = Triggers::new(TriggerAccess::Anyone, Vec::new(), vec![Did::from(peer)]);
    let subject = Subject::Patch(ObjectId::from_str(ISSUE).expect("patch should be valid"));
    let repository = RepoId::from_str(REPOSITORY).expect("repository should be valid");
    let mut context = Context::default();

    for _ in 0..4 {
        let _own = context.classify(
            &Fixture::operation(
                ISSUE,
                PatchAction::Revision {
                    description: "Another revision".to_owned(),
                    base: *ObjectId::from_str(ISSUE).expect("base should be valid"),
                    oid: *ObjectId::from_str(COMMENT).expect("head should be valid"),
                    resolves: BTreeSet::new(),
                },
                member,
                PATCH_TYPENAME.clone(),
            ),
            Member::Nico,
            member,
            repository,
            subject,
            &triggers,
        );
    }

    assert!(
        !context
            .classify(
                &Fixture::operation(
                    OPERATION,
                    PatchAction::Revision {
                        description: "@nico could you rebase this?".to_owned(),
                        base: *ObjectId::from_str(ISSUE).expect("base should be valid"),
                        oid: *ObjectId::from_str(COMMENT).expect("head should be valid"),
                        resolves: BTreeSet::new(),
                    },
                    peer,
                    PATCH_TYPENAME.clone(),
                ),
                Member::Nico,
                member,
                repository,
                subject,
                &triggers,
            )
            .is_empty(),
        "publishing revisions is work, not speech, so it must not exhaust the allowance a member \
         needs to keep hearing about its own patch"
    );
}
