//! The RLM ledger's read-back machinery: the wire record grammar and its
//! line parser, the replay state and the stat-identity cache records, the
//! edge join keys, the path canonicalizers, and the `live_edges`
//! liveness resolver with its session-artifacts index.
use anyhow::Context as _;

use super::{
    bail, canonical_session_path, fs, json, Arc, HashMap, Mutex, OnceLock, Path, PathBuf, Result,
    RlmLedgerDeleteReason, RlmLedgerEdge, Value,
};

/// One replayed ledger record (`meta` records carry no edge and are skipped).
#[derive(Debug, Clone, PartialEq)]
pub(super) enum LedgerRecord {
    Spawn {
        child_id: String,
        parent: String,
        child: String,
        depth: u32,
        name: String,
    },
    Rename {
        child_id: String,
        child: String,
        name: String,
    },
    Delete {
        child_id: String,
        child: String,
        reason: RlmLedgerDeleteReason,
        usage: Option<crate::session_usage::SessionUsageSummary>,
    },
}

fn str_field(record: &Value, key: &str) -> Option<String> {
    record.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Parse one delete record's usage snapshot (TS `SessionUsageSummary`
/// wire shape). A present-but-malformed snapshot is a corrupt record: the
/// record is rejected rather than billing a partial number.
fn parse_deleted_usage(
    usage: &Value,
    line_no: usize,
) -> Result<crate::session_usage::SessionUsageSummary> {
    let invalid = || {
        anyhow::Error::msg(format!(
            "malformed RLM ledger line {line_no}: invalid delete usage"
        ))
    };
    let input_tokens = usage
        .get("inputTokens")
        .and_then(Value::as_u64)
        .ok_or_else(invalid)?;
    let output_tokens = usage
        .get("outputTokens")
        .and_then(Value::as_u64)
        .ok_or_else(invalid)?;
    let cost = usage
        .get("cost")
        .and_then(Value::as_f64)
        .ok_or_else(invalid)?;
    if cost.is_nan() || cost.is_sign_negative() {
        bail!("malformed RLM ledger line {line_no}: invalid delete usage");
    }
    Ok(crate::session_usage::SessionUsageSummary {
        input_tokens,
        output_tokens,
        cost,
    })
}

/// Reject duplicate object keys anywhere in an authority record. Ordinary
/// `serde_json` Value parsing keeps one duplicate, which can hide ownership data.
#[derive(Default)]
struct UniqueJson;

impl<'de> serde::de::Visitor<'de> for UniqueJson {
    type Value = ();
    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("JSON without duplicate object keys")
    }
    fn visit_bool<E>(self, _: bool) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_i64<E>(self, _: i64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_u64<E>(self, _: u64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_f64<E>(self, _: f64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_str<E>(self, _: &str) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_unit<E>(self) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(
        self,
        mut sequence: A,
    ) -> std::result::Result<(), A::Error> {
        while sequence.next_element_seed(UniqueJson)?.is_some() {}
        Ok(())
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(
        self,
        mut map: A,
    ) -> std::result::Result<(), A::Error> {
        let mut keys = std::collections::HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key) {
                return Err(serde::de::Error::custom("duplicate object key"));
            }
            map.next_value_seed(UniqueJson)?;
        }
        Ok(())
    }
}

impl<'de> serde::de::DeserializeSeed<'de> for UniqueJson {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

pub(super) fn validate_unique_json(line: &str) -> Result<()> {
    let mut deserializer = serde_json::Deserializer::from_str(line.trim());
    serde::de::DeserializeSeed::deserialize(UniqueJson, &mut deserializer)
        .map_err(|_| anyhow::anyhow!("RLM ledger authority JSON is malformed or ambiguous"))?;
    deserializer
        .end()
        .map_err(|_| anyhow::anyhow!("RLM ledger authority JSON is malformed or ambiguous"))
}

/// One body line as an authority read classifies it.
pub(super) enum AuthorityLine {
    Record(LedgerRecord),
    Meta,
    /// Not a complete well-formed record: a write that never committed.
    Skip(&'static str),
}

/// One required field of an authority record: its key and its type check.
type RequiredField = (&'static str, fn(&Value) -> bool);

/// The shared per-line authority rule. A record is acknowledged only after its
/// whole line is written and synced, so a torn, non-UTF-8, non-JSON line, or
/// one with an ABSENT required field, never committed and is skipped. A
/// well-formed line with duplicate keys, or a required field PRESENT with the
/// wrong type or an unknown value (op, version, reason, usage), may be a newer
/// or ambiguous committed record and refuses. Callers apply this per line and
/// keep reading; one bad line never ends the scan.
pub(super) fn classify_authority_line(
    raw: &[u8],
    index: usize,
    torn: bool,
) -> Result<AuthorityLine> {
    let line_no = index + 1;
    if torn {
        return Ok(AuthorityLine::Skip("torn"));
    }
    let Ok(line) = std::str::from_utf8(raw) else {
        return Ok(AuthorityLine::Skip("non-UTF-8"));
    };
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Ok(AuthorityLine::Skip("malformed"));
    };
    if validate_unique_json(line).is_err() {
        bail!("ambiguous duplicate-key record on line {line_no}");
    }
    let Some(object) = value.as_object() else {
        return Ok(AuthorityLine::Skip("incomplete"));
    };
    // An ABSENT required field means the record is incomplete and skips. A
    // PRESENT field with the wrong type or an unknown value may come from a
    // newer binary, so it refuses.
    match object.get("v") {
        None => return Ok(AuthorityLine::Skip("incomplete")),
        Some(version) if *version != json!(1) => {
            bail!("unsupported record version on line {line_no}")
        }
        Some(_) => {}
    }
    let op = match object.get("op") {
        None => return Ok(AuthorityLine::Skip("incomplete")),
        Some(Value::String(op)) => op.as_str(),
        Some(_) => bail!("invalid operation field on line {line_no}"),
    };
    let required: &[RequiredField] = match op {
        "meta" => &[("at", Value::is_string), ("sessionsDir", Value::is_string)],
        "spawn" => &[
            ("at", Value::is_string),
            ("childId", Value::is_string),
            ("parent", Value::is_string),
            ("child", Value::is_string),
            ("name", Value::is_string),
            ("depth", Value::is_u64),
        ],
        "rename" => &[
            ("at", Value::is_string),
            ("childId", Value::is_string),
            ("child", Value::is_string),
            ("name", Value::is_string),
        ],
        "delete" => &[
            ("at", Value::is_string),
            ("childId", Value::is_string),
            ("child", Value::is_string),
            ("reason", Value::is_string),
        ],
        _ => bail!("unknown operation on line {line_no}"),
    };
    for (key, valid) in required {
        match object.get(*key) {
            None => return Ok(AuthorityLine::Skip("incomplete")),
            Some(field) if !valid(field) => bail!("invalid {key} field on line {line_no}"),
            Some(_) => {}
        }
    }
    if op == "meta" {
        return Ok(AuthorityLine::Meta);
    }
    // Every required field is present with its type. A remaining rejection is
    // a value this reader does not know (a delete reason, a usage block, a
    // depth range): refuse rather than drop a newer committed record.
    match parse_ledger_line(line, index) {
        Ok(Some(record)) => Ok(AuthorityLine::Record(record)),
        Ok(None) => bail!("unknown operation on line {line_no}"),
        Err(_) => bail!("unknown or invalid field value on line {line_no}"),
    }
}

/// Parse one ledger line: `v:1` records with a known op, `None` for a known
/// version with an unknown op (forward compatibility), an error for anything
/// else - which replay logs and skips: one bad line costs one record, never
/// the ledger.
struct UniqueJson;

impl<'de> serde::de::Visitor<'de> for UniqueJson {
    type Value = ();
    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("JSON without duplicate object keys")
    }
    fn visit_bool<E>(self, _: bool) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_i64<E>(self, _: i64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_u64<E>(self, _: u64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_f64<E>(self, _: f64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_str<E>(self, _: &str) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_unit<E>(self) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(
        self,
        mut sequence: A,
    ) -> std::result::Result<(), A::Error> {
        while sequence.next_element_seed(UniqueJson)?.is_some() {}
        Ok(())
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(
        self,
        mut map: A,
    ) -> std::result::Result<(), A::Error> {
        let mut keys = std::collections::HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key) {
                return Err(serde::de::Error::custom("duplicate object key"));
            }
            map.next_value_seed(UniqueJson)?;
        }
        Ok(())
    }
}

impl<'de> serde::de::DeserializeSeed<'de> for UniqueJson {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

pub(super) fn validate_unique_json(line: &str) -> Result<()> {
    let mut deserializer = serde_json::Deserializer::from_str(line.trim());
    serde::de::DeserializeSeed::deserialize(UniqueJson, &mut deserializer)
        .map_err(|_| anyhow::anyhow!("RLM ledger authority JSON is malformed or ambiguous"))?;
    deserializer
        .end()
        .map_err(|_| anyhow::anyhow!("RLM ledger authority JSON is malformed or ambiguous"))
}

/// One body line as an authority read classifies it.
pub(super) enum AuthorityLine {
    Record(LedgerRecord),
    Meta,
    /// Not a complete well-formed record: a write that never committed.
    Skip(&'static str),
}

/// One required field of an authority record: its key and its type check.
type RequiredField = (&'static str, fn(&Value) -> bool);

/// The shared per-line authority rule. A record is acknowledged only after its
/// whole line is written and synced, so a torn, non-UTF-8, non-JSON line, or
/// one with an ABSENT required field, never committed and is skipped. A
/// well-formed line with duplicate keys, or a required field PRESENT with the
/// wrong type or an unknown value (op, version, reason, usage), may be a newer
/// or ambiguous committed record and refuses. Callers apply this per line and
/// keep reading; one bad line never ends the scan.
pub(super) fn classify_authority_line(
    raw: &[u8],
    index: usize,
    torn: bool,
) -> Result<AuthorityLine> {
    let line_no = index + 1;
    if torn {
        return Ok(AuthorityLine::Skip("torn"));
    }
    let Ok(line) = std::str::from_utf8(raw) else {
        return Ok(AuthorityLine::Skip("non-UTF-8"));
    };
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Ok(AuthorityLine::Skip("malformed"));
    };
    if validate_unique_json(line).is_err() {
        bail!("ambiguous duplicate-key record on line {line_no}");
    }
    let Some(object) = value.as_object() else {
        return Ok(AuthorityLine::Skip("incomplete"));
    };
    // An ABSENT required field means the record is incomplete and skips. A
    // PRESENT field with the wrong type or an unknown value may come from a
    // newer binary, so it refuses.
    match object.get("v") {
        None => return Ok(AuthorityLine::Skip("incomplete")),
        Some(version) if *version != json!(1) => {
            bail!("unsupported record version on line {line_no}")
        }
        Some(_) => {}
    }
    let op = match object.get("op") {
        None => return Ok(AuthorityLine::Skip("incomplete")),
        Some(Value::String(op)) => op.as_str(),
        Some(_) => bail!("invalid operation field on line {line_no}"),
    };
    let required: &[RequiredField] = match op {
        "meta" => &[("at", Value::is_string), ("sessionsDir", Value::is_string)],
        "spawn" => &[
            ("at", Value::is_string),
            ("childId", Value::is_string),
            ("parent", Value::is_string),
            ("child", Value::is_string),
            ("name", Value::is_string),
            ("depth", Value::is_u64),
        ],
        "rename" => &[
            ("at", Value::is_string),
            ("childId", Value::is_string),
            ("child", Value::is_string),
            ("name", Value::is_string),
        ],
        "delete" => &[
            ("at", Value::is_string),
            ("childId", Value::is_string),
            ("child", Value::is_string),
            ("reason", Value::is_string),
        ],
        _ => bail!("unknown operation on line {line_no}"),
    };
    for (key, valid) in required {
        match object.get(*key) {
            None => return Ok(AuthorityLine::Skip("incomplete")),
            Some(field) if !valid(field) => bail!("invalid {key} field on line {line_no}"),
            Some(_) => {}
        }
    }
    if op == "meta" {
        return Ok(AuthorityLine::Meta);
    }
    // Every required field is present with its type. A remaining rejection is
    // a value this reader does not know (a delete reason, a usage block, a
    // depth range): refuse rather than drop a newer committed record.
    match parse_ledger_line(line, index) {
        Ok(Some(record)) => Ok(AuthorityLine::Record(record)),
        Ok(None) => bail!("unknown operation on line {line_no}"),
        Err(_) => bail!("unknown or invalid field value on line {line_no}"),
    }
}

/// Parse one ledger line: `v:1` records with a known op, `None` for a known
/// version with an unknown op (forward compatibility), an error for anything
/// else - which replay logs and skips: one bad line costs one record, never
/// the ledger.
pub(super) fn parse_ledger_line(line: &str, index: usize) -> Result<Option<LedgerRecord>> {
    let line_no = index + 1;
    let record: Value = serde_json::from_str(line.trim())
        .with_context(|| format!("malformed RLM ledger line {line_no}"))?;
    if record.get("v") != Some(&json!(1)) {
        bail!("malformed RLM ledger line {line_no}: unsupported record version");
    }
    if record.get("at").and_then(Value::as_str).is_none() {
        bail!("malformed RLM ledger line {line_no}: missing at");
    }
    let op = record.get("op").and_then(Value::as_str).unwrap_or_default();
    let child_id = || str_field(&record, "childId");
    let child = || str_field(&record, "child");
    match op {
        "spawn" => {
            let (Some(child_id), Some(parent), Some(child), Some(name)) = (
                child_id(),
                str_field(&record, "parent"),
                child(),
                str_field(&record, "name"),
            ) else {
                bail!("malformed RLM ledger line {line_no}: invalid spawn record");
            };
            let Some(depth) = record.get("depth").and_then(Value::as_u64) else {
                bail!("malformed RLM ledger line {line_no}: invalid spawn record");
            };
            if depth < 1 || depth > u64::from(u32::MAX) {
                bail!("malformed RLM ledger line {line_no}: invalid spawn record");
            }
            Ok(Some(LedgerRecord::Spawn {
                child_id,
                parent,
                child,
                depth: depth as u32,
                name,
            }))
        }
        "rename" => {
            let (Some(child_id), Some(child), Some(name)) =
                (child_id(), child(), str_field(&record, "name"))
            else {
                bail!("malformed RLM ledger line {line_no}: invalid rename record");
            };
            Ok(Some(LedgerRecord::Rename {
                child_id,
                child,
                name,
            }))
        }
        "delete" => {
            let (Some(child_id), Some(child)) = (child_id(), child()) else {
                bail!("malformed RLM ledger line {line_no}: invalid delete record");
            };
            let Some(reason) = record
                .get("reason")
                .and_then(Value::as_str)
                .and_then(RlmLedgerDeleteReason::from_wire)
            else {
                bail!("malformed RLM ledger line {line_no}: invalid delete record");
            };
            // The amendment record carries the deleted child's captured
            // own usage; old readers skip unknown fields (no version bump).
            let usage = match record.get("usage") {
                None => None,
                Some(usage) => Some(parse_deleted_usage(usage, line_no)?),
            };
            Ok(Some(LedgerRecord::Delete {
                child_id,
                child,
                reason,
                usage,
            }))
        }
        _ => Ok(None),
    }
}

/// The replayed edge set: replay order plus a key index for record joins.
#[derive(Debug, Default)]
pub(super) struct ReplayState {
    pub(super) edges: Vec<RlmLedgerEdge>,
    pub(super) index: HashMap<String, usize>,
    pub(super) artifact_index: OnceLock<Mutex<ArtifactIndex>>,
    pub(super) deleted_bucket: Mutex<Option<DeletedBucket>>,
}

/// Ledger rows are immutable within a replay, but file moves and transcript
/// edits can change the inputs of the deleted-spend fold without any append.
#[derive(Debug)]
pub(super) struct DeletedBucket {
    pub(super) inputs: Vec<Option<FileIdentity>>,
    pub(super) usage: HashMap<String, crate::session_usage::SessionUsageSummary>,
}

pub(super) fn edge_key(child_id: &str, child: &str) -> String {
    format!(
        "{child_id}\u{0}{}",
        canonical_session_path(Path::new(child)).to_string_lossy()
    )
}

/// One `live_edges` liveness pass: a recorded path that stats resolves
/// as itself; a moved file resolves through its durable session id (the
/// file-name stem) against the sessions dir and the artifacts tree.
pub(super) struct LivePathResolver<'a> {
    agent_dir: PathBuf,
    sessions_dir: PathBuf,
    pub(super) resolved: HashMap<String, Option<PathBuf>>,
    artifact_index: &'a OnceLock<Mutex<ArtifactIndex>>,
    artifact_checked: bool,
}

impl<'a> LivePathResolver<'a> {
    pub(super) fn new(
        agent_dir: PathBuf,
        sessions_dir: PathBuf,
        artifact_index: &'a OnceLock<Mutex<ArtifactIndex>>,
    ) -> Self {
        LivePathResolver {
            agent_dir,
            sessions_dir,
            resolved: HashMap::new(),
            artifact_index,
            artifact_checked: false,
        }
    }

    /// The live session file for one recorded edge path, `None` when the
    /// session is gone everywhere (the edge endpoint is dead).
    pub(super) fn resolve(&mut self, recorded: &str) -> Result<Option<PathBuf>> {
        if let Some(hit) = self.resolved.get(recorded) {
            return Ok(hit.clone());
        }
        let live = self.resolve_uncached(recorded)?;
        self.resolved.insert(recorded.to_string(), live.clone());
        Ok(live)
    }

    fn resolve_uncached(&mut self, recorded: &str) -> Result<Option<PathBuf>> {
        let recorded_path = Path::new(recorded);
        if is_file(recorded_path) {
            return Ok(Some(recorded_path.to_path_buf()));
        }
        let Some(stem) = recorded_path.file_stem() else {
            return Ok(None);
        };
        let id = stem.to_string_lossy();
        if id.is_empty() {
            return Ok(None);
        }
        let sessions_candidate = self.sessions_dir.join(format!("{id}.jsonl"));
        if is_file(&sessions_candidate) {
            return Ok(Some(sessions_candidate));
        }
        let mut index = self
            .artifact_index
            .get_or_init(|| Mutex::new(artifact_session_index(&self.agent_dir)))
            .lock()
            .map_err(|_| anyhow::anyhow!("artifact index cache lock poisoned"))?;
        // Directory identities cover additions, deletion and relocation, including
        // new partitions. Check once per read; unchanged misses retain the shared
        // negative result instead of enumerating every artifact file again.
        if !self.artifact_checked {
            if index
                .directories
                .iter()
                .any(|(path, identity)| path_identity(path) != *identity)
            {
                *index = artifact_session_index(&self.agent_dir);
            }
            self.artifact_checked = true;
        }
        Ok(index
            .paths
            .get(id.as_ref())
            .filter(|path| is_file(path))
            .cloned())
    }
}

#[derive(Debug, Default)]
pub(super) struct ArtifactIndex {
    paths: HashMap<String, PathBuf>,
    directories: Vec<(PathBuf, Option<FileIdentity>)>,
}

#[cfg(test)]
thread_local! {
    pub(super) static ARTIFACT_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The session files under the artifacts tree, keyed by their durable session id (the file-name
/// stem); non-session `.jsonl` sidecars key by their own stems and never collide.
fn artifact_session_index(agent_dir: &Path) -> ArtifactIndex {
    #[cfg(test)]
    ARTIFACT_SCANS.with(|count| count.set(count.get() + 1));
    let mut index = ArtifactIndex::default();
    let root = agent_dir.join(crate::context_tree_children::RLM_SESSION_ARTIFACTS_DIR);
    index.directories.push((root.clone(), path_identity(&root)));
    let Ok(parents) = std::fs::read_dir(&root) else {
        return index;
    };
    for parent in parents.flatten() {
        index
            .directories
            .push((parent.path(), path_identity(&parent.path())));
        let Ok(subs) = std::fs::read_dir(parent.path()) else {
            continue;
        };
        for sub in subs.flatten() {
            index
                .directories
                .push((sub.path(), path_identity(&sub.path())));
            let Ok(files) = std::fs::read_dir(sub.path()) else {
                continue;
            };
            for file in files.flatten() {
                let path = file.path();
                if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
                    if let Some(stem) = path.file_stem() {
                        index.paths.insert(stem.to_string_lossy().to_string(), path);
                    }
                }
            }
        }
    }
    index
}

#[derive(Debug)]
pub(super) struct ReplaySnapshot {
    pub(super) identity: FileIdentity,
    /// Retain the old live object while comparing a replacement on the next read.
    pub(super) file: std::fs::File,
    pub(super) state: Arc<ReplayState>,
}

/// Historical stat-cache key. Ledger cache hits additionally compare retained
/// opened-file identities; these timestamps/length alone never prove an object.
#[derive(Debug, PartialEq, Eq, Clone)]
pub(super) struct FileIdentity {
    size: u64,
    mtime: Option<std::time::SystemTime>,
    #[cfg(unix)]
    ino: Option<u64>,
}

/// Resolve a path lexically (`.`/`..` folded) against the current dir.
pub(super) fn resolve_path(dir: &Path) -> PathBuf {
    use std::path::Component;
    let joined = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(dir)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Preserve the shipping lexical namespace resolution, then make the missing
/// suffix stable through existing ancestors. This names a proposed new key;
/// protected I/O additionally requires `checked_scope` and legacy selection.
pub(super) fn canonicalize_dir(dir: &Path) -> PathBuf {
    canonical_session_path(&resolve_path(dir))
}

pub(super) fn checked_scope(dir: &Path) -> std::io::Result<PathBuf> {
    // Filesystem traversal is authoritative. A lexical `..` result may name a
    // different root through a symlink, so it may not select a separate history.
    let physical = crate::lease::checked_session_path(dir)?;
    let lexical = crate::lease::checked_session_path(&resolve_path(dir))?;
    if physical != lexical {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput,
            format!(
                "ambiguous sessions path traversal in {}; configure the sessions path without `..` after a symlink",
                dir.display()
            )));
    }
    Ok(physical)
}

/// Resolve a rename/delete record whose canonical key does not join an edge
/// (a symlink retargeted after the record was written): a childId carried by
/// exactly one edge is the last durable identity they share.
pub(super) fn sole_edge_by_child_id(state: &ReplayState, child_id: &str) -> Option<usize> {
    let mut sole: Option<usize> = None;
    for (at, edge) in state.edges.iter().enumerate() {
        if edge.child_id != child_id {
            continue;
        }
        if sole.is_some() {
            return None;
        }
        sole = Some(at);
    }
    sole
}

pub(super) fn file_identity(path: &Path) -> Result<Option<FileIdentity>> {
    Ok(path_identity(path))
}

fn path_identity(path: &Path) -> Option<FileIdentity> {
    let metadata = fs::metadata(path).ok()?;
    Some(metadata_identity(&metadata))
}

pub(super) fn opened_file_stat(file: &std::fs::File) -> Result<FileIdentity> {
    Ok(metadata_identity(&file.metadata()?))
}

fn metadata_identity(metadata: &std::fs::Metadata) -> FileIdentity {
    FileIdentity {
        size: metadata.len(),
        mtime: metadata.modified().ok(),
        #[cfg(unix)]
        ino: {
            use std::os::unix::fs::MetadataExt;
            Some(metadata.ino())
        },
    }
}

pub(super) fn is_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file())
}
