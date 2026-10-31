//! The venv dir-layout concern: the override-aware
//! kernel venv dir, the writable-dir fallback, and the interpreter path.

use super::{anyhow, Path, PathBuf};

pub(crate) fn expand_home(path: &str) -> PathBuf {
    let expanded = if path == "~" {
        home_dir()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        PathBuf::from(path)
    };
    if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(expanded)
    }
}

pub(super) fn home_dir() -> PathBuf {
    pa_types::platform::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

/// Directory of the kernel venv, honoring `PRIME_AGENT_KERNEL_VENV`.
#[must_use]
pub fn kernel_venv_dir() -> PathBuf {
    if let Ok(override_dir) = std::env::var("PRIME_AGENT_KERNEL_VENV") {
        if !override_dir.is_empty() {
            return expand_home(&override_dir);
        }
    }
    home_dir().join(".prime").join("agent").join("kernel-venv")
}

fn xdg_kernel_venv_dir() -> PathBuf {
    let data_home = match std::env::var("XDG_DATA_HOME") {
        Ok(value) if !value.is_empty() => expand_home(&value),
        _ => home_dir().join(".local").join("share"),
    };
    data_home.join("prime").join("agent").join("kernel-venv")
}

pub(crate) fn resolve_writable_kernel_venv_dir() -> anyhow::Result<PathBuf> {
    let primary = kernel_venv_dir();
    // An explicit path is exact: its parents are created only after every
    // refusal check passes (`create_explicit_parent`), never here.
    if std::env::var("PRIME_AGENT_KERNEL_VENV").is_ok_and(|v| !v.is_empty()) {
        return Ok(primary);
    }
    if std::fs::create_dir_all(primary.parent().unwrap_or(Path::new("/"))).is_ok() {
        return Ok(primary);
    }
    let fallback = xdg_kernel_venv_dir();
    if std::fs::create_dir_all(fallback.parent().unwrap_or(Path::new("/"))).is_err() {
        return Err(anyhow!(
            "couldn't create kernel venv directory at {} or {}; set PRIME_AGENT_KERNEL_PYTHON to a python with a current prime-agent-runtime installed",
            primary.display(),
            fallback.display()
        ));
    }
    Ok(fallback)
}

/// Create the missing parents of an explicit kernel path. Callers invoke this
/// only once the request will build a new environment.
pub(crate) fn create_explicit_parent(venv: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(venv.parent().unwrap_or(Path::new("/"))).map_err(|error| {
        anyhow!(
            "couldn't create kernel venv parent directories for {}: {error}",
            venv.display()
        )
    })
}

/// Path of the venv's python interpreter.
#[must_use]
pub fn kernel_venv_python(venv: &Path) -> PathBuf {
    if cfg!(windows) {
        venv.join("Scripts").join("python.exe")
    } else {
        venv.join("bin").join("python")
    }
}
