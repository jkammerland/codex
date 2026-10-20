use std::sync::Arc;

use codex_app_server_protocol::*;
use codex_protocol::ThreadId;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::team_model::TeamModelPolicy;
use codex_protocol::team_model::TeamModelSelection;
use codex_protocol::team_model::TeamSelection;
use codex_protocol::team_model::TeamSelectionOrigin;
use core_test_support::streaming_sse::StreamingSseServer;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;

use crate::models::LocalModelIntent;
use crate::service::PreparedTeamHost;
use crate::service::TeamHostService;

async fn model_host() -> anyhow::Result<(
    StreamingSseServer,
    TestCodex,
    Arc<TeamHostService>,
    ThreadId,
)> {
    model_host_with_extensions(Arc::new(
        codex_extension_api::ExtensionRegistryBuilder::new().build(),
    ))
    .await
}

async fn model_host_with_extensions(
    extensions: Arc<codex_extension_api::ExtensionRegistry<codex_core::config::Config>>,
) -> anyhow::Result<(
    StreamingSseServer,
    TestCodex,
    Arc<TeamHostService>,
    ThreadId,
)> {
    let (model, _) = start_streaming_sse_server(Vec::new()).await;
    let fixture = test_codex()
        .with_extensions(extensions)
        .build_with_streaming_server(&model)
        .await?;
    let thread_id: ThreadId = fixture.session_configured.session_id.into();
    let prepared = PreparedTeamHost::load(&fixture.config, "installation".into()).await?;
    let service = TeamHostService::new(prepared, Arc::downgrade(&fixture.thread_manager));
    service
        .enroll(TeamEnrollParams {
            operation_id: None,
            address: TeamAddress {
                machine_id: "machine".into(),
                server_id: "server".into(),
                thread_id: thread_id.to_string(),
            },
            alias: "worker".into(),
            control: TeamControlState {
                coordinator_id: "coordinator".into(),
                revision: 1,
                mode: TeamExecutionMode::Running,
            },
        })
        .await?;
    Ok((model, fixture, service, thread_id))
}

fn coordinator_selection(
    revision: i64,
    model: &str,
    effort: TeamSelection,
) -> ThreadSettingsOverrides {
    ThreadSettingsOverrides {
        model: Some(model.into()),
        effort: match &effort {
            TeamSelection::Value { value } => Some(Some(
                serde_json::from_value(serde_json::json!(value)).unwrap(),
            )),
            TeamSelection::Default => Some(None),
            TeamSelection::Keep | TeamSelection::Inherit => None,
        },
        team_selection: Some(TeamModelSelection {
            revision,
            model: TeamSelection::Value {
                value: model.into(),
            },
            effort,
        }),
        ..Default::default()
    }
}

#[tokio::test]
async fn default_model_request_without_resolved_value_uses_loaded_host_default()
-> anyhow::Result<()> {
    let (model, fixture, service, thread_id) = model_host().await?;
    let default_model = fixture.codex.configured_default_model().await;
    let other_model = if default_model == "gpt-5.2-codex" {
        "gpt-5.1-codex"
    } else {
        "gpt-5.2-codex"
    };
    fixture
        .codex
        .update_thread_settings(ThreadSettingsOverrides {
            model: Some(other_model.into()),
            ..Default::default()
        })
        .await?;
    let original = fixture.codex.thread_settings_snapshot().await;
    let selection = TeamModelSelection {
        revision: 10,
        model: TeamSelection::Default,
        effort: TeamSelection::Keep,
    };

    // Offline normalization leaves the default unresolved. The journal can
    // nevertheless find a loaded thread if resume won that race.
    service
        .journal_model_selection(
            thread_id,
            &ThreadSettingsOverrides {
                team_selection: Some(selection.clone()),
                ..Default::default()
            },
        )
        .await?;
    service.reconcile_model_selection(thread_id).await?;
    let settings = fixture.codex.thread_settings_snapshot().await;
    assert_eq!(
        (
            settings.model,
            settings.reasoning_effort,
            settings.team_model_policy
        ),
        (
            default_model,
            original.reasoning_effort,
            TeamModelPolicy {
                model_origin: TeamSelectionOrigin::Default,
                selection: Some(selection),
                ..original.team_model_policy
            }
        )
    );
    model.shutdown().await;
    Ok(())
}

#[tokio::test]
#[expect(
    clippy::await_holding_invalid_type,
    reason = "holds lifecycle dispatch until explicit pre-ready journal assertions finish"
)]
async fn unloaded_sparse_selections_preserve_pending_fields_and_revision_fences()
-> anyhow::Result<()> {
    let (model, fixture, service, thread_id) = model_host().await?;
    let _operation = service.operations.lock().await;
    let rollout_path = fixture.session_configured.rollout_path.clone().unwrap();
    fixture.codex.shutdown_and_wait().await?;
    fixture.thread_manager.remove_thread(&thread_id).await;

    let model_choice = ThreadSettingsUpdateParams {
        thread_id: thread_id.to_string(),
        team_selection: Some(TeamModelSelection {
            revision: 40,
            model: TeamSelection::Value {
                value: "gpt-5.2-codex".into(),
            },
            effort: TeamSelection::Keep,
        }),
        ..Default::default()
    };
    let effort_choice = ThreadSettingsUpdateParams {
        thread_id: thread_id.to_string(),
        team_selection: Some(TeamModelSelection {
            revision: 41,
            model: TeamSelection::Keep,
            effort: TeamSelection::Default,
        }),
        ..Default::default()
    };
    assert!(service.defer_model_selection(&model_choice).await?);
    assert!(service.defer_model_selection(&effort_choice).await?);
    assert!(service.defer_model_selection(&effort_choice).await?);
    // Pending settings do not close the family execution gate.
    assert!(service.gate.try_enter(thread_id).is_some());
    assert!(
        service
            .defer_model_selection(&model_choice)
            .await
            .unwrap_err()
            .to_string()
            .contains("stale")
    );
    let mut conflicting = effort_choice.clone();
    conflicting.team_selection.as_mut().unwrap().effort = TeamSelection::Value {
        value: "high".into(),
    };
    assert!(
        service
            .defer_model_selection(&conflicting)
            .await
            .unwrap_err()
            .to_string()
            .contains("reused")
    );

    // Ordinary unloaded edits are still handled by the loaded-thread API.
    assert!(
        !service
            .defer_model_selection(&ThreadSettingsUpdateParams {
                thread_id: thread_id.to_string(),
                model: Some("gpt-5.2-codex".into()),
                ..Default::default()
            })
            .await?
    );
    let resumed = fixture
        .thread_manager
        .resume_legacy_thread_from_rollout(
            fixture.config.clone(),
            rollout_path,
            fixture.thread_manager.auth_manager(),
            /*parent_trace*/ None,
            Default::default(),
        )
        .await?;
    // Reproduce Dispatch seeing the registered thread before Ready restores
    // settings. Its stored mail must remain pending without starting a turn.
    let store = service.store.get().unwrap();
    let address = TeamAddress {
        machine_id: "machine".into(),
        server_id: "server".into(),
        thread_id: thread_id.to_string(),
    };
    let receipt = store
        .receive(&TeamDelivery {
            id: "pending-wake".into(),
            thread_id: thread_id.to_string(),
            mode: TeamDeliveryMode::NextTurn,
            wake: true,
            message: TeamMessage {
                id: "wake-message".into(),
                sender: address.clone(),
                recipient: Some(address),
                kind: TeamMessageKind::Request,
                body: "Use the selected model".into(),
                reply_to: None,
                topic: None,
                created_at: 1,
            },
        })
        .await?;
    service
        .dispatch_delivery(
            thread_id,
            crate::delivery::DeliveryBoundary::IdleOrSafeBoundary,
        )
        .await?;
    assert_eq!(
        store.deliveries(&thread_id.to_string()).await?[0].1,
        receipt
    );
    assert_eq!(resumed.thread.team_active_turn_id().await, None);
    assert!(service.model_selection_pending(thread_id).await?);
    service.reconcile_model_selection(thread_id).await?;
    assert!(!service.model_selection_pending(thread_id).await?);
    assert!(service.gate.try_enter(thread_id).is_some());
    let settings = resumed.thread.config_snapshot().await;
    assert_eq!(
        (
            settings.model,
            settings.reasoning_effort,
            settings.team_model_policy
        ),
        (
            "gpt-5.2-codex".into(),
            None,
            TeamModelPolicy {
                model_origin: TeamSelectionOrigin::Explicit,
                effort_origin: TeamSelectionOrigin::Default,
                selection: effort_choice.team_selection,
                superseded_revision: None,
            }
        )
    );
    resumed.thread.shutdown_and_wait().await?;
    model.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn sparse_local_edits_survive_acknowledgement_before_core_and_publish_the_durable_fence()
-> anyhow::Result<()> {
    let (model, fixture, service, thread_id) = model_host().await?;
    let coordinator = coordinator_selection(
        10,
        "gpt-5.2-codex",
        TeamSelection::Value {
            value: "low".into(),
        },
    );
    service
        .journal_model_selection(thread_id, &coordinator)
        .await?;
    let original = fixture.codex.thread_settings_snapshot().await;
    assert_eq!(
        service
            .journal_model_selection(
                thread_id,
                &ThreadSettingsOverrides {
                    model: Some("gpt-5.1-codex".into()),
                    ..Default::default()
                }
            )
            .await?,
        Some(10)
    );
    let store = service.store.get().unwrap();
    let first = store
        .get::<LocalModelIntent>("model_local_intent", &thread_id.to_string())
        .await?
        .unwrap();
    assert_eq!(
        (
            first.model,
            first.effort,
            first.policy.model_origin,
            first.policy.effort_origin
        ),
        (
            "gpt-5.1-codex".into(),
            Some(ReasoningEffort::Low),
            TeamSelectionOrigin::Explicit,
            TeamSelectionOrigin::Explicit
        )
    );
    service
        .journal_model_selection(
            thread_id,
            &ThreadSettingsOverrides {
                effort: Some(None),
                ..Default::default()
            },
        )
        .await?;
    // Neither sparse edit was sent to Core. A process loss here must preserve
    // both the model change and the explicit clear of the parent/default effort.
    assert_eq!(fixture.codex.thread_settings_snapshot().await, original);
    drop(service);
    let prepared = PreparedTeamHost::load(&fixture.config, "installation".into()).await?;
    let recovered = TeamHostService::new(prepared, Arc::downgrade(&fixture.thread_manager));
    let runtime = recovered
        .read(TeamHostReadParams {
            cursor: None,
            limit: None,
        })
        .await?
        .runtime;
    assert_eq!(runtime[0].superseded_selection_revision, Some(10));
    assert!(
        recovered
            .journal_model_selection(thread_id, &coordinator)
            .await
            .is_err()
    );
    recovered.reconcile_model_selection(thread_id).await?;
    recovered.reconcile_model_selection(thread_id).await?;
    assert!(
        recovered.gate.try_enter(thread_id).is_none(),
        "model recovery does not confirm stored Running admission"
    );
    let settings = fixture.codex.thread_settings_snapshot().await;
    assert_eq!(
        (
            settings.model,
            settings.reasoning_effort,
            settings.team_model_policy
        ),
        (
            "gpt-5.1-codex".into(),
            None,
            TeamModelPolicy {
                model_origin: TeamSelectionOrigin::Explicit,
                effort_origin: TeamSelectionOrigin::Default,
                selection: coordinator.team_selection,
                superseded_revision: Some(10)
            },
        )
    );
    assert_eq!(model.requests().await, Vec::<Vec<u8>>::new());
    model.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn newer_coordinator_intent_supersedes_pending_local_recovery_and_keeps_its_omitted_field()
-> anyhow::Result<()> {
    let (model, fixture, service, thread_id) = model_host().await?;
    service
        .journal_model_selection(
            thread_id,
            &coordinator_selection(
                10,
                "gpt-5.2-codex",
                TeamSelection::Value {
                    value: "low".into(),
                },
            ),
        )
        .await?;
    service
        .journal_model_selection(
            thread_id,
            &ThreadSettingsOverrides {
                model: Some("gpt-5.1-codex".into()),
                effort: Some(None),
                ..Default::default()
            },
        )
        .await?;
    let newer = coordinator_selection(11, "gpt-5.2", TeamSelection::Keep);
    service.journal_model_selection(thread_id, &newer).await?;
    assert_eq!(
        service
            .store
            .get()
            .unwrap()
            .get::<LocalModelIntent>("model_local_intent", &thread_id.to_string())
            .await?
            .unwrap()
            .superseded_by,
        Some(11)
    );
    drop(service);
    let prepared = PreparedTeamHost::load(&fixture.config, "installation".into()).await?;
    let recovered = TeamHostService::new(prepared, Arc::downgrade(&fixture.thread_manager));
    recovered.reconcile_model_selection(thread_id).await?;
    let settings = fixture.codex.thread_settings_snapshot().await;
    assert_eq!(
        (
            settings.model,
            settings.reasoning_effort,
            settings.team_model_policy
        ),
        (
            "gpt-5.2".into(),
            None,
            TeamModelPolicy {
                model_origin: TeamSelectionOrigin::Explicit,
                effort_origin: TeamSelectionOrigin::Default,
                selection: newer.team_selection,
                superseded_revision: Some(10)
            },
        )
    );
    // Repeating the acknowledged revision must preserve the full desired
    // snapshot instead of recomputing Keep from the now-changed Core state.
    assert_eq!(
        recovered
            .journal_model_selection(
                thread_id,
                &coordinator_selection(11, "gpt-5.2", TeamSelection::Keep)
            )
            .await?,
        None
    );
    assert_eq!(model.requests().await, Vec::<Vec<u8>>::new());
    model.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn leave_fences_queued_coordinator_selection_and_rejoin_preserves_an_ordinary_local_edit()
-> anyhow::Result<()> {
    let (model, fixture, service, thread_id) = model_host().await?;
    let coordinator = coordinator_selection(
        10,
        "gpt-5.2-codex",
        TeamSelection::Value {
            value: "low".into(),
        },
    );
    service
        .journal_model_selection(thread_id, &coordinator)
        .await?;
    service
        .journal_model_selection(
            thread_id,
            &ThreadSettingsOverrides {
                model: Some("gpt-5.1-codex".into()),
                ..Default::default()
            },
        )
        .await?;
    let mut changed = service.gate.subscribe();
    service
        .control(TeamControlParams {
            control: TeamControlState {
                coordinator_id: "coordinator".into(),
                revision: 2,
                mode: TeamExecutionMode::Stopped,
            },
        })
        .await?;
    while !service.pending_stops().await?.is_empty() {
        changed.changed().await?;
    }
    service
        .leave(TeamLeaveParams {
            operation_id: Some("leave".into()),
            coordinator_id: "coordinator".into(),
            thread_id: thread_id.to_string(),
        })
        .await?;
    assert_eq!(service.gate.root(thread_id), None);
    assert!(
        service
            .journal_model_selection(thread_id, &coordinator)
            .await
            .is_err()
    );
    // Simulate an earlier acknowledged coordinator operation reaching Core only
    // after leave. The installed fence must reject it before changing values.
    assert!(
        fixture
            .codex
            .update_thread_settings(coordinator)
            .await
            .is_err()
    );
    fixture
        .codex
        .update_thread_settings(ThreadSettingsOverrides {
            model: Some("gpt-5.2".into()),
            effort: Some(Some(ReasoningEffort::High)),
            ..Default::default()
        })
        .await?;
    let outside = fixture.codex.thread_settings_snapshot().await;
    let store = service.store.get().unwrap();
    assert_eq!(
        store
            .get::<serde_json::Value>("model_selection_request", &thread_id.to_string())
            .await?,
        None
    );
    assert_eq!(
        store
            .get::<serde_json::Value>("model_local_intent", &thread_id.to_string())
            .await?,
        None
    );
    service
        .enroll(TeamEnrollParams {
            operation_id: Some("rejoin".into()),
            address: TeamAddress {
                machine_id: "machine".into(),
                server_id: "server".into(),
                thread_id: thread_id.to_string(),
            },
            alias: "rejoined".into(),
            control: TeamControlState {
                coordinator_id: "coordinator".into(),
                revision: 3,
                mode: TeamExecutionMode::Running,
            },
        })
        .await?;
    service.reconcile_model_selection(thread_id).await?;
    assert_eq!(fixture.codex.thread_settings_snapshot().await, outside);
    assert_eq!(model.requests().await, Vec::<Vec<u8>>::new());
    model.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn legacy_model_recovery_preserves_same_revision_user_edits_and_rejects_stale_or_superseded_selection()
-> anyhow::Result<()> {
    let (model, fixture, service, thread_id) = model_host().await?;
    let store = service.store.get().unwrap();
    let request = |settings: &ThreadSettingsOverrides| serde_json::json!({ "selection":settings.team_selection, "model":settings.model, "effort":settings.effort });
    let coordinator = coordinator_selection(10, "gpt-5.2-codex", TeamSelection::Default);
    store
        .put(
            "model_selection_request",
            &thread_id.to_string(),
            &request(&coordinator),
        )
        .await?;
    service.reconcile_model_selection(thread_id).await?;
    let applied = fixture.codex.thread_settings_snapshot().await;
    assert_eq!(
        (
            applied.model,
            applied.reasoning_effort,
            applied.team_model_policy
        ),
        (
            "gpt-5.2-codex".into(),
            None,
            TeamModelPolicy {
                model_origin: TeamSelectionOrigin::Explicit,
                effort_origin: TeamSelectionOrigin::Default,
                selection: coordinator.team_selection.clone(),
                superseded_revision: None
            },
        )
    );

    // Old hosts did not journal local intent. An already-applied revision must
    // still preserve a later user edit whose provenance lives only in Core.
    fixture
        .codex
        .update_thread_settings(ThreadSettingsOverrides {
            model: Some("gpt-5.1-codex".into()),
            effort: Some(Some(ReasoningEffort::High)),
            ..Default::default()
        })
        .await?;
    let edited = fixture.codex.thread_settings_snapshot().await;
    service.reconcile_model_selection(thread_id).await?;
    assert_eq!(fixture.codex.thread_settings_snapshot().await, edited);
    assert_eq!(
        store
            .get::<String>("model_recovery_error", &thread_id.to_string())
            .await?,
        None
    );

    let stale = coordinator_selection(9, "gpt-5.2", TeamSelection::Default);
    store
        .put(
            "model_selection_request",
            &thread_id.to_string(),
            &request(&stale),
        )
        .await?;
    let error = service
        .reconcile_model_selection(thread_id)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("stale"));
    assert_eq!(fixture.codex.thread_settings_snapshot().await, edited);
    assert!(
        store
            .get::<String>("model_recovery_error", &thread_id.to_string())
            .await?
            .unwrap()
            .contains("stale")
    );

    // A conflicting policy at the same revision is also rejected atomically.
    let conflicting = coordinator_selection(
        10,
        "gpt-5.2",
        TeamSelection::Value {
            value: "low".into(),
        },
    );
    store
        .put(
            "model_selection_request",
            &thread_id.to_string(),
            &request(&conflicting),
        )
        .await?;
    assert!(
        service
            .reconcile_model_selection(thread_id)
            .await
            .unwrap_err()
            .to_string()
            .contains("reused")
    );
    assert_eq!(fixture.codex.thread_settings_snapshot().await, edited);

    fixture
        .codex
        .update_thread_settings(ThreadSettingsOverrides {
            supersedes_team_selection: Some(11),
            effort: Some(Some(ReasoningEffort::High)),
            ..Default::default()
        })
        .await?;
    let superseded = fixture.codex.thread_settings_snapshot().await;
    store
        .put(
            "model_selection_request",
            &thread_id.to_string(),
            &request(&coordinator),
        )
        .await?;
    assert!(
        service
            .reconcile_model_selection(thread_id)
            .await
            .unwrap_err()
            .to_string()
            .contains("superseded")
    );
    assert_eq!(fixture.codex.thread_settings_snapshot().await, superseded);
    assert_eq!(model.requests().await, Vec::<Vec<u8>>::new());
    model.shutdown().await;
    Ok(())
}

struct BlockedUserModelJournal {
    gate: std::sync::Mutex<
        Option<(
            tokio::sync::oneshot::Sender<()>,
            tokio::sync::oneshot::Receiver<()>,
        )>,
    >,
}

impl codex_extension_api::ThreadLifecycleContributor<codex_core::config::Config>
    for BlockedUserModelJournal
{
    fn on_model_selection_requested<'a>(
        &'a self,
        _thread_id: ThreadId,
        _settings: &'a ThreadSettingsOverrides,
    ) -> codex_extension_api::ExtensionFuture<'a, std::result::Result<Option<i64>, String>> {
        Box::pin(async move {
            let (entered, release) = self
                .gate
                .lock()
                .expect("model journal gate")
                .take()
                .expect("one admitted user journal");
            entered.send(()).expect("waiting for admitted user journal");
            release.await.expect("release admitted user journal");
            Err("test journal deliberately rejected the admitted user edit".into())
        })
    }
}

#[tokio::test]
async fn legacy_model_recovery_does_not_wait_for_the_core_user_input_journal() -> anyhow::Result<()>
{
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let mut extensions = codex_extension_api::ExtensionRegistryBuilder::new();
    extensions.thread_lifecycle_contributor(Arc::new(BlockedUserModelJournal {
        gate: std::sync::Mutex::new(Some((entered_tx, release_rx))),
    }));
    let (model, fixture, service, thread_id) =
        model_host_with_extensions(Arc::new(extensions.build())).await?;
    let coordinator = coordinator_selection(10, "gpt-5.2-codex", TeamSelection::Default);
    service.store.get().unwrap().put("model_selection_request", &thread_id.to_string(), &serde_json::json!({ "selection":coordinator.team_selection, "model":coordinator.model, "effort":coordinator.effort })).await?;
    let user = tokio::spawn({
        let thread = Arc::clone(&fixture.codex);
        async move {
            thread
                .start_or_steer_turn(
                    codex_core::TurnInputRequest::user_input(Vec::new()).with_thread_settings(
                        ThreadSettingsOverrides {
                            model: Some("gpt-5.1-codex".into()),
                            ..Default::default()
                        },
                    ),
                )
                .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(/*secs*/ 10), entered_rx).await??;
    // The accepted input occupies the Core queue while its journal is pending.
    // Recovery must checkpoint independently instead of enqueueing behind it.
    let recovery = tokio::time::timeout(
        std::time::Duration::from_secs(/*secs*/ 10),
        service.reconcile_model_selection(thread_id),
    )
    .await;
    release_tx.send(()).expect("user journal is waiting");
    let user = user.await?.unwrap_err();
    assert!(
        user.to_string()
            .contains("test journal deliberately rejected")
    );
    recovery.map_err(|error| {
        anyhow::anyhow!("legacy recovery waited for the blocked Core input loop: {error}")
    })??;
    let recovered = fixture.codex.thread_settings_snapshot().await;
    assert_eq!(
        (recovered.model, recovered.reasoning_effort),
        ("gpt-5.2-codex".into(), None)
    );
    assert_eq!(model.requests().await, Vec::<Vec<u8>>::new());
    model.shutdown().await;
    Ok(())
}
