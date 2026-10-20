//! Assignments selected before compaction own its failure; later arrivals do not.

use super::*;
use codex_core::config::Config;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_login::CodexAuth;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;
use codex_thread_store::AgentMailEnvelope;
use codex_thread_store::AgentMailboxStore;
use codex_thread_store::AgentTaskFilter;
use codex_thread_store::AgentTaskRecord;
use codex_thread_store::AgentTaskStatus;
use codex_thread_store::LocalAgentMailboxStore;
use core_test_support::ThreadIdle;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use test_case::test_case;
use wiremock::Mock;
use wiremock::Request;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[derive(Default)]
struct CompactionFailureGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl codex_extension_api::TurnLifecycleContributor for CompactionFailureGate {
    fn on_turn_error<'a>(
        &'a self,
        input: codex_extension_api::TurnErrorInput<'a>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if input
                .error_details
                .to_string()
                .contains("fixture compaction failed")
            {
                self.entered.notify_one();
                self.release.notified().await;
            }
        })
    }
}

async fn accept_assignment(
    store: &LocalAgentMailboxStore,
    root: ThreadId,
    target: ThreadId,
    name: &str,
) -> Result<InterAgentCommunication> {
    let message_id = codex_protocol::ResponseItemId::new("agentmail");
    let mut mail = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::root().join("worker").expect("worker path"),
        Vec::new(),
        name.into(),
        /*trigger_turn*/ true,
    );
    mail.id = Some(message_id.clone());
    mail.mail = Some(AgentMailMetadata {
        message_id: message_id.to_string(),
        task_id: Some(name.into()),
        kind: AgentMailKind::Assignment,
        wake: AgentMailWake::Auto,
    });
    store
        .accept_task(
            AgentTaskRecord {
                task_id: name.into(),
                root_id: root,
                owner_id: root,
                target_id: target,
                name: Some("/root/worker".into()),
                status: AgentTaskStatus::Queued,
                turn_id: None,
                result_ref: None,
            },
            AgentMailEnvelope {
                message_id: message_id.to_string(),
                root_id: root,
                sender_id: root,
                recipient_id: target,
                kind: serde_json::to_string(&AgentMailKind::Assignment)?,
                payload_json: serde_json::to_string(&mail)?,
                wake: true,
                task_id: Some(name.into()),
            },
        )
        .await?;
    Ok(mail)
}

#[test_case(false; "failed_selection_does_not_retry")]
#[test_case(true; "late_assignment_gets_its_own_turn")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_sampling_compaction_failure_settles_only_selected_assignment(
    late_assignment: bool,
) -> Result<()> {
    let server = responses::start_mock_server().await;
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let requests = Arc::clone(&captured);
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(move |request: &Request| {
            let bytes = if request
                .headers
                .get("content-encoding")
                .is_some_and(|value| value == "zstd")
            {
                zstd::stream::decode_all(std::io::Cursor::new(&request.body))
                    .expect("decompress model request")
            } else {
                request.body.clone()
            };
            let body: Value = serde_json::from_slice(&bytes).expect("parse model request JSON");
            let (response_id, compact_count) = {
                let mut requests = requests.lock().expect("lock captured model requests");
                requests.push(body.clone());
                (
                    format!("presampling-{}", requests.len()),
                    requests
                        .iter()
                        .filter(|request| request["input"].to_string().contains("compaction_trigger"))
                        .count(),
                )
            };
            let text = body["input"].to_string();
            let event = if text.contains("presampling-root") {
                if text.contains("function_call_output") {
                    responses::ev_assistant_message("root-resting", "Root resting")
                } else {
                    responses::ev_function_call_with_namespace(
                        "spawn-worker",
                        "collaboration",
                        "spawn_agent",
                        &json!({"task_name":"worker", "message":"presampling-child", "fork_turns":"none"}).to_string(),
                    )
                }
            } else if text.contains("compaction_trigger") {
                if compact_count == 1 {
                    return ResponseTemplate::new(400).set_body_json(json!({
                        "error": {"message":"fixture compaction failed", "type":"invalid_request_error"}
                    }));
                }
                json!({"type":"response.output_item.done", "item":{"type":"compaction", "encrypted_content":"SUMMARY"}})
            } else if text.contains("late-during-compaction-error") {
                responses::ev_assistant_message("late-completed", "Late assignment completed")
            } else {
                responses::ev_assistant_message("initial-child", "Initial child completed")
            };
            let tokens = if text.contains("presampling-child")
                && !text.contains("presampling-root")
                && !text.contains("compaction_trigger")
                && !text.contains("late-during-compaction-error")
            {
                150_000
            } else {
                100
            };
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created(&response_id),
                event,
                responses::ev_completed_with_tokens(&response_id, tokens),
            ]))
        })
        .mount(&server)
        .await;

    let gate = Arc::new(CompactionFailureGate::default());
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    extensions.turn_lifecycle_contributor(gate.clone());
    extensions.thread_lifecycle_contributor(Arc::new(ThreadIdle));
    let test = test_codex()
        .with_model("gpt-5.6-sol")
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_extensions(Arc::new(extensions.build()))
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("enable collaboration");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("enable native multi-agent tools");
            config
                .features
                .enable(Feature::WakeOnAgentMessages)
                .expect("enable automatic agent mail turns");
            config
                .features
                .disable(Feature::AsyncCompaction)
                .expect("disable background compaction");
            config.model_auto_compact_token_limit = Some(100_000);
            config.model_post_turn_compact_threshold_percent = 0;
        })
        .build_with_auto_env(&server)
        .await?;
    let mut created = test.thread_manager.subscribe_thread_created();
    test.submit_text_turn("presampling-root").await?;
    let child_id = created.recv().await?;
    let child = test.thread_manager.get_thread(child_id).await?;
    wait_for_event(&child, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    ThreadIdle::wait(&child).await;

    let root = test.session_configured.thread_id;
    let store = LocalAgentMailboxStore::new(
        codex_core::init_state_db(&test.config)
            .await
            .expect("state database"),
    );
    let selected = accept_assignment(&store, root, child_id, "selected-before-compaction").await?;
    child
        .submit(Op::InterAgentCommunication {
            communication: selected.clone(),
            start_options: Default::default(),
        })
        .await?;
    tokio::time::timeout(Duration::from_secs(10), gate.entered.notified()).await?;

    // The failing compaction precedes the sampling request containing this task.
    let selected_record = store
        .pending(root, child_id)
        .await?
        .into_iter()
        .find(|mail| mail.envelope.task_id.as_deref() == Some("selected-before-compaction"))
        .expect("unsubmitted selection");
    assert!(selected_record.recorded_turn_id.is_some());
    assert_eq!(selected_record.submitted_turn_id, None);
    if late_assignment {
        let late =
            accept_assignment(&store, root, child_id, "late-during-compaction-error").await?;
        child
            .submit(Op::InterAgentCommunication {
                communication: late,
                start_options: Default::default(),
            })
            .await?;
        // The settings acknowledgement orders after the mail op, proving the
        // late arrival reached the active turn before error cleanup resumes.
        child
            .update_thread_settings(ThreadSettingsOverrides::default())
            .await?;
    }
    gate.release.notify_one();
    wait_for_event(
        &child,
        |event| matches!(event, EventMsg::TurnComplete(complete) if complete.error.is_some()),
    )
    .await;
    if late_assignment {
        // A queued assignment suppresses the intermediate idle notification.
        // Wait for both turn outcomes before observing the final idle boundary.
        wait_for_event(&child, |event| matches!(event, EventMsg::TurnComplete(complete) if complete.last_agent_message.as_deref() == Some("Late assignment completed"))).await;
        ThreadIdle::wait(&child).await;
    } else {
        ThreadIdle::wait(&child).await;
        assert!(matches!(
            child.agent_status().await,
            AgentStatus::Errored(_)
        ));
        assert_eq!(
            child
                .agent_work()
                .await?
                .expect("work summary")
                .pending_mail,
            0
        );
    }

    let tasks = store
        .tasks(
            root,
            AgentTaskFilter {
                task_ids: vec![
                    "selected-before-compaction".into(),
                    "late-during-compaction-error".into(),
                ],
                ..Default::default()
            },
        )
        .await?
        .tasks;
    let selected_task = tasks
        .iter()
        .find(|task| task.task_id == "selected-before-compaction")
        .expect("selected task");
    assert_eq!(selected_task.status, AgentTaskStatus::Failed);
    assert_eq!(selected_task.turn_id, selected_record.recorded_turn_id);
    assert!(selected_task.result_ref.is_some());
    if late_assignment {
        assert_eq!(tasks.len(), 2);
        let late = tasks
            .iter()
            .find(|task| task.task_id == "late-during-compaction-error")
            .expect("late task");
        assert_eq!(late.status, AgentTaskStatus::Completed);
        assert_ne!(late.turn_id, selected_task.turn_id);
    } else {
        assert_eq!(tasks.len(), 1);
        assert_eq!(store.pending(root, child_id).await?, vec![selected_record]);
    }

    test.thread_manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    let requests = captured.lock().expect("lock final model requests");
    let child_requests = requests
        .iter()
        .filter(|request| !request["input"].to_string().contains("presampling-root"))
        .collect::<Vec<_>>();
    assert_eq!(child_requests.len(), if late_assignment { 4 } else { 2 });
    assert!(
        child_requests[1]["input"]
            .to_string()
            .contains("compaction_trigger")
    );
    assert!(
        !child_requests[1]["input"]
            .to_string()
            .contains("selected-before-compaction")
    );
    Ok(())
}
