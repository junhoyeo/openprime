//! Session-artifact partitioning: registration of per-session scheduled-jobs
//! artifact files, recovery of a session's jobs from its artifact partition,
//! and the partitioned merge-write that keeps each artifact file holding only
//! its session's jobs and dispatches.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::state::{
    merge_fresh_jobs, read_jobs_state, recover_interrupted_in_state, with_state_locks,
    write_jobs_state,
};
use super::{
    iso_from_millis, AgentCronDispatchRecord, AgentCronJobStore, CronJobsState,
    SESSION_SCHEDULED_JOBS_FILENAME,
};
use crate::cron::AgentCronJob;

impl AgentCronJobStore {
    /// Register one session's artifact partition. Idempotent: re-registering
    /// the same directory is a no-op, so call sites can bind on every read.
    pub fn register_session_artifact(&self, session_id: &str, artifact_dir: &Path) -> bool {
        if !self.session_artifact_mode {
            return false;
        }
        let path = artifact_dir.join(SESSION_SCHEDULED_JOBS_FILENAME);
        let mut files = self
            .session_artifact_files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if files.get(session_id) == Some(&path) {
            return false;
        }
        files.insert(session_id.to_string(), path);
        true
    }

    pub fn session_artifacts(&self) -> Vec<(String, PathBuf)> {
        let files = self
            .session_artifact_files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        files.clone().into_iter().collect()
    }

    /// # Errors
    ///
    /// Returns an error when the state lock or write fails.
    pub fn recover_session_artifact(
        &self,
        session_id: &str,
        now: u64,
    ) -> anyhow::Result<Vec<AgentCronJob>> {
        let Some(path) = self
            .session_artifacts()
            .into_iter()
            .find(|(id, _)| id == session_id)
            .map(|(_, path)| path)
        else {
            return Ok(Vec::new());
        };
        with_state_locks(
            std::slice::from_ref(&path),
            || -> anyhow::Result<Vec<AgentCronJob>> {
                let mut state = read_jobs_state(&path);
                let mut recovered = Vec::new();
                if !state.dispatches.is_empty() {
                    recover_interrupted_in_state(&mut state, now, &mut recovered, None);
                    write_jobs_state(&path, &state)?;
                }
                Ok(recovered)
            },
        )
        .ok_or_else(|| anyhow::anyhow!("cron jobs state lock not acquired; skipped"))?
    }
    pub(crate) fn write_jobs_session_artifacts(&self, jobs: &[AgentCronJob]) -> anyhow::Result<()> {
        let artifact_files = self
            .session_artifact_files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let registered: std::collections::HashSet<String> =
            artifact_files.keys().cloned().collect();
        for job in jobs {
            if !registered.contains(&job.session_id) {
                anyhow::bail!(
                    "cron job session artifact not registered: {}",
                    job.session_id
                );
            }
        }
        let paths: Vec<PathBuf> = artifact_files.values().cloned().collect();
        let with_locks = with_state_locks(&paths, || -> anyhow::Result<()> {
            let current_by_session_id: HashMap<String, CronJobsState> = artifact_files
                .iter()
                .map(|(session_id, path)| (session_id.clone(), read_jobs_state(path)))
                .collect();
            let incoming_by_id: HashMap<&str, &AgentCronJob> =
                jobs.iter().map(|job| (job.id.as_str(), job)).collect();
            let mut merged_by_session_id: HashMap<String, Vec<AgentCronJob>> = HashMap::new();
            for (session_id, current) in &current_by_session_id {
                let retained: Vec<AgentCronJob> = current
                    .jobs
                    .iter()
                    .filter(|job| {
                        incoming_by_id
                            .get(job.id.as_str())
                            .is_none_or(|incoming| incoming.session_id == *session_id)
                    })
                    .cloned()
                    .collect();
                let session_jobs: Vec<AgentCronJob> = jobs
                    .iter()
                    .filter(|job| job.session_id == *session_id)
                    .cloned()
                    .collect();
                merged_by_session_id
                    .insert(session_id.clone(), merge_fresh_jobs(retained, session_jobs));
            }
            let session_id_by_job_id: HashMap<String, String> = merged_by_session_id
                .iter()
                .flat_map(|(session_id, session_jobs)| {
                    session_jobs
                        .iter()
                        .map(move |job| (job.id.clone(), session_id.clone()))
                })
                .collect();
            let all_dispatches: Vec<AgentCronDispatchRecord> = current_by_session_id
                .values()
                .flat_map(|state| state.dispatches.clone())
                .collect();
            for (session_id, path) in &artifact_files {
                let current = current_by_session_id
                    .get(session_id)
                    .cloned()
                    .unwrap_or_default();
                let next_state = CronJobsState {
                    jobs: merged_by_session_id
                        .get(session_id)
                        .cloned()
                        .unwrap_or_default(),
                    dispatches: all_dispatches
                        .iter()
                        .filter(|dispatch| {
                            session_id_by_job_id.get(&dispatch.job_id) == Some(session_id)
                        })
                        .cloned()
                        .collect(),
                };
                if serde_json::to_string(&current).unwrap_or_default()
                    != serde_json::to_string(&next_state).unwrap_or_default()
                {
                    write_jobs_state(path, &next_state)?;
                }
            }
            Ok(())
        });
        with_locks.ok_or_else(|| anyhow::anyhow!("cron jobs state lock not acquired; skipped"))?
    }
}

/// Read one scheduled-jobs artifact file's job rows. A missing or
/// unparseable file reads as no jobs.
#[must_use]
pub fn read_scheduled_jobs_artifact(path: &Path) -> Vec<AgentCronJob> {
    read_jobs_state(path).jobs
}

/// Read one scheduled-jobs artifact file's job rows, refusing instead of
/// silently dropping on a read failure, unparseable JSON, or any job row
/// that fails to deserialize. A missing file (no partition registered yet)
/// reads as no jobs - the one case this treats the same as the lenient
/// [`read_scheduled_jobs_artifact`]. Used by the stop/delete cancellation
/// path, which must not treat corrupt or malformed state as empty success;
/// every other caller keeps using the lenient read unchanged.
///
/// # Errors
///
/// Returns an error when the file exists but cannot be read, its content
/// is not a JSON object with a `jobs` array, or any job row fails to
/// deserialize.
pub fn read_scheduled_jobs_artifact_strict(path: &Path) -> anyhow::Result<Vec<AgentCronJob>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(anyhow::Error::new(error)
                .context(format!("read cron schedule partition {}", path.display())));
        }
    };
    let parsed: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(parsed) => parsed,
        Err(error) => {
            return Err(anyhow::Error::new(error)
                .context(format!("parse cron schedule partition {}", path.display())));
        }
    };
    let Some(jobs) = parsed.get("jobs").and_then(|jobs| jobs.as_array()) else {
        anyhow::bail!(
            "cron schedule partition {} has no jobs array",
            path.display()
        );
    };
    let mut result = Vec::with_capacity(jobs.len());
    for job in jobs {
        match serde_json::from_value::<AgentCronJob>(job.clone()) {
            Ok(job) => result.push(job),
            Err(error) => {
                return Err(anyhow::Error::new(error)
                    .context(format!("malformed job row in {}", path.display())));
            }
        }
    }
    Ok(result)
}

/// Cancel the given job ids (if still active/paused) within ONE partition file
/// directly, bypassing the store's registration model entirely. The store
/// registers at most one directory per session id, so it cannot represent two
/// simultaneously-registered partitions for one session (for example a raw
/// and a canonical spelling of an aliased path) - callers that already know,
/// by their own identity resolution, exactly which job ids in which file
/// belong together use this instead, one file at a time, so the raw and
/// canonical partitions cancel independently without merging.
///
/// # Errors
///
/// Returns an error when the file's state lock cannot be acquired or the
/// write fails.
pub fn cancel_matching_ids_in_file<S: std::hash::BuildHasher>(
    path: &Path,
    ids: &std::collections::HashSet<String, S>,
    now: u64,
) -> anyhow::Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let now_iso = iso_from_millis(now);
    let paths = [path.to_path_buf()];
    with_state_locks(&paths, || -> anyhow::Result<usize> {
        // A typed round-trip (CronJobsState/AgentCronJob) would silently drop any
        // unrecognized field on every rewrite - neither struct captures unknown
        // keys. Mutate the raw Value tree in place instead, so every other field
        // (dispatches, future/unknown per-job and top-level fields) survives
        // byte-identical; only the matched jobs' status/nextRunAt/updatedAt change.
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => {
                return Err(anyhow::Error::new(error)
                    .context(format!("read cron schedule partition {}", path.display())));
            }
        };
        let mut value: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(value) => value,
            Err(error) => {
                return Err(anyhow::Error::new(error)
                    .context(format!("parse cron schedule partition {}", path.display())));
            }
        };
        let Some(jobs) = value.get_mut("jobs").and_then(|jobs| jobs.as_array_mut()) else {
            return Ok(0);
        };
        let mut cancelled = 0usize;
        for job in jobs.iter_mut() {
            let matches_id = job
                .get("id")
                .and_then(|id| id.as_str())
                .is_some_and(|id| ids.contains(id));
            if !matches_id {
                continue;
            }
            if job.get("status").and_then(|status| status.as_str()) == Some("cancelled") {
                continue;
            }
            let Some(object) = job.as_object_mut() else {
                continue;
            };
            object.insert(
                "status".to_string(),
                serde_json::Value::String("cancelled".to_string()),
            );
            object.remove("nextRunAt");
            object.insert(
                "updatedAt".to_string(),
                serde_json::Value::String(now_iso.clone()),
            );
            cancelled += 1;
        }
        if cancelled > 0 {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let serialized = serde_json::to_string_pretty(&value)?;
            crate::settings::storage::atomic_write_with(
                path,
                &format!("{serialized}\n"),
                crate::settings::storage::AtomicWriteOptions { fsync: true },
            )?;
        }
        Ok(cancelled)
    })
    .ok_or_else(|| anyhow::anyhow!("cron jobs state lock not acquired; skipped"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cron::store::{input, CancelJobsFilter, SessionBinding};

    #[test]
    fn session_artifact_partitioning() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = AgentCronJobStore::for_session_artifacts();
        let artifacts_a = dir.path().join("a");
        let artifacts_b = dir.path().join("b");
        std::fs::create_dir_all(&artifacts_a).unwrap();
        std::fs::create_dir_all(&artifacts_b).unwrap();
        assert!(store.register_session_artifact("session-1", &artifacts_a));
        assert!(!store.register_session_artifact("session-1", &artifacts_a)); // idempotent
        assert!(store.register_session_artifact("session-2", &artifacts_b));
        let now = 1_700_000_000_000;
        store.create(&input("job a", "every 10m", now)).unwrap();
        let mut job_b = input("job b", "every 20m", now);
        job_b.session_id = "session-2".to_string();
        store.create(&job_b).unwrap();
        let jobs = store.list();
        assert_eq!(jobs.len(), 2);
        let state_a = read_jobs_state(&artifacts_a.join(SESSION_SCHEDULED_JOBS_FILENAME));
        assert_eq!(state_a.jobs.len(), 1);
        assert_eq!(state_a.jobs[0].prompt, "job a");
        let state_b = read_jobs_state(&artifacts_b.join(SESSION_SCHEDULED_JOBS_FILENAME));
        assert_eq!(state_b.jobs.len(), 1);
        let rebound = store
            .rebind_session_jobs(&SessionBinding {
                active_session_id: "live-2".to_string(),
                session_id: "session-1".to_string(),
                session_file: "/w/session.jsonl".to_string(),
                cwd: "/w".to_string(),
            })
            .unwrap();
        assert_eq!(rebound.len(), 2);
        assert!(rebound.iter().all(|job| job.active_session_id == "live-2"));
        let cancelled = store
            .cancel_jobs_for_session(
                &CancelJobsFilter {
                    session_file: Some("/w/session.jsonl".to_string()),
                    ..Default::default()
                },
                now + 1,
            )
            .unwrap();
        // Both jobs share the session file (the rebind moved both), so both cancel.
        assert_eq!(cancelled.len(), 2);
    }
}
