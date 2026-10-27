//! Kernel Python environment bootstrap: the public entry point [`ensure_kernel_python`]
//! resolves (and on first run builds) the venv that runs `python -m rlm.repl`,
//! deduplicating concurrent bootstraps; the venv and version-file machinery lives in
//! [`venv`], the runtime bootstrap code in [`runtime_code`].

pub(crate) mod dir_lock;
mod disk_memo;
mod runtime_code;
pub(crate) mod venv;

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::anyhow;

use dir_lock::acquire_bootstrap_lock;
pub use runtime_code::{
    build_rlm_bootstrap_code, parse_unavailable_python_skills, UnavailablePythonSkills,
    PYTHON_SKILL_IMPORT_ERROR_REPORT_MARKER,
};
pub(crate) use venv::{
    begin_kernel_lease_handoff, register_kernel_python_lease, GenerationLeaseGuard,
};
use venv::{
    bootstrap_venv, expand_home, generation_identity, generation_root, has_prime_agent_runtime,
    kernel_ready, kernel_ready_read_only, kernel_runtime_ready, lease_for_resolver,
    missing_python_skill_import_labels, missing_rlm_extra_import_labels, normalize_python_skills,
    prepare_generation_root, published_candidates, reclaim_explicit_path,
    recorded_unavailable_python_skills, resolve_writable_kernel_venv_dir, BootstrapPythonSkill,
    ExplicitBuild, ExplicitReclaim, GenerationBuild, SUPPORTED_RLM_RUNTIME_ABI,
};
pub use venv::{invalidate_runtime_probe_cache, kernel_venv_dir, kernel_venv_python};

/// One Python skill the kernel should import at bootstrap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelPythonSkill {
    pub name: String,
    pub import_name: String,
    pub package_path: PathBuf,
    pub pyproject_path: PathBuf,
}

/// The default extra packages pre-installed in the kernel venv and promised to
/// the model in the system prompt.
pub const DEFAULT_RLM_EXTRA_PACKAGES: [(&str, &str, &str); 12] = [
    // (uvArg, importName, promptLabel)
    ("requests", "requests", "requests"),
    ("httpx", "httpx", "httpx"),
    ("pyyaml", "yaml", "yaml (PyYAML)"),
    ("tomli", "tomli", "tomli"),
    ("python-dotenv", "dotenv", "dotenv (python-dotenv)"),
    ("pandas", "pandas", "pandas"),
    ("numpy", "numpy", "numpy"),
    ("scipy", "scipy", "scipy"),
    ("beautifulsoup4", "bs4", "bs4 (Beautiful Soup)"),
    ("lxml", "lxml", "lxml"),
    ("pydantic", "pydantic", "pydantic"),
    ("tyro", "tyro", "tyro"),
];

#[must_use]
pub fn default_rlm_extra_uv_args() -> Vec<&'static str> {
    DEFAULT_RLM_EXTRA_PACKAGES
        .iter()
        .map(|(uv, _, _)| *uv)
        .collect()
}

#[must_use]
pub fn default_rlm_extra_import_names() -> Vec<&'static str> {
    DEFAULT_RLM_EXTRA_PACKAGES
        .iter()
        .map(|(_, import, _)| *import)
        .collect()
}

#[must_use]
pub fn default_rlm_extra_import_labels() -> Vec<&'static str> {
    DEFAULT_RLM_EXTRA_PACKAGES
        .iter()
        .map(|(_, _, label)| *label)
        .collect()
}

/// Progress callback for the bootstrap (`ensure_kernel_python`).
pub type KernelBootstrapProgressHandler = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Default)]
pub struct EnsureKernelPythonOptions {
    pub python_skills: Vec<KernelPythonSkill>,
    pub on_progress: Option<KernelBootstrapProgressHandler>,
}

impl EnsureKernelPythonOptions {
    pub(crate) fn report(&self, message: &str) {
        match &self.on_progress {
            Some(handler) => handler(message),
            None => eprintln!("{message}"),
        }
    }
}

fn format_bootstrap_failure(error: &anyhow::Error) -> anyhow::Error {
    let mut message = format!(
        "Failed to set up the Python kernel runtime. {error:#}\n\
         First-time setup needs the bundled local runtime and internet to install uv, Python, and default Python packages; once set up, Prime Agent runs offline. \
         An interrupted runtime upgrade needs network once more, so re-run this while online. \
         Set PRIME_AGENT_KERNEL_PYTHON to a Python with a current prime-agent-runtime and default Python packages installed to skip auto-bootstrap."
    );
    // The packaged exe-adjacent sidecar is the kernel runtime source; when it is missing everywhere
    // (packaged layout and source checkout), name it.
    if venv::packaged_runtime_dir().is_none() {
        let package = venv::package_dir();
        let _ = write!(message,
            "\nThe packaged prime-agent-runtime directory was not found (looked next to the executable at {} and PI_PACKAGE_DIR); reinstall prime-agent so the kernel runtime ships beside the binary.",
            package.display()
        );
    }
    anyhow!(message)
}

/// One in-flight bootstrap per unique options set, joined by concurrent callers.
type InFlightBootstrap = Option<(
    String,
    Arc<tokio::sync::Mutex<Option<anyhow::Result<PathBuf>>>>,
)>;

static IN_FLIGHT: Mutex<InFlightBootstrap> = Mutex::new(None);

/// Resolve the Python interpreter for the kernel: the `PRIME_AGENT_KERNEL_PYTHON`
/// override when valid, else the auto-bootstrapped venv python.
///
/// # Errors
///
/// Returns an error when the override Python is invalid or the venv bootstrap fails.
///
/// # Panics
///
/// The in-flight promise stores its outcome under the same lock, so the `expect` is unreachable.
pub async fn ensure_kernel_python(options: EnsureKernelPythonOptions) -> anyhow::Result<PathBuf> {
    let python_skills = normalize_python_skills(&options.python_skills);
    let key = [
        std::env::var("PRIME_AGENT_KERNEL_PYTHON").unwrap_or_default(),
        std::env::var("PRIME_AGENT_KERNEL_VENV").unwrap_or_default(),
        std::env::var("HOME").unwrap_or_default(),
        std::env::var("XDG_DATA_HOME").unwrap_or_default(),
        serde_json::to_string(&python_skills).unwrap_or_default(),
    ]
    .join("\u{0}");

    let shared = {
        let mut in_flight = IN_FLIGHT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match in_flight.as_ref() {
            Some((existing_key, promise)) if *existing_key == key => promise.clone(),
            _ => {
                let promise = Arc::new(tokio::sync::Mutex::new(None));
                *in_flight = Some((key, promise.clone()));
                promise
            }
        }
    };
    let mut guard = shared.lock().await;
    if guard.is_none() {
        *guard = Some(ensure_kernel_python_uncached(&options, &python_skills).await);
    }
    let outcome = guard.take().expect("outcome was just stored");
    // Drop the registry entry so a later call re-validates instead of reusing
    // a cached rejection forever.
    let mut in_flight = IN_FLIGHT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if matches!(in_flight.as_ref(), Some((_, promise)) if Arc::ptr_eq(promise, &shared)) {
        *in_flight = None;
    }
    outcome
}

async fn ensure_kernel_python_uncached(
    options: &EnsureKernelPythonOptions,
    python_skills: &[BootstrapPythonSkill],
) -> anyhow::Result<PathBuf> {
    if let Ok(override_python) = std::env::var("PRIME_AGENT_KERNEL_PYTHON") {
        if !override_python.is_empty() {
            let python = expand_home(&override_python);
            let python_str = python.to_string_lossy().to_string();
            let mut missing = Vec::new();
            if !has_prime_agent_runtime(&python_str) {
                missing.push(format!(
                    "prime-agent-runtime ABI {SUPPORTED_RLM_RUNTIME_ABI} (queued/error subagent rows) with callable rlm.spawn, rlm.create_session, rlm.host_request, rlm.progress_note, rlm.rename, rlm._set_completion_notices_wired, and explicit harness CRUD methods"
                ));
            }
            if missing.is_empty() {
                let missing_extras = missing_rlm_extra_import_labels(&python_str);
                if !missing_extras.is_empty() {
                    missing.push(format!(
                        "default Python packages ({})",
                        missing_extras.join(", ")
                    ));
                }
            }
            if missing.is_empty() && !options.python_skills.is_empty() {
                let missing_skills =
                    missing_python_skill_import_labels(&python_str, &options.python_skills);
                if !missing_skills.is_empty() {
                    options.report(&format!(
                        "Warning: Python skills unavailable in PRIME_AGENT_KERNEL_PYTHON and will be disabled: {}",
                        missing_skills.join(", ")
                    ));
                }
            }
            if missing.is_empty() {
                return Ok(python);
            }
            return Err(anyhow!(
                "PRIME_AGENT_KERNEL_PYTHON points to a Python missing {}: {}",
                missing.join(" and "),
                python.display()
            ));
        }
    }

    let venv = resolve_writable_kernel_venv_dir()?;
    ensure_kernel_venv(options, python_skills, venv).await
}

async fn ensure_kernel_venv(
    options: &EnsureKernelPythonOptions,
    python_skills: &[BootstrapPythonSkill],
    base_venv: PathBuf,
) -> anyhow::Result<PathBuf> {
    // Validate the bundled source before trusting or touching any environment.
    let source_dir =
        venv::require_runtime_source_dir().map_err(|error| format_bootstrap_failure(&error))?;
    let runtime_identity =
        venv::hash_runtime_source(&source_dir).map_err(|error| format_bootstrap_failure(&error))?;
    let explicit = std::env::var("PRIME_AGENT_KERNEL_VENV").is_ok_and(|value| !value.is_empty());
    let result = if explicit {
        ensure_exact_kernel_venv(
            options,
            python_skills,
            &base_venv,
            &source_dir,
            &runtime_identity,
        )
        .await
    } else {
        ensure_managed_kernel_venv(
            options,
            python_skills,
            &base_venv,
            &source_dir,
            &runtime_identity,
        )
        .await
    };
    result.map_err(|error| format_bootstrap_failure(&error))
}

fn report_recorded_unavailable_skills(options: &EnsureKernelPythonOptions, venv: &std::path::Path) {
    for unavailable in recorded_unavailable_python_skills(venv) {
        options.report(&format!(
            "Warning: Python skill {} remains unavailable in this immutable kernel generation: {}",
            unavailable.import_name, unavailable.reason
        ));
    }
}

fn exact_venv_stale_error(venv: &std::path::Path) -> anyhow::Error {
    anyhow!(
        "PRIME_AGENT_KERNEL_VENV points to a non-current environment at {}. Prime Agent will not replace, relocate, or install into it while another process may use it. Stop dependent sessions and remove or move that path manually, choose a new PRIME_AGENT_KERNEL_VENV, or set PRIME_AGENT_KERNEL_PYTHON",
        venv.display()
    )
}

/// The explicit override is caller-owned and exact. A current path is read
/// only. A missing path is built directly there under its lock. A non-current
/// existing path is refused untouched, except an empty directory (removed
/// with a non-recursive rmdir) and this product's own abandoned build (moved
/// aside, retained); both are then built again.
async fn ensure_exact_kernel_venv(
    options: &EnsureKernelPythonOptions,
    python_skills: &[BootstrapPythonSkill],
    venv: &std::path::Path,
    source_dir: &std::path::Path,
    runtime_identity: &str,
) -> anyhow::Result<PathBuf> {
    let python = kernel_venv_python(venv);
    let python_str = python.to_string_lossy().to_string();
    if kernel_ready_read_only(&python_str, venv, runtime_identity, python_skills) {
        report_recorded_unavailable_skills(options, venv);
        return Ok(python);
    }

    let release_lock = acquire_bootstrap_lock(venv).await.map_err(|error| {
        anyhow!(
            "cannot create the bootstrap lock for the exact PRIME_AGENT_KERNEL_VENV at {}; choose a writable PRIME_AGENT_KERNEL_VENV or set PRIME_AGENT_KERNEL_PYTHON: {error:#}",
            venv.display()
        )
    })?;
    let result = async {
        if kernel_ready_read_only(&python_str, venv, runtime_identity, python_skills) {
            report_recorded_unavailable_skills(options, venv);
            return Ok(python);
        }
        if let ExplicitReclaim::SetAside(aside) = reclaim_explicit_path(venv)? {
            options.report(&format!(
                "Warning: an interrupted kernel setup was moved aside and retained at {}; remove it when no longer needed",
                aside.display()
            ));
        }
        match std::fs::symlink_metadata(venv) {
            Ok(_) => return Err(exact_venv_stale_error(venv)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(anyhow!(
                    "cannot inspect PRIME_AGENT_KERNEL_VENV at {} without risking another owner's files: {error}",
                    venv.display()
                ));
            }
        }
        options.report("› setting up python kernel (one-time, ~30s)…");
        // Atomically reserve the exact missing path before uv. It remains at
        // that path (never renamed while building), so Windows entry points
        // and pyvenv.cfg embed the caller's exact directory.
        std::fs::create_dir(venv).map_err(|error| {
            anyhow!(
                "could not atomically reserve PRIME_AGENT_KERNEL_VENV at {}: {error}",
                venv.display()
            )
        })?;
        crate::platform::perms::restrict_dir(venv)?;
        // Dropped unpublished (failure, or the runtime cancelling a prewarm),
        // the claim moves its own partial directory aside, retained.
        let build = ExplicitBuild::claim(venv)?;
        let bootstrap = bootstrap_venv(
            venv,
            source_dir,
            runtime_identity,
            python_skills,
            options,
            None,
        )
        .await;
        if !build.owns_path() {
            return Err(anyhow!(
                "PRIME_AGENT_KERNEL_VENV ownership changed during bootstrap; the path was retained without publication: {}",
                venv.display()
            ));
        }
        bootstrap?;
        validate_built_environment(&python, venv, runtime_identity, python_skills)?;
        build.publish();
        options.report("✓ ready");
        Ok(python)
    }
    .await;
    drop(release_lock);
    result
}

/// The default managed path first honors a fully current legacy environment.
/// Every replacement is an immutable, leased generation; this function never
/// mutates or deletes the legacy directory.
async fn ensure_managed_kernel_venv(
    options: &EnsureKernelPythonOptions,
    python_skills: &[BootstrapPythonSkill],
    base_venv: &std::path::Path,
    source_dir: &std::path::Path,
    runtime_identity: &str,
) -> anyhow::Result<PathBuf> {
    let legacy_python = kernel_venv_python(base_venv);
    let legacy_python_str = legacy_python.to_string_lossy().to_string();
    if kernel_ready_read_only(
        &legacy_python_str,
        base_venv,
        runtime_identity,
        python_skills,
    ) {
        report_recorded_unavailable_skills(options, base_venv);
        return Ok(legacy_python);
    }

    let root = generation_root(base_venv)?;
    let identity = generation_identity(runtime_identity, python_skills)?;
    let release_lock = acquire_bootstrap_lock(&root).await.map_err(|error| {
        anyhow!(
            "cannot publish a managed Python kernel generation under {} without risking the legacy environment at {}; set PRIME_AGENT_KERNEL_VENV to a writable missing path or set PRIME_AGENT_KERNEL_PYTHON: {error:#}",
            root.display(),
            base_venv.display()
        )
    })?;
    let result = async {
        prepare_generation_root(&root)?;
        for generation in published_candidates(&root, &identity)? {
            let python = kernel_venv_python(&generation);
            let python_str = python.to_string_lossy().to_string();
            if kernel_ready(&python_str, &generation, runtime_identity, python_skills) {
                report_recorded_unavailable_skills(options, &generation);
                // Durable before the root lock releases: pruning can never
                // interleave between selection and resolver ownership.
                lease_for_resolver(&generation)?;
                prepare_generation_root(&root)?;
                return Ok(python);
            }
        }

        options.report("› setting up python kernel (one-time, ~30s)…");
        let build = GenerationBuild::begin(&root, &identity)?;
        let bootstrap = bootstrap_venv(
            build.path(),
            source_dir,
            runtime_identity,
            python_skills,
            options,
            Some(&build),
        )
        .await;
        if let Err(error) = bootstrap {
            build.discard();
            return Err(error);
        }
        let python = kernel_venv_python(build.path());
        if let Err(error) =
            validate_built_environment(&python, build.path(), runtime_identity, python_skills)
        {
            build.discard();
            return Err(error);
        }
        if let Err(error) = build.publish() {
            build.discard();
            return Err(error);
        }
        prepare_generation_root(&root)?;
        options.report("✓ ready");
        Ok(python)
    }
    .await;
    drop(release_lock);
    result
}

fn validate_built_environment(
    python: &std::path::Path,
    venv: &std::path::Path,
    runtime_identity: &str,
    python_skills: &[BootstrapPythonSkill],
) -> anyhow::Result<()> {
    let python_str = python.to_string_lossy().to_string();
    if !kernel_runtime_ready(&python_str, venv, runtime_identity, python_skills) {
        return Err(anyhow!(
            "the new kernel environment failed its runtime/version validation"
        ));
    }
    let missing_packages = missing_rlm_extra_import_labels(&python_str);
    if !missing_packages.is_empty() {
        return Err(anyhow!(
            "the new kernel environment is missing default Python packages: {}",
            missing_packages.join(", ")
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn lock_timeout_prevents_venv_rebuild() {
        let root = tempfile::tempdir().unwrap();
        let venv = root.path().join("kernel-venv");
        std::fs::create_dir(&venv).unwrap();
        let sentinel = venv.join("owner-state");
        std::fs::write(&sentinel, "another bootstrap owns this venv").unwrap();
        let generation_root = generation_root(&venv).unwrap();
        let owner = acquire_bootstrap_lock(&generation_root).await.unwrap();
        let options = EnsureKernelPythonOptions {
            // Fail before any destructive operation or network access if the
            // caller regresses and enters setup without acquiring the lock.
            on_progress: Some(Arc::new(|message| {
                panic!("bootstrap must stop at the lock error, got progress: {message}");
            })),
            ..Default::default()
        };

        let error = tokio::time::timeout(
            std::time::Duration::from_secs(121),
            ensure_kernel_venv(&options, &[], venv),
        )
        .await
        .expect("lock acquisition must have a bounded wait")
        .expect_err("a lock timeout must propagate to the caller");

        assert!(error.to_string().contains("Timed out"), "{error}");
        assert_eq!(
            std::fs::read_to_string(sentinel).unwrap(),
            "another bootstrap owns this venv"
        );
        assert!(root
            .path()
            .join("kernel-venv.generations.bootstrap.lock")
            .exists());
        drop(owner);
    }
}
