//! Team admission and durable participation also cover speculative requests.

use std::sync::Mutex;

use super::*;
use codex_extension_api::ExecutionControl;
use codex_extension_api::ExecutionMode;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::TurnLifecycleContributor;
use codex_extension_api::TurnModelRequestInput;
use codex_login::CodexAuth;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use tokio::sync::oneshot;

struct GatedJournal {
    input: Mutex<Vec<ResponseItem>>,
    gate: Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
    fail: bool,
}

impl TurnLifecycleContributor for GatedJournal {
    fn on_model_request<'a>(
        &'a self,
        input: TurnModelRequestInput<'a>,
    ) -> ExtensionFuture<'a, Result<(), String>> {
        Box::pin(async move {
            *self.input.lock().expect("journal input") = input.input.to_vec();
            let (entered, release) = self
                .gate
                .lock()
                .expect("journal gate")
                .take()
                .expect("one request");
            entered.send(()).expect("waiting for journal");
            release.await.expect("release journal");
            if self.fail {
                Err("durable journal unavailable".into())
            } else {
                Ok(())
            }
        })
    }
}

#[test_case::test_case(false; "recorded before dispatch")]
#[test_case::test_case(true; "journal error prevents dispatch")]
#[tokio::test]
async fn background_compaction_waits_for_admission_and_durable_selected_input(fail: bool) {
    let server = responses::start_mock_server().await;
    let mut provider =
        codex_model_provider_info::built_in_model_providers(/*openai_base_url*/ None)["openai"]
            .clone();
    provider.base_url = Some(format!("{}/v1", server.uri()));
    provider.supports_websockets = false;
    let (mut sess, turn, _events) =
        crate::session::tests::make_session_and_context_with_auth_and_config_and_rx(
            CodexAuth::create_dummy_chatgpt_auth_for_testing(),
            Vec::new(),
            move |config| {
                config.model = Some("gpt-5.2".into());
                config.model_provider = provider;
            },
        )
        .await;
    let response = responses::mount_sse_once(&server, responses::sse(vec![
        serde_json::json!({"type":"response.output_item.done", "item": {"type":"compaction", "encrypted_content":"prepared-summary"}}),
        responses::ev_completed("prepared"),
    ])).await;
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let journal = Arc::new(GatedJournal {
        input: Mutex::new(Vec::new()),
        gate: Mutex::new(Some((entered_tx, release_rx))),
        fail,
    });
    let control = ExecutionControl::default();
    control.apply(sess.thread_id, 1, ExecutionMode::Paused);
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.execution_control(control.clone());
    extensions.turn_lifecycle_contributor(journal.clone());
    Arc::get_mut(&mut sess)
        .expect("unique session")
        .services
        .extensions = Arc::new(extensions.build());
    let step = sess
        .capture_step_context(turn, &CancellationToken::new())
        .await
        .expect("capture step");
    let selected: ResponseItem = serde_json::from_value(serde_json::json!({
        "type":"message", "id":"msg_team-selected", "role":"user", "content":[{"type":"input_text", "text":"selected team delivery"}]
    })).expect("selected message");
    let expected = vec![selected.clone()];
    let thread_id = sess.thread_id;
    let mut request = Box::pin(async move {
        let prompt = Prompt {
            input: vec![selected],
            ..Default::default()
        };
        let metadata = CodexResponsesMetadata::new(
            "installation".into(),
            "session".into(),
            thread_id.to_string(),
            "window".into(),
        );
        run_remote_compaction_request_v2(
            &sess,
            &step,
            &prompt,
            &metadata,
            CompactionRequestTransport::Background(
                None,
                sess.services.model_client.auth_owner_generation(),
            ),
        )
        .await
    });

    // Poll the actual request to its admission boundary while Pause is active.
    assert!(futures::poll!(request.as_mut()).is_pending());
    assert!(journal.input.lock().expect("journal input").is_empty());
    assert!(response.requests().is_empty());
    control.apply(thread_id, 2, ExecutionMode::Running);
    let request = tokio::spawn(request);
    entered_rx
        .await
        .expect("request reaches journal after Resume");
    assert_eq!(*journal.input.lock().expect("journal input"), expected);
    assert!(
        response.requests().is_empty(),
        "provider dispatch must wait for journal acknowledgement"
    );
    release_tx.send(()).expect("journal is waiting");
    let result = request.await.expect("request task");
    if fail {
        assert!(
            matches!(result, Err(error) if error.to_string().contains("durable journal unavailable"))
        );
        assert!(response.requests().is_empty());
    } else {
        assert_eq!(result.expect("compaction").response_id, "prepared");
        // HTTP prepends base instructions. Verify the independently expected
        // user message; the journal assertion above retains its delivery marker.
        let sent = response.single_request().body_json()["input"].clone();
        let users = sent
            .as_array()
            .expect("request input")
            .iter()
            .filter(|item| item["role"] == "user")
            .collect::<Vec<_>>();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0]["type"], "message");
        assert_eq!(
            users[0]["content"],
            serde_json::json!([{ "type":"input_text", "text":"selected team delivery" }])
        );
    }
    assert_eq!(
        control.snapshot(thread_id).expect("enrollment").in_flight,
        0
    );
}
