use super::*;
use pretty_assertions::assert_eq;

fn agent(
    id: &str,
    parent: Option<&str>,
    model_origin: TeamSelectionOrigin,
    effort_origin: TeamSelectionOrigin,
) -> Agent {
    Agent {
        discovery: Default::default(),
        enrollment: TeamEnrollment {
            address: TeamAddress {
                machine_id: "worker".into(),
                server_id: "primary".into(),
                thread_id: id.into(),
            },
            alias: id.into(),
            manually_held: false,
            continuation_turn_id: None,
        },
        runtime: Some(TeamAgentRuntime {
            activity: Default::default(),
            environments: Vec::new(),
            thread_id: id.into(),
            root_thread_id: "parent".into(),
            parent_thread_id: parent.map(str::to_owned),
            alias: id.into(),
            loaded: true,
            archived: false,
            turn_id: None,
            current_model: None,
            current_effort: None,
            next_model: Some("model".into()),
            next_effort: Some("medium".into()),
            model_error: None,
            model_origin,
            effort_origin,
            requested_selection_revision: None,
            applied_selection_revision: None,
            superseded_selection_revision: None,
            in_flight: 0,
            reconciling: false,
            manually_held: false,
            continuation_turn_id: None,
            jobs: Vec::new(),
        }),
        status: "idle".into(),
        project: None,
        current_model: None,
        current_effort: None,
        next_model: Some("model".into()),
        next_effort: Some("medium".into()),
        requested_selection: None,
    }
}

#[test]
fn inherited_effort_can_clear_a_child_resolved_default_without_replacing_its_role_model()
-> Result<()> {
    let mut parent = agent(
        "parent",
        None,
        TeamSelectionOrigin::Explicit,
        TeamSelectionOrigin::Configured,
    );
    parent.next_effort = None;
    let child = agent(
        "child",
        Some("parent"),
        TeamSelectionOrigin::Role,
        TeamSelectionOrigin::Inherited,
    );
    let (model, effort) = inherited_fields(child.runtime.as_ref().unwrap());
    assert_eq!(
        (&model, &effort),
        (&TeamSelection::Keep, &TeamSelection::Inherit)
    );
    let desired = resolve_selection(&child, &[parent, child.clone()], model, effort)?;
    assert_eq!(
        desired,
        RequestedSelection {
            selection: TeamModelSelection {
                revision: 0,
                model: TeamSelection::Keep,
                effort: TeamSelection::Inherit
            },
            model: None,
            effort: None,
        }
    );
    assert!(requires_update(&child, &desired));
    Ok(())
}

#[test]
fn explicit_same_value_choices_are_preserved_when_parent_changes() -> Result<()> {
    let mut parent = agent(
        "parent",
        None,
        TeamSelectionOrigin::Explicit,
        TeamSelectionOrigin::Explicit,
    );
    let child = agent(
        "child",
        Some("parent"),
        TeamSelectionOrigin::Explicit,
        TeamSelectionOrigin::Role,
    );
    parent.next_model = Some("other-model".into());
    parent.next_effort = Some("high".into());
    let (model, effort) = inherited_fields(child.runtime.as_ref().unwrap());
    assert_eq!(
        (&model, &effort),
        (&TeamSelection::Keep, &TeamSelection::Keep)
    );
    let desired = resolve_selection(&child, &[parent, child.clone()], model, effort)?;
    assert!(!requires_update(&child, &desired));
    assert_eq!((desired.model, desired.effort), (None, None));
    Ok(())
}

#[test]
fn inheritance_compares_fields_independently_and_never_invents_effort() -> Result<()> {
    let mut parent = agent(
        "parent",
        None,
        TeamSelectionOrigin::Explicit,
        TeamSelectionOrigin::Explicit,
    );
    parent.next_effort = None;
    let child = agent(
        "child",
        Some("parent"),
        TeamSelectionOrigin::Inherited,
        TeamSelectionOrigin::Explicit,
    );
    let (model, effort) = inherited_fields(child.runtime.as_ref().unwrap());
    assert_eq!(
        (&model, &effort),
        (&TeamSelection::Inherit, &TeamSelection::Keep)
    );
    let desired = resolve_selection(&child, &[parent, child.clone()], model, effort)?;
    assert!(!requires_update(&child, &desired));
    assert_eq!(desired.effort, None);
    Ok(())
}

#[test]
fn a_model_without_effort_choices_keeps_default_effort_unset() -> Result<()> {
    let catalog: Model = serde_json::from_value(serde_json::json!({
        "id":"custom", "model":"custom", "displayName":"Custom", "description":"Custom provider model",
        "hidden":false, "supportedReasoningEfforts":[], "defaultReasoningEffort":"medium", "isDefault":true
    }))?;
    let agent = agent(
        "root",
        None,
        TeamSelectionOrigin::Configured,
        TeamSelectionOrigin::Configured,
    );
    let model = TeamSelection::Value {
        value: "custom".into(),
    };
    validate_selection(&agent, &[catalog], &model, &TeamSelection::Default)?;
    let desired = resolve_selection(&agent, &[], model, TeamSelection::Default)?;
    assert_eq!(desired.effort, None);
    assert_eq!(desired.selection.effort, TeamSelection::Default);
    Ok(())
}

#[test]
fn an_applied_inherited_default_is_not_requeued_for_a_hosts_resolved_effort() -> Result<()> {
    let mut parent = agent(
        "parent",
        None,
        TeamSelectionOrigin::Explicit,
        TeamSelectionOrigin::Configured,
    );
    parent.next_effort = None;
    let child = agent(
        "child",
        Some("parent"),
        TeamSelectionOrigin::Role,
        TeamSelectionOrigin::Inherited,
    );
    let desired = resolve_selection(
        &child,
        &[parent, child.clone()],
        TeamSelection::Keep,
        TeamSelection::Inherit,
    )?;
    let previous = RequestedSelection {
        selection: TeamModelSelection {
            revision: 7,
            ..desired.selection.clone()
        },
        ..desired.clone()
    };
    assert!(requires_update(&child, &desired));
    assert!(same_inherited_intent(&previous, &desired));
    Ok(())
}
