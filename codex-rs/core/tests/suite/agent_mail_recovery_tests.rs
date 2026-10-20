//! Failure and compatibility boundaries for the native mail runtime.

use super::*;
use codex_core::config::Config;
use codex_extension_api::ExtensionRegistryBuilder;
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

fn request_body(request: &Request) -> Value {
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
    serde_json::from_slice(&bytes).expect("responses request")
}

fn tool_output(body: &Value, call_id: &str) -> Option<Value> {
    body["input"].as_array()?.iter().find_map(|item| {
        if item["type"] != "function_call_output" || item["call_id"] != call_id {
            return None;
        }
        let output = &item["output"];
        let text = output.as_str().or_else(|| {
            output
                .as_array()?
                .iter()
                .find_map(|part| part["text"].as_str())
        })?;
        serde_json::from_str(text).ok()
    })
}

#[test_case(false; "completion_mail")]
#[test_case(true; "assignment_mail")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_reopen_holds_mail_until_explicit_input(assignment: bool) -> Result<()> {
    let server = responses::start_mock_server().await;
    let gate = Arc::new(LeafGate::default());
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    extensions.tool_lifecycle_contributor(gate.clone());
    extensions.thread_lifecycle_contributor(Arc::new(ThreadIdle));
    let configure = |config: &mut Config| {
        // This fixture gates the handler before it can update the plan.
        config.update_plan_enabled = true;
        config
            .features
            .enable(Feature::WakeOnAgentMessages)
            .expect("enable WakeOnAgentMessages");
    };
    let test = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .with_config(configure)
        .build_with_auto_env(&server)
        .await?;
    let root = test.session_configured.thread_id;
    let store = LocalAgentMailboxStore::new(
        codex_core::init_state_db(&test.config)
            .await
            .expect("state database"),
    );
    let accepted_id = codex_protocol::ResponseItemId::new("agentmail");
    let mut accepted_mail = InterAgentCommunication::new(
        AgentPath::root().join("worker").expect("worker path"),
        AgentPath::root(),
        Vec::new(),
        "Work that I will interrupt after acceptance".into(),
        /*trigger_turn*/ false,
    );
    accepted_mail.id = Some(accepted_id.clone());
    accepted_mail.mail = Some(AgentMailMetadata {
        message_id: accepted_id.to_string(),
        task_id: None,
        kind: AgentMailKind::DirectMessage,
        wake: AgentMailWake::Auto,
    });
    let accepted_envelope = AgentMailEnvelope {
        message_id: accepted_id.to_string(),
        root_id: root,
        sender_id: root,
        recipient_id: root,
        kind: serde_json::to_string(&AgentMailKind::DirectMessage)?,
        payload_json: serde_json::to_string(&accepted_mail)?,
        wake: true,
        task_id: None,
    };
    store.accept(accepted_envelope.clone()).await?;
    let initial = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("held-initial"),
            responses::ev_function_call(
                "hold-leaf",
                "update_plan",
                &json!({"plan":[{"step":"Wait for cancellation", "status":"in_progress"}]})
                    .to_string(),
            ),
            responses::ev_completed("held-initial"),
        ]),
    )
    .await;
    test.codex
        .submit(Op::InterAgentCommunication {
            communication: accepted_mail,
            start_options: Default::default(),
        })
        .await?;
    tokio::time::timeout(Duration::from_secs(10), gate.entered.notified()).await?;

    // Created was observed before the tool reached its pre-handler gate. The
    // subsequent interrupt must retain that receipt without running the tool.
    let accepted = store.accept(accepted_envelope.clone()).await?.record;
    assert!(accepted.submitted_turn_id.is_some());
    let plan_executed = std::sync::atomic::AtomicBool::new(false);
    test.codex.submit(Op::Interrupt).await?;
    let aborted = wait_for_event(&test.codex, |event| {
        if matches!(event, EventMsg::PlanUpdate(_)) {
            plan_executed.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        matches!(event, EventMsg::TurnAborted(_))
    })
    .await;
    // Interrupt emits TurnAborted without the normal completion idle callback.
    // An ordered settings acknowledgement waits for abort cleanup to finish.
    core_test_support::submit_thread_settings(&test.codex, ThreadSettingsOverrides::default())
        .await?;
    assert_eq!(initial.requests().len(), 1);
    assert!(!plan_executed.load(std::sync::atomic::Ordering::Relaxed));
    let EventMsg::TurnAborted(aborted) = aborted else {
        unreachable!("matched aborted turn")
    };
    assert_eq!(accepted.submitted_turn_id, aborted.turn_id);
    assert_eq!(store.accept(accepted_envelope).await?.record, accepted);
    assert_eq!(store.pending(root, root).await?, Vec::new());

    let message_id = codex_protocol::ResponseItemId::new("agentmail");
    let task_id = assignment.then(|| "held-assignment".to_string());
    let kind = if assignment {
        AgentMailKind::Assignment
    } else {
        AgentMailKind::Completion
    };
    let mut mail = InterAgentCommunication::new(
        AgentPath::root().join("worker").expect("worker path"),
        AgentPath::root(),
        Vec::new(),
        "mail accepted while manually held".into(),
        assignment,
    );
    mail.id = Some(message_id.clone());
    mail.mail = Some(AgentMailMetadata {
        message_id: message_id.to_string(),
        task_id: task_id.clone(),
        kind: kind.clone(),
        wake: AgentMailWake::Auto,
    });
    let envelope = AgentMailEnvelope {
        message_id: message_id.to_string(),
        root_id: root,
        sender_id: root,
        recipient_id: root,
        kind: serde_json::to_string(&kind)?,
        payload_json: serde_json::to_string(&mail)?,
        wake: true,
        task_id: task_id.clone(),
    };
    if let Some(task_id) = task_id {
        store
            .accept_task(
                AgentTaskRecord {
                    task_id,
                    root_id: root,
                    owner_id: root,
                    target_id: root,
                    name: Some("/root".into()),
                    status: AgentTaskStatus::Held,
                    turn_id: None,
                    result_ref: None,
                },
                envelope,
            )
            .await?;
    } else {
        store.accept(envelope).await?;
    }

    // Startup must restore the interrupt before either the trigger-turn shortcut
    // or the ordinary automatic-mail admission path can reserve work.
    let resumed = test_codex()
        .with_config(configure)
        .restart_with_auto_env(&server, &test)
        .await?;
    assert_eq!(resumed.codex.agent_status().await, AgentStatus::Interrupted);
    assert_eq!(store.pending(root, root).await?.len(), 1);
    let continuation = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("explicit-resume"),
            responses::ev_assistant_message("resume-ack", "Held mail reviewed"),
            responses::ev_completed("explicit-resume"),
        ]),
    )
    .await;
    resumed.submit_text_turn("Continue now").await?;
    resumed.codex.shutdown_and_wait().await?;
    let request = continuation.single_request();
    assert_eq!(
        request
            .inputs_of_type("agent_message")
            .iter()
            .filter(|item| item
                .to_string()
                .contains("mail accepted while manually held"))
            .count(),
        1
    );
    assert_eq!(store.pending(root, root).await?, Vec::new());
    Ok(())
}

#[test_case(false; "durable")]
#[test_case(true; "ephemeral")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_rejected_before_created_settles_only_attempted_assignment(
    ephemeral: bool,
) -> Result<()> {
    let server = responses::start_mock_server().await;
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let requests = Arc::clone(&captured);
    Mock::given(method("POST")).and(path("/v1/responses")).respond_with(move |request: &Request| {
        let body = request_body(request);
        let response_id = {
            let mut requests = requests.lock().expect("captured requests");
            requests.push(body.clone());
            format!("failure-response-{}", requests.len())
        };
        if !body["input"].to_string().contains("failure-root-only") {
            return ResponseTemplate::new(400).set_body_json(json!({"error":{"message":"fixture assignment rejected before creation","type":"invalid_request_error"}}));
        }
        let event = if tool_output(&body, "barrier").is_some() {
            responses::ev_assistant_message("failure-reviewed", "Failure observed")
        } else if let Some(receipt) = tool_output(&body, "spawn-child") {
            responses::ev_function_call("barrier", "wait_all", &json!({"task_ids":[receipt["task_id"]]}).to_string())
        } else {
            responses::ev_function_call_with_namespace("spawn-child", "collaboration", "spawn_agent", &json!({"task_name":"worker", "message":"failure-child-only", "fork_turns":"none"}).to_string())
        };
        responses::sse_response(responses::sse(vec![responses::ev_response_created(&response_id), event, responses::ev_completed(&response_id)]))
    }).mount(&server).await;
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    extensions.thread_lifecycle_contributor(Arc::new(ThreadIdle));
    let test = test_codex()
        .with_model("gpt-5.6-sol")
        .with_extensions(Arc::new(extensions.build()))
        .with_config(move |config| {
            config.ephemeral = ephemeral;
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
        })
        .build_with_auto_env(&server)
        .await?;
    let mut created = test.thread_manager.subscribe_thread_created();
    test.submit_text_turn("failure-root-only").await?;
    ThreadIdle::wait(&test.codex).await;
    let child_id = created.recv().await?;
    let child = test.thread_manager.get_thread(child_id).await?;
    ThreadIdle::wait(&child).await;
    let requests = captured.lock().expect("captured requests").clone();
    assert_eq!(requests.len(), 4);
    let final_request = requests
        .iter()
        .find(|body| tool_output(body, "barrier").is_some())
        .expect("barrier completion request");
    let result = tool_output(final_request, "barrier").expect("barrier output");
    assert_eq!(result["all_done"], true);
    assert_eq!(result["tasks"][0]["status"], "failed");
    assert!(
        final_request["input"]
            .to_string()
            .contains("fixture assignment rejected before creation")
    );

    let root = test.session_configured.thread_id;
    let store = LocalAgentMailboxStore::new(
        codex_core::init_state_db(&test.config)
            .await
            .expect("state database"),
    );
    let tasks = store
        .tasks(
            root,
            AgentTaskFilter {
                target_id: Some(child_id),
                ..Default::default()
            },
        )
        .await?
        .tasks;
    if ephemeral {
        assert_eq!(tasks, Vec::new());
        assert_eq!(store.pending(root, child_id).await?, Vec::new());
        assert_eq!(store.pending(root, root).await?, Vec::new());
    } else {
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status, AgentTaskStatus::Failed);
        assert!(tasks[0].turn_id.is_some());
        let pending = store.pending(root, child_id).await?;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].submitted_turn_id, None);
        assert!(pending[0].recorded_turn_id.is_some());
        assert_eq!(
            child
                .agent_work()
                .await?
                .expect("enabled work summary")
                .pending_mail,
            0
        );
    }
    test.thread_manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    Ok(())
}

#[test_case(false; "feature_disabled")]
#[test_case(true; "feature_enabled")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_completion_preserves_notification_format_and_feature_gate(
    enabled: bool,
) -> Result<()> {
    let server = responses::start_mock_server().await;
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let requests = Arc::clone(&captured);
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(move |request: &Request| {
            let body = request_body(request);
            let response_id = {
                let mut requests = requests.lock().expect("captured requests");
                requests.push(body.clone());
                format!("legacy-response-{}", requests.len())
            };
            let text = body["input"].to_string();
            let event = if text.contains("legacy-root-only") {
                if text.contains("<subagent_notification>") {
                    responses::ev_assistant_message("legacy-reviewed", "Legacy result reviewed")
                } else if tool_output(&body, "spawn-child").is_some() {
                    responses::ev_assistant_message("legacy-root-rest", "Legacy parent resting")
                } else {
                    responses::ev_function_call_with_namespace(
                        "spawn-child",
                        "multi_agent_v1",
                        "spawn_agent",
                        &json!({"message":"legacy-child-only", "fork_context":false}).to_string(),
                    )
                }
            } else if body["input"]
                .as_array()
                .expect("request input")
                .iter()
                .any(|item| {
                    item["type"] == "function_call_output" && item["call_id"] == "hold-leaf"
                })
            {
                // update_plan returns plain text, unlike the JSON spawn receipt.
                responses::ev_assistant_message("legacy-child-result", "legacy child result")
            } else {
                responses::ev_function_call(
                    "hold-leaf",
                    "update_plan",
                    &json!({"plan":[{"step":"Wait for parent", "status":"in_progress"}]})
                        .to_string(),
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
        .with_model_info_override("gpt-5.5", |model| {
            model.multi_agent_version = Some(codex_protocol::protocol::MultiAgentVersion::V1);
        })
        .with_extensions(Arc::new(extensions.build()))
        .with_config(move |config| {
            config.update_plan_enabled = true;
            config
                .features
                .enable(Feature::Collab)
                .expect("enable Collab");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("disable MultiAgentV2");
            if enabled {
                config
                    .features
                    .enable(Feature::WakeOnAgentMessages)
                    .expect("enable WakeOnAgentMessages");
            } else {
                config
                    .features
                    .disable(Feature::WakeOnAgentMessages)
                    .expect("disable WakeOnAgentMessages");
            }
        })
        .build_with_auto_env(&server)
        .await?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "legacy-root-only".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    tokio::time::timeout(Duration::from_secs(10), gate.entered.notified()).await?;
    wait_for_event(&test.codex, |event| matches!(event, EventMsg::TurnComplete(complete) if complete.last_agent_message.as_deref() == Some("Legacy parent resting"))).await;
    ThreadIdle::wait(&test.codex).await;
    gate.release.notify_one();

    // The raw history event is a deterministic delivery boundary for both paths.
    let notification = wait_for_event(&test.codex, |event| matches!(event, EventMsg::RawResponseItem(raw) if serde_json::to_string(&raw.item).is_ok_and(|item| item.contains("<subagent_notification>")))).await;
    let EventMsg::RawResponseItem(raw) = notification else {
        unreachable!("matched raw notification")
    };
    let codex_protocol::models::ResponseItem::Message {
        role,
        content,
        internal_chat_message_metadata_passthrough,
        ..
    } = raw.item
    else {
        panic!("legacy notification remains a user message")
    };
    assert_eq!(role, "user");
    let [codex_protocol::models::ContentItem::InputText { text }] = content.as_slice() else {
        panic!("legacy text content")
    };
    let payload = text
        .strip_prefix("<subagent_notification>\n")
        .and_then(|text| text.strip_suffix("\n</subagent_notification>"))
        .expect("original notification envelope");
    let payload: Value = serde_json::from_str(payload)?;
    assert_eq!(
        payload["status"],
        json!({"completed":"legacy child result"})
    );
    let metadata = serde_json::to_value(internal_chat_message_metadata_passthrough)?;
    assert!(
        metadata
            .to_string()
            .contains("multi_agent.subagent_notification")
    );
    if !enabled {
        assert_eq!(captured.lock().expect("captured requests").len(), 4);
        test.codex
            .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Review the result now".into(),
                text_elements: Vec::new(),
            }]))
            .await?;
    }
    wait_for_event(&test.codex, |event| matches!(event, EventMsg::TurnComplete(complete) if complete.last_agent_message.as_deref() == Some("Legacy result reviewed"))).await;
    ThreadIdle::wait(&test.codex).await;
    let requests = captured.lock().expect("captured requests").clone();
    assert_eq!(requests.len(), 5);
    let final_request = requests.last().expect("final request");
    assert_eq!(
        final_request["input"]
            .as_array()
            .expect("request input array")
            .iter()
            .filter(|item| item.to_string().contains("<subagent_notification>"))
            .count(),
        1
    );
    test.thread_manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_direct_mail_stays_pending_across_reopen_until_new_work() -> Result<()> {
    let server = responses::start_mock_server().await;
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let requests = Arc::clone(&captured);
    Mock::given(method("POST")).and(path("/v1/responses")).respond_with(move |request: &Request| {
        let body = request_body(request);
        let mut requests = requests.lock().expect("captured requests");
        requests.push(body);
        if requests.len() == 1 {
            ResponseTemplate::new(400).set_body_json(json!({"error":{"message":"direct mail rejected before creation","type":"invalid_request_error"}}))
        } else {
            responses::sse_response(responses::sse(vec![responses::ev_response_created("new-work"), responses::ev_assistant_message("new-work-ack", "Reviewed retained and new mail"), responses::ev_completed("new-work")]))
        }
    }).mount(&server).await;
    let configure = |config: &mut Config| {
        config
            .features
            .enable(Feature::WakeOnAgentMessages)
            .expect("enable WakeOnAgentMessages");
    };
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    extensions.thread_lifecycle_contributor(Arc::new(ThreadIdle));
    let test = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .with_config(configure)
        .build_with_auto_env(&server)
        .await?;
    let root = test.session_configured.thread_id;
    let store = LocalAgentMailboxStore::new(
        codex_core::init_state_db(&test.config)
            .await
            .expect("state database"),
    );
    let mail = |text: &str| {
        let id = codex_protocol::ResponseItemId::new("agentmail");
        let mut mail = InterAgentCommunication::new(
            AgentPath::root().join("worker").expect("worker path"),
            AgentPath::root(),
            Vec::new(),
            text.into(),
            /*trigger_turn*/ false,
        );
        mail.id = Some(id.clone());
        mail.mail = Some(AgentMailMetadata {
            message_id: id.to_string(),
            task_id: None,
            kind: AgentMailKind::DirectMessage,
            wake: AgentMailWake::Auto,
        });
        mail
    };
    let original = mail("original rejected direct mail");
    store
        .accept(AgentMailEnvelope {
            message_id: original
                .mail
                .as_ref()
                .expect("original mail metadata")
                .message_id
                .clone(),
            root_id: root,
            sender_id: root,
            recipient_id: root,
            kind: serde_json::to_string(&AgentMailKind::DirectMessage)?,
            payload_json: serde_json::to_string(&original)?,
            wake: true,
            task_id: None,
        })
        .await?;
    test.codex
        .submit(Op::InterAgentCommunication {
            communication: original,
            start_options: Default::default(),
        })
        .await?;
    wait_for_event(
        &test.codex,
        |event| matches!(event, EventMsg::TurnComplete(complete) if complete.error.is_some()),
    )
    .await;
    ThreadIdle::wait(&test.codex).await;

    let resumed = test_codex()
        .with_config(configure)
        .restart_with_auto_env(&server, &test)
        .await?;
    assert!(matches!(
        resumed.codex.agent_status().await,
        AgentStatus::Errored(_)
    ));
    assert_eq!(captured.lock().expect("captured requests").len(), 1);
    let pending = store.pending(root, root).await?;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].submitted_turn_id, None);

    // A distinct accepted message authorizes continuation; retained failed input
    // is included once, and its receipt advances only after the accepted request.
    let fresh = mail("new authorized direct mail");
    store
        .accept(AgentMailEnvelope {
            message_id: fresh
                .mail
                .as_ref()
                .expect("fresh mail metadata")
                .message_id
                .clone(),
            root_id: root,
            sender_id: root,
            recipient_id: root,
            kind: serde_json::to_string(&AgentMailKind::DirectMessage)?,
            payload_json: serde_json::to_string(&fresh)?,
            wake: true,
            task_id: None,
        })
        .await?;
    resumed
        .codex
        .submit(Op::InterAgentCommunication {
            communication: fresh,
            start_options: Default::default(),
        })
        .await?;
    wait_for_event(&resumed.codex, |event| matches!(event, EventMsg::TurnComplete(complete) if complete.last_agent_message.as_deref() == Some("Reviewed retained and new mail"))).await;
    resumed.codex.shutdown_and_wait().await?;
    let requests = captured.lock().expect("captured requests").clone();
    assert_eq!(requests.len(), 2);
    for text in [
        "original rejected direct mail",
        "new authorized direct mail",
    ] {
        assert_eq!(
            requests[1]["input"]
                .as_array()
                .expect("request input array")
                .iter()
                .filter(|item| item["type"] == "agent_message" && item.to_string().contains(text))
                .count(),
            1
        );
    }
    assert_eq!(store.pending(root, root).await?, Vec::new());
    Ok(())
}
