use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Result;
use codex_app_server_protocol::*;
use hmac::Hmac;
use hmac::Mac;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use sha2::Sha256;
use uuid::Uuid;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

use crate::config::GiteaConfig;
use crate::coordinator::Coordinator;
use crate::model::*;

async fn webhook(coordinator: &Coordinator, payload: &Value) -> Result<Vec<String>> {
    let bytes = serde_json::to_vec(payload)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(b"test-secret")?;
    mac.update(&bytes);
    let signature = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    coordinator
        .gitea_webhook(
            &signature,
            "issue_comment",
            &Uuid::new_v4().to_string(),
            &bytes,
        )
        .await
}

#[tokio::test]
async fn signed_ingress_and_uncertain_reply_survive_restart_without_an_echo_loop() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let server = MockServer::start().await;
    let mut manifest = crate::tests::manifest(directory.path());
    let token_file = directory.path().join("gitea-token");
    let secret_file = directory.path().join("gitea-secret");
    tokio::fs::write(&token_file, "test-token").await?;
    tokio::fs::write(&secret_file, "test-secret\n").await?;
    manifest.gitea = Some(GiteaConfig {
        url: server.uri(),
        token_file,
        webhook_secret_file: secret_file,
        repositories: vec!["example/project".into()],
        assignees: Default::default(),
    });
    let coordinator = Coordinator::open(manifest.clone()).await?;
    let recipient = crate::tests::agent(&coordinator, "reviewer").await?;
    let payload = json!({"action":"created", "repository":{"full_name":"example/project"},
        "issue":{"number":12,"title":"Review"}, "sender":{"login":"author"},
        "comment":{"id":42, "updated_at":"2026-01-01T00:00:00Z", "body":"@reviewer please review"}});

    let messages = webhook(&coordinator, &payload).await?;
    assert_eq!(messages.len(), 1);
    assert_eq!(webhook(&coordinator, &payload).await?, messages);
    let mut foreign = payload.clone();
    foreign["repository"]["full_name"] = "other/project".into();
    assert!(webhook(&coordinator, &foreign).await.is_err());
    assert!(
        coordinator
            .gitea_webhook(
                "invalid",
                "issue_comment",
                &Uuid::new_v4().to_string(),
                b"{}"
            )
            .await
            .is_err()
    );
    assert_eq!(coordinator.snapshot().await?.messages.len(), 1);

    let original = coordinator
        .store
        .get::<MessageRecord>("message", &messages[0])
        .await?
        .unwrap();
    let mut reply = coordinator
        .operator_message(
            Some(original.message.sender),
            "@reviewer completed".into(),
            TeamMessageKind::Reply,
            Some(messages[0].clone()),
            None,
        )
        .await?;
    reply.sender = recipient;
    let reply_id = reply.id.clone();
    coordinator
        .ingest(reply, TeamDeliveryMode::NextTurn)
        .await?;
    coordinator
        .execute(TeamRequest::Control {
            mode: TeamExecutionMode::Paused,
        })
        .await?;
    coordinator.publish().await?;
    coordinator.publish_gitea().await?;
    assert!(server.received_requests().await.unwrap().is_empty());

    // The service creates the comment, but loses its successful response. The
    // coordinator must inspect that conversation before any explicit retry.
    let created_body = Arc::new(Mutex::new(String::new()));
    let capture = created_body.clone();
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/example/project/issues/12/comments"))
        .respond_with(move |request: &wiremock::Request| {
            let request: Value = serde_json::from_slice(&request.body).unwrap();
            *capture.lock().unwrap() = request["body"].as_str().unwrap().to_owned();
            ResponseTemplate::new(502)
        })
        .expect(1)
        .mount(&server)
        .await;
    coordinator
        .execute(TeamRequest::Control {
            mode: TeamExecutionMode::Running,
        })
        .await?;
    coordinator.publish().await?;
    coordinator.publish_gitea().await?;
    assert!(
        coordinator
            .store
            .get::<MessageRecord>("message", &reply_id)
            .await?
            .unwrap()
            .error
            .is_some()
    );
    drop(coordinator);

    let coordinator = Coordinator::open(manifest).await?;
    let comment_url = format!("{}/example/project/issues/12#issuecomment-99", server.uri());
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/example/project/issues/12/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "id":99, "body":created_body.lock().unwrap().clone(), "html_url":comment_url
        }])))
        .expect(1)
        .mount(&server)
        .await;
    coordinator
        .execute(TeamRequest::Retry {
            message_id: reply_id.clone(),
        })
        .await?;
    coordinator.publish_gitea().await?;
    coordinator.publish_gitea().await?;
    let stored = coordinator
        .store
        .get::<MessageRecord>("message", &reply_id)
        .await?
        .unwrap();
    assert_eq!(stored.error, None);
    assert_eq!(stored.external_url, Some(comment_url));
    let mut echo = payload;
    echo["comment"]["body"] = created_body.lock().unwrap().clone().into();
    assert_eq!(webhook(&coordinator, &echo).await?, Vec::<String>::new());
    server.verify().await;
    Ok(())
}

#[tokio::test]
async fn project_discovery_uses_the_configured_instance_capability() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let server = MockServer::start().await;
    let token_file = directory.path().join("token");
    tokio::fs::write(&token_file, "test-token").await?;
    let mut manifest = crate::tests::manifest(directory.path());
    manifest.gitea = Some(GiteaConfig {
        url: server.uri(),
        token_file,
        webhook_secret_file: directory.path().join("secret"),
        repositories: vec!["example/project".into()],
        assignees: Default::default(),
    });
    let coordinator = Coordinator::open(manifest).await?;
    Mock::given(method("GET"))
        .and(path("/api/v1/version"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"version":"test"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/example/project/projects"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    let inspected = coordinator.gitea_inspect("example/project").await?;
    assert_eq!(inspected["projects"]["available"], false);
    assert_eq!(inspected["projectWebhooks"], "unverified");
    assert!(coordinator.gitea_inspect("other/project").await.is_err());
    server.verify().await;
    Ok(())
}
