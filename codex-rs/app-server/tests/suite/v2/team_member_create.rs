//! Creation identity survives dropped responses and partial durable progress.

use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_sequence;
use codex_app_server_protocol::*;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use codex_state::SqliteConfig;
use codex_team_extension::MemberCreateIntent;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;

async fn start(home: &Path) -> Result<TestAppServer> {
    TestAppServer::builder()
        .with_codex_home(home)
        .without_managed_config()
        .build_initialized()
        .await
}

async fn prepare(app: &mut TestAppServer, cwd: &Path) -> Result<TeamMemberCreateParams> {
    let _: TeamControlResponse = app
        .request(|request_id| ClientRequest::TeamControl {
            request_id,
            params: TeamControlParams { control: control() },
        })
        .await?;
    let host: TeamHostReadResponse = app
        .request(|request_id| ClientRequest::TeamHostRead {
            request_id,
            params: TeamHostReadParams {
                cursor: None,
                limit: None,
            },
        })
        .await?;
    Ok(TeamMemberCreateParams {
        operation_id: uuid::Uuid::now_v7().to_string(),
        coordinator_id: control().coordinator_id,
        installation_id: host.installation_id,
        cwd: cwd
            .to_str()
            .context("test checkout path must be UTF-8")?
            .into(),
        model: None,
        effort: None,
    })
}

fn control() -> TeamControlState {
    TeamControlState {
        coordinator_id: "member-coordinator".into(),
        revision: 1,
        mode: TeamExecutionMode::Stopped,
    }
}

async fn create(
    app: &mut TestAppServer,
    params: &TeamMemberCreateParams,
) -> Result<TeamMemberCreateResponse> {
    app.request(|request_id| ClientRequest::TeamMemberCreate {
        request_id,
        params: params.clone(),
    })
    .await
}

#[tokio::test]
async fn concurrent_creation_retries_materialize_one_idle_root_and_preserve_stop() -> Result<()> {
    let server = create_mock_responses_server_sequence(Vec::new()).await;
    let home = tempfile::tempdir()?;
    MockResponsesConfig::new(&server.uri()).write(home.path())?;
    let mut app = start(home.path()).await?;
    let params = prepare(&mut app, home.path()).await?;

    let first = app
        .send_request("team/memberCreate", Some(serde_json::to_value(&params)?))
        .await?;
    let second = app
        .send_request("team/memberCreate", Some(serde_json::to_value(&params)?))
        .await?;
    let first: TeamMemberCreateResponse = app.read_response(first).await?;
    let second: TeamMemberCreateResponse = app.read_response(second).await?;
    assert_eq!(first, second);
    assert_eq!(first.phase, TeamMemberCreatePhase::Created);
    let loaded: ThreadLoadedListResponse = app
        .request(|request_id| ClientRequest::ThreadLoadedList {
            request_id,
            params: ThreadLoadedListParams {
                cursor: None,
                limit: None,
            },
        })
        .await?;
    assert_eq!(loaded.data, vec![first.thread_id.clone()]);

    // The member is attachable before its first turn or a server restart.
    let turns: ThreadTurnsListResponse = app
        .request(|request_id| ClientRequest::ThreadTurnsList {
            request_id,
            params: ThreadTurnsListParams {
                thread_id: first.thread_id.clone(),
                cursor: None,
                limit: Some(10),
                sort_direction: None,
                items_view: None,
            },
        })
        .await?;
    assert_eq!(
        turns,
        ThreadTurnsListResponse {
            data: Vec::new(),
            next_cursor: None,
            backwards_cursor: None
        }
    );

    let items: ThreadItemsListResponse = app
        .request(|request_id| ClientRequest::ThreadItemsList {
            request_id,
            params: ThreadItemsListParams {
                thread_id: first.thread_id.clone(),
                turn_id: None,
                cursor: None,
                limit: Some(10),
                sort_direction: None,
            },
        })
        .await?;
    assert_eq!(
        items,
        ThreadItemsListResponse {
            data: Vec::new(),
            next_cursor: None,
            backwards_cursor: None
        }
    );

    let read: ThreadReadResponse = app
        .request(|request_id| ClientRequest::ThreadRead {
            request_id,
            params: ThreadReadParams {
                thread_id: first.thread_id.clone(),
                include_turns: true,
            },
        })
        .await?;
    assert!(read.thread.turns.is_empty());
    assert!(
        read.thread
            .path
            .as_ref()
            .is_some_and(|path| path.as_path().is_file())
    );

    // The creation fence closes admission even before the coordinator enrolls it.
    let denied = app
        .send_turn_start_request(TurnStartParams {
            thread_id: first.thread_id.clone(),
            input: vec![UserInput::Text {
                text: "must remain held".into(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let error = app
        .read_stream_until_error_message(RequestId::Integer(denied))
        .await?;
    assert!(
        error.error.message.contains("held by team control"),
        "{error:?}"
    );

    let _: TeamEnrollResponse = app
        .request(|request_id| ClientRequest::TeamEnroll {
            request_id,
            params: TeamEnrollParams {
                operation_id: Some(uuid::Uuid::now_v7().to_string()),
                address: TeamAddress {
                    machine_id: "local".into(),
                    server_id: "primary".into(),
                    thread_id: first.thread_id,
                },
                alias: "Builder".into(),
                control: control(),
            },
        })
        .await?;
    let host: TeamHostReadResponse = app
        .request(|request_id| ClientRequest::TeamHostRead {
            request_id,
            params: TeamHostReadParams {
                cursor: None,
                limit: None,
            },
        })
        .await?;
    assert_eq!(host.control, Some(control()));
    assert_eq!(host.data.len(), 1);
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| !request.url.path().ends_with("/responses"))
    );
    Ok(())
}

#[tokio::test]
async fn lost_creation_outcome_recovers_reserved_root_after_restart() -> Result<()> {
    let server = create_mock_responses_server_sequence(Vec::new()).await;
    let home = tempfile::tempdir()?;
    MockResponsesConfig::new(&server.uri()).write(home.path())?;
    let mut app = start(home.path()).await?;
    let params = prepare(&mut app, home.path()).await?;
    let created = create(&mut app, &params).await?;
    assert_eq!(created.phase, TeamMemberCreatePhase::Created);
    assert!(app.shutdown_gracefully().await?.success());

    // Reproduce a crash after history was flushed but before the outcome committed.
    let pool = SqliteConfig::new_for_testing(AbsolutePathBuf::try_from(home.path())?)
        .open_read_write_pool(&home.path().join("team/host.sqlite"))
        .await?;
    let changed = sqlx::query("UPDATE record SET payload = json_set(payload, '$.outcome', NULL) WHERE namespace = 'member_create' AND id = ?")
        .bind(format!("{}:{}", params.coordinator_id, params.operation_id)).execute(&pool).await?;
    assert_eq!(changed.rows_affected(), 1);
    pool.close().await;

    let mut app = start(home.path()).await?;
    let replay = create(&mut app, &params).await?;
    assert_eq!(replay, created);
    let read: ThreadReadResponse = app
        .request(|request_id| ClientRequest::ThreadRead {
            request_id,
            params: ThreadReadParams {
                thread_id: replay.thread_id,
                include_turns: true,
            },
        })
        .await?;
    assert!(read.thread.turns.is_empty());
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| !request.url.path().ends_with("/responses"))
    );
    Ok(())
}

#[tokio::test]
async fn prepared_creation_can_start_but_ambiguous_missing_root_requires_inspection() -> Result<()>
{
    let server = create_mock_responses_server_sequence(Vec::new()).await;
    let home = tempfile::tempdir()?;
    MockResponsesConfig::new(&server.uri()).write(home.path())?;
    let mut app = start(home.path()).await?;
    let prepared = prepare(&mut app, home.path()).await?;
    let mut ambiguous = prepared.clone();
    ambiguous.operation_id = uuid::Uuid::now_v7().to_string();
    assert!(app.shutdown_gracefully().await?.success());

    let pool = SqliteConfig::new_for_testing(AbsolutePathBuf::try_from(home.path())?)
        .open_read_write_pool(&home.path().join("team/host.sqlite"))
        .await?;
    let prepared_id = codex_protocol::ThreadId::new().to_string();
    let ambiguous_id = codex_protocol::ThreadId::new().to_string();
    for intent in [
        MemberCreateIntent {
            params: prepared.clone(),
            thread_id: prepared_id.clone(),
            starting: false,
            outcome: None,
        },
        MemberCreateIntent {
            params: ambiguous.clone(),
            thread_id: ambiguous_id.clone(),
            starting: true,
            outcome: None,
        },
    ] {
        sqlx::query("INSERT INTO record(namespace, id, payload) VALUES ('member_create', ?, ?)")
            .bind(format!(
                "{}:{}",
                intent.params.coordinator_id, intent.params.operation_id
            ))
            .bind(serde_json::to_string(&intent)?)
            .execute(&pool)
            .await?;
    }
    pool.close().await;

    let mut app = start(home.path()).await?;
    let created = create(&mut app, &prepared).await?;
    assert_eq!(
        (created.thread_id, created.phase),
        (prepared_id.clone(), TeamMemberCreatePhase::Created)
    );
    let uncertain = create(&mut app, &ambiguous).await?;
    assert_eq!(
        (uncertain.thread_id, uncertain.phase),
        (ambiguous_id, TeamMemberCreatePhase::NeedsInspection)
    );
    let loaded: ThreadLoadedListResponse = app
        .request(|request_id| ClientRequest::ThreadLoadedList {
            request_id,
            params: ThreadLoadedListParams {
                cursor: None,
                limit: None,
            },
        })
        .await?;
    assert_eq!(loaded.data, vec![prepared_id]);
    Ok(())
}

#[tokio::test]
async fn rejected_checkout_is_durable_and_operation_identity_cannot_change() -> Result<()> {
    let server = create_mock_responses_server_sequence(Vec::new()).await;
    let home = tempfile::tempdir()?;
    MockResponsesConfig::new(&server.uri()).write(home.path())?;
    let mut app = start(home.path()).await?;
    let missing = home.path().join("missing");
    let params = prepare(&mut app, &missing).await?;
    let failed = create(&mut app, &params).await?;
    assert_eq!(failed.phase, TeamMemberCreatePhase::Failed);
    std::fs::create_dir(&missing)?;
    assert_eq!(create(&mut app, &params).await?, failed);

    let changed = TeamMemberCreateParams {
        model: Some("different".into()),
        ..params
    };
    let id = app
        .send_request("team/memberCreate", Some(serde_json::to_value(changed)?))
        .await?;
    let error = app
        .read_stream_until_error_message(RequestId::Integer(id))
        .await?;
    assert!(
        error.error.message.contains("conflicting content"),
        "{error:?}"
    );
    let loaded: ThreadLoadedListResponse = app
        .request(|request_id| ClientRequest::ThreadLoadedList {
            request_id,
            params: ThreadLoadedListParams {
                cursor: None,
                limit: None,
            },
        })
        .await?;
    assert!(loaded.data.is_empty());
    Ok(())
}

#[tokio::test]
async fn creation_uses_host_catalog_without_model_or_effort_fallback() -> Result<()> {
    let server = create_mock_responses_server_sequence(Vec::new()).await;
    let home = tempfile::tempdir()?;
    let mut model = codex_models_manager::bundled_models_response()?
        .models
        .into_iter()
        .next()
        .context("bundled catalog is empty")?;
    model.slug = "gpt-6.1-sol".into();
    model.supported_in_api = true;
    model.visibility = codex_protocol::openai_models::ModelVisibility::List;
    model.default_reasoning_level = Some(ReasoningEffort::Medium);
    model.supported_reasoning_levels = vec![ReasoningEffortPreset {
        effort: ReasoningEffort::Medium,
        description: "Medium".into(),
    }];
    let catalog = home.path().join("member-models.json");
    std::fs::write(
        &catalog,
        serde_json::to_vec(&serde_json::json!({"models": [model]}))?,
    )?;
    MockResponsesConfig::new(&server.uri())
        .with_model("gpt-6.1-sol")
        .with_root_config(&format!(
            "model_catalog_json = {}",
            serde_json::to_string(&catalog)?
        ))
        .write(home.path())?;
    let mut app = start(home.path()).await?;
    let mut params = prepare(&mut app, home.path()).await?;
    params.model = Some("not-in-catalog".into());
    let unavailable = create(&mut app, &params).await?;
    assert_eq!(unavailable.phase, TeamMemberCreatePhase::Failed);
    assert!(
        unavailable
            .error
            .as_deref()
            .is_some_and(|error| error.contains("not-in-catalog")),
        "{unavailable:?}"
    );
    params.operation_id = uuid::Uuid::now_v7().to_string();
    params.model = Some("gpt-6.1-sol".into());
    params.effort = Some(ReasoningEffort::Ultra);
    let unsupported = create(&mut app, &params).await?;
    assert_eq!(unsupported.phase, TeamMemberCreatePhase::Failed);
    assert!(
        unsupported
            .error
            .as_deref()
            .is_some_and(|error| error.contains("reasoning effort ultra")),
        "{unsupported:?}"
    );
    params.operation_id = uuid::Uuid::now_v7().to_string();
    params.effort = Some(ReasoningEffort::Medium);
    let created = create(&mut app, &params).await?;
    assert_eq!(created.phase, TeamMemberCreatePhase::Created, "{created:?}");
    let resumed: ThreadResumeResponse = app
        .request(|request_id| ClientRequest::ThreadResume {
            request_id,
            params: ThreadResumeParams {
                thread_id: created.thread_id,
                ..Default::default()
            },
        })
        .await?;
    assert_eq!(resumed.model, "gpt-6.1-sol");
    assert_eq!(resumed.reasoning_effort, Some(ReasoningEffort::Medium));
    Ok(())
}
