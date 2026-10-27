//! Venv discovery, build, lock, and the shared `.bootstrap-version` cache:
//! the machine state behind [`super::ensure_kernel_python`]. The version
//! file is a cross-session cache, not a per-session manifest.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context};
use sha2::Digest;
use tokio::io::{AsyncRead, AsyncReadExt};

use super::{
    default_rlm_extra_uv_args, EnsureKernelPythonOptions, KernelPythonSkill,
    DEFAULT_RLM_EXTRA_PACKAGES,
};

// The concern children (the flows + the shared record stay in the
// composition root).
mod diagnostics;
mod generation;
mod layout;
mod probe;
mod runtime_source;
mod skills;
mod uv;
mod version;

use diagnostics::{
    ambient_secret_values, bounded_stderr_tail, sanitize_bootstrap_diagnostic, STDERR_BUFFER_BYTES,
};
pub(crate) use generation::{
    begin_kernel_lease_handoff, register_kernel_python_lease, GenerationLeaseGuard,
};
pub(super) use generation::{
    generation_identity, generation_root, lease_for_resolver, prepare_generation_root,
    published_candidates, GenerationBuild,
};
use layout::home_dir;
pub(crate) use layout::{expand_home, resolve_writable_kernel_venv_dir};
pub use layout::{kernel_venv_dir, kernel_venv_python};
pub use probe::invalidate_runtime_probe_cache;
#[cfg(test)]
use probe::{installed_rlm_dir, lock_probe_memo, runtime_probe_key};
// The memo-clear helper and the live-probe package-dir walk exist only behind
// the unix tests (see their gates in probe.rs and tests.rs).
#[cfg(all(test, unix))]
use probe::{clear_in_process_probe_memo_for_tests, installed_package_dir};
pub(crate) use probe::{
    has_prime_agent_runtime, has_required_default_imports, has_state_snapshot_runtime,
    missing_python_skill_import_labels, missing_rlm_extra_import_labels, spawn_environment_ready,
    SpawnProbe, SUPPORTED_RLM_RUNTIME_ABI,
};
use probe::{has_prime_agent_runtime_memoized, installed_runtime_identity};
#[cfg(test)]
pub(crate) use probe::{probe_memo_has_for_tests, seed_probe_memo_for_tests};
use runtime_source::collect_python_files;
pub(super) use runtime_source::{hash_runtime_source, require_runtime_source_dir};
pub(super) use runtime_source::{package_dir, packaged_runtime_dir};
#[cfg(test)]
use skills::{
    file_content_hash, read_python_skill_dependency_names, read_python_skill_project_name,
};
pub(crate) use skills::{normalize_python_skills, BootstrapPythonSkill};
pub(crate) use uv::ensure_uv;
#[cfg(test)]
use uv::windows_executable_candidates;
#[cfg(test)]
use version::{
    bootstrap_base_version_current, recorded_skills_cover, write_bootstrap_version,
    BOOTSTRAP_SCHEMA,
};
use version::{
    bootstrap_skill_key, bootstrap_version_current, read_bootstrap_version,
    read_bootstrap_version_raw, write_bootstrap_version_with_outcome, STATE_SNAPSHOT_REQUIREMENT,
};

const PYTHON_VERSION: &str = "3.11";
pub(crate) const BOOTSTRAP_LOCK_NAME: &str = ".bootstrap.lock";
pub(crate) const BOOTSTRAP_LOCK_RETRY_MS: u64 = 100;
pub(crate) const BOOTSTRAP_LOCK_STALE_WITHOUT_PID_MS: u64 = 30_000;
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct BootstrapUnavailablePythonSkill {
    pub(super) import_name: String,
    pub(super) package_path: String,
    pub(super) pyproject_path: String,
    pub(super) pyproject_hash: String,
    pub(super) reason: String,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct BootstrapVersion {
    schema: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    runtime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    extra_uv_args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    python_skills: Option<Vec<BootstrapPythonSkill>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    requested_python_skills: Option<Vec<BootstrapPythonSkill>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unavailable_python_skills: Option<Vec<BootstrapUnavailablePythonSkill>>,
}

#[derive(Clone, Default)]
struct RetainedStderr {
    bytes: Vec<u8>,
    leading_record_truncated: bool,
}

struct AbortTaskOnDrop {
    handle: tokio::task::AbortHandle,
    armed: bool,
}

impl AbortTaskOnDrop {
    fn new(handle: tokio::task::AbortHandle) -> Self {
        Self {
            handle,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for AbortTaskOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.handle.abort();
        }
    }
}

async fn drain_stderr_tail(
    mut stderr: impl AsyncRead + Unpin,
    shared: &Mutex<RetainedStderr>,
) -> std::io::Result<()> {
    let mut retained = RetainedStderr {
        bytes: Vec::with_capacity(STDERR_BUFFER_BYTES),
        leading_record_truncated: false,
    };
    let mut chunk = [0u8; 4 * 1024];
    loop {
        let count = stderr.read(&mut chunk).await?;
        if count == 0 {
            break;
        }
        let mut combined = Vec::with_capacity(retained.bytes.len() + count);
        combined.extend_from_slice(&retained.bytes);
        combined.extend_from_slice(&chunk[..count]);
        if combined.len() > STDERR_BUFFER_BYTES {
            let split = combined.len() - STDERR_BUFFER_BYTES;
            retained.leading_record_truncated = combined[split - 1] != b'\n';
            retained.bytes = combined.split_off(split);
        } else {
            retained.bytes = combined;
        }
        *shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = retained.clone();
    }
    Ok(())
}

async fn run_async(command: &str, args: &[String]) -> anyhow::Result<()> {
    let secrets = ambient_secret_values();
    let command_label = sanitize_bootstrap_diagnostic(command, &secrets);
    let mut process = tokio::process::Command::new(command);
    process
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // Hidden window on Windows (TS `spawnHidden`).
    crate::platform::process::set_no_window(process.as_std_mut());
    if cfg!(windows) {
        process.env("PYTHONUTF8", "1");
    }
    process.kill_on_drop(true);
    let mut child = process
        .spawn()
        .with_context(|| format!("failed to spawn bootstrap command {command_label}"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("bootstrap command {command_label} did not expose stderr"))?;
    let retained = Arc::new(Mutex::new(RetainedStderr::default()));
    let retained_for_reader = Arc::clone(&retained);
    let mut reader =
        tokio::spawn(async move { drain_stderr_tail(stderr, &retained_for_reader).await });
    let mut reader_abort = AbortTaskOnDrop::new(reader.abort_handle());
    let status_result = child.wait().await;
    match tokio::time::timeout(Duration::from_millis(250), &mut reader).await {
        Ok(Ok(result)) => {
            reader_abort.disarm();
            result.with_context(|| {
                format!("failed to drain bootstrap command {command_label} stderr")
            })?;
        }
        Ok(Err(error)) => {
            reader_abort.disarm();
            return Err(anyhow!(
                "bootstrap command {command_label} stderr reader failed: {error}"
            ));
        }
        Err(_) => {
            // A descendant inherited stderr. Cancelling the async reader drops
            // our pipe handle; bootstrap completion never waits for foreign EOF.
            reader.abort();
            let _ = reader.await;
            reader_abort.disarm();
        }
    }

    let status = status_result
        .with_context(|| format!("failed to wait for bootstrap command {command_label}"))?;
    let retained = retained
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&retained.bytes);
    let tail = bounded_stderr_tail(&stderr, retained.leading_record_truncated, &secrets);
    let reason = status.code().map_or_else(
        || "terminated by a signal".to_string(),
        |code| format!("failed with exit code {code}"),
    );
    if tail.is_empty() {
        Err(anyhow!("bootstrap command {command_label} {reason}"))
    } else {
        Err(anyhow!(
            "bootstrap command {command_label} {reason}\nstderr (safe tail):\n{tail}"
        ))
    }
}

pub(in crate::kernel::bootstrap) async fn bootstrap_venv(
    venv: &Path,
    source_dir: &Path,
    runtime_identity: &str,
    python_skills: &[BootstrapPythonSkill],
    options: &EnsureKernelPythonOptions,
    generation_build: Option<&GenerationBuild>,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(venv.parent().unwrap_or(Path::new("/")))?;
    let uv = ensure_uv()?;
    let python = kernel_venv_python(venv);

    let venv_str = venv.to_string_lossy().to_string();
    let python_str = python.to_string_lossy().to_string();
    let mut install_args = vec![
        "pip".to_string(),
        "install".to_string(),
        "--python".to_string(),
        python_str,
        source_dir.to_string_lossy().to_string(),
    ];
    install_args.push(STATE_SNAPSHOT_REQUIREMENT.to_string());
    for uv_arg in default_rlm_extra_uv_args() {
        install_args.push(uv_arg.to_string());
    }

    run_async(
        &uv,
        &[
            "python".to_string(),
            "install".to_string(),
            PYTHON_VERSION.to_string(),
        ],
    )
    .await?;
    run_async(
        &uv,
        &[
            "venv".to_string(),
            venv_str,
            "--python".to_string(),
            PYTHON_VERSION.to_string(),
            "--seed".to_string(),
        ],
    )
    .await?;
    if let Some(build) = generation_build {
        build.claim_created_path()?;
    }
    run_async(&uv, &install_args).await?;
    sync_python_skills(
        &uv,
        venv,
        &python,
        source_dir,
        runtime_identity,
        python_skills,
        options,
    )
    .await
}

/// Install/refresh the editable Python skills recorded in the version file. Only skills missing or
/// changed are installed. Per-skill failures warn and continue.
pub(crate) async fn sync_python_skills(
    uv: &str,
    venv: &Path,
    python: &Path,
    source_dir: &Path,
    runtime_identity: &str,
    python_skills: &[BootstrapPythonSkill],
    options: &EnsureKernelPythonOptions,
) -> anyhow::Result<()> {
    let version = read_bootstrap_version(venv);
    // Previously installed skills still present on disk: their records carry over so
    // sessions with different skill sets share one venv cache.
    let current_python_skills: HashMap<String, BootstrapPythonSkill> = version
        .as_ref()
        .and_then(|v| v.python_skills.clone())
        .unwrap_or_default()
        .into_iter()
        .filter(|recorded| Path::new(&recorded.package_path).is_dir())
        .map(|skill| (bootstrap_skill_key(&skill), skill))
        .collect();
    let python_str = python.to_string_lossy().to_string();
    let mut installed: HashMap<String, BootstrapPythonSkill> = current_python_skills;
    let mut install_failures = HashMap::new();
    let mut missing: Vec<&BootstrapPythonSkill> = Vec::new();
    for skill in python_skills {
        let key = bootstrap_skill_key(skill);
        if installed.get(&key).is_some_and(|existing| {
            existing.pyproject_path == skill.pyproject_path
                && existing.pyproject_hash == skill.pyproject_hash
        }) {
            continue;
        }
        missing.push(skill);
    }
    if !missing.is_empty() {
        // One uv invocation installs the whole batch of missing skills: a fresh bootstrap otherwise
        // pays one process plus build-backend startup per install. A batch failure falls back to
        // the per-skill loop.
        let mut install_args = vec![
            "pip".to_string(),
            "install".to_string(),
            "--python".to_string(),
            python_str.clone(),
            // Skills may depend on the runtime too. Keep their resolver
            // pinned to the same local source, including per-skill retries.
            source_dir.to_string_lossy().to_string(),
        ];
        for skill in &missing {
            install_args.push("--editable".to_string());
            install_args.push(skill.package_path.clone());
        }
        if run_async(uv, &install_args).await.is_ok() {
            for skill in &missing {
                installed.insert(bootstrap_skill_key(skill), (*skill).clone());
            }
        } else {
            for skill in &missing {
                match run_async(
                    uv,
                    &[
                        "pip".to_string(),
                        "install".to_string(),
                        "--python".to_string(),
                        python_str.clone(),
                        source_dir.to_string_lossy().to_string(),
                        "--editable".to_string(),
                        skill.package_path.clone(),
                    ],
                )
                .await
                {
                    Ok(()) => {
                        installed.insert(bootstrap_skill_key(skill), (*skill).clone());
                    }
                    Err(error) => {
                        install_failures.insert(bootstrap_skill_key(skill), error.to_string());
                    }
                }
            }
        }
    }

    // Publication records observable import readiness, not a package manager's
    // exit status. Every requested fingerprint lands in exactly one side of
    // installed ∪ unavailable, including an installed module that fails import.
    let mut unavailable = Vec::new();
    for skill in python_skills {
        let key = bootstrap_skill_key(skill);
        let import = run_async(
            &python_str,
            &["-c".to_string(), format!("import {}", skill.import_name)],
        )
        .await;
        match import {
            Ok(()) => {
                installed.insert(key, skill.clone());
            }
            Err(import_error) => {
                installed.remove(&key);
                let reason = install_failures.remove(&key).map_or_else(
                    || import_error.to_string(),
                    |install_error| format!("{install_error}; import probe failed: {import_error}"),
                );
                options.report(&format!(
                    "Warning: Python skill {} is unavailable in this kernel generation: {reason}",
                    skill.import_name
                ));
                unavailable.push(BootstrapUnavailablePythonSkill {
                    import_name: skill.import_name.clone(),
                    package_path: skill.package_path.clone(),
                    pyproject_path: skill.pyproject_path.clone(),
                    pyproject_hash: skill.pyproject_hash.clone(),
                    reason,
                });
            }
        }
    }

    let mut merged: Vec<BootstrapPythonSkill> = installed.into_values().collect();
    merged.sort_by(|a, b| {
        a.package_path
            .cmp(&b.package_path)
            .then(a.import_name.cmp(&b.import_name))
    });
    unavailable.sort_by(|a, b| {
        a.package_path
            .cmp(&b.package_path)
            .then(a.import_name.cmp(&b.import_name))
    });
    write_bootstrap_version_with_outcome(
        venv,
        runtime_identity,
        &merged,
        python_skills,
        &unavailable,
    )
}

pub(crate) fn recorded_unavailable_python_skills(
    venv: &Path,
) -> Vec<BootstrapUnavailablePythonSkill> {
    read_bootstrap_version(venv)
        .and_then(|version| version.unavailable_python_skills)
        .unwrap_or_default()
}

pub(crate) fn kernel_ready_read_only(
    python: &str,
    venv: &Path,
    runtime_identity: &str,
    python_skills: &[BootstrapPythonSkill],
) -> bool {
    bootstrap_version_current(
        read_bootstrap_version(venv).as_ref(),
        runtime_identity,
        python_skills,
    ) && has_prime_agent_runtime(python)
        && has_state_snapshot_runtime(python)
        && has_required_default_imports(python)
}

pub(crate) fn kernel_runtime_ready(
    python: &str,
    venv: &Path,
    runtime_identity: &str,
    python_skills: &[BootstrapPythonSkill],
) -> bool {
    let (version, raw) = read_bootstrap_version_raw(venv);
    bootstrap_version_current(version.as_ref(), runtime_identity, python_skills)
        && has_prime_agent_runtime_memoized(
            python,
            runtime_identity,
            &raw,
            &installed_runtime_identity(Path::new(python), venv),
            venv,
        )
}

pub(crate) fn kernel_ready(
    python: &str,
    venv: &Path,
    runtime_identity: &str,
    python_skills: &[BootstrapPythonSkill],
) -> bool {
    kernel_runtime_ready(python, venv, runtime_identity, python_skills)
        && has_required_default_imports(python)
}

#[cfg(test)]
mod tests;
