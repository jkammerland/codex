//! Injected context interrupts assignment barriers only while it is pending.

use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_core::config::Config;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ToolLifecycleContributor;
use codex_extension_api::ToolLifecycleFuture;
use codex_extension_api::ToolStartInput;
use codex_features::Feature;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use core_test_support::ThreadIdle;
use core_test_support::responses;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use test_case::test_case;
use tokio::sync::Notify;
use wiremock::Mock;
use wiremock::Request;
use wiremock::matchers::method;
use wiremock::matchers::path;

const ROOT_PROMPT: &str = "barrier-steering-root: delegate and await the accepted assignment";
const INJECTION: &str = "Keep waiting for the same assignment after reading this context";

#[derive(Default)]
struct Gates {
    worker_entered: Notify,
    release_worker: Notify,
    barrier_entered: Notify,
    release_barrier: Notify,
}

impl ToolLifecycleContributor for Gates {
    fn on_tool_start<'a>(&'a self, input: ToolStartInput<'a>) -> ToolLifecycleFuture<'a> {
        Box::pin(async move {
            match input.call_id {
                "worker-hold" => {
                    self.worker_entered.notify_one();
                    self.release_worker.notified().await;
                }
                "barrier" => {
                    self.barrier_entered.notify_one();
                    self.release_barrier.notified().await;
                }
                _ => {}
            }
        })
    }
}

fn output(body: &Value, call_id: &str) -> Option<Value> {
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

#[derive(Clone, Copy)]
enum InjectionTiming {
    BeforeWait,
    DuringWait,
}

#[test_case(InjectionTiming::BeforeWait; "already_pending_injection")]
#[test_case(InjectionTiming::DuringWait; "injection_while_waiting")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injected_context_interrupts_barrier_and_is_consumed_before_next_wait(
    timing: InjectionTiming,
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
                    .expect("request decompression")
            } else {
                request.body.clone()
            };
            let body: Value = serde_json::from_slice(&bytes).expect("responses request");
            let response_id = {
                let mut requests = requests.lock().expect("captured requests");
                requests.push(body.clone());
                format!("response-{}", requests.len())
            };
            let event = if body["input"].to_string().contains(ROOT_PROMPT) {
                if output(&body, "barrier-again").is_some() {
                    responses::ev_assistant_message("root-done", "Reviewed the result")
                } else if let Some(receipt) = output(&body, "spawn") {
                    let call_id = if output(&body, "barrier").is_some() {
                        "barrier-again"
                    } else {
                        "barrier"
                    };
                    responses::ev_function_call(
                        call_id,
                        "wait_all",
                        &json!({"task_ids": [receipt["task_id"]]}).to_string(),
                    )
                } else {
                    responses::ev_function_call_with_namespace(
                        "spawn",
                        "collaboration",
                        "spawn_agent",
                        r#"{"task_name":"worker","message":"Complete the assignment","fork_turns":"none"}"#,
                    )
                }
            } else if output(&body, "worker-hold").is_some() {
                responses::ev_assistant_message("worker-done", "Assignment result")
            } else {
                responses::ev_function_call_with_namespace(
                    "worker-hold",
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

    let gates = Arc::new(Gates::default());
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    extensions.tool_lifecycle_contributor(gates.clone());
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
                    .expect("enable collaboration");
            }
            config.multi_agent_v2.max_concurrent_threads_per_session = 2;
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
    tokio::time::timeout(Duration::from_secs(10), async {
        gates.worker_entered.notified().await;
        gates.barrier_entered.notified().await;
    })
    .await?;
    let worker = test
        .thread_manager
        .get_thread(created.recv().await?)
        .await?;
    let EventMsg::TurnStarted(worker_turn) =
        wait_for_event(&worker, |event| matches!(event, EventMsg::TurnStarted(_))).await
    else {
        unreachable!("matched worker turn start")
    };

    // The pre-handler gate places the injection before activity subscription;
    // the item-started event places it after subscription in the other case.
    if matches!(timing, InjectionTiming::BeforeWait) {
        test.codex
            .inject_if_running(vec![responses::user_message_item(INJECTION)])
            .await
            .expect("inject before barrier subscription");
    }
    gates.release_barrier.notify_one();
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ItemStarted(event)
            if matches!(&event.item, TurnItem::CollabAgentToolCall(call) if call.id == "barrier"))
    })
    .await;
    if matches!(timing, InjectionTiming::DuringWait) {
        test.codex
            .inject_if_running(vec![responses::user_message_item(INJECTION)])
            .await
            .expect("inject during active barrier");
    }
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ItemStarted(event)
            if matches!(&event.item, TurnItem::CollabAgentToolCall(call) if call.id == "barrier-again"))
    })
    .await;

    // Consumption retains the context in history but clears its interruption.
    // The second barrier must therefore report the worker's actual completion.
    gates.release_worker.notify_one();
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    ThreadIdle::wait(&test.codex).await;
    ThreadIdle::wait(&worker).await;
    {
        let captured = captured.lock().expect("captured requests");
        let parent = captured
            .iter()
            .filter(|body| body["input"].to_string().contains(ROOT_PROMPT))
            .collect::<Vec<_>>();
        assert_eq!(parent.len(), 4);
        let receipt = output(parent[1], "spawn").expect("accepted assignment receipt");
        let interrupted = output(parent[2], "barrier").expect("interrupted barrier");
        let resumed = output(parent[3], "barrier-again").expect("completed barrier");
        assert_eq!(interrupted["tasks"].as_array().expect("tasks").len(), 1);
        assert_eq!(
            interrupted,
            json!({
                "reason": "user_input",
                "all_done": false,
                "tasks": [{
                    "task_id": receipt["task_id"],
                    "task_name": "/root/worker",
                    "status": "running",
                    "turn_id": worker_turn.turn_id,
                    "result_ref": null,
                }],
            })
        );
        assert_eq!(resumed["tasks"].as_array().expect("tasks").len(), 1);
        assert!(resumed["tasks"][0]["result_ref"].is_string());
        assert_eq!(
            resumed,
            json!({
                "reason": "all_done",
                "all_done": true,
                "tasks": [{
                    "task_id": receipt["task_id"],
                    "task_name": "/root/worker",
                    "status": "completed",
                    "turn_id": worker_turn.turn_id,
                    "result_ref": resumed["tasks"][0]["result_ref"],
                }],
            })
        );
        for request in &parent[2..] {
            assert_eq!(
                request["input"]
                    .as_array()
                    .expect("input")
                    .iter()
                    .filter(|item| item["role"] == "user")
                    .flat_map(|item| item["content"].as_array().expect("message content"))
                    .filter(|part| part["text"] == INJECTION)
                    .count(),
                1,
            );
        }
    }
    test.thread_manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    Ok(())
}
