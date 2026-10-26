//! The runtime-ready probe concern: the two-layer memo (in-process map + the cross-process on-disk
//! verdict) that skips re-probing a venv nobody damaged.

use super::{
    collect_python_files, Digest, HashMap, KernelPythonSkill, Mutex, Path, PathBuf, Stdio,
    DEFAULT_RLM_EXTRA_PACKAGES,
};

fn python_imports(python: &str, module_name: &str) -> bool {
    run_quiet(python, &["-c", &format!("import {module_name}")])
}

fn run_quiet(command: &str, args: &[&str]) -> bool {
    let mut child = std::process::Command::new(command);
    child
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Hidden window on Windows (TS `spawnHidden`).
    crate::platform::process::set_no_window(&mut child);
    if cfg!(windows) {
        child.env("PYTHONUTF8", "1");
    }
    matches!(child.status(), Ok(status) if status.success())
}

/// Exact internal runtime ABI understood by this host. Closed enum additions
/// require an explicit new host/runtime pairing; unknown newer ABIs fail closed.
pub(crate) const SUPPORTED_RLM_RUNTIME_ABI: u64 = 2;

/// One pre-spawn probe interpreter, awaited without blocking the async
/// runtime and bounded by `bound`. An interpreter that outlives the bound, or
/// a caller that drops this future (dispose, a start timeout), kills the
/// probe child: a wedged probe must never wedge the kernel start.
async fn probe_quiet_with_env(
    command: &str,
    args: &[&str],
    env: &HashMap<String, String>,
    cwd: Option<&Path>,
    bound: std::time::Duration,
) -> bool {
    let mut command = tokio::process::Command::new(command);
    command
        .args(args)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    crate::platform::process::set_no_window(command.as_std_mut());
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    match tokio::time::timeout(bound, child.wait()).await {
        Ok(status) => matches!(status, Ok(status) if status.success()),
        Err(_elapsed) => {
            // Fail closed, and reap the probe instead of leaking it.
            let _ = child.kill().await;
            false
        }
    }
}

/// The runtime-ready assertion from the TS product: a current
/// prime-agent-runtime with the callable RLM surface, harness CRUD, bash
/// handles, and protocol version 3.
const RUNTIME_READY_CHECK: &str = "import inspect; import rlm; assert type(getattr(rlm, 'RLM_RUNTIME_ABI', None)) is int and rlm.RLM_RUNTIME_ABI == 2, 'prime-agent-runtime ABI 2 is required (queued/error subagent rows)'; from rlm import McpIntegration; import rlm.mcp as mcp; from rlm.harness import HarnessEntry; _harness_methods = ['create_memory', 'update_memory', 'delete_memory', 'create_skill', 'update_skill', 'delete_skill', 'create_subagent', 'update_subagent', 'delete_subagent', 'create_prompt_note', 'update_prompt_note', 'delete_prompt_note', 'record_refinement']; _mcp_discovery_methods = ['list_plugins', 'search_plugins', 'list_connections', 'search_tools', 'describe_tool']; assert callable(mcp.list_tools); assert callable(mcp.call_tool); assert all(callable(getattr(mcp, _m, None)) for _m in _mcp_discovery_methods), \"rlm.mcp is missing MCP discovery methods (list_plugins, search_plugins, list_connections, search_tools, describe_tool); the kernel venv needs a current prime-agent-runtime\"; assert callable(rlm.spawn); assert hasattr(rlm, 'rlm'); assert callable(rlm.rlm.spawn); assert inspect.signature(rlm.spawn).parameters['name'].default is inspect.Parameter.empty; assert not hasattr(rlm, 'run'); assert not hasattr(rlm.rlm, 'run'); assert callable(rlm.host_request); assert callable(rlm.find_models); assert callable(rlm.rlm.find_models); assert callable(rlm.create_session); assert callable(rlm.rlm.create_session); assert callable(rlm.progress_note); assert callable(rlm.rlm.progress_note); assert callable(rlm.rename); assert callable(rlm.rlm.rename); assert hasattr(rlm, 'harness'); assert hasattr(rlm, 'get_harness_state'); assert hasattr(rlm.rlm, 'harness'); assert hasattr(rlm.rlm, 'get_harness_state'); assert all(callable(getattr(_harness, _method, None)) for _harness in (rlm.harness, rlm.rlm.harness) for _method in _harness_methods); assert 'reference' in HarnessEntry.__dataclass_fields__; assert 'scope' in HarnessEntry.__dataclass_fields__; assert 'reference' in inspect.signature(rlm.harness.create_skill).parameters; assert 'reference' in inspect.signature(rlm.harness.update_skill).parameters; assert 'global_' in inspect.signature(rlm.harness.create_memory).parameters; assert 'global_' in inspect.signature(rlm.get_harness_state).parameters; assert not hasattr(rlm, 'background'); assert not hasattr(rlm.rlm, 'background'); from rlm.bash import BashHandle, BashResult; assert callable(rlm.bash); assert {'persistent', 'yield_after'} <= set(inspect.signature(rlm.bash).parameters); assert callable(rlm.persistent_shells); assert callable(rlm._set_completion_notices_wired); assert all(callable(getattr(BashHandle, _m, None)) for _m in ('tail', 'output', 'poll', 'kill')); assert {'exit_code', 'output', 'duration'} <= set(BashResult.__dataclass_fields__); import rlm.repl as _repl; assert callable(_repl.main); assert callable(_repl.emit); assert callable(_repl.host_request); assert callable(_repl.is_active); assert _repl.PROTOCOL_VERSION == 3; assert callable(rlm.emit); assert not hasattr(rlm, 'HOST_COMM_TARGET'); assert not hasattr(mcp, 'install_shutdown_hook')";

pub(crate) fn has_prime_agent_runtime(python: &str) -> bool {
    run_quiet(python, &["-c", RUNTIME_READY_CHECK])
}

pub(crate) fn has_state_snapshot_runtime(python: &str) -> bool {
    python_imports(python, "dill")
}

pub(crate) fn has_required_default_imports(python: &str) -> bool {
    let imports = DEFAULT_RLM_EXTRA_PACKAGES
        .iter()
        .map(|(_, import, _)| *import)
        .collect::<Vec<_>>()
        .join(",");
    run_quiet(python, &["-c", &format!("import {imports}")])
}

/// The pre-spawn check of the exact interpreter, env, and cwd the kernel will
/// run with. Each probe interpreter gets `bound`; one that does not exit in
/// time fails the check closed. Async so the probes never block a runtime
/// thread (a blocked current-thread runtime cannot even fire its timers).
pub(crate) async fn spawn_environment_ready(
    python: &str,
    env: &HashMap<String, String>,
    cwd: Option<&Path>,
    bound: std::time::Duration,
) -> bool {
    let imports = DEFAULT_RLM_EXTRA_PACKAGES
        .iter()
        .map(|(_, import, _)| *import)
        .collect::<Vec<_>>()
        .join(",");
    probe_quiet_with_env(python, &["-c", RUNTIME_READY_CHECK], env, cwd, bound).await
        && probe_quiet_with_env(python, &["-c", "import dill"], env, cwd, bound).await
        && probe_quiet_with_env(
            python,
            &["-c", &format!("import {imports}")],
            env,
            cwd,
            bound,
        )
        .await
}

pub(crate) fn missing_rlm_extra_import_labels(python: &str) -> Vec<&'static str> {
    DEFAULT_RLM_EXTRA_PACKAGES
        .iter()
        .filter(|(_, import, _)| !python_imports(python, import))
        .map(|(_, _, label)| *label)
        .collect()
}

pub(crate) fn missing_python_skill_import_labels(
    python: &str,
    python_skills: &[KernelPythonSkill],
) -> Vec<String> {
    python_skills
        .iter()
        .filter(|skill| !python_imports(python, &skill.import_name))
        .map(|skill| format!("{} ({})", skill.name, skill.import_name))
        .collect()
}

/// Process-global memo of a successful runtime-ready probe, tiered above the cross-process on-disk
/// memo ([`super::super::disk_memo`]): the probe is a full interpreter start. Memoized on success
/// only: the key carries every input the probe observes; a failed kernel start drops both layers
/// ([`invalidate_runtime_probe_cache`]).
static RUNTIME_PROBE_MEMO: Mutex<Option<HashMap<String, PathBuf>>> = Mutex::new(None);

pub(super) fn lock_probe_memo() -> std::sync::MutexGuard<'static, Option<HashMap<String, PathBuf>>>
{
    RUNTIME_PROBE_MEMO
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Identity of the runtime as installed in the venv: the interpreter binary's stat plus a content
/// hash of the installed `rlm` package tree. Out-of-band damage changes this identity, so a
/// memoized probe result can never mask a mutated install.
pub(super) fn installed_runtime_identity(python: &Path, venv: &Path) -> String {
    let mut hasher = sha2::Sha256::new();
    match std::fs::metadata(python) {
        Ok(meta) => {
            let modified = meta
                .modified()
                .map_or_else(|_| "no-mtime".to_string(), |time| format!("{time:?}"));
            hasher.update(format!("py:{}:{}:{modified}", python.display(), meta.len()).as_bytes());
        }
        Err(error) => hasher.update(format!("py-error:{}:{error}", python.display()).as_bytes()),
    }
    for package in ["rlm", "dill"] {
        match installed_package_dir(venv, package) {
            Some(dir) => match hash_python_tree(&dir) {
                Ok(hash) => hasher.update(format!("{package}:{hash}").as_bytes()),
                Err(error) => hasher.update(format!("{package}-error:{error}").as_bytes()),
            },
            None => hasher.update(format!("{package}-missing").as_bytes()),
        }
    }
    format!("sha256:{:x}", hasher.finalize())
}

/// The installed `rlm` package under the venv's site-packages: the Windows layout
/// `<venv>/Lib/site-packages/rlm` (no python-version layer) or the Unix layout
/// `<venv>/lib/python*/site-packages/rlm`.
#[cfg(test)]
pub(super) fn installed_rlm_dir(venv: &Path) -> Option<PathBuf> {
    installed_package_dir(venv, "rlm")
}

pub(super) fn installed_package_dir(venv: &Path, package: &str) -> Option<PathBuf> {
    let lib = venv.join("lib");
    let windows_layout = lib.join("site-packages").join(package);
    if windows_layout.is_dir() {
        return Some(windows_layout);
    }
    let entries = std::fs::read_dir(&lib).ok()?;
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        if !entry.file_name().to_string_lossy().starts_with("python") {
            continue;
        }
        let installed = entry.path().join("site-packages").join(package);
        if installed.is_dir() {
            return Some(installed);
        }
    }
    None
}

/// Content hash of a python tree: every `.py` file's relative path and
/// bytes, in sorted order (same witness shape as `hash_runtime_source`).
fn hash_python_tree(dir: &Path) -> anyhow::Result<String> {
    let mut files = Vec::new();
    collect_python_files(dir, &mut files)?;
    files.sort();
    let mut hasher = sha2::Sha256::new();
    for file in &files {
        let relative = file.strip_prefix(dir)?;
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update(&std::fs::read(file)?);
        hasher.update([0]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// The memo key: every input the runtime-ready probe observes.
pub(super) fn runtime_probe_key(
    python: &str,
    runtime_identity: &str,
    version_raw: &str,
    installed_identity: &str,
) -> String {
    format!(
        "{python}\u{0}{runtime_identity}\u{0}{installed_identity}\u{0}sha256:{:x}",
        sha2::Sha256::digest(version_raw.as_bytes())
    )
}

/// The runtime-ready check, memoized on success across two layers: the process-global map first,
/// then the on-disk cross-process memo. `version_raw` is the raw `.bootstrap-version` text the
/// caller already read. The key is recomputed fresh on every call so a hit skips only the two
/// interpreter probes. A caller-owned `PRIME_AGENT_KERNEL_PYTHON` override never reaches this.
pub(super) fn has_prime_agent_runtime_memoized(
    python: &str,
    runtime_identity: &str,
    version_raw: &str,
    installed_identity: &str,
    venv: &Path,
) -> bool {
    let key = runtime_probe_key(python, runtime_identity, version_raw, installed_identity);
    let memo_path = super::super::disk_memo::disk_memo_path(venv);
    if lock_probe_memo()
        .as_ref()
        .is_some_and(|memo| memo.contains_key(&key))
    {
        return true;
    }
    if super::super::disk_memo::disk_memo_hit(&memo_path, &key) {
        let mut memo = lock_probe_memo();
        let entries = memo.get_or_insert_with(HashMap::new);
        if entries.len() >= 16 {
            entries.clear();
        }
        entries.insert(key, memo_path);
        return true;
    }
    if !has_prime_agent_runtime(python) || !python_imports(python, "dill") {
        return false;
    }
    let mut memo = lock_probe_memo();
    let entries = memo.get_or_insert_with(HashMap::new);
    if entries.len() >= 16 {
        entries.clear();
    }
    super::super::disk_memo::disk_memo_write(&memo_path, &key);
    entries.insert(key, memo_path);
    true
}

/// Drop every memoized runtime-ready result, both layers: the in-process map dies with this call.
/// The next kernel start re-runs the probe.
pub fn invalidate_runtime_probe_cache() {
    let tracked: Vec<PathBuf> = lock_probe_memo()
        .take()
        .map(|memo| memo.values().cloned().collect())
        .unwrap_or_default();
    for path in tracked {
        super::super::disk_memo::disk_memo_invalidate(&path);
    }
}

/// Drop only the in-process memo layer, leaving the on-disk layer intact:
/// the fresh-process simulation the disk-memo oracles use. Unix only.
#[cfg(all(test, unix))]
pub(crate) fn clear_in_process_probe_memo_for_tests() {
    *lock_probe_memo() = None;
}

#[cfg(all(test, unix))]
mod spawn_probe_tests {
    use super::{spawn_environment_ready, HashMap};
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    /// A probe interpreter that never exits fails the check closed within
    /// its bound, and is killed and reaped rather than leaked.
    #[tokio::test]
    async fn wedged_spawn_probe_fails_closed_within_its_bound() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let python = dir.path().join("wedged-probe-python");
        let pid_file = dir.path().join("probe-pid");
        std::fs::write(
            &python,
            format!(
                "#!/bin/sh\nif [ \"${{1:-}}\" = \"-c\" ]; then echo $$ > '{}'; exec sleep 600; fi\nexit 1\n",
                pid_file.display()
            ),
        )
        .expect("write fake python");
        std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake python");
        let started = Instant::now();
        let ready = tokio::time::timeout(
            Duration::from_secs(10),
            spawn_environment_ready(
                &python.to_string_lossy(),
                &HashMap::new(),
                None,
                Duration::from_secs(2),
            ),
        )
        .await
        .expect("the bound must settle the probe");
        assert!(!ready, "a probe that never exits must fail closed");
        assert!(started.elapsed() < Duration::from_secs(8));
        let pid = std::fs::read_to_string(&pid_file).expect("the probe ran");
        let state = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", pid.trim()])
            .output()
            .expect("ps");
        assert!(
            String::from_utf8_lossy(&state.stdout).trim().is_empty(),
            "the timed-out probe interpreter must be killed and reaped"
        );
    }
}

#[cfg(test)]
mod runtime_abi_tests {
    use super::{RUNTIME_READY_CHECK, SUPPORTED_RLM_RUNTIME_ABI};

    #[test]
    fn runtime_probe_requires_the_exact_integer_abi_two() {
        assert_eq!(SUPPORTED_RLM_RUNTIME_ABI, 2);
        assert!(RUNTIME_READY_CHECK.contains(
            "type(getattr(rlm, 'RLM_RUNTIME_ABI', None)) is int and rlm.RLM_RUNTIME_ABI == 2"
        ));
        assert!(!RUNTIME_READY_CHECK.contains(">= 2"));
    }

    #[test]
    #[ignore = "live: PA_TEST_RUNTIME_ABI_PYTHON must name an installed packaged ABI-2 runtime"]
    fn configured_packaged_runtime_satisfies_the_full_probe() {
        let python = std::env::var("PA_TEST_RUNTIME_ABI_PYTHON")
            .expect("set PA_TEST_RUNTIME_ABI_PYTHON for this live verifier");
        assert!(super::has_prime_agent_runtime(&python));
    }
}
