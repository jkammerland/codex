use super::now_unix_seconds;
use codex_protocol::ThreadId;
use fd_lock::RwLock;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;

const SESSION_IMPORT_LEDGER_FILE: &str = "external_agent_session_imports.json";
const SESSION_IMPORT_LEDGER_LOCK_FILE: &str = "external_agent_session_imports.lock";
const SESSION_HASH_BUFFER_SIZE: usize = 64 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ImportedExternalAgentSessionLedger {
    records: Vec<ImportedExternalAgentSessionRecord>,
    #[serde(default)]
    detected_connector_records: Vec<DetectedExternalAgentSessionConnectorRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct DetectedExternalAgentSessionConnectorRecord {
    source_path: PathBuf,
    connector_names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ImportedExternalAgentSessionRecord {
    source_path: PathBuf,
    content_sha256: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    initial_import_pending: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_content_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_target_model_item_count: Option<usize>,
    imported_thread_id: ThreadId,
    imported_at: i64,
    #[serde(default)]
    source_modified_at: Option<i64>,
    #[serde(default)]
    connector_names: Vec<String>,
    #[serde(default)]
    title: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct CompletedExternalAgentSessionImport {
    pub source_path: PathBuf,
    pub source_content_sha256: String,
    pub imported_thread_id: ThreadId,
    pub connector_names: Vec<String>,
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionImportSourceMapping {
    New {
        imported_thread_id: ThreadId,
    },
    Current,
    Unique {
        source_content_sha256: String,
        imported_thread_id: ThreadId,
    },
    Ambiguous,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedConnectorCandidate {
    pub name: String,
    pub session_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ExistingSessionAppendPosition {
    pub current_model_item_count: usize,
    pub legacy_pending_model_item_count: Option<usize>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ImportedSourceState {
    pub source_modified_at: Option<i64>,
}

pub fn has_current_session_been_imported(
    codex_home: &Path,
    source_path: &Path,
) -> io::Result<bool> {
    load_import_ledger(codex_home)?.contains_current_source(source_path)
}

#[cfg(test)]
pub(crate) fn record_imported_session(
    codex_home: &Path,
    source_path: &Path,
    imported_thread_id: ThreadId,
) -> io::Result<()> {
    let source_path = canonical_source_path(source_path)?;
    record_completed_session_imports(
        codex_home,
        vec![CompletedExternalAgentSessionImport {
            source_content_sha256: session_content_sha256(&source_path)?,
            source_path,
            imported_thread_id,
            connector_names: Vec::new(),
            title: None,
        }],
    )
}

pub(crate) fn reserve_session_import(
    codex_home: &Path,
    source_path: &Path,
    source_content_sha256: &str,
    candidate_thread_id: ThreadId,
) -> io::Result<SessionImportSourceMapping> {
    let Some((source_path, source_modified_at)) =
        stable_source_version(source_path, source_content_sha256)?
    else {
        return Ok(SessionImportSourceMapping::Ambiguous);
    };

    with_import_ledger_write(codex_home, |ledger| {
        let mut matching_records = ledger
            .records
            .iter()
            .filter(|record| record.source_path == source_path);
        let Some(record) = matching_records.next() else {
            ledger.records.push(ImportedExternalAgentSessionRecord {
                source_path,
                content_sha256: source_content_sha256.to_string(),
                initial_import_pending: true,
                pending_content_sha256: None,
                pending_target_model_item_count: None,
                imported_thread_id: candidate_thread_id,
                imported_at: now_unix_seconds(),
                source_modified_at,
                connector_names: Vec::new(),
                title: None,
            });
            return Ok((
                SessionImportSourceMapping::New {
                    imported_thread_id: candidate_thread_id,
                },
                ImportLedgerMutation::Changed,
            ));
        };
        if matching_records.next().is_some() {
            return Ok((
                SessionImportSourceMapping::Ambiguous,
                ImportLedgerMutation::Unchanged,
            ));
        }
        let mapping = if record.initial_import_pending {
            SessionImportSourceMapping::New {
                imported_thread_id: record.imported_thread_id,
            }
        } else if record.content_sha256 == source_content_sha256 {
            SessionImportSourceMapping::Current
        } else {
            SessionImportSourceMapping::Unique {
                source_content_sha256: record.content_sha256.clone(),
                imported_thread_id: record.imported_thread_id,
            }
        };
        Ok((mapping, ImportLedgerMutation::Unchanged))
    })
}

pub(super) fn begin_existing_session_append(
    codex_home: &Path,
    source_path: &Path,
    thread_id: ThreadId,
    expected_source_content_sha256: &str,
    source_content_sha256: &str,
    position: ExistingSessionAppendPosition,
) -> io::Result<Option<usize>> {
    if expected_source_content_sha256 == source_content_sha256 {
        return Ok(None);
    }
    let Some((source_path, _)) = stable_source_version(source_path, source_content_sha256)? else {
        return Ok(None);
    };

    with_import_ledger_write(codex_home, |ledger| {
        let mut matching_indices = ledger
            .records
            .iter()
            .enumerate()
            .filter_map(|(index, record)| (record.source_path == source_path).then_some(index));
        let Some(index) = matching_indices.next() else {
            return Ok((None, ImportLedgerMutation::Unchanged));
        };
        if matching_indices.next().is_some() {
            return Ok((None, ImportLedgerMutation::Unchanged));
        }
        let record = &mut ledger.records[index];
        if record.imported_thread_id != thread_id
            || record.content_sha256 != expected_source_content_sha256
        {
            return Ok((None, ImportLedgerMutation::Unchanged));
        }
        if record.pending_content_sha256.is_some() {
            let Some(append_base_model_item_count) = record
                .pending_target_model_item_count
                .or(position.legacy_pending_model_item_count)
            else {
                return Ok((None, ImportLedgerMutation::Unchanged));
            };
            let mutation = if record.pending_content_sha256.as_deref()
                != Some(source_content_sha256)
                || record.pending_target_model_item_count != Some(append_base_model_item_count)
            {
                record.pending_content_sha256 = Some(source_content_sha256.to_string());
                record.pending_target_model_item_count = Some(append_base_model_item_count);
                ImportLedgerMutation::Changed
            } else {
                ImportLedgerMutation::Unchanged
            };
            return Ok((Some(append_base_model_item_count), mutation));
        }
        record.pending_content_sha256 = Some(source_content_sha256.to_string());
        record.pending_target_model_item_count = Some(position.current_model_item_count);
        Ok((
            Some(position.current_model_item_count),
            ImportLedgerMutation::Changed,
        ))
    })
}

pub(super) fn checkpoint_existing_session_import(
    codex_home: &Path,
    source_path: &Path,
    thread_id: ThreadId,
    expected_source_content_sha256: &str,
    source_content_sha256: &str,
) -> io::Result<bool> {
    let Some((source_path, source_modified_at)) =
        stable_source_version(source_path, source_content_sha256)?
    else {
        return Ok(false);
    };

    with_import_ledger_write(codex_home, |ledger| {
        let mut matching_indices = ledger
            .records
            .iter()
            .enumerate()
            .filter_map(|(index, record)| (record.source_path == source_path).then_some(index));
        let Some(index) = matching_indices.next() else {
            return Ok((false, ImportLedgerMutation::Unchanged));
        };
        if matching_indices.next().is_some() {
            return Ok((false, ImportLedgerMutation::Unchanged));
        }
        let record = &mut ledger.records[index];
        if record.imported_thread_id != thread_id
            || record.content_sha256 != expected_source_content_sha256
            || record.pending_content_sha256.as_deref() != Some(source_content_sha256)
        {
            return Ok((false, ImportLedgerMutation::Unchanged));
        }
        record.content_sha256 = source_content_sha256.to_string();
        record.pending_content_sha256 = None;
        record.pending_target_model_item_count = None;
        record.imported_at = now_unix_seconds();
        record.source_modified_at = source_modified_at;
        Ok((true, ImportLedgerMutation::Changed))
    })
}

pub fn record_completed_session_imports(
    codex_home: &Path,
    imports: Vec<CompletedExternalAgentSessionImport>,
) -> io::Result<()> {
    if imports.is_empty() {
        return Ok(());
    }
    with_import_ledger_write(codex_home, move |ledger| {
        let imported_at = now_unix_seconds();
        for import in imports {
            let source_modified_at =
                stable_source_version(&import.source_path, &import.source_content_sha256)
                    .ok()
                    .flatten()
                    .and_then(|(_, source_modified_at)| source_modified_at);
            if let Some(index) = ledger.records.iter().rposition(|record| {
                record.source_path == import.source_path
                    && record.initial_import_pending
                    && record.imported_thread_id == import.imported_thread_id
            }) {
                let mut record = ledger.records.remove(index);
                record.content_sha256 = import.source_content_sha256;
                record.initial_import_pending = false;
                record.pending_content_sha256 = None;
                record.pending_target_model_item_count = None;
                record.imported_at = imported_at;
                record.source_modified_at = source_modified_at;
                append_connector_names(&mut record.connector_names, import.connector_names);
                record.title = import.title;
                ledger.records.push(record);
                continue;
            }
            if let Some(index) = ledger.records.iter().rposition(|record| {
                record.source_path == import.source_path
                    && !record.initial_import_pending
                    && record.content_sha256 == import.source_content_sha256
            }) {
                let mut record = ledger.records.remove(index);
                record.imported_thread_id = import.imported_thread_id;
                record.pending_content_sha256 = None;
                record.pending_target_model_item_count = None;
                record.imported_at = imported_at;
                record.source_modified_at = source_modified_at.or(record.source_modified_at);
                append_connector_names(&mut record.connector_names, import.connector_names);
                record.title = import.title;
                ledger.records.push(record);
                continue;
            }
            let mut connector_names = Vec::new();
            append_connector_names(&mut connector_names, import.connector_names);
            ledger.records.push(ImportedExternalAgentSessionRecord {
                source_path: import.source_path,
                content_sha256: import.source_content_sha256,
                initial_import_pending: false,
                pending_content_sha256: None,
                pending_target_model_item_count: None,
                imported_thread_id: import.imported_thread_id,
                imported_at,
                source_modified_at,
                connector_names,
                title: import.title,
            });
        }
        Ok(((), ImportLedgerMutation::Changed))
    })
}

pub fn record_detected_session_connectors(
    codex_home: &Path,
    connector_names_by_source_path: BTreeMap<PathBuf, Vec<String>>,
) -> io::Result<()> {
    if connector_names_by_source_path.is_empty() {
        return Ok(());
    }
    with_import_ledger_write(codex_home, move |ledger| {
        for (source_path, connector_names) in connector_names_by_source_path {
            let source_path = canonical_source_path(&source_path)?;
            if let Some(record) = ledger
                .detected_connector_records
                .iter_mut()
                .find(|record| record.source_path == source_path)
            {
                append_connector_names(&mut record.connector_names, connector_names);
                continue;
            }
            let mut detected_connector_names = Vec::new();
            append_connector_names(&mut detected_connector_names, connector_names);
            ledger
                .detected_connector_records
                .push(DetectedExternalAgentSessionConnectorRecord {
                    source_path,
                    connector_names: detected_connector_names,
                });
        }
        Ok(((), ImportLedgerMutation::Changed))
    })
}

pub fn append_imported_session_connector_names(
    codex_home: &Path,
    connector_names_by_source_path: BTreeMap<PathBuf, Vec<String>>,
) -> io::Result<()> {
    if connector_names_by_source_path.is_empty() {
        return Ok(());
    }
    with_import_ledger_write(codex_home, move |ledger| {
        for (source_path, connector_names) in connector_names_by_source_path {
            let source_path = canonical_source_path(&source_path)?;
            for record in ledger
                .records
                .iter_mut()
                .filter(|record| record.source_path == source_path)
            {
                append_connector_names(&mut record.connector_names, connector_names.clone());
            }
        }
        Ok(((), ImportLedgerMutation::Changed))
    })
}

pub fn read_imported_connector_candidates(
    codex_home: &Path,
) -> io::Result<Vec<ImportedConnectorCandidate>> {
    let ledger = load_import_ledger(codex_home)?;
    let mut connector_names_by_source = BTreeMap::<PathBuf, Vec<String>>::new();
    for record in ledger.detected_connector_records {
        connector_names_by_source
            .entry(record.source_path)
            .or_default()
            .extend(record.connector_names);
    }
    for record in ledger.records {
        connector_names_by_source
            .entry(record.source_path)
            .or_default()
            .extend(record.connector_names);
    }
    let mut candidates_by_name = BTreeMap::<String, ImportedConnectorCandidate>::new();
    for connector_names in connector_names_by_source.into_values() {
        let mut connector_names_by_key = BTreeMap::new();
        for name in connector_names
            .into_iter()
            .filter_map(|name| super::normalized_connector_display_name(Some(&name)))
        {
            connector_names_by_key
                .entry(name.to_lowercase())
                .or_insert(name);
        }
        for (key, name) in connector_names_by_key {
            let candidate = candidates_by_name
                .entry(key)
                .or_insert(ImportedConnectorCandidate {
                    name,
                    session_count: 0,
                });
            candidate.session_count = candidate.session_count.saturating_add(1);
        }
    }
    let mut candidates = candidates_by_name.into_values().collect::<Vec<_>>();
    candidates.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(candidates)
}

fn append_connector_names(connector_names: &mut Vec<String>, additional_names: Vec<String>) {
    for name in additional_names {
        if connector_names
            .iter()
            .any(|existing_name| existing_name.eq_ignore_ascii_case(&name))
        {
            continue;
        }
        connector_names.push(name);
    }
}

impl ImportedExternalAgentSessionLedger {
    pub(crate) fn source_states(&self) -> HashMap<&Path, ImportedSourceState> {
        let mut states = HashMap::new();
        for record in &self.records {
            if record.initial_import_pending {
                continue;
            }
            states.insert(
                record.source_path.as_path(),
                ImportedSourceState {
                    source_modified_at: record.source_modified_at,
                },
            );
        }
        states
    }

    pub(crate) fn contains_current_source(&self, source_path: &Path) -> io::Result<bool> {
        if self.records.is_empty() {
            return Ok(false);
        }
        let source_path = canonical_source_path(source_path)?;
        if !self
            .records
            .iter()
            .any(|record| record.source_path == source_path)
        {
            return Ok(false);
        }
        let content_sha256 = session_content_sha256(&source_path)?;
        Ok(self.records.iter().any(|record| {
            record.source_path == source_path
                && !record.initial_import_pending
                && record.content_sha256 == content_sha256
        }))
    }

    pub(crate) fn refresh_current_source(
        &mut self,
        source_path: &Path,
        source_modified_at: i64,
    ) -> io::Result<bool> {
        let source_path = canonical_source_path(source_path)?;
        if !self
            .records
            .iter()
            .any(|record| record.source_path == source_path)
        {
            return Ok(false);
        }
        let content_sha256 = session_content_sha256(&source_path)?;
        let Some(index) = self.records.iter().rposition(|record| {
            record.source_path == source_path
                && !record.initial_import_pending
                && record.content_sha256 == content_sha256
        }) else {
            return Ok(false);
        };
        let mut record = self.records.remove(index);
        record.imported_at = now_unix_seconds();
        record.source_modified_at = Some(source_modified_at);
        self.records.push(record);
        Ok(true)
    }
}

pub(crate) fn refresh_current_source(
    codex_home: &Path,
    source_path: &Path,
    source_modified_at: i64,
) -> io::Result<bool> {
    with_import_ledger_write(codex_home, |ledger| {
        let refreshed = ledger.refresh_current_source(source_path, source_modified_at)?;
        Ok((
            refreshed,
            if refreshed {
                ImportLedgerMutation::Changed
            } else {
                ImportLedgerMutation::Unchanged
            },
        ))
    })
}

pub(crate) fn load_import_ledger(
    codex_home: &Path,
) -> io::Result<ImportedExternalAgentSessionLedger> {
    with_import_ledger_read(codex_home, || load_import_ledger_unlocked(codex_home))
}

fn load_import_ledger_unlocked(
    codex_home: &Path,
) -> io::Result<ImportedExternalAgentSessionLedger> {
    let path = import_ledger_path(codex_home);
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(ImportedExternalAgentSessionLedger::default());
        }
        Err(err) => return Err(err),
    };
    serde_json::from_str(&raw).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid external agent session import ledger: {err}"),
        )
    })
}

fn save_import_ledger_unlocked(
    codex_home: &Path,
    ledger: &ImportedExternalAgentSessionLedger,
) -> io::Result<()> {
    fs::create_dir_all(codex_home)?;
    let path = import_ledger_path(codex_home);
    let raw = serde_json::to_string_pretty(ledger).map_err(io::Error::other)?;
    codex_utils_path::write_atomically(&path, &raw)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImportLedgerMutation {
    Changed,
    Unchanged,
}

fn with_import_ledger_read<T>(
    codex_home: &Path,
    operation: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    let lock = RwLock::new(open_import_ledger_lock(codex_home)?);
    let _guard = lock.read()?;
    operation()
}

fn with_import_ledger_write<T>(
    codex_home: &Path,
    operation: impl FnOnce(
        &mut ImportedExternalAgentSessionLedger,
    ) -> io::Result<(T, ImportLedgerMutation)>,
) -> io::Result<T> {
    let mut lock = RwLock::new(open_import_ledger_lock(codex_home)?);
    let _guard = lock.write()?;
    let mut ledger = load_import_ledger_unlocked(codex_home)?;
    let (result, mutation) = operation(&mut ledger)?;
    if mutation == ImportLedgerMutation::Changed {
        save_import_ledger_unlocked(codex_home, &ledger)?;
    }
    Ok(result)
}

fn open_import_ledger_lock(codex_home: &Path) -> io::Result<File> {
    fs::create_dir_all(codex_home)?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(codex_home.join(SESSION_IMPORT_LEDGER_LOCK_FILE))
}

fn import_ledger_path(codex_home: &Path) -> PathBuf {
    codex_home.join(SESSION_IMPORT_LEDGER_FILE)
}

fn canonical_source_path(path: &Path) -> io::Result<PathBuf> {
    fs::canonicalize(path)
}

fn session_content_sha256(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; SESSION_HASH_BUFFER_SIZE];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    Ok(format!("{digest:x}"))
}

fn session_modified_at(path: &Path) -> io::Result<Option<i64>> {
    Ok(fs::metadata(path)?
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_nanos()).ok()))
}

fn stable_source_version(
    source_path: &Path,
    source_content_sha256: &str,
) -> io::Result<Option<(PathBuf, Option<i64>)>> {
    let source_path = canonical_source_path(source_path)?;
    let source_modified_at = session_modified_at(&source_path)?;
    if session_content_sha256(&source_path)? != source_content_sha256
        || source_modified_at != session_modified_at(&source_path)?
    {
        return Ok(None);
    }
    Ok(Some((source_path, source_modified_at)))
}

#[cfg(test)]
#[path = "ledger_tests.rs"]
mod tests;
