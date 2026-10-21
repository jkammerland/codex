//! Exercises listener attachment against real native mailbox and outgoing notification state.

use super::super::thread_agent_work;
use super::super::thread_lifecycle::EnsureConversationListenerResult;
use super::super::thread_lifecycle::ListenerTaskContext;
use super::super::thread_lifecycle::ensure_conversation_listener;
use super::super::thread_summary::thread_started_notification;
use super::build_thread_from_loaded_snapshot;
use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::OutgoingEnvelope;
use crate::outgoing_message::OutgoingMessage;
use crate::outgoing_message::OutgoingMessageSender;
use crate::skills_watcher::SkillsWatcher;
use crate::thread_state::ConnectionCapabilities;
use crate::thread_state::ThreadStateManager;
use crate::thread_status::ThreadWatchManager;
use anyhow::Context;
use anyhow::Result;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::TurnStatus;
use codex_core::TurnInputRequest;
use codex_core::TurnInputSubmission;
use codex_core::config::Config;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ToolLifecycleContributor;
use codex_extension_api::ToolLifecycleFuture;
use codex_extension_api::ToolStartInput;
use codex_features::Feature;
use codex_protocol::openai_models::ToolMode;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::time::timeout;

#[derive(Default)]
struct MailToolGate {
    entered: Notify,
    release: Notify,
}

impl ToolLifecycleContributor for MailToolGate {
    fn on_tool_start<'a>(&'a self, input: ToolStartInput<'a>) -> ToolLifecycleFuture<'a> {
        Box::pin(async move {
            if input.call_id == "hold-after-mail" {
                self.entered.notify_one();
                self.release.notified().await;
            }
        })
    }
}

#[tokio::test]
async fn delayed_listener_publishes_mail_accepted_after_initial_snapshot() -> Result<()> {
    let server = responses::start_mock_server().await;
    let requests = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("queue-mail"),
                responses::ev_function_call(
                    "send-mail",
                    "send_message",
                    &json!({"target": "/root", "message": "queued update", "wake": false})
                        .to_string(),
                ),
                responses::ev_function_call_with_namespace(
                    "hold-after-mail",
                    "collaboration",
                    "list_agents",
                    "{}",
                ),
                responses::ev_completed("queue-mail"),
            ]),
            responses::sse_completed("after-test"),
        ],
    )
    .await;
    let tool_gate = Arc::new(MailToolGate::default());
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    extensions.tool_lifecycle_contributor(tool_gate.clone());
    let test = test_codex()
        .with_model("gpt-5.6-sol")
        .with_model_info_override("gpt-5.6-sol", |model| {
            model.tool_mode = Some(ToolMode::Direct);
            model.multi_agent_version = Some(MultiAgentVersion::V2);
        })
        .with_extensions(Arc::new(extensions.build()))
        .with_config(|config| {
            config.ephemeral = true;
            for feature in [
                Feature::Collab,
                Feature::MultiAgentV2,
                Feature::WakeOnAgentMessages,
            ] {
                config
                    .features
                    .enable(feature)
                    .expect("enable native agent mail");
            }
        })
        .build_with_auto_env(&server)
        .await?;
    let thread_id = test.codex.startup_metadata().thread_id;
    let connection_id = ConnectionId(1);
    let thread_state_manager = ThreadStateManager::new();
    thread_state_manager
        .connection_initialized(connection_id, ConnectionCapabilities::default())
        .await;
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel(64);
    let outgoing = Arc::new(OutgoingMessageSender::new(
        outgoing_tx,
        codex_analytics::AnalyticsEventsClient::disabled(),
    ));
    let thread_watch_manager = ThreadWatchManager::new_with_outgoing(Arc::clone(&outgoing));
    thread_watch_manager
        .upsert_thread_silently(&thread_id.to_string())
        .await;
    let skills_watcher = SkillsWatcher::new(
        test.thread_manager.skills_service(),
        &test.config.codex_home,
        Arc::clone(&outgoing),
    );
    let listener_gate = Arc::new(Notify::new());
    let result: Result<()> = async {
        let context = ListenerTaskContext {
            thread_manager: Arc::clone(&test.thread_manager),
            thread_state_manager: thread_state_manager.clone(),
            outgoing: Arc::clone(&outgoing),
            pending_thread_unloads: Default::default(),
            thread_watch_manager,
            codex_home: test.codex_home_path().to_path_buf(),
            skills_watcher: Arc::clone(&skills_watcher),
            turn_cost_worker: None,
            listener_start_gate: Some(Arc::clone(&listener_gate)),
        };
        let attached = ensure_conversation_listener(
            context,
            thread_id,
            connection_id,
            /*raw_events_enabled*/ false,
        )
        .await
        .map_err(|error| anyhow::anyhow!(error.message))?;
        assert!(matches!(
            attached,
            EnsureConversationListenerResult::Attached
        ));

        // Match start/fork publication: identity precedes the initial work snapshot.
        let mut thread = build_thread_from_loaded_snapshot(
            thread_id,
            &test.codex.config_snapshot().await,
            &test.codex,
        );
        let initial = thread_agent_work::snapshot(&test.codex)
            .await
            .context("initial work")?;
        assert_eq!((initial.pending_mail, &initial.waiting_tasks), (0, &None));
        thread.agent_work = Some(initial.clone());
        outgoing
            .send_server_notification(ServerNotification::ThreadStarted(
                thread_started_notification(thread),
            ))
            .await;
        thread_agent_work::publish(thread_id, &test.codex, &outgoing).await;
        let mut identity_published = false;
        timeout(Duration::from_secs(10), async {
            loop {
                let envelope = outgoing_rx.recv().await.context("initial notification")?;
                if let OutgoingEnvelope::Broadcast {
                    message: OutgoingMessage::AppServerNotification(notification),
                } = envelope
                {
                    match notification.notification {
                        ServerNotification::ThreadStarted(started) => {
                            assert_eq!(started.thread.id, thread_id.to_string());
                            identity_published = true;
                        }
                        ServerNotification::ThreadAgentWorkUpdated(update) => {
                            assert!(identity_published, "work must follow thread identity");
                            assert_eq!(update.thread_id, thread_id.to_string());
                            assert_eq!(update.agent_work, Some(initial.clone()));
                            break Ok::<_, anyhow::Error>(());
                        }
                        _ => {}
                    }
                }
            }
        })
        .await??;

        // Queue mail while the listener cannot run. The tool gate prevents another
        // inference from consuming it or producing a second mailbox mutation.
        let mut work_updates = test.codex.watch_agent_work();
        let TurnInputSubmission::Started { turn_id, .. } = test
            .codex
            .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Queue mail and stop at the tool gate.".to_string(),
                text_elements: Vec::new(),
            }]))
            .await?
        else {
            anyhow::bail!("expected a fresh root turn");
        };
        timeout(Duration::from_secs(10), tool_gate.entered.notified()).await?;
        let pending = timeout(Duration::from_secs(10), async {
            loop {
                let work = thread_agent_work::snapshot(&test.codex)
                    .await
                    .context("queued work")?;
                if work.pending_mail == 1 {
                    break Ok::<_, anyhow::Error>(work);
                }
                work_updates.changed().await?;
            }
        })
        .await??;
        assert!(pending.revision > initial.revision);
        assert_eq!(requests.requests().len(), 1);

        listener_gate.notify_one();
        let delivered = timeout(Duration::from_secs(10), async {
            loop {
                let envelope = outgoing_rx.recv().await.context("changed notification")?;
                if let OutgoingEnvelope::Broadcast {
                    message: OutgoingMessage::AppServerNotification(notification),
                } = envelope
                    && let ServerNotification::ThreadAgentWorkUpdated(update) =
                        notification.notification
                {
                    break Ok::<_, anyhow::Error>(update);
                }
            }
        })
        .await??;
        assert_eq!(delivered.thread_id, thread_id.to_string());
        assert_eq!(delivered.agent_work, Some(pending));
        assert_eq!(requests.requests().len(), 1);

        // Let the resumed turn finish before shutdown can cancel its second request.
        tool_gate.release.notify_one();
        let completed = timeout(Duration::from_secs(10), async {
            loop {
                let envelope = outgoing_rx.recv().await.context("turn completion")?;
                if let OutgoingEnvelope::ToConnection {
                    connection_id: recipient,
                    message: OutgoingMessage::AppServerNotification(notification),
                    ..
                } = envelope
                    && let ServerNotification::TurnCompleted(completed) = notification.notification
                {
                    assert_eq!(recipient, connection_id);
                    break Ok::<_, anyhow::Error>(completed);
                }
            }
        })
        .await??;
        assert_eq!(
            (
                completed.thread_id,
                completed.turn.id,
                completed.turn.status,
                completed.turn.error,
            ),
            (thread_id.to_string(), turn_id, TurnStatus::Completed, None)
        );
        assert_eq!(requests.requests().len(), 2);
        Ok(())
    }
    .await;

    listener_gate.notify_one();
    tool_gate.release.notify_one();
    thread_state_manager.remove_thread_state(thread_id).await;
    skills_watcher.shutdown();
    test.thread_manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    result
}
