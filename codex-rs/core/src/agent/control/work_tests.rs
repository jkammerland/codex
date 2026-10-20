use super::*;
use crate::ThreadManager;
use crate::agent::api::AgentTurnOutcome;
use crate::config::test_config;
use codex_login::CodexAuth;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_thread_store::InMemoryAgentMailboxStore;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(AgentStatus::Errored("transport refused".into()), None, AgentTaskStatus::Failed; "failed before response created")]
#[test_case(AgentStatus::Interrupted, None, AgentTaskStatus::Held; "interrupted before response created")]
#[test_case(AgentStatus::Errored("denied".into()), Some(CodexErrorInfo::TooManyDenials), AgentTaskStatus::Held; "safety stop before response created")]
#[test_case(AgentStatus::Completed(Some("unaccepted output".into())), None, AgentTaskStatus::Queued; "no success without response created")]
#[tokio::test]
async fn attempted_assignments_settle_without_consuming_unsubmitted_or_later_mail(
    status: AgentStatus,
    error_info: Option<CodexErrorInfo>,
    expected_status: AgentTaskStatus,
) {
    let mut config = test_config().await;
    config
        .features
        .enable(codex_features::Feature::WakeOnAgentMessages)
        .unwrap();
    let manager = ThreadManager::with_models_provider_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
    );
    let target = manager
        .start_thread(crate::StartThreadOptions::new(config))
        .await
        .unwrap();
    let control = manager.agent_control();
    let store: Arc<dyn AgentMailboxStore> = Arc::new(InMemoryAgentMailboxStore::default());
    assert!(control.runtime.mail_store.set(Arc::clone(&store)).is_ok());
    let root_id = control.session_id.into();
    let target_id = target.thread_id;
    let task = AgentTaskRecord {
        task_id: "attempted".into(),
        root_id,
        owner_id: root_id,
        target_id,
        name: Some("worker".into()),
        status: AgentTaskStatus::Queued,
        turn_id: None,
        result_ref: None,
    };
    let envelope = AgentMailEnvelope {
        message_id: "request".into(),
        root_id,
        sender_id: root_id,
        recipient_id: target_id,
        kind: "assignment".into(),
        payload_json: "{}".into(),
        wake: true,
        task_id: Some(task.task_id.clone()),
    };
    store
        .accept_task(task.clone(), envelope.clone())
        .await
        .unwrap();
    store
        .mark_recorded(
            root_id,
            target_id,
            vec![envelope.message_id.clone()],
            "attempt".into(),
        )
        .await
        .unwrap();
    store
        .mark_attempted(
            root_id,
            target_id,
            vec![envelope.message_id.clone()],
            "attempt".into(),
        )
        .await
        .unwrap();

    // A task accepted after the selected request must not inherit its failure.
    let mut later = task.clone();
    later.task_id = "later".into();
    let mut later_mail = envelope.clone();
    later_mail.message_id = "later-request".into();
    later_mail.task_id = Some(later.task_id.clone());
    store.accept_task(later.clone(), later_mail).await.unwrap();
    let pending_before = store.pending(root_id, target_id).await.unwrap();
    assert!(
        pending_before
            .iter()
            .all(|record| record.submitted_turn_id.is_none())
    );

    control
        .settle_consumed_tasks(&AgentTurnOutcome {
            thread_id: target_id,
            turn_id: "attempt".into(),
            source: SessionSource::SubAgent(SubAgentSource::Other("worker".into())),
            parent_turn_id: None,
            initiating_agent_path: None,
            status,
            error_info,
        })
        .await
        .unwrap();
    let completions = store.pending(root_id, root_id).await.unwrap();
    let mut expected = task;
    expected.status = expected_status;
    expected.turn_id = Some("attempt".into());
    if expected_status == AgentTaskStatus::Failed {
        assert_eq!(completions.len(), 1);
        assert_eq!(
            completions[0].envelope.task_id,
            Some(expected.task_id.clone())
        );
        expected.result_ref = Some(completions[0].envelope.message_id.clone());
    } else {
        assert_eq!(completions, vec![]);
    }
    assert_eq!(
        store
            .tasks(root_id, AgentTaskFilter::default())
            .await
            .unwrap()
            .tasks,
        vec![expected, later]
    );
    assert_eq!(
        store.pending(root_id, target_id).await.unwrap(),
        pending_before
    );
    target.thread.shutdown_and_wait().await.unwrap();
}
