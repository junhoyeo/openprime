//! The legacy subagent topology surfaces: the pre-ledger per-parent registry reader with its
//! bounded header-line probe, and the per-child display sidecar entry with its atomic writer.
use std::io::{Read as _, Write as _};

use anyhow::Context as _;

use super::{fs, Deserialize, HashMap, Path, PathBuf, Result, Serialize, Value};

/// One legacy `rlm-subagents.jsonl` registry entry (the pre-ledger topology
/// store; still read for seeding and hydration metadata). The fields beyond
/// the edge (prompt, model, node ids) are display-grade.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyRlmSubagentEntry {
    #[serde(default)]
    pub child_id: String,
    #[serde(default)]
    pub session_name: String,
    #[serde(default)]
    pub session_file: String,
    #[serde(default)]
    pub rlm_depth: u32,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub session_dir: String,
    #[serde(default)]
    pub parent_session_id: String,
    #[serde(default)]
    pub parent_session_file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rlm_parent_node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Value>,
    #[serde(default)]
    pub created_at: u64,
}

/// The per-child display file (`rlm-subagent.json` in the child's session
/// dir): display-grade hydration metadata, never topology.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RlmSubagentDisplayEntry {
    /// Always `rlm_subagent`; a file of any other type is not a display entry and reads as absent.
    #[serde(default, rename = "type")]
    pub type_tag: String,
    #[serde(default)]
    pub child_id: String,
    #[serde(default)]
    pub session_name: String,
    #[serde(default)]
    pub session_dir: String,
    #[serde(default)]
    pub session_file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rlm_parent_node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Value>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub created_at: u64,
}

/// Read one child's display entry; `None` when absent, unreadable, or not
/// describing the requested child (a stale file from a re-used session dir).
#[must_use]
pub fn read_rlm_subagent_display(child_session_dir: &Path) -> Option<RlmSubagentDisplayEntry> {
    let content = fs::read_to_string(child_session_dir.join("rlm-subagent.json")).ok()?;
    let entry: RlmSubagentDisplayEntry = serde_json::from_str(&content).ok()?;
    if entry.type_tag != "rlm_subagent"
        || !matches!(entry.status.as_str(), "running" | "completed" | "deleted")
    {
        return None;
    }
    Some(entry)
}

/// Atomically write one child's display entry (a tombstone refusal keeps the
/// deleted child deleted).
///
/// # Errors
///
/// Returns an error on a display-dir, serialization, or temp-write failure.
pub fn write_rlm_subagent_display(entry: &RlmSubagentDisplayEntry) -> Result<bool> {
    if entry.status != "deleted"
        && read_rlm_subagent_display(Path::new(&entry.session_dir))
            .is_some_and(|current| current.status == "deleted")
    {
        return Ok(false);
    }
    let dir = Path::new(&entry.session_dir);
    fs::create_dir_all(dir)?;
    let payload = serde_json::to_string(entry)?;
    // TS `writeFileAtomicSync` temp naming: a unique temp per writer, so
    // two processes never share one temp.
    let temp = dir.join(format!(
        "rlm-subagent.json.{}.{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    // The temp carries the TS display writer's 0o600 mode; the rename
    // preserves it onto the final file.
    let mut options = fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    pa_core::platform::perms::set_private_mode(&mut options);
    let mut file = options.open(&temp)?;
    file.write_all(format!("{payload}\n").as_bytes())?;
    // TS `writeFileAtomicSync(..., { fsync: true })`: the temp is durable
    // before the rename, so a crash mid-write leaves a stale temp and the
    // previous file intact - never a half-written display state.
    file.sync_all()?;
    pa_core::platform::rename_onto(&temp, &dir.join("rlm-subagent.json"))
        .with_context(|| format!("persist rlm-subagent display at {}", dir.display()))?;
    Ok(true)
}

/// The bounded header read cap for the legacy registry probe: the header id rides the file's first
/// line, so the read stays bounded; a first line longer than the cap reads as absent.
pub(super) const LEGACY_REGISTRY_HEADER_READ_MAX_BYTES: usize = 64 * 1024;

/// The legacy registry path for one parent session file: the parent's
/// artifacts dir, keyed by the session header id. The probe stays bounded
/// — the passive walk probes every parent once, so a whole-file read
/// would be linear in transcript bytes.
pub(super) fn legacy_registry_path(session_file: &Path) -> Option<PathBuf> {
    let line = crate::session_store::read_first_line_bounded(
        session_file,
        LEGACY_REGISTRY_HEADER_READ_MAX_BYTES,
    )?;
    let text = std::str::from_utf8(&line).ok()?;
    let header: Value = serde_json::from_str(text.trim()).ok()?;
    let header_id = header.get("id")?.as_str()?;
    // TS `getSessionArtifactsRoot`: the artifacts tree is the sibling of
    // the session file's directory, keyed by the session header id.
    let artifacts_root = session_file.parent()?.parent()?.join("session-artifacts");
    Some(artifacts_root.join(header_id).join("rlm-subagents.jsonl"))
}

/// Tolerant reader for a per-parent legacy registry (TS `readLegacyRlmSubagentRegistry`): latest
/// entry per childId, malformed lines ignored, a missing file an empty registry.
pub(crate) fn read_legacy_registry(session_file: &Path) -> Vec<LegacyRlmSubagentEntry> {
    let Some(registry) = legacy_registry_path(session_file) else {
        return Vec::new();
    };
    let Ok(content) = fs::read_to_string(&registry) else {
        return Vec::new();
    };
    let mut latest: HashMap<String, LegacyRlmSubagentEntry> = HashMap::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(entry) = parse_legacy_entry(trimmed) else {
            continue;
        };
        latest.insert(entry.child_id.clone(), entry);
    }
    latest.into_values().collect()
}

fn parse_legacy_entry(line: &str) -> Result<LegacyRlmSubagentEntry> {
    let entry: LegacyRlmSubagentEntry = serde_json::from_str(line)?;
    anyhow::ensure!(
        !entry.child_id.is_empty()
            && !entry.session_file.is_empty()
            && matches!(entry.status.as_str(), "running" | "completed" | "deleted"),
        "invalid legacy child identity or status"
    );
    Ok(entry)
}

pub(super) struct LegacySeedBudget {
    pub(super) bytes: usize,
    pub(super) files: usize,
    pub(super) records: usize,
}

impl Default for LegacySeedBudget {
    fn default() -> Self {
        Self {
            bytes: super::RLM_LEDGER_MAX_BYTES as usize,
            files: super::RLM_LEDGER_MAX_RECORDS,
            records: super::RLM_LEDGER_MAX_RECORDS,
        }
    }
}

/// Authority import uses the same row parser without display degradation.
/// A root header owns a registry only through a nonempty string id, matching
/// shipped readers. Non-JSON and id-less roots contribute nothing. I/O errors,
/// path-unsafe ids and every present registry error remain fatal.
pub(super) fn read_legacy_registry_checked(
    session_file: &Path,
    budget: &mut LegacySeedBudget,
) -> Result<Vec<LegacyRlmSubagentEntry>> {
    anyhow::ensure!(budget.files > 0, "legacy seed exceeds aggregate file limit");
    budget.files -= 1;
    match fs::symlink_metadata(session_file) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
        Err(error) => return Err(error).context("inspect legacy parent"),
        Ok(metadata) => anyhow::ensure!(
            metadata.file_type().is_file(),
            "legacy parent is not a regular file"
        ),
    }
    let limit = LEGACY_REGISTRY_HEADER_READ_MAX_BYTES.min(budget.bytes);
    let mut line = Vec::new();
    std::io::BufRead::read_until(
        &mut std::io::BufReader::new(fs::File::open(session_file)?.take(limit as u64 + 1)),
        b'\n',
        &mut line,
    )?;
    anyhow::ensure!(
        line.len() <= limit,
        "legacy root header exceeds aggregate authority limit"
    );
    budget.bytes -= line.len();
    let Ok(header) = serde_json::from_slice::<Value>(&line) else {
        return Ok(Vec::new());
    };
    let Some(id) = header
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    else {
        return Ok(Vec::new());
    };
    anyhow::ensure!(
        Path::new(id).components().count() == 1
            && !matches!(id, "." | "..")
            && !id.contains(['/', '\\']),
        "legacy parent {} has a path-unsafe durable identity",
        session_file.display()
    );
    let registry = session_file
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| anyhow::anyhow!("legacy parent has no artifact root"))?
        .join("session-artifacts")
        .join(id)
        .join("rlm-subagents.jsonl");
    // Orphan registries are ignored: the path is keyed by a root id, legacy
    // registries never carried enforced tombstones, and the ledger is the
    // tombstone authority.
    read_checked_registry(&registry, budget)
}

/// Strictly read one present registry. Shipped readers keep the latest row
/// per child id; deleted rows are returned so the seed can import them as
/// ledger tombstones. A child id whose rows name different paths is ambiguous.
fn read_checked_registry(
    registry: &Path,
    budget: &mut LegacySeedBudget,
) -> Result<Vec<LegacyRlmSubagentEntry>> {
    match fs::symlink_metadata(registry) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect legacy registry {}", registry.display()))
        }
        Ok(metadata) => anyhow::ensure!(
            metadata.file_type().is_file(),
            "legacy registry {} is not a regular file",
            registry.display()
        ),
    }
    let mut bytes = Vec::new();
    fs::File::open(registry)?
        .take(budget.bytes as u64 + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= budget.bytes,
        "legacy seed exceeds aggregate byte limit"
    );
    budget.bytes -= bytes.len();
    let mut checked = || -> Result<Vec<LegacyRlmSubagentEntry>> {
        anyhow::ensure!(
            bytes.is_empty() || bytes.ends_with(b"\n"),
            "unterminated record"
        );
        let content = std::str::from_utf8(&bytes).context("not UTF-8")?;
        let mut latest: Vec<(LegacyRlmSubagentEntry, PathBuf)> = Vec::new();
        for line in content.lines().filter(|line| !line.trim().is_empty()) {
            anyhow::ensure!(
                budget.records > 0,
                "legacy seed exceeds aggregate record limit"
            );
            budget.records -= 1;
            let entry = parse_legacy_entry(line)?;
            anyhow::ensure!(
                Path::new(&entry.session_file).is_absolute(),
                "child path is not absolute"
            );
            crate::lease::checked_session_path(Path::new(&entry.session_file))?;
            let path = super::canonical_session_path(Path::new(&entry.session_file));
            match latest
                .iter_mut()
                .find(|(prior, _)| prior.child_id == entry.child_id)
            {
                Some((prior, prior_path)) => {
                    anyhow::ensure!(
                        *prior_path == path,
                        "child {} has ambiguous paths",
                        entry.child_id
                    );
                    *prior = entry;
                }
                None => latest.push((entry, path)),
            }
        }
        Ok(latest.into_iter().map(|(entry, _)| entry).collect())
    };
    checked().with_context(|| format!("legacy registry {} refused", registry.display()))
}
