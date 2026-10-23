//! Compact discovery preserves full details and durable arrival order on request.

use anyhow::Result;
use anyhow::bail;
use codex_app_server_protocol::*;
use serde_json::Value;
use serde_json::json;

use crate::TeamHostService;

#[derive(serde::Serialize, serde::Deserialize, Default)]
pub(crate) struct CachedDirectory {
    #[serde(flatten)]
    pub(crate) directory: TeamDirectorySnapshot,
    #[serde(default, rename = "lastSynchronizedAt")]
    pub(crate) last_synchronized_at: Option<i64>,
}

impl TeamHostService {
    pub(crate) async fn resolve_team_target(
        &self,
        target: &TeamAgentTarget,
    ) -> Result<(TeamAddress, Option<String>)> {
        let store = self
            .store
            .get()
            .ok_or_else(|| anyhow::anyhow!("team host is not enrolled"))?;
        let directory = store
            .get::<TeamDirectorySnapshot>("directory_cache", "current")
            .await?
            .unwrap_or_default();
        match target {
            TeamAgentTarget::Address(address) => Ok((
                address.clone(),
                directory
                    .data
                    .iter()
                    .find(|entry| entry.address == *address)
                    .map(|entry| entry.alias.clone()),
            )),
            TeamAgentTarget::Alias(alias) => {
                let mut matches = directory
                    .data
                    .into_iter()
                    .filter(|entry| entry.alias == *alias);
                let entry = matches.next().ok_or_else(|| anyhow::anyhow!("no enrolled teammate has exact alias {alias:?}; use directory to find its address"))?;
                if matches.next().is_some() {
                    bail!("alias {alias:?} is ambiguous; use a canonical address");
                }
                Ok((entry.address, Some(entry.alias)))
            }
        }
    }

    pub(crate) async fn agent_directory(
        &self,
        cursor: Option<String>,
        limit: Option<u32>,
        query: Option<String>,
        details: bool,
    ) -> Result<Value> {
        let store = self
            .store
            .get()
            .ok_or_else(|| anyhow::anyhow!("team host is not enrolled"))?;
        let cached = store
            .get::<CachedDirectory>("directory_cache", "current")
            .await?
            .unwrap_or_default();
        let terms = query
            .as_deref()
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_lowercase)
            .collect::<Vec<_>>();
        let mut entries = Vec::new();
        for entry in cached.directory.data {
            let key = serde_json::to_string(&entry.address)?;
            let haystack = format!(
                "{} {} {}",
                entry.alias,
                key,
                serde_json::to_string(&entry.discovery)?
            )
            .to_lowercase();
            if cursor.as_ref().is_some_and(|cursor| &key <= cursor)
                || !terms.iter().all(|term| haystack.contains(term))
            {
                continue;
            }
            let value = if details {
                serde_json::to_value(entry)?
            } else {
                json!({
                    "address":entry.address,"alias":entry.alias,"availability":entry.discovery.availability,"environments":entry.discovery.environments
                })
            };
            entries.push((key, value));
        }
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        let mut result = page(entries, limit);
        result["revision"] = json!(cached.directory.revision);
        result["lastSynchronizedAt"] = json!(cached.last_synchronized_at);
        Ok(result)
    }

    pub(crate) async fn agent_inbox(
        &self,
        thread_id: &str,
        cursor: Option<String>,
        limit: Option<u32>,
    ) -> Result<Value> {
        let store = self
            .store
            .get()
            .ok_or_else(|| anyhow::anyhow!("team host is not enrolled"))?;
        let deliveries = store.deliveries(thread_id).await?;
        let start = if let Some(id) = cursor
            .as_deref()
            .and_then(|cursor| cursor.strip_prefix("arrival:"))
        {
            deliveries
                .iter()
                .position(|(delivery, _)| delivery.id == id)
                .map(|index| index + 1)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "inbox cursor is no longer available; restart from the first page"
                    )
                })?
        } else {
            0
        };
        let mut entries = Vec::new();
        for (delivery, receipt) in deliveries.into_iter().skip(start) {
            // Accept legacy UUID cursors, while new pages follow receiver arrival order.
            if cursor.as_ref().is_some_and(|cursor| {
                !cursor.starts_with("arrival:") && delivery.message.id <= *cursor
            }) {
                continue;
            }
            let key = format!("arrival:{}", delivery.id);
            let mut message = serde_json::to_value(delivery.message)?;
            message["deliveryId"] = json!(delivery.id);
            message["delivery"] = json!(delivery.mode);
            message["receipt"] = json!(receipt);
            entries.push((key, message));
        }
        let mut result = page(entries, limit);
        result["freshness"] = json!("current");
        Ok(result)
    }
}

pub(crate) fn page<T: serde::Serialize>(mut data: Vec<(String, T)>, limit: Option<u32>) -> Value {
    let limit = limit.unwrap_or(20).clamp(1, 1000) as usize;
    let next_cursor = if data.len() > limit {
        data.truncate(limit);
        data.last().map(|entry| entry.0.clone())
    } else {
        None
    };
    json!({"data":data.into_iter().map(|(_,value)|value).collect::<Vec<_>>(),"nextCursor":next_cursor})
}
