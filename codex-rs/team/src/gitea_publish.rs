use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::TeamExecutionMode;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;

use crate::coordinator::Coordinator;
use crate::gitea::GiteaClient;
use crate::gitea::Origin;
use crate::model::MessageRecord;

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
enum PublicationState {
    Ready,
    Sending,
    Published,
}

#[derive(Serialize, Deserialize)]
struct Publication {
    origin: Origin,
    marker: String,
    body: String,
    state: PublicationState,
    comment_id: Option<u64>,
    url: Option<String>,
}

pub(crate) fn start(coordinator: &Arc<Coordinator>) {
    if coordinator.manifest.gitea.is_none() {
        return;
    }
    let coordinator = coordinator.clone();
    let mut changed = coordinator.changed.subscribe();
    tokio::spawn(async move {
        loop {
            if let Err(error) = coordinator.publish_gitea().await {
                tracing::error!(%error, "Gitea outbox requires attention");
            }
            if changed.changed().await.is_err() {
                break;
            }
        }
    });
}

impl Coordinator {
    pub(crate) async fn gitea_is_mirror(
        &self,
        repository: &str,
        number: u64,
        body: &str,
    ) -> Result<bool> {
        if !body.contains("<!-- codex-team:") {
            return Ok(false);
        }
        Ok(self
            .store
            .list::<Publication>("gitea_publication", "", u32::MAX)
            .await?
            .iter()
            .any(|(_, publication)| {
                publication.origin.repository == repository
                    && publication.origin.number == number
                    && publication.body == body
            }))
    }

    #[allow(
        clippy::await_holding_invalid_type,
        reason = "serializes durable state changes and dispatch across asynchronous operations"
    )]
    async fn publish_gitea(&self) -> Result<()> {
        for (id, record) in self
            .store
            .list::<MessageRecord>("message", "", u32::MAX)
            .await?
        {
            if !record.published
                || record.error.is_some()
                || record.external_url.is_some()
                || record
                    .message
                    .recipient
                    .as_ref()
                    .is_none_or(|recipient| recipient.machine_id != "gitea")
            {
                continue;
            }
            if self.current_control().await?.mode != TeamExecutionMode::Running {
                break;
            }
            if let Err(error) = self.publish_gitea_reply(&record).await {
                let _mutation = self.mutations.lock().await;
                let mut record = self
                    .store
                    .get::<MessageRecord>("message", &id)
                    .await?
                    .context("reply disappeared")?;
                record.error = Some(format!(
                    "Gitea reply requires reconciliation: {error}. Use team retry after inspecting the conversation."
                ));
                self.store.put("message", &id, &record).await?;
                self.notify();
            }
        }
        Ok(())
    }

    #[allow(
        clippy::await_holding_invalid_type,
        reason = "serializes durable state changes and dispatch across asynchronous operations"
    )]
    async fn publish_gitea_reply(&self, record: &MessageRecord) -> Result<()> {
        let config = self
            .manifest
            .gitea
            .as_ref()
            .context("Gitea integration is disabled")?;
        let id = &record.message.id;
        let mut publication = match self
            .store
            .get::<Publication>("gitea_publication", id)
            .await?
        {
            Some(publication) => publication,
            None => {
                let origin = self
                    .store
                    .get::<Origin>(
                        "gitea_origin",
                        record
                            .message
                            .reply_to
                            .as_deref()
                            .context("reply origin is missing")?,
                    )
                    .await?
                    .context("Gitea origin is missing")?;
                ensure!(
                    config.repositories.contains(&origin.repository),
                    "reply repository is no longer selected"
                );
                let marker = format!(
                    "<!-- codex-team:{}:{id} -->",
                    self.current_control().await?.coordinator_id
                );
                let alias = self
                    .agents()
                    .await?
                    .into_iter()
                    .find(|agent| agent.enrollment.address == record.message.sender)
                    .map(|agent| agent.enrollment.alias)
                    .unwrap_or_else(|| "Team operator".into());
                let publication = Publication {
                    origin,
                    body: format!("**{alias}**\n\n{}\n\n{marker}", record.message.body),
                    marker,
                    state: PublicationState::Ready,
                    comment_id: None,
                    url: None,
                };
                self.store
                    .put("gitea_publication", id, &publication)
                    .await?;
                publication
            }
        };
        ensure!(
            config.repositories.contains(&publication.origin.repository),
            "repository is no longer enabled for Gitea publication"
        );
        let client = GiteaClient::open(config).await?;
        let path = format!(
            "repos/{}/issues/{}/comments",
            publication.origin.repository, publication.origin.number
        );
        if publication.state == PublicationState::Sending {
            let existing = client.list(&path).await?.into_iter().find(|comment| {
                comment.get("body").and_then(Value::as_str) == Some(&publication.body)
            });
            if let Some(comment) = existing {
                publication.comment_id = comment.get("id").and_then(Value::as_u64);
                publication.url = comment
                    .get("html_url")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                publication.state = PublicationState::Published;
            } else {
                ensure!(
                    self.store
                        .get::<bool>("gitea_retry", id)
                        .await?
                        .unwrap_or(false),
                    "an earlier POST may have reached Gitea but its comment is not yet confirmed"
                );
                publication.state = PublicationState::Ready;
            }
        }
        if publication.state == PublicationState::Ready {
            // Persist admission before the external side effect. A restart must
            // inspect the original conversation before it can retry this POST.
            let _mutation = self.mutations.lock().await;
            if self.current_control().await?.mode != TeamExecutionMode::Running {
                return Ok(());
            }
            publication.state = PublicationState::Sending;
            self.store
                .put("gitea_publication", id, &publication)
                .await?;
            self.store.delete("gitea_retry", id).await?;
            drop(_mutation);
            let comment: Value = client
                .request(reqwest::Method::POST, &path)?
                .json(&json!({"body":publication.body}))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            publication.comment_id = comment.get("id").and_then(Value::as_u64);
            publication.url = comment
                .get("html_url")
                .and_then(Value::as_str)
                .map(str::to_owned);
            publication.state = PublicationState::Published;
        }
        ensure!(
            publication.comment_id.is_some(),
            "Gitea did not return a comment id"
        );
        self.store
            .put("gitea_publication", id, &publication)
            .await?;
        let _mutation = self.mutations.lock().await;
        let mut current = self
            .store
            .get::<MessageRecord>("message", id)
            .await?
            .context("reply disappeared")?;
        current.external_url = Some(publication.url.unwrap_or(publication.origin.url));
        current.error = None;
        self.store.put("message", id, &current).await?;
        self.notify();
        Ok(())
    }
}

#[cfg(test)]
#[path = "gitea_publish_tests.rs"]
mod tests;
