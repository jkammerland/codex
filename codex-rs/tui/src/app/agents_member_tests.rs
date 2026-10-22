use super::*;
use crossterm::event::KeyCode;
use pretty_assertions::assert_eq;

fn draft() -> MemberDraft {
    MemberDraft {
        request: TeamMemberCreateRequest {
            machine_id: "windows".into(),
            server_id: "primary".into(),
            alias: "Builder".into(),
            cwd: r"C:\repos\project".into(),
            model: None,
            effort: None,
        },
        host_label: "Windows / primary".into(),
    }
}

fn model() -> Model {
    serde_json::from_value(serde_json::json!({
        "id": "gpt-6.1-sol", "model": "gpt-6.1-sol", "displayName": "GPT 6.1 Sol",
        "description": "Workhorse model", "hidden": false,
        "supportedReasoningEfforts": [
            {"reasoningEffort": "medium", "description": "Balanced effort"},
            {"reasoningEffort": "ultra", "description": "Maximum effort"}
        ],
        "defaultReasoningEffort": "medium", "isDefault": true
    }))
    .unwrap()
}

#[tokio::test]
async fn empty_team_menu_offers_creation_and_enrollment() {
    let (mut app, mut events, _) = crate::app::tests::make_test_app_with_channels().await;
    app.open_agents_team_general_menu();
    insta::assert_snapshot!(
        "empty_team_member_actions",
        crate::chatwidget::tests::helpers::render_bottom_popup(&app.chat_widget, 52)
    );
    app.chat_widget.handle_key_event(KeyCode::Enter.into());
    assert!(
        matches!(events.try_recv().unwrap(), AppEvent::AgentsMember(action) if matches!(*action, MemberAction::Open))
    );
    assert!(app.agents_hosts.team_operations.is_empty());
}

#[tokio::test]
async fn empty_machine_list_explains_configuration_without_starting_creation() {
    let (mut app, mut events, _) = crate::app::tests::make_test_app_with_channels().await;
    app.handle_agents_member(MemberAction::Open);
    insta::assert_snapshot!(
        "member_empty_machine_list",
        crate::chatwidget::tests::helpers::render_bottom_popup(&app.chat_widget, 52)
    );
    app.chat_widget.handle_key_event(KeyCode::Enter.into());
    assert!(events.try_recv().is_err());
    assert!(app.agents_hosts.team_operations.is_empty());
    assert!(app.chat_widget.no_modal_or_popup_active());
}

#[tokio::test]
async fn member_catalog_selection_retains_host_model_and_ultra_effort() {
    let (mut app, mut events, _) = crate::app::tests::make_test_app_with_channels().await;
    let mut picker = member_models(draft(), vec![model()]);
    (picker.items.remove(1).actions.remove(0))(&app.app_event_tx);
    let AppEvent::AgentsMember(action) = events.try_recv().unwrap() else {
        panic!("expected member model selection")
    };
    app.handle_agents_member(*action);
    app.chat_widget.handle_key_event(KeyCode::End.into());
    app.chat_widget.handle_key_event(KeyCode::Enter.into());
    let AppEvent::AgentsMember(action) = events.try_recv().unwrap() else {
        panic!("expected effort selection")
    };
    let MemberAction::Confirm(selected) = *action else {
        panic!("expected member confirmation")
    };
    assert_eq!(selected.request.machine_id, "windows");
    assert_eq!(selected.request.cwd, r"C:\repos\project");
    assert_eq!(selected.request.model.as_deref(), Some("gpt-6.1-sol"));
    assert_eq!(selected.request.effort.as_deref(), Some("ultra"));
    assert!(app.agents_hosts.team_operations.is_empty());
    app.handle_agents_member(MemberAction::Confirm(selected));
    insta::assert_snapshot!(
        "member_confirmation_narrow",
        crate::chatwidget::tests::helpers::render_bottom_popup(&app.chat_widget, 46)
    );
    app.chat_widget.handle_key_event(KeyCode::Esc.into());
    assert!(app.agents_hosts.team_operations.is_empty());
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn model_default_effort_is_explicit_instead_of_inheriting_host_configuration() {
    let (mut app, mut events, _) = crate::app::tests::make_test_app_with_channels().await;
    app.handle_agents_member(MemberAction::Model {
        draft: draft(),
        model: Box::new(model()),
    });
    app.chat_widget.handle_key_event(KeyCode::Enter.into());

    let AppEvent::AgentsMember(action) = events.try_recv().unwrap() else {
        panic!("expected model default effort selection")
    };
    let MemberAction::Confirm(selected) = *action else {
        panic!("expected member confirmation")
    };
    assert_eq!(selected.request.model.as_deref(), Some("gpt-6.1-sol"));
    assert_eq!(selected.request.effort.as_deref(), Some("medium"));
}

#[tokio::test]
async fn cancelled_catalog_does_not_reopen_or_submit_creation() {
    let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let id = Uuid::new_v4();
    app.agents_hosts.member_catalog_request = Some(id);
    app.handle_agents_member(MemberAction::CancelCatalog(id));
    app.handle_agents_member(MemberAction::ModelsLoaded {
        request_id: id,
        draft: draft(),
        result: Ok(vec![model()]),
    });
    assert!(app.chat_widget.no_modal_or_popup_active());
    assert!(app.agents_hosts.team_operations.is_empty());
}

#[tokio::test]
async fn unavailable_catalog_shows_retry_without_submitting_defaults() {
    let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let id = Uuid::new_v4();
    app.agents_hosts.member_catalog_request = Some(id);
    app.chat_widget.show_selection_view(SelectionViewParams {
        view_id: Some(CATALOG_VIEW),
        ..SelectionViewParams::picker()
    });
    app.handle_agents_member(MemberAction::ModelsLoaded {
        request_id: id,
        draft: draft(),
        result: Err("The selected machine disconnected".into()),
    });
    insta::assert_snapshot!(
        "member_catalog_unavailable",
        crate::chatwidget::tests::helpers::render_bottom_popup(&app.chat_widget, 60)
    );
    assert!(app.agents_hosts.team_operations.is_empty());
}
