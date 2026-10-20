//! Mail continuation retains current settings and consumes only submitted input.

use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_features::Feature;
use codex_protocol::AgentPath;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::protocol::AgentMailKind;
use codex_protocol::protocol::AgentMailMetadata;
use codex_protocol::protocol::AgentMailWake;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[path = "agent_mail_recovery_tests.rs"]
mod recovery;

#[path = "agent_mail_presampling_tests.rs"]
mod presampling;

#[test_case(ModeKind::Default; "default_mode")]
#[test_case(ModeKind::Plan; "plan_mode")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_mail_continues_with_current_mode_and_retains_queue_only_mail(
    mode: ModeKind,
) -> Result<()> {
    let server = responses::start_mock_server().await;
    let test = test_codex()
        .with_config(|config| {
            config
                .features
                .enable(Feature::WakeOnAgentMessages)
                .expect("enable WakeOnAgentMessages");
        })
        .build_with_auto_env(&server)
        .await?;
    core_test_support::submit_thread_settings(
        test.codex.as_ref(),
        ThreadSettingsOverrides {
            collaboration_mode: Some(CollaborationMode {
                mode,
                settings: Settings {
                    model: test.session_configured.model.clone(),
                    reasoning_effort: None,
                    developer_instructions: None,
                },
            }),
            ..Default::default()
        },
    )
    .await?;
    let initial = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("initial"),
            responses::ev_assistant_message("initial-answer", "Initial work complete"),
            responses::ev_completed("initial"),
        ]),
    )
    .await;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Initial task".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(initial.requests().len(), 1);

    // Both ops share the submission queue. The automatic request must include
    // the earlier quiet mail alongside the later waking message.
    let continuation = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("continuation"),
            responses::ev_assistant_message("continued-answer", "Both notes received"),
            responses::ev_completed("continuation"),
        ]),
    )
    .await;
    for (text, wake) in [
        ("quiet progress", AgentMailWake::QueueOnly),
        ("ready for review", AgentMailWake::Auto),
    ] {
        let mut mail = InterAgentCommunication::new(
            AgentPath::root().join("worker").expect("worker path"),
            AgentPath::root(),
            Vec::new(),
            text.into(),
            /*trigger_turn*/ false,
        );
        mail.mail = Some(AgentMailMetadata {
            message_id: codex_protocol::ResponseItemId::new("testmail").to_string(),
            task_id: None,
            kind: AgentMailKind::DirectMessage,
            wake,
        });
        mail.id = Some(codex_protocol::ResponseItemId::from_server(
            mail.mail
                .as_ref()
                .expect("typed mail metadata")
                .message_id
                .clone(),
        ));
        test.codex
            .submit(Op::InterAgentCommunication {
                communication: mail,
                start_options: Default::default(),
            })
            .await?;
    }
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    test.codex.shutdown_and_wait().await?;
    let request = continuation.single_request();
    let messages = request.inputs_of_type("agent_message");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["content"][0]["text"], "quiet progress");
    assert_eq!(messages[1]["content"][0]["text"], "ready for review");
    assert_eq!(
        test.codex.config_snapshot().await.collaboration_mode.mode,
        mode
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delayed_delivery_of_reconciled_mail_does_not_start_an_extra_request() -> Result<()> {
    use codex_core::config::Config;
    use codex_extension_api::ExtensionRegistryBuilder;
    use codex_thread_store::AgentMailEnvelope;
    use codex_thread_store::AgentMailboxStore;
    use codex_thread_store::LocalAgentMailboxStore;
    use core_test_support::ThreadIdle;
    use std::sync::Arc;

    let server = responses::start_mock_server().await;
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    extensions.thread_lifecycle_contributor(Arc::new(ThreadIdle));
    let test = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .with_config(|config| {
            config
                .features
                .enable(Feature::WakeOnAgentMessages)
                .expect("enable WakeOnAgentMessages");
        })
        .build_with_auto_env(&server)
        .await?;
    let responses = responses::mount_sse_sequence(
        &server,
        ["Consumed reconciled mail", "Consumed fresh mail"]
            .into_iter()
            .enumerate()
            .map(|(index, text)| {
                let id = format!("mail-{index}");
                responses::sse(vec![
                    responses::ev_response_created(&id),
                    responses::ev_assistant_message(&format!("answer-{index}"), text),
                    responses::ev_completed(&id),
                ])
            })
            .collect(),
    )
    .await;
    let root = test.session_configured.thread_id;
    let store = LocalAgentMailboxStore::new(
        codex_core::init_state_db(&test.config)
            .await
            .expect("state database"),
    );
    let mail = |text: &str| {
        let id = codex_protocol::ResponseItemId::new("agentmail");
        let mut communication = InterAgentCommunication::new(
            AgentPath::root().join("worker").expect("worker path"),
            AgentPath::root(),
            Vec::new(),
            text.to_string(),
            /*trigger_turn*/ false,
        );
        communication.id = Some(id.clone());
        communication.mail = Some(AgentMailMetadata {
            message_id: id.to_string(),
            task_id: None,
            kind: AgentMailKind::DirectMessage,
            wake: AgentMailWake::Auto,
        });
        communication
    };
    let envelope = |communication: &InterAgentCommunication| -> Result<AgentMailEnvelope> {
        Ok(AgentMailEnvelope {
            message_id: communication
                .mail
                .as_ref()
                .expect("typed mail metadata")
                .message_id
                .clone(),
            root_id: root,
            sender_id: root,
            recipient_id: root,
            kind: serde_json::to_string(&AgentMailKind::DirectMessage)?,
            payload_json: serde_json::to_string(communication)?,
            wake: true,
            task_id: None,
        })
    };

    // Acceptance precedes delivery. The running request reconciles M from the
    // durable store and receives Created before its delivery Op is released.
    let consumed = mail("message reconciled before delivery");
    store.accept(envelope(&consumed)?).await?;
    test.submit_text_turn("Read the accepted mail").await?;
    ThreadIdle::wait(&test.codex).await;
    assert_eq!(store.pending(root, root).await?, Vec::new());
    test.codex
        .submit(Op::InterAgentCommunication {
            communication: consumed,
            start_options: Default::default(),
        })
        .await?;
    core_test_support::submit_thread_settings(&test.codex, ThreadSettingsOverrides::default())
        .await?;

    // The acknowledgement follows delivery's complete scheduling decision.
    // Either a still-running duplicate turn or a completed extra request fails.
    assert!(
        test.codex
            .active_turn_environment_selections()
            .await
            .is_none()
    );
    assert_eq!(responses.requests().len(), 1);
    let fresh = mail("distinct authorized message");
    store.accept(envelope(&fresh)?).await?;
    test.codex
        .submit(Op::InterAgentCommunication {
            communication: fresh,
            start_options: Default::default(),
        })
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(complete) if complete.last_agent_message.as_deref() == Some("Consumed fresh mail"))
    })
    .await;
    ThreadIdle::wait(&test.codex).await;
    test.codex.shutdown_and_wait().await?;

    let requests = responses.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].inputs_of_type("agent_message").len(), 1);
    let messages = requests[1].inputs_of_type("agent_message");
    assert_eq!(messages.len(), 2);
    assert_eq!(
        messages[0]["content"][0]["text"],
        "message reconciled before delivery"
    );
    assert_eq!(
        messages[1]["content"][0]["text"],
        "distinct authorized message"
    );
    assert_eq!(store.pending(root, root).await?, Vec::new());
    Ok(())
}

#[derive(Default)]
struct LeafGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl codex_extension_api::ToolLifecycleContributor for LeafGate {
    fn on_tool_start<'a>(
        &'a self,
        input: codex_extension_api::ToolStartInput<'a>,
    ) -> codex_extension_api::ToolLifecycleFuture<'a> {
        Box::pin(async move {
            if input.call_id == "hold-leaf" {
                self.entered.notify_one();
                self.release.notified().await;
            }
        })
    }
}

#[test_case(false; "resident_parent")]
#[test_case(true; "evicted_plan_parent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grandchild_completion_wakes_its_completed_parent(evict_parent: bool) -> Result<()> {
    use codex_core::config::Config;
    use codex_extension_api::ExtensionRegistryBuilder;
    use core_test_support::ThreadIdle;
    use serde_json::Value;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::Mutex;
    use wiremock::Mock;
    use wiremock::Request;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    let server = responses::start_mock_server().await;
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = Arc::clone(&requests);
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(move |request: &Request| {
            let bytes = if request
                .headers
                .get("content-encoding")
                .is_some_and(|value| value == "zstd")
            {
                zstd::stream::decode_all(std::io::Cursor::new(&request.body))
                    .expect("request decompression")
            } else {
                request.body.clone()
            };
            let body: Value = serde_json::from_slice(&bytes).expect("responses request");
            let response_id = {
                let mut captured = captured.lock().expect("captured requests lock");
                captured.push(body.clone());
                format!("response-{}", captured.len())
            };
            let text = body["input"].to_string();
            let has_output = |id: &str| {
                body["input"]
                    .as_array()
                    .expect("request input array")
                    .iter()
                    .any(|item| item["type"] == "function_call_output" && item["call_id"] == id)
            };
            let spawn = |id: &str, name: &str, prompt: &str| {
                responses::ev_function_call_with_namespace(
                    id,
                    "collaboration",
                    "spawn_agent",
                    &json!({"task_name":name,"message":prompt,"fork_turns":"none"}).to_string(),
                )
            };
            let event = if text.contains("mail-root-only") {
                if has_output("spawn-parent") {
                    responses::ev_assistant_message("root-done", "Root resting")
                } else {
                    spawn("spawn-parent", "parent", "mail-parent-only")
                }
            } else if text.contains("mail-parent-only") {
                if text.contains("leaf result ready") {
                    responses::ev_assistant_message("parent-reviewed", "Reviewed the leaf result")
                } else if has_output("spawn-leaf") {
                    responses::ev_assistant_message("parent-parked", "Parent resting")
                } else {
                    spawn("spawn-leaf", "leaf", "mail-leaf-only")
                }
            } else if has_output("hold-leaf") {
                responses::ev_assistant_message("leaf-done", "leaf result ready")
            } else {
                responses::ev_function_call_with_namespace(
                    "hold-leaf",
                    "collaboration",
                    "list_agents",
                    "{}",
                )
            };
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created(&response_id),
                event,
                responses::ev_completed(&response_id),
            ]))
        })
        .mount(&server)
        .await;
    let gate = Arc::new(LeafGate::default());
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    extensions.tool_lifecycle_contributor(gate.clone());
    extensions.thread_lifecycle_contributor(Arc::new(ThreadIdle));
    let test = test_codex()
        .with_model("gpt-5.6-sol")
        .with_extensions(Arc::new(extensions.build()))
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("enable Collab");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("enable MultiAgentV2");
            config
                .features
                .enable(Feature::WakeOnAgentMessages)
                .expect("enable WakeOnAgentMessages");
            config.multi_agent_v2.max_concurrent_threads_per_session = 3;
        })
        .build_with_auto_env(&server)
        .await?;
    let mut created = test.thread_manager.subscribe_thread_created();
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "mail-root-only".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let parent_id = created.recv().await?;
    let mut parent = test.thread_manager.get_thread(parent_id).await?;
    tokio::time::timeout(std::time::Duration::from_secs(10), gate.entered.notified()).await?;
    let leaf_id = created.recv().await?;
    let leaf = test.thread_manager.get_thread(leaf_id).await?;
    wait_for_event(&parent, |event| matches!(event, EventMsg::TurnComplete(complete) if complete.last_agent_message.as_deref() == Some("Parent resting"))).await;
    ThreadIdle::wait(&parent).await;
    assert_eq!(
        parent
            .config_snapshot()
            .await
            .session_source
            .get_agent_path()
            .expect("nested parent agent path")
            .as_str(),
        "/root/parent"
    );
    if evict_parent {
        // Give the idle parent its own settings after the child has already
        // started. Reloading from the child's config would lose this choice.
        parent
            .update_thread_settings(ThreadSettingsOverrides {
                collaboration_mode: Some(CollaborationMode {
                    mode: ModeKind::Plan,
                    settings: Settings {
                        model: "gpt-5.6-luna".into(),
                        reasoning_effort: Some(codex_protocol::openai_models::ReasoningEffort::Low),
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            })
            .await?;
        assert_ne!(
            parent.config_snapshot().await.model,
            leaf.config_snapshot().await.model
        );
        assert_eq!(
            test.thread_manager
                .try_evict_v2_thread(Arc::clone(&parent))
                .await?,
            codex_core::ThreadEvictionOutcome::Evicted
        );
        assert!(test.thread_manager.get_thread(parent_id).await.is_err());
    }
    let expected = parent.config_snapshot().await;
    gate.release.notify_one();
    if evict_parent {
        let reloaded =
            tokio::time::timeout(std::time::Duration::from_secs(10), created.recv()).await??;
        assert_eq!(reloaded, parent_id);
        let replacement = test.thread_manager.get_thread(reloaded).await?;
        assert!(!Arc::ptr_eq(&parent, &replacement));
        parent = replacement;
    }
    wait_for_event(&parent, |event| matches!(event, EventMsg::TurnComplete(complete) if complete.last_agent_message.as_deref() == Some("Reviewed the leaf result"))).await;
    ThreadIdle::wait(&parent).await;
    assert_eq!(
        parent.agent_status().await,
        codex_protocol::protocol::AgentStatus::Completed(Some("Reviewed the leaf result".into()))
    );
    let actual = parent.config_snapshot().await;
    assert_eq!(
        (
            actual.model,
            actual.reasoning_effort,
            actual.collaboration_mode,
            actual.permission_profile
        ),
        (
            expected.model,
            expected.reasoning_effort,
            expected.collaboration_mode,
            expected.permission_profile
        )
    );
    let parent_requests = requests
        .lock()
        .expect("captured requests lock")
        .iter()
        .filter(|request| {
            let text = request["input"].to_string();
            text.contains("mail-parent-only") && !text.contains("mail-root-only")
        })
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(parent_requests.len(), 3);
    assert_eq!(
        parent_requests[2]["input"]
            .as_array()
            .expect("request input array")
            .iter()
            .filter(|item| item["type"] == "agent_message"
                && item["author"] == "/root/parent/leaf"
                && item.to_string().contains("leaf result ready"))
            .count(),
        1
    );
    if evict_parent {
        assert_eq!(parent_requests[2]["model"], "gpt-5.6-luna");
    }
    test.thread_manager
        .shutdown_all_threads_bounded(std::time::Duration::from_secs(5))
        .await;
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RestoredMailHistory {
    Absent,
    HistoryAndReceipt,
    HistoryWithoutReceipt,
}

#[test_case(RestoredMailHistory::Absent; "not_yet_recorded")]
#[test_case(RestoredMailHistory::HistoryAndReceipt; "already_in_history")]
#[test_case(RestoredMailHistory::HistoryWithoutReceipt; "history_append_without_receipt")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reopen_restores_unsubmitted_mail_without_duplicate_history(
    history: RestoredMailHistory,
) -> Result<()> {
    use codex_thread_store::AgentMailEnvelope;
    use codex_thread_store::AgentMailboxStore;
    use codex_thread_store::LocalAgentMailboxStore;

    let server = responses::start_mock_server().await;
    let configure = |config: &mut codex_core::config::Config| {
        config
            .features
            .enable(Feature::WakeOnAgentMessages)
            .expect("enable WakeOnAgentMessages");
    };
    let test = test_codex()
        .with_config(configure)
        .build_with_auto_env(&server)
        .await?;
    let initial = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("initial"),
            responses::ev_assistant_message("done", "Finished"),
            responses::ev_completed("initial"),
        ]),
    )
    .await;
    test.submit_text_turn("Initial task before closing").await?;
    assert_eq!(initial.requests().len(), 1);

    // Model a crash after durable acceptance, optionally after a history append,
    // but before any inference request containing this mail was accepted.
    let id = codex_protocol::ResponseItemId::new("agentmail");
    let root_id = test.session_configured.thread_id;
    let mut communication = InterAgentCommunication::new(
        AgentPath::root().join("worker").expect("worker path"),
        AgentPath::root(),
        Vec::new(),
        "durable late result".into(),
        /*trigger_turn*/ false,
    );
    communication.id = Some(id.clone());
    communication.mail = Some(AgentMailMetadata {
        message_id: id.to_string(),
        task_id: None,
        kind: AgentMailKind::Completion,
        wake: AgentMailWake::Auto,
    });
    let db = codex_core::init_state_db(&test.config)
        .await
        .expect("state database");
    let store = LocalAgentMailboxStore::new(db);
    store
        .accept(AgentMailEnvelope {
            message_id: id.to_string(),
            root_id,
            sender_id: root_id,
            recipient_id: root_id,
            kind: serde_json::to_string(&AgentMailKind::Completion)?,
            payload_json: serde_json::to_string(&communication)?,
            wake: true,
            task_id: None,
        })
        .await?;
    if history != RestoredMailHistory::Absent {
        test.codex
            .inject_response_items(vec![communication.to_model_input_item()])
            .await?;
    }
    if history == RestoredMailHistory::HistoryAndReceipt {
        store
            .mark_recorded(root_id, root_id, vec![id.to_string()], "closed-turn".into())
            .await?;
    }
    assert_eq!(
        store.pending(root_id, root_id).await?[0]
            .recorded_turn_id
            .is_some(),
        history == RestoredMailHistory::HistoryAndReceipt
    );
    let continuation = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("reopened"),
            responses::ev_assistant_message("ack", "Late result reviewed"),
            responses::ev_completed("reopened"),
        ]),
    )
    .await;
    let resumed = test_codex()
        .with_config(configure)
        .restart_with_auto_env(&server, &test)
        .await?;
    wait_for_event(&resumed.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    resumed.codex.shutdown_and_wait().await?;
    let request = continuation.single_request();
    assert_eq!(
        request
            .inputs_of_type("agent_message")
            .iter()
            .filter(|item| item.to_string().contains("durable late result"))
            .count(),
        1
    );
    assert_eq!(store.pending(root_id, root_id).await?, Vec::new());
    Ok(())
}
