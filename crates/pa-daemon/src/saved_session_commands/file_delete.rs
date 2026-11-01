//! Saved transcript removal. Never convert an installed Trash failure into an unlink.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

use super::remove_session_artifacts;

/// Resolve the installed tool before execution. ENOENT from exec alone is not
/// proof of absence: an installed script can have a missing interpreter.
fn trash_program() -> std::io::Result<Option<PathBuf>> {
    let search = std::env::var_os("PATH").ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "PATH is unset; cannot establish whether trash is installed",
        )
    })?;
    for directory in std::env::split_paths(&search) {
        let candidate = directory.join(if cfg!(windows) { "trash.exe" } else { "trash" });
        match std::fs::symlink_metadata(&candidate) {
            Ok(_) => return std::path::absolute(candidate).map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

/// The hook runs only after verified removal and before artifact cleanup, so
/// its schedule store remains available. An absent transcript is an honest
/// no-op failure, consistent with the existing unlink-only platform contract.
pub(crate) fn delete_session_file_after_file_removed(
    path: &Path,
    after_file_removed: &dyn Fn(&Path),
) -> Value {
    let program = match trash_program() {
        Ok(program) => program,
        Err(error) => {
            return json!({"ok": false, "error": format!("Could not locate trash: {error}; no files were removed")})
        }
    };
    delete_with_program(path, after_file_removed, program.as_deref())
}

fn delete_with_program(
    path: &Path,
    after_file_removed: &dyn Fn(&Path),
    program: Option<&Path>,
) -> Value {
    // Do not canonicalize: a symlink is the entry to remove, not its target.
    // Absolute operands cannot begin with '-' and need no '--', which Apple's
    // trash(8) argument grammar does not document.
    let absolute = match std::path::absolute(path) {
        Ok(path) => path,
        Err(error) => return json!({"ok": false, "error": error.to_string()}),
    };
    match std::fs::symlink_metadata(&absolute) {
        Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return json!({"ok": false, "error": "Session path is not a file or symlink; nothing was removed"})
        }
        Err(error) => {
            return json!({"ok": false, "error": format!("Session was not removed: {error}")})
        }
    }
    let method = if let Some(program) = program {
        let output = match Command::new(program).arg(&absolute).output() {
            Ok(output) => output,
            Err(error) => {
                return json!({"ok": false, "error": format!("Could not run installed trash: {error}; permanent deletion was not attempted")})
            }
        };
        if !output.status.success() {
            let hint: String = String::from_utf8_lossy(&output.stderr)
                .trim()
                .chars()
                .take(200)
                .collect();
            return json!({"ok": false, "error": format!("trash failed ({}): {hint}; permanent deletion was not attempted", output.status)});
        }
        "trash"
    } else {
        // Existing documented fallback for platforms without a Trash CLI. The
        // wire method explicitly says unlink, never claims recoverable trash.
        if let Err(error) = std::fs::remove_file(&absolute) {
            return json!({"ok": false, "error": format!("Permanent deletion failed: {error}")});
        }
        "unlink"
    };
    match std::fs::symlink_metadata(&absolute) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            after_file_removed(path);
            remove_session_artifacts(path);
            json!({"ok": true, "method": method})
        }
        Ok(_) => {
            json!({"ok": false, "error": "Removal did not remove the session entry; artifacts were retained"})
        }
        Err(error) => {
            json!({"ok": false, "error": format!("Could not verify session removal: {error}; artifacts were retained")})
        }
    }
}

#[cfg(test)]
mod tests;
