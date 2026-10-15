use super::*;
use codex_app_server_protocol::TeamEnrollment;
use pretty_assertions::assert_eq;

fn parent() -> Agent {
    Agent {
        enrollment: TeamEnrollment {
            address: TeamAddress {
                machine_id: "worker".into(),
                server_id: "primary".into(),
                thread_id: "parent".into(),
            },
            alias: "builder".into(),
            manually_held: false,
            continuation_turn_id: None,
        },
        runtime: None,
        status: "working".into(),
        project: None,
        current_model: Some("current-model".into()),
        current_effort: Some("medium".into()),
        next_model: Some("next-model".into()),
        next_effort: Some("high".into()),
    }
}

#[test]
fn a_child_can_inherit_the_running_parent_before_next_turn_settings_apply() {
    let parent = parent();
    let child = Selection::current(&parent);
    let binding = reconcile_inheritance(&parent, &child, None).expect("inherited selection");
    assert_eq!(binding.applied, child);
    assert_ne!(binding.applied, Selection::next(&parent));
    assert_eq!(binding.pending, None);
}

#[test]
fn a_configured_role_and_a_later_local_override_remain_user_owned() {
    let parent = parent();
    let role = Selection {
        model: Some("review-model".into()),
        effort: Some("low".into()),
    };
    assert!(reconcile_inheritance(&parent, &role, None).is_none());
    let previous = Inheritance {
        parent: parent.enrollment.address.clone(),
        applied: Selection::current(&parent),
        pending: Some(Selection::next(&parent)),
    };
    assert!(reconcile_inheritance(&parent, &role, Some(previous)).is_none());
}

#[test]
fn lost_setting_acknowledgement_does_not_turn_inheritance_into_an_override() {
    let parent = parent();
    let previous = Inheritance {
        parent: parent.enrollment.address.clone(),
        applied: Selection::current(&parent),
        pending: Some(Selection::next(&parent)),
    };
    let binding =
        reconcile_inheritance(&parent, &Selection::next(&parent), Some(previous)).unwrap();
    assert_eq!(binding.applied, Selection::next(&parent));
    assert_eq!(binding.pending, None);
    let resumed = reconcile_inheritance(&parent, &binding.applied.clone(), Some(binding)).unwrap();
    assert_eq!(resumed.applied, Selection::next(&parent));
}
