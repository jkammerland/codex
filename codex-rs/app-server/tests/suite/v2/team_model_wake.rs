use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::create_mock_responses_server_sequence;
use codex_app_server_protocol::*;
use codex_protocol::openai_models::ReasoningEffort;
use pretty_assertions::assert_eq;
use serde_json::Value;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unloaded_team_selection_precedes_stored_mailbox_wake() -> Result<()> {
    let server = create_mock_responses_server_sequence(vec![
        create_final_assistant_message_sse_response("thread persisted")?,
        create_final_assistant_message_sse_response("mail handled")?,
    ])
    .await;
    let home = tempfile::tempdir()?;
    MockResponsesConfig::new(&server.uri())
        .with_model("gpt-5.1")
        .with_root_config("thread_unload_delay_secs = 0")
        .write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = app.start_thread(ThreadStartParams::default()).await?.thread;
    app.start_turn_and_wait_for_completion(TurnStartParams {
        thread_id: thread.id.clone(),
        input: vec![UserInput::Text {
            text: "Persist this thread before unloading it".into(),
            text_elements: Vec::new(),
        }],
        ..Default::default()
    })
    .await?;

    // Queue durable mail while held, so resuming the host can wake without a new delivery.
    let address = TeamAddress {
        machine_id: "local".into(),
        server_id: "primary".into(),
        thread_id: thread.id.clone(),
    };
    let control = TeamControlState {
        coordinator_id: "model-wake-test".into(),
        revision: 1,
        mode: TeamExecutionMode::Paused,
    };
    let _: TeamEnrollResponse = app
        .request(|request_id| ClientRequest::TeamEnroll {
            request_id,
            params: TeamEnrollParams {
                operation_id: None,
                address: address.clone(),
                alias: "reviewer".into(),
                control: control.clone(),
            },
        })
        .await?;
    let queued: TeamDeliverResponse = app
        .request(|request_id| ClientRequest::TeamDeliver {
            request_id,
            params: TeamDeliverParams {
                coordinator_id: control.coordinator_id.clone(),
                delivery: TeamDelivery {
                    id: "stored-mail".into(),
                    thread_id: thread.id.clone(),
                    mode: TeamDeliveryMode::NextTurn,
                    wake: true,
                    message: TeamMessage {
                        id: "stored-request".into(),
                        sender: TeamAddress {
                            thread_id: "peer".into(),
                            ..address.clone()
                        },
                        recipient: Some(address.clone()),
                        kind: TeamMessageKind::Request,
                        body: "Handle the stored request using the chosen model".into(),
                        reply_to: None,
                        topic: None,
                        created_at: 1,
                    },
                },
            },
        })
        .await?;
    assert_eq!(queued.receipt.state, TeamReceiptState::Pending);
    let unsubscribed: ThreadUnsubscribeResponse = app
        .request(|request_id| ClientRequest::ThreadUnsubscribe {
            request_id,
            params: ThreadUnsubscribeParams {
                thread_id: thread.id.clone(),
            },
        })
        .await?;
    assert_eq!(unsubscribed.status, ThreadUnsubscribeStatus::Unsubscribed);
    let closed: ThreadClosedNotification = app.read_notification("thread/closed").await?;
    assert_eq!(closed.thread_id, thread.id);
    let loaded: ThreadLoadedListResponse = app
        .request(|request_id| ClientRequest::ThreadLoadedList {
            request_id,
            params: ThreadLoadedListParams::default(),
        })
        .await?;
    assert_eq!(loaded.data, Vec::<String>::new());

    // Admit the new selection while unloaded, before Ready can dispatch the stored mail.
    let selection = TeamModelSelection {
        revision: 2,
        model: TeamSelection::Value {
            value: "gpt-5.5".into(),
        },
        effort: TeamSelection::Value {
            value: "high".into(),
        },
    };
    let _: ThreadSettingsUpdateResponse = app
        .request(|request_id| ClientRequest::ThreadSettingsUpdate {
            request_id,
            params: ThreadSettingsUpdateParams {
                thread_id: thread.id.clone(),
                model: Some("gpt-5.5".into()),
                effort: Some(ReasoningEffort::High),
                team_selection: Some(selection),
                ..Default::default()
            },
        })
        .await?;
    let _: TeamControlResponse = app
        .request(|request_id| ClientRequest::TeamControl {
            request_id,
            params: TeamControlParams {
                control: TeamControlState {
                    revision: 2,
                    mode: TeamExecutionMode::Running,
                    ..control
                },
            },
        })
        .await?;
    let _: ThreadResumeResponse = app
        .request(|request_id| ClientRequest::ThreadResume {
            request_id,
            params: ThreadResumeParams {
                thread_id: thread.id.clone(),
                ..Default::default()
            },
        })
        .await?;
    let completed: TurnCompletedNotification = app.read_notification("turn/completed").await?;
    assert_eq!(completed.thread_id, thread.id);
    assert_eq!(completed.turn.status, TurnStatus::Completed);

    let requests = server
        .received_requests()
        .await
        .expect("recorded model requests")
        .into_iter()
        .filter(|request| request.url.path().ends_with("/responses"))
        .map(|request| serde_json::from_slice::<Value>(&request.body))
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["model"], "gpt-5.1");
    assert_eq!(requests[1]["model"], "gpt-5.5");
    assert_eq!(requests[1]["reasoning"]["effort"], "high");
    assert_eq!(
        requests[1]["input"]
            .as_array()
            .expect("model input")
            .iter()
            .filter(|item| item
                .to_string()
                .contains("Handle the stored request using the chosen model"))
            .count(),
        1
    );
    Ok(())
}
