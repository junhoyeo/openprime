//! The version-file concern: the schema consts, the `.bootstrap-version` read/write.

use super::{
    default_rlm_extra_uv_args, BootstrapPythonSkill, BootstrapUnavailablePythonSkill,
    BootstrapVersion, Path,
};

/// Schema of `.bootstrap-version`; a mismatch rebuilds the venv.
pub(super) const BOOTSTRAP_SCHEMA: u64 = 9;
pub(super) const STATE_SNAPSHOT_REQUIREMENT: &str = "dill";
const BOOTSTRAP_VERSION_FILE: &str = ".bootstrap-version";
pub(super) fn read_bootstrap_version(venv: &Path) -> Option<BootstrapVersion> {
    let raw = std::fs::read_to_string(venv.join(BOOTSTRAP_VERSION_FILE)).ok()?;
    let parsed: BootstrapVersion = serde_json::from_str(&raw).ok()?;
    (parsed.schema > 0).then_some(parsed)
}

fn extra_uv_args_match(a: Option<&[String]>, b: &[&str]) -> bool {
    match a {
        None => false,
        Some(a) => a.iter().map(String::as_str).eq(b.iter().copied()),
    }
}

/// Identity of one recorded skill: the install root is the package path, and
/// the editable install follows that path.
pub(super) fn bootstrap_skill_key(skill: &BootstrapPythonSkill) -> String {
    format!("{}\u{0}{}", skill.import_name, skill.package_path)
}

/// True when the recorded installs cover every current skill at the same path with the same
/// pyproject hash. Extra recorded skills from other sessions are fine: the venv is a shared cache.
pub(super) fn recorded_skills_cover(
    recorded: Option<&[BootstrapPythonSkill]>,
    current: &[BootstrapPythonSkill],
) -> bool {
    if current.is_empty() {
        return true;
    }
    let Some(recorded) = recorded else {
        return false;
    };
    current.iter().all(|skill| {
        recorded.iter().any(|entry| {
            bootstrap_skill_key(entry) == bootstrap_skill_key(skill)
                && entry.pyproject_path == skill.pyproject_path
                && entry.pyproject_hash == skill.pyproject_hash
        })
    })
}

fn requested_skills_match(
    recorded: &[BootstrapPythonSkill],
    current: &[BootstrapPythonSkill],
) -> bool {
    recorded.len() == current.len()
        && current.iter().all(|skill| {
            recorded.iter().any(|entry| {
                bootstrap_skill_key(entry) == bootstrap_skill_key(skill)
                    && entry.pyproject_path == skill.pyproject_path
                    && entry.pyproject_hash == skill.pyproject_hash
            })
        })
}

fn requested_outcome_matches(
    version: &BootstrapVersion,
    requested: &[BootstrapPythonSkill],
) -> bool {
    let installed = version.python_skills.as_deref().unwrap_or_default();
    let unavailable = version
        .unavailable_python_skills
        .as_deref()
        .unwrap_or_default();
    if installed.len() + unavailable.len() != requested.len() {
        return false;
    }
    requested.iter().all(|skill| {
        let installed_matches = installed
            .iter()
            .filter(|entry| {
                bootstrap_skill_key(entry) == bootstrap_skill_key(skill)
                    && entry.pyproject_path == skill.pyproject_path
                    && entry.pyproject_hash == skill.pyproject_hash
            })
            .count();
        let unavailable_matches = unavailable
            .iter()
            .filter(|entry| {
                entry.import_name == skill.import_name
                    && entry.package_path == skill.package_path
                    && entry.pyproject_path == skill.pyproject_path
                    && entry.pyproject_hash == skill.pyproject_hash
                    && !entry.reason.is_empty()
            })
            .count();
        installed_matches + unavailable_matches == 1
    })
}

pub(super) fn bootstrap_base_version_current(
    version: Option<BootstrapVersion>,
    runtime_identity: &str,
) -> bool {
    match version {
        Some(version) => {
            version.schema == BOOTSTRAP_SCHEMA
                && version.runtime.as_deref() == Some(runtime_identity)
                && version.snapshot.as_deref() == Some(STATE_SNAPSHOT_REQUIREMENT)
                && extra_uv_args_match(
                    version.extra_uv_args.as_deref(),
                    &default_rlm_extra_uv_args(),
                )
        }
        None => false,
    }
}

pub(super) fn bootstrap_version_current(
    version: Option<&BootstrapVersion>,
    runtime_identity: &str,
    python_skills: &[BootstrapPythonSkill],
) -> bool {
    version.is_some_and(|version| {
        bootstrap_base_version_current(Some(version.clone()), runtime_identity)
            && version.requested_python_skills.as_deref().map_or_else(
                || recorded_skills_cover(version.python_skills.as_deref(), python_skills),
                |requested| {
                    requested_skills_match(requested, python_skills)
                        && requested_outcome_matches(version, requested)
                },
            )
    })
}

#[cfg(test)]
pub(crate) fn write_bootstrap_version(
    venv: &Path,
    runtime_identity: &str,
    python_skills: &[BootstrapPythonSkill],
) -> anyhow::Result<()> {
    write_bootstrap_version_record(venv, runtime_identity, python_skills, None, None)
}

pub(super) fn write_bootstrap_version_with_outcome(
    venv: &Path,
    runtime_identity: &str,
    installed_python_skills: &[BootstrapPythonSkill],
    requested_python_skills: &[BootstrapPythonSkill],
    unavailable_python_skills: &[BootstrapUnavailablePythonSkill],
) -> anyhow::Result<()> {
    write_bootstrap_version_record(
        venv,
        runtime_identity,
        installed_python_skills,
        Some(requested_python_skills),
        Some(unavailable_python_skills),
    )
}

fn write_bootstrap_version_record(
    venv: &Path,
    runtime_identity: &str,
    installed_python_skills: &[BootstrapPythonSkill],
    requested_python_skills: Option<&[BootstrapPythonSkill]>,
    unavailable_python_skills: Option<&[BootstrapUnavailablePythonSkill]>,
) -> anyhow::Result<()> {
    let version = BootstrapVersion {
        schema: BOOTSTRAP_SCHEMA,
        runtime: Some(runtime_identity.to_string()),
        snapshot: Some(STATE_SNAPSHOT_REQUIREMENT.to_string()),
        extra_uv_args: Some(
            default_rlm_extra_uv_args()
                .into_iter()
                .map(String::from)
                .collect(),
        ),
        python_skills: Some(installed_python_skills.to_vec()),
        requested_python_skills: requested_python_skills.map(<[_]>::to_vec),
        unavailable_python_skills: unavailable_python_skills.map(<[_]>::to_vec),
    };
    std::fs::write(
        venv.join(BOOTSTRAP_VERSION_FILE),
        format!("{}\n", serde_json::to_string(&version)?),
    )?;
    Ok(())
}

/// The parsed `.bootstrap-version` plus its raw text (the probe-memo key
/// input), in one read.
pub(super) fn read_bootstrap_version_raw(venv: &Path) -> (Option<BootstrapVersion>, String) {
    let raw = std::fs::read_to_string(venv.join(BOOTSTRAP_VERSION_FILE)).unwrap_or_default();
    let parsed: Option<BootstrapVersion> = serde_json::from_str(&raw).ok();
    (parsed.filter(|v| v.schema > 0), raw)
}
