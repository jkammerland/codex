use super::*;

#[test]
fn explicit_same_value_and_independent_effort_keep_ownership() {
    let mut policy = TeamModelPolicy {
        model_origin: TeamSelectionOrigin::Inherited,
        effort_origin: TeamSelectionOrigin::Role,
        selection: None,
        superseded_revision: None,
    };
    policy
        .apply(&TeamModelSelection {
            revision: 1,
            model: TeamSelection::Value {
                value: "parent-model".into(),
            },
            effort: TeamSelection::Keep,
        })
        .unwrap();
    assert_eq!(policy.model_origin, TeamSelectionOrigin::Explicit);
    assert_eq!(policy.effort_origin, TeamSelectionOrigin::Role);
}

#[test]
fn selection_replay_is_idempotent_but_conflicting_revision_is_rejected() {
    let mut policy = TeamModelPolicy::default();
    let selection = TeamModelSelection {
        revision: 2,
        model: TeamSelection::Inherit,
        effort: TeamSelection::Default,
    };
    policy.apply(&selection).unwrap();
    let accepted = policy.clone();
    policy.apply(&selection).unwrap();
    assert_eq!(policy, accepted);

    let mut conflicting = selection.clone();
    conflicting.effort = TeamSelection::Value {
        value: "high".into(),
    };
    assert_eq!(
        policy.apply(&conflicting),
        Err("model selection revision was reused with different policy")
    );
    conflicting.revision = 1;
    assert_eq!(
        policy.apply(&conflicting),
        Err("model selection revision is stale")
    );
    assert_eq!(policy, accepted);
}
