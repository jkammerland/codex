use super::*;
use crate::app::agents_hosts::ConnectionScope;
use crate::app::agents_hosts::HostKey;
use crate::app::agents_hosts::ThreadTarget;
use crate::app::agents_hosts_models::ModelAction;
use crate::app::agents_hosts_models::ModelPickerContext;

fn context(preset: ModelPreset) -> ModelPickerContext {
    let target = ThreadTarget {
        host: HostKey {
            machine_id: "laptop".into(),
            server_id: "primary".into(),
        },
        thread_id: ThreadId::new(),
    };
    ModelPickerContext {
        request_id: uuid::Uuid::new_v4(),
        scope: ConnectionScope {
            host: target.host.clone(),
            epoch: 3,
        },
        target,
        generation: 4,
        current_model: preset.model.clone(),
        current_effort: None,
        origins: Some((
            codex_app_server_protocol::TeamSelectionOrigin::Explicit,
            codex_app_server_protocol::TeamSelectionOrigin::Default,
        )),
        presets: vec![preset],
    }
}

#[tokio::test]
async fn pending_target_picker_can_be_cancelled_without_a_late_reopen() {
    let (mut chat, mut events, _op_rx) = make_chatwidget_manual(Some("gpt-5.5")).await;
    let request_id = uuid::Uuid::new_v4();
    chat.begin_agents_model_popup(request_id);
    assert_chatwidget_snapshot!(
        "agents_target_model_loading",
        render_bottom_popup(&chat, /*width*/ 80)
    );
    chat.handle_key_event(KeyEvent::from(KeyCode::Esc));
    assert!(!chat.finish_agents_model_loading(request_id));
    assert!(chat.no_modal_or_popup_active());
    assert!(
        matches!(events.try_recv().expect("cancel loading"), AppEvent::AgentsHostModel(action) if matches!(*action, ModelAction::CancelRequest(id) if id == request_id))
    );

    // A superseded response cannot dismiss or replace the newer request.
    let newer = uuid::Uuid::new_v4();
    chat.begin_agents_model_popup(newer);
    assert!(!chat.finish_agents_model_loading(request_id));
    assert!(!chat.no_modal_or_popup_active());
    assert!(chat.finish_agents_model_loading(newer));
}

#[tokio::test]
async fn preempted_target_picker_does_not_replace_a_later_modal() {
    let (mut chat, _events, _op_rx) = make_chatwidget_manual(Some("gpt-5.5")).await;
    let request_id = uuid::Uuid::new_v4();
    chat.begin_agents_model_popup(request_id);
    chat.show_selection_view(SelectionViewParams {
        view_id: Some("later-modal"),
        title: Some("Later choice".into()),
        ..Default::default()
    });
    assert!(!chat.finish_agents_model_loading(request_id));
    assert_eq!(chat.bottom_pane.active_view_id(), Some("later-modal"));
}

#[tokio::test]
async fn empty_or_hidden_target_catalog_leaves_no_model_target_armed() {
    let (mut chat, _events, _op_rx) = make_chatwidget_manual(Some("gpt-5.5")).await;
    let preset = get_available_model(&chat, "gpt-5.5");
    for hidden in [false, true] {
        let mut target = context(preset.clone());
        if hidden {
            target.presets[0].show_in_picker = false;
        } else {
            target.presets.clear();
        }
        chat.open_agents_model_popup(target);
        assert!(chat.agents_model_picker.is_none());
        assert!(chat.no_modal_or_popup_active());
    }
}

#[tokio::test]
async fn unlisted_target_model_label_does_not_borrow_the_foreground_selection() {
    let (mut chat, _events, _op_rx) = make_chatwidget_manual(Some("gpt-5.5")).await;
    let preset = get_available_model(&chat, "gpt-5.5");
    let mut target = context(preset.clone());
    target.current_model = "owning-machine-custom-model".into();
    let mut auto = preset;
    auto.model = "codex-auto-test".into();
    target.presets.push(auto);
    chat.open_agents_model_popup(target);
    // Keep the long description on one line so this checks target identity
    // independently of the shared list's wrapping and truncation behavior.
    let rendered = render_bottom_popup(&chat, /*width*/ 160);
    assert!(
        rendered.contains("current: owning-machine-custom-model"),
        "{rendered}"
    );
    assert!(!rendered.contains("current: gpt-5.5"), "{rendered}");
}

#[tokio::test]
async fn target_picker_uses_owning_catalog_and_emits_one_task_only_change() {
    let (mut chat, mut events, _op_rx) = make_chatwidget_manual(Some("gpt-5.5")).await;
    let mut preset = get_available_model(&chat, "gpt-5.5");
    preset.model = "host-specific-model".into();
    preset.display_name = "Laptop model".into();
    preset.supported_reasoning_efforts.clear();
    let target = context(preset.clone());
    chat.open_agents_model_popup(target.clone());
    assert_chatwidget_snapshot!(
        "agents_target_model_picker",
        render_bottom_popup(&chat, /*width*/ 80)
    );
    chat.handle_key_event(KeyEvent::from(KeyCode::Enter));
    let popup = events.try_recv().expect("model effort action");
    let AppEvent::OpenReasoningPopup { model } = popup else {
        panic!("expected existing reasoning view");
    };
    chat.open_reasoning_popup(model);
    match events.try_recv().expect("task model action") {
        AppEvent::AgentsHostModel(action) => match *action {
            ModelAction::Apply {
                context,
                model,
                effort,
            } => {
                assert_eq!(context.target, target.target);
                assert_eq!(context.scope, target.scope);
                assert_eq!(model, "host-specific-model");
                assert_eq!(effort, None);
            }
            other => panic!("unexpected model action: {other:?}"),
        },
        other => panic!("unexpected model event: {other:?}"),
    }
    assert!(
        events.try_recv().is_err(),
        "task choice must not save defaults or change the foreground"
    );
    assert_eq!(chat.current_model(), "gpt-5.5");
}

#[tokio::test]
async fn cancelled_target_picker_cannot_redirect_later_foreground_choices() {
    let (mut chat, mut events, _op_rx) = make_chatwidget_manual(Some("gpt-5.5")).await;
    let target = context(get_available_model(&chat, "gpt-5.5"));
    chat.open_agents_model_popup(target);
    chat.handle_key_event(KeyEvent::from(KeyCode::Esc));
    match events.try_recv().expect("target cancellation") {
        AppEvent::AgentsHostModel(action) => match *action {
            ModelAction::Dismiss(context) => chat.clear_closed_agents_model_popup(&context),
            other => panic!("unexpected model action: {other:?}"),
        },
        other => panic!("unexpected model event: {other:?}"),
    }
    assert!(chat.agents_model_picker.is_none());
    assert!(chat.no_modal_or_popup_active());
}
