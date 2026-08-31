//! Selects a smaller resume context for legacy subagent rollouts.
//!
//! Legacy subagents copied the parent's full rollout into every child rollout. Migrating those
//! files verbatim can preserve gigabytes of duplicated history even though a resumed subagent only
//! needs its latest bounded model context.
//!
//! This module reverse-scans the legacy rollout for a safe model-context checkpoint and returns the
//! smallest replay needed to resume from there. Malformed reverse-scanned records are skipped. If
//! we cannot prove that a bounded replay is safe, we return `None` and let the caller fall back to
//! full tolerant replay.

use std::fs::File;
use std::path::PathBuf;

use codex_protocol::protocol::SessionMetaLine;
use codex_rollout::ModelContextScan;
use codex_rollout::ModelContextScanMode;
use codex_rollout::ModelContextScanProgress;
use codex_rollout::ReverseJsonlScanner;
use codex_rollout::RolloutItem;
use codex_rollout::ScanOutcome;
use serde_json::Value;

use super::line_parser;
use super::migration_error;
use crate::ThreadStoreResult;

/// Returns a bounded replay when the legacy child has enough durable context to reconstruct from
/// a suffix. Without that proof, the caller streams the complete legacy replay instead.
pub(super) async fn select_bounded_context(
    rollout_path: PathBuf,
    session_meta: SessionMetaLine,
) -> ThreadStoreResult<Option<Vec<RolloutItem>>> {
    tokio::task::spawn_blocking(move || {
        let file = File::open(rollout_path).map_err(migration_error)?;
        let mut scanner = ReverseJsonlScanner::new(file)
            .map_err(migration_error)?
            .with_max_record_bytes(super::MAX_ROLLOUT_LINE_BYTES)
            .with_oversized_records_rejected()
            .with_max_records(codex_rollout::MAX_PRESERVED_DURABLE_SCAN_ITEMS);
        let mut scan = ModelContextScan::new(ModelContextScanMode::PreserveDurableAgentState);

        while let Some(outcome) = scanner.scan_next::<Value>().map_err(migration_error)? {
            let value = match outcome {
                ScanOutcome::Parsed(value) => value,
                ScanOutcome::Rejected(_) => return Ok(None),
            };
            let line = match line_parser::parse_legacy_rollout_value(value) {
                Ok(Some(line)) => line,
                Ok(None) => continue,
                Err(_) => return Ok(None),
            };
            match scan.push(line.item) {
                ModelContextScanProgress::Continue => {}
                ModelContextScanProgress::Complete => {
                    let mut items = scan.finish(session_meta);
                    items.retain(|item| !matches!(item, RolloutItem::SessionMeta(_)));
                    return Ok(Some(items));
                }
                ModelContextScanProgress::LimitExceeded => {
                    return Err(migration_error(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "durable model-context scan exceeded its safety limits ({} scanned records, {} retained state records, or {} retained state bytes)",
                            codex_rollout::MAX_PRESERVED_DURABLE_SCAN_ITEMS,
                            codex_rollout::MAX_PRESERVED_DURABLE_STATE_ITEMS,
                            codex_rollout::MAX_PRESERVED_DURABLE_STATE_BYTES,
                        ),
                    )));
                }
            }
        }

        if scan.has_bounded_suffix() {
            let mut items = scan.finish(session_meta);
            items.retain(|item| !matches!(item, RolloutItem::SessionMeta(_)));
            Ok(Some(items))
        } else {
            Ok(None)
        }
    })
    .await
    .map_err(migration_error)?
}
