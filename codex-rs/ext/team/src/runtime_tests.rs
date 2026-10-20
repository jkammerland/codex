use std::sync::Arc;

use codex_app_server_protocol::*;
use codex_core::TurnInputRequest;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::request_user_input::RequestUserInputResponse;
use codex_protocol::user_input::UserInput;
use core_test_support::TempDirExt;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::local_requests;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio::sync::oneshot;

use crate::PreparedTeamHost;
use crate::TeamHostService;

enum RuntimeWait {
    Approval,
    Input,
}

async fn runtime_activity(service: &TeamHostService) -> anyhow::Result<TeamActivity> {
    let host = service
        .read(TeamHostReadParams {
            cursor: None,
            limit: None,
        })
        .await?;
    assert_eq!(host.runtime.len(), 1);
    Ok(host.runtime[0].activity)
}

#[tokio::test]
async fn runtime_refresh_preserves_pending_approval_and_clears_it_after_response()
-> anyhow::Result<()> {
    interactive_runtime_refresh(RuntimeWait::Approval).await
}

#[tokio::test]
async fn runtime_refresh_preserves_pending_input_and_clears_it_after_response() -> anyhow::Result<()>
{
    interactive_runtime_refresh(RuntimeWait::Input).await
}

async fn interactive_runtime_refresh(wait: RuntimeWait) -> anyhow::Result<()> {
    let (tool, args, mode, expected) = match wait {
        RuntimeWait::Approval => (
            "exec_command",
            json!({
                "cmd": "echo team-runtime",
                "sandbox_permissions": "require_escalated",
                "justification": "Exercise a pending approval without executing the command."
            }),
            ModeKind::Default,
            TeamActivity::WaitingForApproval,
        ),
        RuntimeWait::Input => (
            "request_user_input",
            json!({
                "questions": [{
                    "id": "choice",
                    "header": "Choice",
                    "question": "Which option?",
                    "options": [
                        {"label": "First", "description": "Use the first option."},
                        {"label": "Second", "description": "Use the second option."}
                    ]
                }]
            }),
            ModeKind::Plan,
            TeamActivity::WaitingForInput,
        ),
    };
    let (finish, finishing) = oneshot::channel();
    let (model, _) = start_streaming_sse_server(vec![
        vec![StreamingSseChunk {
            gate: None,
            body: sse(vec![
                ev_response_created("interactive"),
                ev_function_call("interaction", tool, &args.to_string()),
                ev_completed("interactive"),
            ]),
        }],
        vec![StreamingSseChunk {
            gate: Some(finishing),
            body: sse(vec![
                ev_assistant_message("answer", "done"),
                ev_completed("finished"),
            ]),
        }],
    ])
    .await;
    let fixture = test_codex().build_with_streaming_server(&model).await?;
    let prepared = PreparedTeamHost::load(&fixture.config, "installation".into()).await?;
    let service = TeamHostService::new(prepared, Arc::downgrade(&fixture.thread_manager));
    service
        .enroll(TeamEnrollParams {
            operation_id: None,
            address: TeamAddress {
                machine_id: "machine".into(),
                server_id: "server".into(),
                thread_id: fixture.session_configured.session_id.to_string(),
            },
            alias: "worker".into(),
            control: TeamControlState {
                coordinator_id: "coordinator".into(),
                revision: 1,
                mode: TeamExecutionMode::Running,
            },
        })
        .await?;
    assert_eq!(runtime_activity(&service).await?, TeamActivity::Idle);

    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::workspace_write(), fixture.cwd.path());
    fixture
        .codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Ask for the required interaction.".into(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                environments: Some(local_requests(fixture.cwd.abs())),
                approval_policy: Some(AskForApproval::OnRequest),
                approvals_reviewer: Some(ApprovalsReviewer::User),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                collaboration_mode: Some(CollaborationMode {
                    mode,
                    settings: Settings {
                        model: fixture.session_configured.model.clone(),
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            }),
        )
        .await?;
    let event = wait_for_event(&fixture.codex, |event| {
        matches!(
            event,
            EventMsg::ExecApprovalRequest(_)
                | EventMsg::RequestUserInput(_)
                | EventMsg::TurnComplete(_)
        )
    })
    .await;
    let response = match (wait, event) {
        (RuntimeWait::Approval, EventMsg::ExecApprovalRequest(request)) => Op::ExecApproval {
            id: request.approval_id.unwrap_or(request.call_id),
            turn_id: Some(request.turn_id),
            decision: ReviewDecision::denied("Decline the test command."),
        },
        (RuntimeWait::Input, EventMsg::RequestUserInput(request)) => Op::UserInputAnswer {
            id: request.turn_id,
            response: RequestUserInputResponse {
                answers: Default::default(),
            },
        },
        (_, event) => panic!("expected the requested interaction, got {event:?}"),
    };

    // Repeated host refreshes and a fresh host instance must recover the live wait.
    assert_eq!(runtime_activity(&service).await?, expected);
    assert_eq!(runtime_activity(&service).await?, expected);
    drop(service);
    let recovered = TeamHostService::new(
        PreparedTeamHost::load(&fixture.config, "installation".into()).await?,
        Arc::downgrade(&fixture.thread_manager),
    );
    assert_eq!(runtime_activity(&recovered).await?, expected);

    // Hold the next model response open so clearing the wait cannot look idle.
    fixture.codex.submit(response).await?;
    model.wait_for_request_count(/*count*/ 2).await;
    assert_eq!(runtime_activity(&recovered).await?, TeamActivity::Working);
    finish.send(()).expect("release the model response");
    wait_for_event(&fixture.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(runtime_activity(&recovered).await?, TeamActivity::Idle);
    fixture.codex.shutdown_and_wait().await?;
    model.shutdown().await;
    Ok(())
}
