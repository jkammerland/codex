//! Fixed assignment barriers preserve quiet waits across partial worker results.

use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_core::config::Config;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ToolLifecycleContributor;
use codex_extension_api::ToolLifecycleFuture;
use codex_extension_api::ToolStartInput;
use codex_features::Feature;
use codex_protocol::AgentPath;
use codex_protocol::items::CollabAgentTool;
use codex_protocol::items::CollabAgentToolCallItem;
use codex_protocol::items::CollabAgentToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::AgentMailKind;
use codex_protocol::protocol::AgentMailMetadata;
use codex_protocol::protocol::AgentMailWake;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::ThreadIdle;
use core_test_support::responses;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use test_case::test_case;
use tokio::sync::Notify;
use wiremock::Mock;
use wiremock::Request;
use wiremock::matchers::method;
use wiremock::matchers::path;

const ROOT_PROMPT: &str = "root-only: delegate two independent assignments and wait for both";

#[derive(Default)]
struct BarrierGates {
    entered: [Notify; 2],
    release: [Notify; 2],
    waiting: Notify,
    waiting_again: Notify,
}

impl ToolLifecycleContributor for BarrierGates {
    fn on_tool_start<'a>(&'a self, input: ToolStartInput<'a>) -> ToolLifecycleFuture<'a> {
        Box::pin(async move {
            match input.call_id {
                "hold-a" | "hold-b" => {
                    let index = usize::from(input.call_id == "hold-b");
                    self.entered[index].notify_one();
                    self.release[index].notified().await;
                }
                "barrier" => self.waiting.notify_one(),
                "barrier-again" => self.waiting_again.notify_one(),
                _ => {}
            }
        })
    }
}

fn body_json(request: &Request) -> Value {
    let body = if request
        .headers
        .get("content-encoding")
        .is_some_and(|value| value == "zstd")
    {
        zstd::stream::decode_all(std::io::Cursor::new(&request.body))
            .expect("request decompression")
    } else {
        request.body.clone()
    };
    serde_json::from_slice(&body).expect("responses request")
}

fn output(body: &Value, call_id: &str) -> Option<Value> {
    body["input"].as_array()?.iter().find_map(|item| {
        if item["type"] != "function_call_output" || item["call_id"] != call_id {
            return None;
        }
        let value = &item["output"];
        let text = value.as_str().or_else(|| {
            value
                .as_array()?
                .iter()
                .find_map(|part| part["text"].as_str())
        })?;
        serde_json::from_str(text).ok()
    })
}

async fn deliver_mail(
    test: &TestCodex,
    kind: AgentMailKind,
    wake: AgentMailWake,
    text: &str,
) -> Result<()> {
    let id = codex_protocol::ResponseItemId::new("barrier-mail");
    let mut communication = InterAgentCommunication::new(
        AgentPath::root().join("a").expect("worker path"),
        AgentPath::root(),
        Vec::new(),
        text.to_string(),
        /*trigger_turn*/ false,
    );
    communication.id = Some(id.clone());
    communication.mail = Some(AgentMailMetadata {
        message_id: id.to_string(),
        task_id: (kind == AgentMailKind::Completion)
            .then(|| "an-unselected-assignment".to_string()),
        kind,
        wake,
    });
    test.codex
        .submit(Op::InterAgentCommunication {
            communication,
            start_options: Default::default(),
        })
        .await?;
    Ok(())
}

#[test_case(0, false, None; "first_worker_completes_first")]
#[test_case(1, false, None; "second_worker_completes_first")]
#[test_case(0, true, None; "user_steering_then_same_receipts")]
#[test_case(0, false, Some(AgentMailKind::DirectMessage); "help_then_same_receipts")]
#[test_case(0, false, Some(AgentMailKind::Completion); "unselected_result_then_same_receipts")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_results_and_queue_only_progress_do_not_resample_parent(
    first: usize,
    steer: bool,
    interrupt_mail: Option<AgentMailKind>,
) -> Result<()> {
    let interrupted = steer || interrupt_mail.is_some();
    let server = responses::start_mock_server().await;
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let requests = Arc::clone(&captured);
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(move |request: &Request| {
            let body = body_json(request);
            let response_id = {
                let mut requests = requests.lock().expect("captured requests");
                requests.push(body.clone());
                format!("response-{}", requests.len())
            };
            let text = body["input"].to_string();
            let call = |id, tool, args: Value| {
                if matches!(tool, "wait_all" | "send_message") {
                    responses::ev_function_call(id, tool, &args.to_string())
                } else {
                    responses::ev_function_call_with_namespace(id, "collaboration", tool, &args.to_string())
                }
            };
            let events = if text.contains(ROOT_PROMPT) {
                if output(&body, "barrier-again").is_some() || output(&body, "barrier").is_some_and(|result| result["all_done"] == true) {
                    vec![responses::ev_assistant_message("root-done", "Reviewed both results")]
                } else if output(&body, "barrier").is_some() {
                    let a = output(&body, "spawn-a").expect("first receipt");
                    let b = output(&body, "spawn-b").expect("second receipt");
                    vec![call("barrier-again", "wait_all", json!({"task_ids": [a["task_id"], b["task_id"]]}))]
                } else if let (Some(a), Some(b)) = (output(&body, "spawn-a"), output(&body, "spawn-b")) {
                    vec![call("barrier", "wait_all", json!({"task_ids": [a["task_id"], b["task_id"], a["task_id"]]}))]
                } else {
                    vec![
                        call("spawn-a", "spawn_agent", json!({"task_name":"a", "message":"worker-a-only", "fork_turns":"none"})),
                        call("spawn-b", "spawn_agent", json!({"task_name":"b", "message":"worker-b-only", "fork_turns":"none"})),
                    ]
                }
            } else {
                let name = if text.contains("worker-a-only") { "a" } else { "b" };
                let hold = format!("hold-{name}");
                // list_agents has JSON output; its completion releases the final inference.
                if output(&body, &hold).is_some() {
                    vec![responses::ev_assistant_message(&format!("done-{name}"), &format!("result-{name}"))]
                } else {
                    vec![
                        call(&format!("progress-{name}"), "send_message", json!({"target":"/root", "message":format!("progress-{name}"), "wake":false})),
                        call(&hold, "list_agents", json!({})),
                    ]
                }
            };
            responses::sse_response(responses::sse([
                vec![responses::ev_response_created(&response_id)],
                events,
                vec![responses::ev_completed(&response_id)],
            ].concat()))
        })
        .mount(&server)
        .await;

    let gates = Arc::new(BarrierGates::default());
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    extensions.tool_lifecycle_contributor(gates.clone());
    extensions.thread_lifecycle_contributor(Arc::new(ThreadIdle));
    let test = test_codex()
        .with_model("gpt-5.6-sol")
        .with_extensions(Arc::new(extensions.build()))
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("enable collaboration");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("enable multi-agent v2");
            config
                .features
                .enable(Feature::WakeOnAgentMessages)
                .expect("enable waking agent mail");
            config.multi_agent_v2.max_concurrent_threads_per_session = 3;
        })
        .build_with_auto_env(&server)
        .await?;
    let mut created = test.thread_manager.subscribe_thread_created();
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: ROOT_PROMPT.to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;

    // Both workers and the parent's barrier are now controlled by observable tool boundaries.
    tokio::time::timeout(Duration::from_secs(10), async {
        gates.entered[0].notified().await;
        gates.entered[1].notified().await;
        gates.waiting.notified().await;
    })
    .await?;
    // Queue-only mail arriving during the barrier must wait for the next real continuation.
    deliver_mail(
        &test,
        AgentMailKind::DirectMessage,
        AgentMailWake::QueueOnly,
        "late quiet progress",
    )
    .await?;
    if let Some(kind) = interrupt_mail {
        deliver_mail(&test, kind, AgentMailWake::Auto, "help or unrelated result").await?;
        tokio::time::timeout(Duration::from_secs(10), gates.waiting_again.notified()).await?;
    }
    if steer {
        test.codex
            .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Keep waiting after this user correction".to_string(),
                text_elements: Vec::new(),
            }]))
            .await?;
        tokio::time::timeout(Duration::from_secs(10), gates.waiting_again.notified()).await?;
    }
    let mut workers = Vec::new();
    let mut worker_ids = Vec::new();
    for _ in 0..2 {
        let id = created.recv().await?;
        worker_ids.push(id);
        let thread = test.thread_manager.get_thread(id).await?;
        let name = thread
            .config_snapshot()
            .await
            .session_source
            .get_agent_path()
            .expect("worker path")
            .name()
            .to_string();
        workers.push((name, thread));
    }
    workers.sort_by(|(left, _), (right, _)| left.cmp(right));
    let workers = workers
        .into_iter()
        .map(|(_, thread)| thread)
        .collect::<Vec<_>>();
    gates.release[first].notify_one();
    wait_for_event(&workers[first], |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    ThreadIdle::wait(&workers[first]).await;
    let root_count = || {
        captured
            .lock()
            .expect("captured requests")
            .iter()
            .filter(|body| body["input"].to_string().contains(ROOT_PROMPT))
            .count()
    };
    assert_eq!(root_count(), 2 + usize::from(interrupted));

    gates.release[1 - first].notify_one();
    let mut completed_waits = Vec::new();
    wait_for_event(&test.codex, |event| {
        if let EventMsg::ItemCompleted(event) = event
            && let TurnItem::CollabAgentToolCall(call) = &event.item
            && call.tool == CollabAgentTool::Wait
        {
            completed_waits.push(call.clone());
        }
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    ThreadIdle::wait(&test.codex).await;
    ThreadIdle::wait(&workers[1 - first]).await;
    assert_eq!(root_count(), 3 + usize::from(interrupted));

    // Inspect the actual terminal-facing events, independently of the model's
    // wait_all JSON result. Interrupted barriers still have two running workers.
    assert_eq!(completed_waits.len(), 1 + usize::from(interrupted));
    worker_ids.sort_by_key(ToString::to_string);
    for (index, mut call) in completed_waits.into_iter().enumerate() {
        call.receiver_thread_ids.sort_by_key(ToString::to_string);
        let status = if interrupted && index == 0 {
            AgentStatus::Running
        } else {
            AgentStatus::Completed(None)
        };
        assert_eq!(
            call,
            CollabAgentToolCallItem {
                id: if index == 0 {
                    "barrier"
                } else {
                    "barrier-again"
                }
                .to_string(),
                tool: CollabAgentTool::Wait,
                status: CollabAgentToolCallStatus::Completed,
                sender_thread_id: test.session_configured.thread_id,
                receiver_thread_ids: worker_ids.clone(),
                receiver_agents: Vec::new(),
                prompt: None,
                model: None,
                reasoning_effort: None,
                agents_states: worker_ids
                    .iter()
                    .map(|id| (*id, status.clone()))
                    .collect::<HashMap<_, _>>(),
            }
        );
    }
    {
        let captured = captured.lock().expect("captured requests");
        assert_eq!(captured.len(), 7 + usize::from(interrupted));
        if interrupted {
            let interrupted = captured
                .iter()
                .find_map(|body| output(body, "barrier"))
                .expect("interrupted barrier output");
            assert_eq!(
                interrupted["reason"],
                if steer { "user_input" } else { "message" }
            );
            assert_eq!(interrupted["all_done"], false);
        }
        let call_id = if interrupted {
            "barrier-again"
        } else {
            "barrier"
        };
        let barrier = captured
            .iter()
            .find_map(|body| output(body, call_id))
            .expect("barrier output");
        assert_eq!(barrier["reason"], "all_done");
        assert_eq!(barrier["all_done"], true);
        let tasks = barrier["tasks"].as_array().expect("task states");
        assert_eq!(tasks.len(), 2);
        assert!(
            tasks
                .iter()
                .all(|task| task["status"] == "completed" && task["result_ref"].is_string())
        );
        assert_ne!(tasks[0]["task_id"], tasks[1]["task_id"]);
        let final_input = captured
            .iter()
            .find(|body| output(body, call_id).is_some())
            .expect("parent request after barrier")["input"]
            .to_string();
        assert!(final_input.contains("late quiet progress"));
        assert!(final_input.contains("progress-a"));
        assert!(final_input.contains("progress-b"));
        assert!(final_input.contains("result-a"));
        assert!(final_input.contains("result-b"));
    }

    test.thread_manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multiple_assignments_to_one_worker_emit_one_wait_receiver() -> Result<()> {
    const PROMPT: &str = "root-only: assign two tasks to the same worker and wait for both";
    let server = responses::start_mock_server().await;
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let requests = Arc::clone(&captured);
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(move |request: &Request| {
            let body = body_json(request);
            let response_id = {
                let mut requests = requests.lock().expect("captured requests");
                requests.push(body.clone());
                format!("response-{}", requests.len())
            };
            let event = if body["input"].to_string().contains(PROMPT) {
                if output(&body, "both-assignments").is_some() {
                    responses::ev_assistant_message("root-done", "Reviewed both assignments")
                } else if let Some(followup) = output(&body, "followup") {
                    let initial = output(&body, "spawn").expect("initial assignment receipt");
                    responses::ev_function_call(
                        "both-assignments",
                        "wait_all",
                        &json!({"task_ids": [initial["task_id"], followup["task_id"]]}).to_string(),
                    )
                } else if output(&body, "spawn").is_some() {
                    responses::ev_function_call_with_namespace(
                        "followup",
                        "collaboration",
                        "followup_task",
                        r#"{"target":"worker","message":"Complete the followup assignment"}"#,
                    )
                } else {
                    responses::ev_function_call_with_namespace(
                        "spawn",
                        "collaboration",
                        "spawn_agent",
                        r#"{"task_name":"worker","message":"Complete the initial assignment","fork_turns":"none"}"#,
                    )
                }
            } else {
                responses::ev_assistant_message("worker-done", "Assignment completed")
            };
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created(&response_id),
                event,
                responses::ev_completed(&response_id),
            ]))
        })
        .mount(&server)
        .await;

    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    extensions.thread_lifecycle_contributor(Arc::new(ThreadIdle));
    let test = test_codex()
        .with_model("gpt-5.6-sol")
        .with_extensions(Arc::new(extensions.build()))
        .with_config(|config| {
            for feature in [
                Feature::Collab,
                Feature::MultiAgentV2,
                Feature::WakeOnAgentMessages,
            ] {
                config
                    .features
                    .enable(feature)
                    .expect("enable collaboration feature");
            }
            config.multi_agent_v2.max_concurrent_threads_per_session = 2;
        })
        .build_with_auto_env(&server)
        .await?;
    let mut created = test.thread_manager.subscribe_thread_created();
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: PROMPT.to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;

    let mut wait_items = Vec::new();
    wait_for_event(&test.codex, |event| {
        let item = match event {
            EventMsg::ItemStarted(event) => Some(&event.item),
            EventMsg::ItemCompleted(event) => Some(&event.item),
            _ => None,
        };
        if let Some(TurnItem::CollabAgentToolCall(call)) = item
            && call.id == "both-assignments"
        {
            wait_items.push(call.clone());
        }
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let worker_id = created.recv().await?;

    // Both accepted receipts remain in the result, but terminal events describe
    // the one worker once at both the beginning and the end of the barrier.
    assert_eq!(wait_items.len(), 2);
    let mut expected = CollabAgentToolCallItem {
        id: "both-assignments".to_string(),
        tool: CollabAgentTool::Wait,
        status: CollabAgentToolCallStatus::InProgress,
        sender_thread_id: test.session_configured.thread_id,
        receiver_thread_ids: vec![worker_id],
        receiver_agents: Vec::new(),
        prompt: None,
        model: None,
        reasoning_effort: None,
        agents_states: HashMap::new(),
    };
    assert_eq!(wait_items[0], expected);
    expected.status = CollabAgentToolCallStatus::Completed;
    expected.agents_states = HashMap::from([(worker_id, AgentStatus::Completed(None))]);
    assert_eq!(wait_items[1], expected);
    {
        let captured = captured.lock().expect("captured requests");
        let result = captured
            .iter()
            .find_map(|body| output(body, "both-assignments"))
            .expect("barrier result");
        let tasks = result["tasks"].as_array().expect("selected task results");
        assert_eq!(result["reason"], "all_done");
        assert_eq!(result["all_done"], true);
        assert_eq!(tasks.len(), 2);
        assert_ne!(tasks[0]["task_id"], tasks[1]["task_id"]);
        assert!(tasks.iter().all(|task| task["status"] == "completed"));
    }

    test.thread_manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_barrier_gate_preserves_catalog_help_and_exposes_wake_override() -> Result<()> {
    let server = responses::start_mock_server().await;
    let response = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse_completed("disabled"),
            responses::sse_completed("enabled"),
        ],
    )
    .await;
    for enabled in [false, true] {
        let test = test_codex()
            .with_model_info_override("gpt-5.6-sol", |model| {
                // This test inspects JSON schemas, so choose the direct tool surface explicitly.
                model.tool_mode = Some(codex_protocol::openai_models::ToolMode::Direct);
                model.multi_agent_version = Some(codex_protocol::protocol::MultiAgentVersion::V2);
                model.model_messages.as_mut().expect("model messages").tools = Some(
                    serde_json::from_value(json!({"multi_agent": {"send_message": {
                        "description": "Catalog message routing guidance.",
                        "parameters": r#"{"type":"object","properties":{"target":{"type":"string"},"message":{"type":"string"}},"required":["target","message"],"additionalProperties":false}"#
                    }}})).expect("catalog tool messages")
                );
            })
            .with_config(move |config| {
                config
                    .features
                    .enable(Feature::MultiAgentV2)
                    .expect("enable multi-agent v2");
                if enabled {
                    config
                        .features
                        .enable(Feature::WakeOnAgentMessages)
                        .expect("enable waking agent mail");
                } else {
                    config
                        .features
                        .disable(Feature::WakeOnAgentMessages)
                        .expect("disable waking agent mail");
                }
                assert_eq!(
                    config.multi_agent_v2.tool_namespace.as_deref(),
                    Some("collaboration")
                );
            })
            .build_with_auto_env(&server)
            .await?;
        test.submit_turn("Inspect the available tools.").await?;
    }
    let requests = response.requests();
    assert_eq!(requests.len(), 2);
    for (request, enabled) in requests.iter().zip([false, true]) {
        let body = request.body_json();
        // Responses Lite carries the effective schemas in an additional_tools prefix.
        let namespaces = body["input"]
            .as_array()
            .expect("input")
            .iter()
            .find(|item| item["type"] == "additional_tools")
            .expect("Responses Lite tool prefix")["tools"]
            .as_array()
            .expect("tool namespaces");
        let collaboration_tools = namespaces
            .iter()
            .find(|namespace| namespace["name"] == "collaboration")
            .expect("default collaboration namespace")["tools"]
            .as_array()
            .expect("namespace tools");
        let send_namespace = if enabled {
            "functions"
        } else {
            "collaboration"
        };
        let send = namespaces
            .iter()
            .find(|namespace| namespace["name"] == send_namespace)
            .expect("message tool namespace")["tools"]
            .as_array()
            .expect("message namespace tools")
            .iter()
            .find(|tool| tool["name"] == "send_message")
            .expect("send_message");
        assert_eq!(
            collaboration_tools
                .iter()
                .any(|tool| tool["name"] == "send_message"),
            !enabled
        );
        let description = send["description"].as_str().expect("send description");
        assert!(description.starts_with("Catalog message routing guidance."));
        assert_eq!(description.contains("queue-only progress"), enabled);
        assert_eq!(
            send["parameters"]["properties"]["wake"].is_object(),
            enabled
        );
        let barrier_namespaces = namespaces
            .iter()
            .filter(|namespace| {
                namespace["tools"]
                    .as_array()
                    .expect("namespace tools")
                    .iter()
                    .any(|tool| tool["name"] == "wait_all")
            })
            .map(|namespace| namespace["name"].as_str().expect("namespace name"))
            .collect::<Vec<_>>();
        assert_eq!(
            barrier_namespaces,
            if enabled {
                vec!["functions"]
            } else {
                Vec::new()
            }
        );
    }
    Ok(())
}
