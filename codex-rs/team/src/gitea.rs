//! Optional signed issue/PR ingress. Project APIs are discovered on demand.

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::*;
use hmac::Hmac;
use hmac::Mac;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use sha2::Sha256;
use uuid::Uuid;

use crate::config::GiteaConfig;
use crate::coordinator::Coordinator;
use crate::model::MessageRecord;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct IntegrationIdentity {
    id: String,
    base: String,
}

impl IntegrationIdentity {
    pub(crate) fn configured(config: &GiteaConfig) -> Result<Self> {
        let base = url::Url::parse(&format!("{}/", config.url.trim_end_matches('/')))?.to_string();
        Ok(Self {
            id: Uuid::new_v5(&Uuid::NAMESPACE_URL, base.as_bytes()).to_string(),
            base,
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Origin {
    #[serde(default)]
    pub integration: Option<IntegrationIdentity>,
    pub repository: String,
    pub number: u64,
    pub url: String,
}

pub(crate) struct GiteaClient {
    pub http: reqwest::Client,
    pub base: url::Url,
    token: String,
}

impl GiteaClient {
    pub async fn open(config: &GiteaConfig) -> Result<Self> {
        Ok(Self {
            http: codex_http_client::build_reqwest_client_with_custom_ca(
                reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()),
            )?,
            base: url::Url::parse(&format!("{}/", config.url.trim_end_matches('/')))?,
            token: crate::transport::token(Some(&config.token_file))
                .await?
                .context("Gitea token is missing")?,
        })
    }

    pub fn request(&self, method: reqwest::Method, path: &str) -> Result<reqwest::RequestBuilder> {
        Ok(self
            .http
            .request(method, self.base.join(&format!("api/v1/{path}"))?)
            .header("authorization", format!("token {}", self.token)))
    }

    pub async fn list(&self, path: &str) -> Result<Vec<Value>> {
        let mut data = Vec::new();
        let mut page = 1_u64;
        loop {
            let response = self
                .request(reqwest::Method::GET, path)?
                .query(&[("page", page), ("limit", 100)])
                .send()
                .await?
                .error_for_status()?;
            let more = response
                .headers()
                .get("link")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.contains("rel=\"next\""));
            let entries = response.json::<Vec<Value>>().await?;
            let count = entries.len();
            data.extend(entries);
            if !more && count < 100 {
                break;
            }
            page = page.checked_add(1).context("Gitea pagination overflow")?;
        }
        Ok(data)
    }
}

pub(crate) fn verify_signature(secret: &[u8], signature: &str, body: &[u8]) -> Result<()> {
    ensure!(
        !secret.is_empty() && signature.len() == 64,
        "invalid webhook signature"
    );
    let bytes = signature
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair)?;
            Ok(u8::from_str_radix(text, 16)?)
        })
        .collect::<Result<Vec<_>>>()?;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).context("invalid webhook secret")?;
    mac.update(body);
    mac.verify_slice(&bytes)
        .context("invalid webhook signature")?;
    Ok(())
}

fn mentions(body: &str, alias: &str) -> bool {
    let mention = format!("@{alias}");
    body.match_indices(&mention).any(|(offset, _)| {
        let continuation = |ch: char| ch.is_alphanumeric() || matches!(ch, '_' | '-' | '/' | '.');
        body[..offset]
            .chars()
            .next_back()
            .is_none_or(|ch| !continuation(ch))
            && body[offset + mention.len()..]
                .chars()
                .next()
                .is_none_or(|ch| !continuation(ch))
    })
}

impl Coordinator {
    pub(crate) async fn gitea_webhook(
        &self,
        signature: &str,
        event: &str,
        delivery: &str,
        body: &[u8],
    ) -> Result<Vec<String>> {
        let config = self
            .manifest
            .gitea
            .as_ref()
            .context("Gitea integration is disabled")?;
        let secret = tokio::fs::read_to_string(&config.webhook_secret_file).await?;
        verify_signature(secret.trim_end().as_bytes(), signature, body)?;
        ensure!(
            Uuid::parse_str(delivery).is_ok(),
            "webhook delivery id must be a UUID"
        );
        let payload: Value = serde_json::from_slice(body)?;
        let repository = payload
            .pointer("/repository/full_name")
            .and_then(Value::as_str)
            .context("webhook repository is missing")?;
        ensure!(
            config
                .repositories
                .iter()
                .any(|selected| selected == repository),
            "repository is not selected for team integration"
        );
        let action = payload
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let addressed_event = match event {
            "issues" | "pull_request" => matches!(action, "opened" | "edited" | "reopened"),
            "issue_comment" | "pull_request_comment" => matches!(action, "created" | "edited"),
            "issue_assign" | "pull_request_assign" => action == "assigned",
            "pull_request_review_request" => action == "review_requested",
            _ => false,
        };
        if !addressed_event {
            return Ok(Vec::new());
        }
        let issue = payload
            .get("issue")
            .or_else(|| payload.get("pull_request"))
            .context("issue or pull request is missing")?;
        let number = issue
            .get("number")
            .or_else(|| payload.get("number"))
            .and_then(Value::as_u64)
            .context("issue number is missing")?;
        let content = payload.get("comment").unwrap_or(issue);
        let text = content
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if self.gitea_is_mirror(repository, number, text).await? {
            return Ok(Vec::new());
        }
        let base = url::Url::parse(&format!("{}/", config.url.trim_end_matches('/')))?;
        let kind = if payload.get("pull_request").is_some()
            || payload.get("is_pull").and_then(Value::as_bool) == Some(true)
        {
            "pulls"
        } else {
            "issues"
        };
        let integration = IntegrationIdentity::configured(config)?;
        let origin = Origin {
            integration: Some(integration.clone()),
            repository: repository.into(),
            number,
            url: base
                .join(&format!("{repository}/{kind}/{number}"))?
                .to_string(),
        };
        let mut targets = Vec::new();
        for agent in self.agents().await? {
            let assigned = matches!(action, "assigned" | "opened" | "review_requested")
                && issue
                    .get("assignees")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .chain(payload.get("requested_reviewer"))
                    .filter_map(|assignee| assignee.get("login").and_then(Value::as_str))
                    .any(|login| config.assignees.get(login) == Some(&agent.enrollment.alias));
            if !mentions(text, &agent.enrollment.alias) && !assigned {
                continue;
            }
            let semantic_key = json!([
                integration,
                repository,
                number,
                event,
                action,
                content.get("id"),
                content.get("updated_at"),
                text,
                agent.enrollment.address
            ]);
            let id =
                Uuid::new_v5(&Uuid::NAMESPACE_URL, semantic_key.to_string().as_bytes()).to_string();
            if self
                .store
                .get::<MessageRecord>("message", &id)
                .await?
                .is_some()
            {
                targets.push(id);
                continue;
            }
            let title = issue
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let author = payload
                .pointer("/sender/login")
                .and_then(Value::as_str)
                .unwrap_or("Gitea contributor");
            let mut message = self
                .operator_message(
                    Some(agent.enrollment.address),
                    format!(
                        "Gitea {repository}#{number}: {title}\nFrom {author}\n{}\n\n{text}",
                        origin.url
                    ),
                    TeamMessageKind::Request,
                    None,
                    None,
                )
                .await?;
            message.id = id.clone();
            message.sender = TeamAddress {
                machine_id: "gitea".into(),
                server_id: integration.id.clone(),
                thread_id: format!("{repository}#{number}"),
            };
            // Acknowledge the webhook only after both origin and message are
            // durable. A crash between writes reuses the same semantic ID.
            self.store.put("gitea_origin", &id, &origin).await?;
            self.ingest(message, TeamDeliveryMode::NextTurn).await?;
            targets.push(id);
        }
        Ok(targets)
    }

    pub(crate) async fn gitea_inspect(&self, repository: &str) -> Result<Value> {
        let config = self
            .manifest
            .gitea
            .as_ref()
            .context("Gitea integration is disabled")?;
        ensure!(
            config
                .repositories
                .iter()
                .any(|selected| selected == repository),
            "repository is not selected for team integration"
        );
        let client = GiteaClient::open(config).await?;
        let version: Value = client
            .request(reqwest::Method::GET, "version")?
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let response = client
            .request(
                reqwest::Method::GET,
                &format!("repos/{repository}/projects"),
            )?
            .query(&[("limit", 1)])
            .send()
            .await?;
        let projects = if matches!(
            response.status(),
            reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::METHOD_NOT_ALLOWED
        ) {
            json!({"available":false, "detail":"This instance or repository does not expose the project API."})
        } else {
            response.error_for_status()?;
            json!({"available":true, "data":client.list(&format!("repos/{repository}/projects")).await?})
        };
        Ok(
            json!({"version":version, "repository":repository, "projects":projects, "projectWebhooks":"unverified"}),
        )
    }
}

#[cfg(test)]
#[path = "gitea_tests.rs"]
mod tests;
