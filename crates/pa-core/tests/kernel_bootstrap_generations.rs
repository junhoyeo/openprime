//! Verifiers for the managed Python-kernel environment publication contract.

#![cfg(unix)]

use std::path::{Path, PathBuf};

use pa_core::kernel::bootstrap::{
    ensure_kernel_python, invalidate_runtime_probe_cache, EnsureKernelPythonOptions,
    KernelPythonSkill,
};
use pa_core::kernel::manager::{KernelStartOptions, ReplKernelManager};
use pa_core::kernel::shared::KernelManagerOptions;

static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct EnvRestore(Vec<(String, Option<String>)>);

impl EnvRestore {
    fn configure(values: &[(&str, Option<String>)]) -> Self {
        let previous = values
            .iter()
            .map(|(name, _)| ((*name).to_string(), std::env::var(name).ok()))
            .collect();
        for (name, value) in values {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
        Self(previous)
    }
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        for (name, value) in self.0.drain(..) {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

fn write_executable(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text).unwrap();
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

fn runtime_fixture(root: &Path) -> PathBuf {
    let runtime = root.join("package/prime-agent-runtime");
    std::fs::create_dir_all(runtime.join("src/rlm")).unwrap();
    std::fs::write(
        runtime.join("pyproject.toml"),
        "[project]\nname = 'prime-agent-runtime'\nversion = '0.1.0'\n",
    )
    .unwrap();
    std::fs::write(runtime.join("src/rlm/__init__.py"), "").unwrap();
    std::fs::write(runtime.join("src/rlm/repl.py"), "").unwrap();
    root.join("package")
}

fn fake_uv(root: &Path) -> (PathBuf, PathBuf) {
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = root.join("uv.log");
    // /bin/sh may materialize heredocs outside TMPDIR on macOS. Keep
    // every fixture byte inside the owned root instead.
    write_executable(
        &bin.join("python-fixture"),
        r#"#!/bin/sh
if [ "${UV_FAIL_SKILL:-}" = "1" ] && [ "${2:-}" = "import broken_skill" ]; then exit 1; fi
if [ "${FAKE_MISSING_DEFAULT:-}" = "1" ]; then
  case "${2:-}" in *pandas*) exit 1 ;; esac
fi
exit 0
"#,
    );
    write_executable(
        &bin.join("uv"),
        r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$UV_LOG"
if [ "$1" = "python" ]; then exit 0; fi
if [ "$1" = "venv" ]; then
  target="$2"
  [ -d "$target" ]
  [ -z "$(ls -A "$target")" ]
  if [ "${UV_SWAP_TARGET:-}" = "1" ]; then
    mv "$target" "$target.owned-before-swap"
    mkdir "$target"
    printf foreign > "$target/foreign-owner-state"
  fi
  if [ "${UV_FAIL_VENV:-}" = "1" ]; then
    printf 'Authorization: Bearer synthetic-failure-secret\n' >&2
    exit 17
  fi
  if [ -n "${UV_OWNER_DIR:-}" ] && mkdir "$UV_OWNER_DIR" 2>/dev/null; then
    if [ "${UV_PARTIAL:-}" = "1" ]; then
      mkdir -p "$target/lib"
      printf partial > "$target/lib/partial-install"
    fi
    printf x > "$UV_STARTED_FIFO"
    cat "$UV_RELEASE_FIFO" >/dev/null
  fi
  mkdir -p "$target/bin"
  cp "$(dirname "$0")/python-fixture" "$target/bin/python"
  exit 0
fi
if [ "$1" = "pip" ]; then
  if [ "${UV_FAIL_SKILL:-}" = "1" ]; then
    case " $* " in *" --editable "*) exit 19 ;; esac
  fi
  exit 0
fi
exit 2
"#,
    );
    (bin, log)
}

#[tokio::test]
async fn stale_explicit_kernel_venv_is_never_replaced_or_deleted() {
    let _serial = ENV_LOCK.lock().await;
    invalidate_runtime_probe_cache();
    let root = tempfile::tempdir().unwrap();
    let package = runtime_fixture(root.path());
    let (bin, log) = fake_uv(root.path());
    let exact = root.path().join("explicit-kernel-venv");
    std::fs::create_dir_all(&exact).unwrap();
    let sentinel = exact.join("active-owner-state");
    std::fs::write(&sentinel, "must survive").unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let _env = EnvRestore::configure(&[
        ("PATH", Some(path)),
        (
            "PI_PACKAGE_DIR",
            Some(package.to_string_lossy().into_owned()),
        ),
        (
            "PRIME_AGENT_KERNEL_VENV",
            Some(exact.to_string_lossy().into_owned()),
        ),
        ("PRIME_AGENT_KERNEL_PYTHON", None),
        ("UV_LOG", Some(log.to_string_lossy().into_owned())),
        ("UV_FAIL_VENV", None),
    ]);

    let error = ensure_kernel_python(EnsureKernelPythonOptions::default())
        .await
        .expect_err("a stale exact override must fail closed");
    assert!(
        error.to_string().contains("PRIME_AGENT_KERNEL_VENV"),
        "{error:#}"
    );
    assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "must survive");
    assert!(
        !log.exists() || std::fs::read_to_string(log).unwrap().is_empty(),
        "a stale explicit path must not start uv"
    );
}

fn bootstrap_env(root: &Path, bin: &Path, package: &Path, log: &Path) -> EnvRestore {
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    EnvRestore::configure(&[
        ("PATH", Some(path)),
        (
            "PI_PACKAGE_DIR",
            Some(package.to_string_lossy().into_owned()),
        ),
        ("PRIME_AGENT_KERNEL_VENV", None),
        ("PRIME_AGENT_KERNEL_PYTHON", None),
        (
            "HOME",
            Some(root.join("home").to_string_lossy().into_owned()),
        ),
        ("XDG_DATA_HOME", None),
        ("UV_LOG", Some(log.to_string_lossy().into_owned())),
        ("UV_FAIL_VENV", None),
        ("UV_SWAP_TARGET", None),
        ("UV_FAIL_SKILL", None),
        ("FAKE_MISSING_DEFAULT", None),
        ("UV_OWNER_DIR", None),
        ("UV_STARTED_FIFO", None),
        ("UV_RELEASE_FIFO", None),
        ("UV_PARTIAL", None),
    ])
}

#[tokio::test]
async fn missing_explicit_kernel_venv_builds_at_the_exact_path() {
    let _serial = ENV_LOCK.lock().await;
    invalidate_runtime_probe_cache();
    let root = tempfile::tempdir().unwrap();
    let package = runtime_fixture(root.path());
    let (bin, log) = fake_uv(root.path());
    let exact = root.path().join("explicit-kernel-venv");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let _env = EnvRestore::configure(&[
        ("PATH", Some(path)),
        (
            "PI_PACKAGE_DIR",
            Some(package.to_string_lossy().into_owned()),
        ),
        (
            "PRIME_AGENT_KERNEL_VENV",
            Some(exact.to_string_lossy().into_owned()),
        ),
        ("PRIME_AGENT_KERNEL_PYTHON", None),
        ("UV_LOG", Some(log.to_string_lossy().into_owned())),
        ("UV_FAIL_VENV", None),
    ]);

    let python = ensure_kernel_python(EnsureKernelPythonOptions::default())
        .await
        .unwrap();
    assert_eq!(python, exact.join("bin/python"));
    assert!(python.is_file());
    assert!(!root
        .path()
        .join("explicit-kernel-venv.generations")
        .exists());
    let invocations = std::fs::read_to_string(log).unwrap();
    assert!(invocations.contains(&format!("venv {} --python 3.11 --seed", exact.display())));
    assert!(!invocations.contains("--clear"));
    assert!(!invocations.contains("--force"));
}

#[tokio::test]
async fn active_generation_survives_replacement_and_a_failed_later_build() {
    let _serial = ENV_LOCK.lock().await;
    invalidate_runtime_probe_cache();
    let root = tempfile::tempdir().unwrap();
    let package = runtime_fixture(root.path());
    let (bin, log) = fake_uv(root.path());
    let _env = bootstrap_env(root.path(), &bin, &package, &log);

    let first = ensure_kernel_python(EnsureKernelPythonOptions::default())
        .await
        .unwrap();
    let first_venv = first.parent().unwrap().parent().unwrap().to_path_buf();
    let first_sentinel = first_venv.join("active-owner-state");
    std::fs::write(&first_sentinel, "first stays").unwrap();

    let repl = package.join("prime-agent-runtime/src/rlm/repl.py");
    std::fs::write(&repl, "RUNTIME = 2\n").unwrap();
    let second = ensure_kernel_python(EnsureKernelPythonOptions::default())
        .await
        .unwrap();
    let second_venv = second.parent().unwrap().parent().unwrap().to_path_buf();
    assert_ne!(first_venv, second_venv);
    assert_eq!(
        std::fs::read_to_string(&first_sentinel).unwrap(),
        "first stays"
    );
    let second_sentinel = second_venv.join("second-owner-state");
    std::fs::write(&second_sentinel, "second stays").unwrap();

    std::fs::write(&repl, "RUNTIME = 3\n").unwrap();
    std::env::set_var("UV_FAIL_VENV", "1");
    let error = ensure_kernel_python(EnsureKernelPythonOptions::default())
        .await
        .expect_err("the third private build is forced to fail");
    std::env::remove_var("UV_FAIL_VENV");
    assert!(
        !error.to_string().contains("synthetic-failure-secret"),
        "{error:#}"
    );
    assert_eq!(
        std::fs::read_to_string(first_sentinel).unwrap(),
        "first stays"
    );
    assert_eq!(
        std::fs::read_to_string(second_sentinel).unwrap(),
        "second stays"
    );

    let generation_root = root
        .path()
        .join("home/.prime/agent/kernel-venv.generations");
    let published = std::fs::read_dir(&generation_root)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join(".generation-published").is_file())
        .count();
    assert_eq!(published, 2, "failed builds never publish");
}

#[tokio::test]
async fn concurrent_callers_converge_on_one_published_generation() {
    let _serial = ENV_LOCK.lock().await;
    invalidate_runtime_probe_cache();
    let root = tempfile::tempdir().unwrap();
    let package = runtime_fixture(root.path());
    let (bin, log) = fake_uv(root.path());
    let _env = bootstrap_env(root.path(), &bin, &package, &log);

    let (first, second) = tokio::join!(
        ensure_kernel_python(EnsureKernelPythonOptions::default()),
        ensure_kernel_python(EnsureKernelPythonOptions::default())
    );
    let first = first.unwrap();
    let second = second.unwrap();
    assert_eq!(first, second);
    let invocations = std::fs::read_to_string(log).unwrap();
    assert_eq!(
        invocations
            .lines()
            .filter(|line| line.starts_with("venv "))
            .count(),
        1
    );
    let generation_root = root
        .path()
        .join("home/.prime/agent/kernel-venv.generations");
    assert_eq!(
        std::fs::read_dir(generation_root)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().join(".generation-published").is_file())
            .count(),
        1
    );
}

#[tokio::test]
async fn current_explicit_symlink_is_reused_read_only_but_stale_symlink_is_refused() {
    use std::os::unix::fs::symlink;

    let _serial = ENV_LOCK.lock().await;
    invalidate_runtime_probe_cache();
    let root = tempfile::tempdir().unwrap();
    let package = runtime_fixture(root.path());
    let (bin, log) = fake_uv(root.path());
    let exact = root.path().join("real-explicit-venv");
    let alias = root.path().join("explicit-alias");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let _env = EnvRestore::configure(&[
        ("PATH", Some(path)),
        (
            "PI_PACKAGE_DIR",
            Some(package.to_string_lossy().into_owned()),
        ),
        (
            "PRIME_AGENT_KERNEL_VENV",
            Some(exact.to_string_lossy().into_owned()),
        ),
        ("PRIME_AGENT_KERNEL_PYTHON", None),
        ("UV_LOG", Some(log.to_string_lossy().into_owned())),
        ("UV_FAIL_VENV", None),
    ]);
    ensure_kernel_python(EnsureKernelPythonOptions::default())
        .await
        .unwrap();
    let sentinel = exact.join("owner-state");
    std::fs::write(&sentinel, "untouched").unwrap();
    let probe_memo = exact.join(".runtime-probe-memo.json");
    let _ = std::fs::remove_file(&probe_memo);
    invalidate_runtime_probe_cache();
    symlink(&exact, &alias).unwrap();
    std::env::set_var("PRIME_AGENT_KERNEL_VENV", &alias);
    let reused = ensure_kernel_python(EnsureKernelPythonOptions::default())
        .await
        .unwrap();
    assert_eq!(reused, alias.join("bin/python"));
    assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "untouched");
    assert!(
        !probe_memo.exists(),
        "reusing an explicit environment is read-only"
    );

    std::fs::write(
        package.join("prime-agent-runtime/src/rlm/repl.py"),
        "RUNTIME = 'changed'\n",
    )
    .unwrap();
    let error = ensure_kernel_python(EnsureKernelPythonOptions::default())
        .await
        .expect_err("a stale symlinked explicit environment is not rebuilt");
    assert!(error.to_string().contains("will not replace"), "{error:#}");
    assert!(alias.is_symlink());
    assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "untouched");
}

#[test]
fn bootstrap_subprocess_entry() {
    let Ok(result_path) = std::env::var("PA_KERNEL_BOOTSTRAP_CHILD_RESULT") else {
        return;
    };
    invalidate_runtime_probe_cache();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let python = runtime
        .block_on(ensure_kernel_python(EnsureKernelPythonOptions::default()))
        .unwrap();
    std::fs::write(result_path, python.to_string_lossy().as_bytes()).unwrap();
}

fn assert_child_success(label: &str, output: &std::process::Output) {
    assert!(
        output.status.success(),
        "{label} child failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn same_identity_cross_process_callers_publish_once() {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::Duration;

    use nix::sys::stat::Mode;
    use nix::unistd::mkfifo;

    let root = tempfile::tempdir().unwrap();
    let package = runtime_fixture(root.path());
    let (bin, log) = fake_uv(root.path());
    let home = root.path().join("home");
    let owner_dir = root.path().join("uv-owner");
    let started_fifo = root.path().join("uv-started.fifo");
    let release_fifo = root.path().join("uv-release.fifo");
    mkfifo(&started_fifo, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
    mkfifo(&release_fifo, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
    let first_result = root.path().join("first-result");
    let second_result = root.path().join("second-result");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let executable = std::env::current_exe().unwrap();

    let spawn_child = |result: &Path| {
        let mut command = Command::new(&executable);
        command
            .args(["--exact", "bootstrap_subprocess_entry", "--nocapture"])
            .env("PATH", &path)
            .env("PI_PACKAGE_DIR", &package)
            .env("HOME", &home)
            .env("UV_LOG", &log)
            .env("UV_OWNER_DIR", &owner_dir)
            .env("UV_STARTED_FIFO", &started_fifo)
            .env("UV_RELEASE_FIFO", &release_fifo)
            .env("PA_KERNEL_BOOTSTRAP_CHILD_RESULT", result)
            .env_remove("PRIME_AGENT_KERNEL_VENV")
            .env_remove("PRIME_AGENT_KERNEL_PYTHON")
            .env_remove("XDG_DATA_HOME")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.spawn().unwrap()
    };

    let (started_tx, started_rx) = mpsc::channel();
    let started_path = started_fifo.clone();
    let reader = std::thread::spawn(move || {
        let mut started = std::fs::File::open(started_path).unwrap();
        let mut byte = [0u8; 1];
        started.read_exact(&mut byte).unwrap();
        started_tx.send(()).unwrap();
    });
    let first = spawn_child(&first_result);
    let second = spawn_child(&second_result);
    started_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("one builder reached the uv publication rendezvous");
    let mut release = std::fs::OpenOptions::new()
        .write(true)
        .open(&release_fifo)
        .unwrap();
    release.write_all(b"x").unwrap();
    drop(release);
    reader.join().unwrap();

    let first = first.wait_with_output().unwrap();
    let second = second.wait_with_output().unwrap();
    assert_child_success("first", &first);
    assert_child_success("second", &second);
    assert_eq!(
        std::fs::read_to_string(first_result).unwrap(),
        std::fs::read_to_string(second_result).unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .filter(|line| line.starts_with("venv "))
            .count(),
        1,
        "the root lock and marker converge cross-process callers"
    );
    let generation_root = home.join(".prime/agent/kernel-venv.generations");
    assert_eq!(
        std::fs::read_dir(generation_root)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().join(".generation-published").is_file())
            .count(),
        1
    );
}

fn build_record_path(exact: &Path) -> PathBuf {
    exact.with_file_name(format!(
        "{}.prime-agent-build.json",
        exact.file_name().unwrap().to_string_lossy()
    ))
}

fn abandoned_siblings(exact: &Path) -> Vec<PathBuf> {
    let prefix = format!(
        "{}.abandoned-",
        exact.file_name().unwrap().to_string_lossy()
    );
    std::fs::read_dir(exact.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
        .map(|entry| entry.path())
        .collect()
}

/// The paused fake uv's `cat` may outlive its killed shell: release it when
/// a reader is still there, never block when none is.
fn release_paused_uv(release_fifo: &Path) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    if let Ok(mut release) = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits())
        .open(release_fifo)
    {
        let _ = release.write_all(b"x");
    }
}

/// Blocks until the fake uv reaches its rendezvous inside `uv venv`.
fn wait_for_paused_uv(started_fifo: &Path) {
    use std::io::Read;

    let mut started = std::fs::File::open(started_fifo).unwrap();
    let mut byte = [0u8; 1];
    started.read_exact(&mut byte).unwrap();
}

struct PauseFifos {
    owner_dir: PathBuf,
    started: PathBuf,
    release: PathBuf,
}

fn pause_fifos(root: &Path) -> PauseFifos {
    use nix::sys::stat::Mode;
    use nix::unistd::mkfifo;

    let fifos = PauseFifos {
        owner_dir: root.join("uv-owner"),
        started: root.join("uv-started.fifo"),
        release: root.join("uv-release.fifo"),
    };
    mkfifo(&fifos.started, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
    mkfifo(&fifos.release, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
    fifos
}

fn explicit_env(
    bin: &Path,
    package: &Path,
    log: &Path,
    exact: &Path,
    pause: Option<&PauseFifos>,
) -> EnvRestore {
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let lossy = |path: &Path| Some(path.to_string_lossy().into_owned());
    EnvRestore::configure(&[
        ("PATH", Some(path)),
        ("PI_PACKAGE_DIR", lossy(package)),
        ("PRIME_AGENT_KERNEL_VENV", lossy(exact)),
        ("PRIME_AGENT_KERNEL_PYTHON", None),
        ("UV_LOG", lossy(log)),
        ("UV_FAIL_VENV", None),
        ("UV_SWAP_TARGET", None),
        ("UV_FAIL_SKILL", None),
        ("FAKE_MISSING_DEFAULT", None),
        ("UV_OWNER_DIR", pause.and_then(|p| lossy(&p.owner_dir))),
        ("UV_STARTED_FIFO", pause.and_then(|p| lossy(&p.started))),
        ("UV_RELEASE_FIFO", pause.and_then(|p| lossy(&p.release))),
        ("UV_PARTIAL", pause.map(|_| "1".to_string())),
    ])
}

/// A short print session drops its prewarm mid-build (the runtime shuts
/// down). The attempt moves its own partial directory aside, retained, so
/// the next session builds the exact path again.
#[tokio::test]
async fn explicit_build_dropped_mid_bootstrap_is_set_aside_and_rebuilt() {
    let _serial = ENV_LOCK.lock().await;
    invalidate_runtime_probe_cache();
    let root = tempfile::tempdir().unwrap();
    let package = runtime_fixture(root.path());
    let (bin, log) = fake_uv(root.path());
    let exact = root.path().join("explicit-kernel-venv");
    let pause = pause_fifos(root.path());
    let _env = explicit_env(&bin, &package, &log, &exact, Some(&pause));

    let attempt = tokio::spawn(ensure_kernel_python(EnsureKernelPythonOptions::default()));
    let started = pause.started.clone();
    tokio::task::spawn_blocking(move || wait_for_paused_uv(&started))
        .await
        .unwrap();
    attempt.abort();
    assert!(attempt.await.unwrap_err().is_cancelled());
    release_paused_uv(&pause.release);

    assert!(
        std::fs::symlink_metadata(&exact).is_err(),
        "the dropped attempt left its partial build at the exact path"
    );
    let abandoned = abandoned_siblings(&exact);
    assert_eq!(abandoned.len(), 1, "{abandoned:?}");
    assert_eq!(
        std::fs::read_to_string(abandoned[0].join("lib/partial-install")).unwrap(),
        "partial",
        "the abandoned build is retained with its contents"
    );
    assert!(!build_record_path(&exact).exists());

    let python = ensure_kernel_python(EnsureKernelPythonOptions::default())
        .await
        .expect("the next session builds the exact path");
    assert_eq!(python, exact.join("bin/python"));
    assert!(!build_record_path(&exact).exists());
}

/// A bootstrap process killed mid-build (no destructor runs) leaves its
/// partial directory and build record. The next caller proves the record
/// is its own product's, the owner dead, and the directory the recorded
/// one; it moves the directory aside, retained, and rebuilds.
#[test]
fn explicit_build_killed_mid_bootstrap_is_set_aside_and_rebuilt() {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let root = tempfile::tempdir().unwrap();
    let package = runtime_fixture(root.path());
    let (bin, log) = fake_uv(root.path());
    let exact = root.path().join("explicit-kernel-venv");
    let pause = pause_fifos(root.path());
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "bootstrap_subprocess_entry", "--nocapture"])
        .env("PATH", &path)
        .env("PI_PACKAGE_DIR", &package)
        .env("PRIME_AGENT_KERNEL_VENV", &exact)
        .env("UV_LOG", &log)
        .env("UV_OWNER_DIR", &pause.owner_dir)
        .env("UV_STARTED_FIFO", &pause.started)
        .env("UV_RELEASE_FIFO", &pause.release)
        .env("UV_PARTIAL", "1")
        .env(
            "PA_KERNEL_BOOTSTRAP_CHILD_RESULT",
            root.path().join("never-written"),
        )
        .env_remove("PRIME_AGENT_KERNEL_PYTHON")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    wait_for_paused_uv(&pause.started);
    let group = nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap());
    nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL).unwrap();
    child.wait().unwrap();
    assert!(exact.join("lib/partial-install").is_file());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let python = runtime.block_on(async {
        let _serial = ENV_LOCK.lock().await;
        invalidate_runtime_probe_cache();
        let _env = explicit_env(&bin, &package, &log, &exact, None);
        ensure_kernel_python(EnsureKernelPythonOptions::default()).await
    });
    let python = python.expect("an abandoned own build is set aside and rebuilt");
    assert_eq!(python, exact.join("bin/python"));
    assert!(!exact.join("lib/partial-install").exists());
    let abandoned = abandoned_siblings(&exact);
    assert_eq!(abandoned.len(), 1, "{abandoned:?}");
    assert_eq!(
        std::fs::read_to_string(abandoned[0].join("lib/partial-install")).unwrap(),
        "partial",
        "the abandoned build is retained with its contents"
    );
    assert!(!build_record_path(&exact).exists());
}

/// An EMPTY exact directory holds nothing to lose: a non-recursive rmdir
/// (which fails if anything appears) clears it, then the path is built.
#[tokio::test]
async fn empty_explicit_dir_is_removed_without_recursion_and_rebuilt() {
    let _serial = ENV_LOCK.lock().await;
    invalidate_runtime_probe_cache();
    let root = tempfile::tempdir().unwrap();
    let package = runtime_fixture(root.path());
    let (bin, log) = fake_uv(root.path());
    let exact = root.path().join("explicit-kernel-venv");
    std::fs::create_dir(&exact).unwrap();
    let _env = explicit_env(&bin, &package, &log, &exact, None);

    let python = ensure_kernel_python(EnsureKernelPythonOptions::default())
        .await
        .expect("an empty exact directory is rebuilt");
    assert_eq!(python, exact.join("bin/python"));
    assert!(abandoned_siblings(&exact).is_empty());
}

fn write_build_record(exact: &Path, identity_of: &Path, owner_pid: u32) {
    let start_id = pa_types::platform::process::process_start_id(owner_pid);
    write_build_record_with_start_id(exact, identity_of, owner_pid, start_id.as_deref());
}

fn write_build_record_with_start_id(
    exact: &Path,
    identity_of: &Path,
    owner_pid: u32,
    start_id: Option<&str>,
) {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = std::fs::metadata(identity_of).unwrap();
    let parent = std::fs::metadata(exact.parent().unwrap()).unwrap();
    let record = serde_json::json!({
        "version": 3,
        "token": "00000000-0000-4000-8000-000000000000",
        "directory_identity": { "volume": metadata.dev(), "file": metadata.ino() },
        "path": exact.parent().unwrap().canonicalize().unwrap().join(exact.file_name().unwrap()),
        "parent_identity": { "volume": parent.dev(), "file": parent.ino() },
        "owner": { "pid": owner_pid, "process_start_id": start_id },
    });
    let path = build_record_path(exact);
    std::fs::write(&path, format!("{record}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

async fn assert_refused_untouched(exact: &Path, log: &Path) {
    let error = ensure_kernel_python(EnsureKernelPythonOptions::default())
        .await
        .expect_err("not provably an abandoned own build");
    assert!(error.to_string().contains("will not replace"), "{error:#}");
    assert_eq!(
        std::fs::read_to_string(exact.join("lib/partial-install")).unwrap(),
        "partial"
    );
    assert!(abandoned_siblings(exact).is_empty());
    assert!(
        !log.exists() || std::fs::read_to_string(log).unwrap().is_empty(),
        "a refused explicit path must not start uv"
    );
}

fn dead_pid() -> u32 {
    let mut dead = std::process::Command::new("true").spawn().unwrap();
    let pid = dead.id();
    dead.wait().unwrap();
    pid
}

fn partial_explicit_dir(root: &Path) -> PathBuf {
    let exact = root.join("explicit-kernel-venv");
    std::fs::create_dir_all(exact.join("lib")).unwrap();
    std::fs::write(exact.join("lib/partial-install"), "partial").unwrap();
    exact
}

/// A live pid whose start id cannot be read is unknowable, never dead.
#[tokio::test]
async fn explicit_build_record_with_an_unknown_owner_is_refused_untouched() {
    let _serial = ENV_LOCK.lock().await;
    invalidate_runtime_probe_cache();
    let root = tempfile::tempdir().unwrap();
    let package = runtime_fixture(root.path());
    let (bin, log) = fake_uv(root.path());
    let exact = partial_explicit_dir(root.path());
    write_build_record_with_start_id(&exact, &exact, std::process::id(), None);
    let _env = explicit_env(&bin, &package, &log, &exact, None);

    assert_refused_untouched(&exact, &log).await;
    assert!(build_record_path(&exact).exists());
}

/// A path that got as far as a version file is a stale environment, not an
/// abandoned build, whatever its record says.
#[tokio::test]
async fn explicit_build_record_beside_a_version_file_is_refused_untouched() {
    let _serial = ENV_LOCK.lock().await;
    invalidate_runtime_probe_cache();
    let root = tempfile::tempdir().unwrap();
    let package = runtime_fixture(root.path());
    let (bin, log) = fake_uv(root.path());
    let exact = partial_explicit_dir(root.path());
    std::fs::write(exact.join(".bootstrap-version"), "{\"schema\":1}\n").unwrap();
    write_build_record(&exact, &exact, dead_pid());
    let _env = explicit_env(&bin, &package, &log, &exact, None);

    assert_refused_untouched(&exact, &log).await;
}

/// An exact directory the caller cannot read gets the unchanged non-current
/// refusal naming the path, not a bare I/O error, and stays untouched.
#[tokio::test]
async fn unreadable_explicit_dir_gets_the_unchanged_refusal() {
    use std::os::unix::fs::PermissionsExt;

    struct RestoreMode(PathBuf);
    impl Drop for RestoreMode {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
    }

    let _serial = ENV_LOCK.lock().await;
    invalidate_runtime_probe_cache();
    let root = tempfile::tempdir().unwrap();
    let package = runtime_fixture(root.path());
    let (bin, log) = fake_uv(root.path());
    let exact = partial_explicit_dir(root.path());
    std::fs::set_permissions(&exact, std::fs::Permissions::from_mode(0o000)).unwrap();
    let restore = RestoreMode(exact.clone());
    let _env = explicit_env(&bin, &package, &log, &exact, None);

    let error = ensure_kernel_python(EnsureKernelPythonOptions::default())
        .await
        .expect_err("an unreadable non-current path is refused");
    let message = error.to_string();
    assert!(
        message.contains(&format!(
            "PRIME_AGENT_KERNEL_VENV points to a non-current environment at {}",
            exact.display()
        )),
        "{error:#}"
    );
    drop(restore);
    assert_eq!(
        std::fs::read_to_string(exact.join("lib/partial-install")).unwrap(),
        "partial"
    );
    assert!(abandoned_siblings(&exact).is_empty());
}

/// When the attempt cannot move its own directory aside, it keeps the
/// build record, so a later run (once this owner is dead) reclaims the
/// path. A pre-existing aside target stands in for a failed rename (on
/// Windows, a killed uv still holding handles).
#[tokio::test]
async fn failed_set_aside_keeps_the_build_record() {
    let _serial = ENV_LOCK.lock().await;
    invalidate_runtime_probe_cache();
    let root = tempfile::tempdir().unwrap();
    let package = runtime_fixture(root.path());
    let (bin, log) = fake_uv(root.path());
    let exact = root.path().join("explicit-kernel-venv");
    let pause = pause_fifos(root.path());
    let _env = explicit_env(&bin, &package, &log, &exact, Some(&pause));

    let attempt = tokio::spawn(ensure_kernel_python(EnsureKernelPythonOptions::default()));
    let started = pause.started.clone();
    tokio::task::spawn_blocking(move || wait_for_paused_uv(&started))
        .await
        .unwrap();
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(build_record_path(&exact)).unwrap()).unwrap();
    let blocker = exact.with_file_name(format!(
        "explicit-kernel-venv.abandoned-{}",
        record["token"].as_str().unwrap()
    ));
    std::fs::write(&blocker, "occupied").unwrap();
    attempt.abort();
    assert!(attempt.await.unwrap_err().is_cancelled());
    release_paused_uv(&pause.release);

    assert_eq!(
        std::fs::read_to_string(exact.join("lib/partial-install")).unwrap(),
        "partial",
        "the directory stays at the path when the rename cannot run"
    );
    assert!(
        build_record_path(&exact).is_file(),
        "the record must survive so a later run can reclaim the path"
    );
    assert_eq!(std::fs::read_to_string(&blocker).unwrap(), "occupied");
}

/// A record whose owner is alive is a build in progress (or unknowable):
/// the directory is refused and untouched.
#[tokio::test]
async fn explicit_build_record_with_a_live_owner_is_refused_untouched() {
    let _serial = ENV_LOCK.lock().await;
    invalidate_runtime_probe_cache();
    let root = tempfile::tempdir().unwrap();
    let package = runtime_fixture(root.path());
    let (bin, log) = fake_uv(root.path());
    let exact = root.path().join("explicit-kernel-venv");
    std::fs::create_dir_all(exact.join("lib")).unwrap();
    std::fs::write(exact.join("lib/partial-install"), "partial").unwrap();
    write_build_record(&exact, &exact, std::process::id());
    let _env = explicit_env(&bin, &package, &log, &exact, None);

    assert_refused_untouched(&exact, &log).await;
    assert!(build_record_path(&exact).exists());
}

/// A record naming another directory (the path was swapped after the
/// build began) never authorizes moving the current one.
#[tokio::test]
async fn explicit_build_record_for_another_directory_is_refused_untouched() {
    let _serial = ENV_LOCK.lock().await;
    invalidate_runtime_probe_cache();
    let root = tempfile::tempdir().unwrap();
    let package = runtime_fixture(root.path());
    let (bin, log) = fake_uv(root.path());
    let exact = root.path().join("explicit-kernel-venv");
    std::fs::create_dir_all(exact.join("lib")).unwrap();
    std::fs::write(exact.join("lib/partial-install"), "partial").unwrap();
    let other = root.path().join("other-dir");
    std::fs::create_dir(&other).unwrap();
    write_build_record(&exact, &other, dead_pid());
    let _env = explicit_env(&bin, &package, &log, &exact, None);

    assert_refused_untouched(&exact, &log).await;
}

#[tokio::test]
async fn explicit_external_runtime_requires_the_subagent_row_abi_before_start() {
    let _serial = ENV_LOCK.lock().await;
    invalidate_runtime_probe_cache();
    let root = tempfile::tempdir().unwrap();
    let python = root.path().join("callable-compatible-python");
    write_executable(
        &python,
        r#"#!/bin/sh
set -eu
if [ "${1:-}" = "-c" ] && printf '%s' "${2:-}" | grep -q RLM_RUNTIME_ABI; then
  [ "${FAKE_RUNTIME_ABI:-absent}" = "int:2" ]
  exit
fi
exit 0
"#,
    );
    let sentinel = root.path().join("external-runtime-owner-state");
    std::fs::write(&sentinel, "never mutate").unwrap();
    let _env = EnvRestore::configure(&[
        (
            "PRIME_AGENT_KERNEL_PYTHON",
            Some(python.to_string_lossy().into_owned()),
        ),
        ("PRIME_AGENT_KERNEL_VENV", None),
        ("FAKE_RUNTIME_ABI", None),
    ]);

    for unsupported in [
        None,
        Some("int:1"),
        Some("int:3"),
        Some("str:2"),
        Some("float:2.0"),
        Some("bool:true"),
        Some("malformed"),
    ] {
        match unsupported {
            Some(value) => std::env::set_var("FAKE_RUNTIME_ABI", value),
            None => std::env::remove_var("FAKE_RUNTIME_ABI"),
        }
        let error = ensure_kernel_python(EnsureKernelPythonOptions::default())
            .await
            .expect_err("every ABI other than the exact integer 2 must fail before start");
        let message = error.to_string();
        assert!(message.contains("ABI 2"), "{message}");
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "never mutate");
    }

    std::env::set_var("FAKE_RUNTIME_ABI", "int:2");
    let resolved = ensure_kernel_python(EnsureKernelPythonOptions::default())
        .await
        .unwrap();
    assert_eq!(resolved, python);
    assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "never mutate");
}

#[tokio::test]
async fn relative_external_python_is_anchored_before_kernel_cwd_changes() {
    let _serial = ENV_LOCK.lock().await;
    invalidate_runtime_probe_cache();
    let cwd = std::env::current_dir().unwrap();
    let root = tempfile::tempdir_in(&cwd).unwrap();
    let python = root.path().join("abi2-python");
    write_executable(
        &python,
        r#"#!/bin/sh
set -eu
if [ "${1:-}" = "-c" ] && printf '%s' "${2:-}" | grep -q RLM_RUNTIME_ABI; then
  [ "${FAKE_RUNTIME_ABI:-}" = "int:2" ]
  exit
fi
exit 0
"#,
    );
    let relative = python
        .strip_prefix(&cwd)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let _env = EnvRestore::configure(&[
        ("PRIME_AGENT_KERNEL_PYTHON", Some(relative)),
        ("PRIME_AGENT_KERNEL_VENV", None),
        ("FAKE_RUNTIME_ABI", Some("int:2".to_string())),
    ]);
    let resolved = ensure_kernel_python(EnsureKernelPythonOptions::default())
        .await
        .unwrap();
    assert!(resolved.is_absolute());
    assert_eq!(resolved, python);
}

#[tokio::test]
async fn issue44_managed_and_explicit_optional_skill_failure_is_immutable_truthful_and_reusable() {
    let _serial = ENV_LOCK.lock().await;
    for override_name in [None, Some("explicit-kernel-venv")] {
    invalidate_runtime_probe_cache();
    let root = tempfile::tempdir().unwrap();
    let package = runtime_fixture(root.path());
    let (bin, log) = fake_uv(root.path());
    let _env = bootstrap_env(root.path(), &bin, &package, &log);
    let _explicit = EnvRestore::configure(&[("PRIME_AGENT_KERNEL_VENV", override_name.map(|name| root.path().join(name).display().to_string()))]);
    let first = ensure_kernel_python(EnsureKernelPythonOptions::default())
        .await
        .unwrap();
    let first_sentinel = first
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("still-usable");
    std::fs::write(&first_sentinel, "yes").unwrap();

    let skill_dir = root.path().join("skills/broken-skill");
    std::fs::create_dir_all(skill_dir.join("src/broken_skill")).unwrap();
    let pyproject = skill_dir.join("pyproject.toml");
    std::fs::write(
        &pyproject,
        "[project]\nname='broken-skill'\nversion='0.1.0'\n[build-system]\nrequires=['hatchling']\nbuild-backend='hatchling.build'\n",
    )
    .unwrap();
    std::fs::write(skill_dir.join("src/broken_skill/__init__.py"), "").unwrap();
    let options = || EnsureKernelPythonOptions {
        python_skills: vec![KernelPythonSkill {
            name: "broken-skill".to_string(),
            import_name: "broken_skill".to_string(),
            package_path: skill_dir.clone(),
            pyproject_path: pyproject.clone(),
        }],
        on_progress: None,
    };

    std::env::set_var("UV_FAIL_SKILL", "1");
    let partial = ensure_kernel_python(options()).await.unwrap();
    std::env::remove_var("UV_FAIL_SKILL");
    assert_ne!(partial, first);
    assert_eq!(std::fs::read_to_string(&first_sentinel).unwrap(), "yes");
    let partial_venv = partial.parent().unwrap().parent().unwrap();
    let marker: serde_json::Value =
        serde_json::from_slice(&std::fs::read(partial_venv.join(".bootstrap-version")).unwrap())
            .unwrap();
    assert_eq!(
        marker["requested_python_skills"].as_array().unwrap().len(),
        1
    );
    assert!(marker["python_skills"].as_array().unwrap().is_empty());
    let unavailable = marker["unavailable_python_skills"].as_array().unwrap();
    assert_eq!(unavailable.len(), 1);
    assert_eq!(unavailable[0]["import_name"], "broken_skill");
    assert!(unavailable[0]["reason"]
        .as_str()
        .unwrap()
        .contains("exit code 19"));

    let venv_calls_before = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .filter(|line| line.starts_with("venv "))
        .count();
    let reused = ensure_kernel_python(options()).await.unwrap();
    assert_eq!(
        reused, partial,
        "identical failed request reuses immutable outcome"
    );
    let venv_calls_after = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .filter(|line| line.starts_with("venv "))
        .count();
    assert_eq!(venv_calls_after, venv_calls_before, "no retry storm");

    let partial_sentinel = partial_venv.join("partial-stays");
    std::fs::write(&partial_sentinel, "yes").unwrap();
    std::fs::write(
        &pyproject,
        "[project]\nname='broken-skill'\nversion='0.2.0'\n[build-system]\nrequires=['hatchling']\nbuild-backend='hatchling.build'\n",
    )
    .unwrap();
    std::env::set_var("UV_FAIL_SKILL", "1");
    let changed = ensure_kernel_python(options()).await.unwrap();
    std::env::remove_var("UV_FAIL_SKILL");
    assert_ne!(
        changed, partial,
        "changed requested fingerprint gets a new generation"
    );
    assert_eq!(std::fs::read_to_string(partial_sentinel).unwrap(), "yes");
    }
}

#[tokio::test]
async fn external_runtime_missing_optional_skill_warns_without_mutation() {
    let _serial = ENV_LOCK.lock().await;
    invalidate_runtime_probe_cache();
    let root = tempfile::tempdir().unwrap();
    let python = root.path().join("abi2-python");
    write_executable(
        &python,
        r#"#!/bin/sh
set -eu
if [ "${1:-}" = "-c" ] && printf '%s' "${2:-}" | grep -q RLM_RUNTIME_ABI; then
  exit 0
fi
if [ "${2:-}" = "import optional_skill" ]; then exit 1; fi
exit 0
"#,
    );
    let sentinel = root.path().join("external-owner-state");
    std::fs::write(&sentinel, "unchanged").unwrap();
    let skill_dir = root.path().join("optional-skill");
    std::fs::create_dir(&skill_dir).unwrap();
    let pyproject = skill_dir.join("pyproject.toml");
    std::fs::write(&pyproject, "[project]\nname='optional-skill'\n").unwrap();
    let _env = EnvRestore::configure(&[
        (
            "PRIME_AGENT_KERNEL_PYTHON",
            Some(python.to_string_lossy().into_owned()),
        ),
        ("PRIME_AGENT_KERNEL_VENV", None),
    ]);
    let warnings = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = warnings.clone();
    let resolved = ensure_kernel_python(EnsureKernelPythonOptions {
        python_skills: vec![KernelPythonSkill {
            name: "optional-skill".to_string(),
            import_name: "optional_skill".to_string(),
            package_path: skill_dir,
            pyproject_path: pyproject,
        }],
        on_progress: Some(std::sync::Arc::new(move |message| {
            sink.lock().unwrap().push(message.to_string());
        })),
    })
    .await
    .unwrap();
    assert_eq!(resolved, python);
    assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "unchanged");
    let warnings = warnings.lock().unwrap();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("optional-skill (optional_skill)"));
}

#[tokio::test]
async fn missing_required_default_import_never_publishes_over_a_usable_generation() {
    let _serial = ENV_LOCK.lock().await;
    invalidate_runtime_probe_cache();
    let root = tempfile::tempdir().unwrap();
    let package = runtime_fixture(root.path());
    let (bin, log) = fake_uv(root.path());
    let _env = bootstrap_env(root.path(), &bin, &package, &log);
    let first = ensure_kernel_python(EnsureKernelPythonOptions::default())
        .await
        .unwrap();
    let first_sentinel = first
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("required-base-stays");
    std::fs::write(&first_sentinel, "yes").unwrap();
    std::fs::write(
        package.join("prime-agent-runtime/src/rlm/repl.py"),
        "RUNTIME = 'requires-new-generation'\n",
    )
    .unwrap();
    std::env::set_var("FAKE_MISSING_DEFAULT", "1");
    let error = ensure_kernel_python(EnsureKernelPythonOptions::default())
        .await
        .expect_err("a missing promised default import is fatal before publication");
    std::env::remove_var("FAKE_MISSING_DEFAULT");
    assert!(
        error.to_string().contains("default Python packages"),
        "{error:#}"
    );
    assert_eq!(std::fs::read_to_string(first_sentinel).unwrap(), "yes");
    let generation_root = root
        .path()
        .join("home/.prime/agent/kernel-venv.generations");
    assert_eq!(
        std::fs::read_dir(generation_root)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().join(".generation-published").is_file())
            .count(),
        1
    );
}

#[tokio::test]
async fn exact_override_identity_swap_is_retained_and_never_accepted() {
    let _serial = ENV_LOCK.lock().await;
    invalidate_runtime_probe_cache();
    let root = tempfile::tempdir().unwrap();
    let package = runtime_fixture(root.path());
    let (bin, log) = fake_uv(root.path());
    let exact = root.path().join("explicit-kernel-venv");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let _env = EnvRestore::configure(&[
        ("PATH", Some(path)),
        (
            "PI_PACKAGE_DIR",
            Some(package.to_string_lossy().into_owned()),
        ),
        (
            "PRIME_AGENT_KERNEL_VENV",
            Some(exact.to_string_lossy().into_owned()),
        ),
        ("PRIME_AGENT_KERNEL_PYTHON", None),
        ("UV_LOG", Some(log.to_string_lossy().into_owned())),
        ("UV_SWAP_TARGET", Some("1".to_string())),
    ]);
    let error = ensure_kernel_python(EnsureKernelPythonOptions::default())
        .await
        .expect_err("a swapped exact target cannot pass post-uv ownership validation");
    assert!(error.to_string().contains("ownership changed"), "{error:#}");
    assert_eq!(
        std::fs::read_to_string(exact.join("foreign-owner-state")).unwrap(),
        "foreign"
    );
    assert!(exact.with_extension("owned-before-swap").exists());
    assert!(abandoned_siblings(&exact).is_empty());
    assert!(build_record_path(&exact).exists(), "unknown ownership is retained");
}

#[tokio::test]
async fn direct_manager_python_is_abi_checked_before_spawn() {
    let root = tempfile::tempdir().unwrap();
    let python = root.path().join("old-runtime-python");
    let spawned = root.path().join("spawned");
    write_executable(
        &python,
        &format!(
            "#!/bin/sh\nif [ \"${{1:-}}\" = \"-c\" ]; then exit 1; fi\nprintf spawned > '{}'\nexit 1\n",
            spawned.display()
        ),
    );
    let manager = ReplKernelManager::new(KernelManagerOptions {
        python: Some(python),
        ..KernelManagerOptions::default()
    });
    let error = manager
        .start(KernelStartOptions::default())
        .await
        .expect_err("direct low-level Python injection still requires ABI 2");
    assert!(error.to_string().contains("ABI 2"), "{error:#}");
    assert!(
        !spawned.exists(),
        "unsupported runtime was rejected before -m rlm.repl"
    );
}

#[tokio::test]
async fn direct_manager_probe_uses_the_exact_kernel_cwd() {
    let root = tempfile::tempdir().unwrap();
    let kernel_cwd = root.path().join("kernel-cwd");
    std::fs::create_dir(&kernel_cwd).unwrap();
    let python = root.path().join("cwd-sensitive-python");
    let spawned = root.path().join("spawned");
    let kernel_cwd_canonical = kernel_cwd.canonicalize().unwrap();
    write_executable(
        &python,
        &format!(
            "#!/bin/sh\nif [ \"${{1:-}}\" = \"-c\" ]; then [ \"$PWD\" != '{}' ]; exit; fi\nprintf spawned > '{}'\nexit 1\n",
            kernel_cwd_canonical.display(),
            spawned.display()
        ),
    );
    let manager = ReplKernelManager::new(KernelManagerOptions {
        python: Some(python),
        cwd: Some(kernel_cwd),
        ..KernelManagerOptions::default()
    });
    let error = manager
        .start(KernelStartOptions::default())
        .await
        .expect_err("probe must reject the runtime shadowed by the kernel cwd");
    assert!(error.to_string().contains("spawn environment"), "{error:#}");
    assert!(!spawned.exists());
}

#[tokio::test]
async fn direct_manager_probe_uses_the_exact_kernel_environment() {
    let root = tempfile::tempdir().unwrap();
    let python = root.path().join("env-sensitive-python");
    let spawned = root.path().join("spawned");
    write_executable(
        &python,
        &format!(
            "#!/bin/sh\nif [ \"${{1:-}}\" = \"-c\" ]; then [ \"${{PYTHONPATH:-}}\" != \"old-runtime-shadow\" ]; exit; fi\nprintf spawned > '{}'\nexit 1\n",
            spawned.display()
        ),
    );
    let manager = ReplKernelManager::new(KernelManagerOptions {
        python: Some(python),
        env: std::collections::HashMap::from([(
            "PYTHONPATH".to_string(),
            "old-runtime-shadow".to_string(),
        )]),
        ..KernelManagerOptions::default()
    });
    let error = manager
        .start(KernelStartOptions::default())
        .await
        .expect_err("probe must see interpreter-affecting kernel env overlays");
    assert!(error.to_string().contains("spawn environment"), "{error:#}");
    assert!(!spawned.exists());
}

#[tokio::test]
async fn issue44_enrolled_explicit_root_updates_without_mutating_the_base() {
    let _serial = ENV_LOCK.lock().await;
    invalidate_runtime_probe_cache();
    let root = tempfile::tempdir().unwrap();
    let package = runtime_fixture(root.path());
    let (bin, log) = fake_uv(root.path());
    let exact = root.path().join("explicit-kernel-venv");
    let _env = explicit_env(&bin, &package, &log, &exact, None);
    let first = ensure_kernel_python(EnsureKernelPythonOptions::default()).await.unwrap();
    let original = std::fs::read(exact.join(".bootstrap-version")).unwrap();
    std::fs::write(package.join("prime-agent-runtime/src/rlm/repl.py"), "RUNTIME = 44\n").unwrap();
    let (second, concurrent) = tokio::join!(
        ensure_kernel_python(EnsureKernelPythonOptions::default()),
        ensure_kernel_python(EnsureKernelPythonOptions::default()),
    );
    let second = second.expect("issue44: enrolled explicit root selects an immutable sibling");
    assert_eq!(second, concurrent.unwrap());
    assert_ne!(first, second);
    assert_eq!(std::fs::read(exact.join(".bootstrap-version")).unwrap(), original);
    let generation = second.parent().unwrap().parent().unwrap();
    assert_eq!(generation.parent().unwrap(), exact.with_file_name("explicit-kernel-venv.generations"));
    assert!(generation.join(".generation-published").is_file());
    assert!(generation.join(".leases").is_dir());
    invalidate_runtime_probe_cache();
    assert_eq!(ensure_kernel_python(EnsureKernelPythonOptions::default()).await.unwrap(), second);
    let calls = std::fs::read_to_string(log).unwrap();
    assert_eq!(calls.lines().filter(|line| line.starts_with("venv ")).count(), 2);
    assert_eq!(calls.lines().filter(|line| line.starts_with("pip ") && line.contains(&first.display().to_string())).count(), 1);
}

#[tokio::test]
async fn issue44_copied_or_damaged_ownership_never_authorizes_generation_updates() {
    let _serial = ENV_LOCK.lock().await;
    invalidate_runtime_probe_cache();
    let root = tempfile::tempdir().unwrap();
    let package = runtime_fixture(root.path());
    let (bin, log) = fake_uv(root.path());
    let exact = root.path().join("explicit-kernel-venv");
    let _env = explicit_env(&bin, &package, &log, &exact, None);
    ensure_kernel_python(EnsureKernelPythonOptions::default()).await.unwrap();
    let ownership = exact.with_file_name("explicit-kernel-venv.prime-agent-managed.json");
    let original = std::fs::read(&ownership).expect("new explicit builds persist ownership");
    std::fs::write(package.join("prime-agent-runtime/src/rlm/repl.py"), "RUNTIME = 44\n").unwrap();
    let before = std::fs::read(&log).unwrap();
    for invalid in [b"{}".to_vec(), b"not-json".to_vec()] {
        std::fs::write(&ownership, invalid).unwrap();
        assert!(ensure_kernel_python(EnsureKernelPythonOptions::default()).await.is_err());
        assert_eq!(std::fs::read(&log).unwrap(), before);
    }
    std::fs::write(&ownership, &original).unwrap();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&ownership).unwrap().permissions().mode() & 0o777, 0o600);
    std::fs::set_permissions(&ownership, std::fs::Permissions::from_mode(0o666)).unwrap();
    assert!(ensure_kernel_python(EnsureKernelPythonOptions::default()).await.is_err());
    assert_eq!(std::fs::metadata(&ownership).unwrap().permissions().mode() & 0o777, 0o666, "no permission repair");
    std::fs::set_permissions(&ownership, std::fs::Permissions::from_mode(0o600)).unwrap();
    let preserved = root.path().join("original-owner-record");
    std::fs::rename(&ownership, &preserved).unwrap();
    std::os::unix::fs::symlink(&preserved, &ownership).unwrap();
    assert!(ensure_kernel_python(EnsureKernelPythonOptions::default()).await.is_err());
    assert!(ownership.is_symlink());
    std::fs::remove_file(&ownership).unwrap();
    std::fs::rename(&preserved, &ownership).unwrap();
    let displaced = root.path().join("retained-original");
    std::fs::rename(&exact, &displaced).unwrap();
    std::fs::create_dir(&exact).unwrap();
    std::fs::copy(displaced.join(".bootstrap-version"), exact.join(".bootstrap-version")).unwrap();
    std::fs::write(exact.join("foreign"), "retained").unwrap();
    assert!(ensure_kernel_python(EnsureKernelPythonOptions::default()).await.is_err());
    assert_eq!(std::fs::read_to_string(exact.join("foreign")).unwrap(), "retained");
    assert_eq!(std::fs::read(&log).unwrap(), before);
    assert_eq!(std::fs::read(&ownership).unwrap(), original);
    assert!(!root.path().join("explicit-kernel-venv.generations").exists());
}

#[tokio::test]
async fn issue44_current_legacy_root_is_read_only_but_not_silently_enrolled() {
    let _serial = ENV_LOCK.lock().await;
    invalidate_runtime_probe_cache();
    let root = tempfile::tempdir().unwrap();
    let package = runtime_fixture(root.path());
    let (bin, log) = fake_uv(root.path());
    let exact = root.path().join("explicit-kernel-venv");
    let _env = explicit_env(&bin, &package, &log, &exact, None);
    let first = ensure_kernel_python(EnsureKernelPythonOptions::default()).await.unwrap();
    let ownership = exact.with_file_name("explicit-kernel-venv.prime-agent-managed.json");
    std::fs::remove_file(&ownership).unwrap(); // represents an old published explicit environment
    let before = std::fs::read(&log).unwrap();
    let observed: Vec<_> = [exact.clone(), exact.join(".bootstrap-version"), first.clone()]
        .into_iter().map(|path| {
            let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
            let bytes = path.is_file().then(|| std::fs::read(&path).unwrap());
            (path, modified, bytes)
        }).collect();
    assert_eq!(ensure_kernel_python(EnsureKernelPythonOptions::default()).await.unwrap(), first);
    for (path, modified, bytes) in observed {
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), modified);
        if let Some(bytes) = bytes { assert_eq!(std::fs::read(path).unwrap(), bytes); }
    }
    assert!(!ownership.exists(), "current legacy paths must not be adopted");
    std::fs::write(&ownership, "unknown-management-record").unwrap();
    let ownership_mtime = std::fs::metadata(&ownership).unwrap().modified().unwrap();
    assert_eq!(ensure_kernel_python(EnsureKernelPythonOptions::default()).await.unwrap(), first);
    assert_eq!(std::fs::read_to_string(&ownership).unwrap(), "unknown-management-record");
    assert_eq!(std::fs::metadata(&ownership).unwrap().modified().unwrap(), ownership_mtime);
    std::fs::remove_file(&ownership).unwrap();
    std::fs::write(package.join("prime-agent-runtime/src/rlm/repl.py"), "RUNTIME = 44\n").unwrap();
    let error = ensure_kernel_python(EnsureKernelPythonOptions::default()).await.unwrap_err();
    assert!(error.to_string().contains("new, missing PRIME_AGENT_KERNEL_VENV"), "{error:#}");
    assert_eq!(std::fs::read(&log).unwrap(), before);
    assert!(!root.path().join("explicit-kernel-venv.generations").exists());
}

#[tokio::test]
async fn issue44_interrupted_validated_base_finishes_publication_before_updating() {
    let _serial = ENV_LOCK.lock().await;
    invalidate_runtime_probe_cache();
    let root = tempfile::tempdir().unwrap();
    let package = runtime_fixture(root.path());
    let (bin, log) = fake_uv(root.path());
    let exact = root.path().join("explicit-kernel-venv");
    let _env = explicit_env(&bin, &package, &log, &exact, None);
    let first = ensure_kernel_python(EnsureKernelPythonOptions::default()).await.unwrap();
    let ownership = exact.with_file_name("explicit-kernel-venv.prime-agent-managed.json");
    std::fs::remove_file(&ownership).unwrap();
    write_build_record(&exact, &exact, dead_pid());
    assert_eq!(ensure_kernel_python(EnsureKernelPythonOptions::default()).await.unwrap(), first);
    assert!(ownership.is_file(), "dead validated build is published after revalidation");
    assert!(!build_record_path(&exact).exists());
    std::fs::write(package.join("prime-agent-runtime/src/rlm/repl.py"), "RUNTIME = 44\n").unwrap();
    assert_ne!(ensure_kernel_python(EnsureKernelPythonOptions::default()).await.unwrap(), first);
}

/// Explicit acceptance verifier: uses real offline uv installs and the approved
/// read-only CPython toolchain. Run with --ignored --exact, never auto-skip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual: requires a coordinator-owned offline uv cache and ABI-2 test runtime"]
async fn issue44_real_uv_keeps_two_live_kernels_on_immutable_generations() {
    use pa_core::kernel::shared::{ExecuteOptions, ExecuteStatus, KernelShutdownOptions};
    use sha2::Digest;
    use std::time::Duration;

    let _serial = ENV_LOCK.lock().await;
    invalidate_runtime_probe_cache();
    let root = tempfile::tempdir().unwrap().keep();
    eprintln!("issue44 real-uv evidence: {}", root.display());
    let cache = PathBuf::from(std::env::var("PA_KERNEL_VERIFY_CACHE").expect("explicit private offline cache required"));
    let target = PathBuf::from(std::env::var("CARGO_TARGET_DIR").expect("private Cargo target required"));
    assert!(cache.canonicalize().unwrap().starts_with(target.canonicalize().unwrap()));
    let approved = PathBuf::from(std::env::var("PA_E2E_KERNEL_PYTHON").expect("approved ABI-2 runtime required"));
    let interpreter = approved.canonicalize().unwrap();
    let toolchain = interpreter.parent().unwrap().parent().unwrap();
    let toolchains = root.join("python-toolchains");
    std::fs::create_dir(&toolchains).unwrap();
    std::os::unix::fs::symlink(toolchain, toolchains.join(toolchain.file_name().unwrap())).unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../prime-agent-runtime");
    let package = root.join("package");
    let runtime_source = package.join("prime-agent-runtime");
    copy_runtime_fixture(&source, &runtime_source);
    let runtime_init = runtime_source.join("src/rlm/__init__.py");
    let original_init = std::fs::read_to_string(&runtime_init).unwrap();
    std::fs::write(&runtime_init, format!("{original_init}\nISSUE44_RUNTIME = 1\n")).unwrap();
    let exact = root.join(format!("generation-{}-{:020}-{}", "a".repeat(20), 1, uuid::Uuid::new_v4()));
    let skill = root.join("skill");
    std::fs::create_dir_all(skill.join("src/issue44_skill")).unwrap();
    let pyproject = skill.join("pyproject.toml");
    std::fs::write(&pyproject, "[project]\nname='issue44-skill'\nversion='0.1.0'\n[build-system]\nrequires=['hatchling']\nbuild-backend='hatchling.build'\n[tool.hatch.build.targets.wheel]\npackages=['src/issue44_skill']\n").unwrap();
    std::fs::write(skill.join("src/issue44_skill/__init__.py"), "VALUE = 44\n").unwrap();
    let python_skill = KernelPythonSkill { name: "issue44-skill".into(), import_name: "issue44_skill".into(), package_path: skill, pyproject_path: pyproject };
    let options = || EnsureKernelPythonOptions { python_skills: vec![python_skill.clone()], on_progress: None };
    let _env = EnvRestore::configure(&[
        ("PRIME_AGENT_KERNEL_PYTHON", None),
        ("PRIME_AGENT_KERNEL_VENV", Some(exact.display().to_string())),
        ("PI_PACKAGE_DIR", Some(package.display().to_string())),
        ("UV_CACHE_DIR", Some(cache.display().to_string())),
        ("UV_PYTHON_INSTALL_DIR", Some(toolchains.display().to_string())),
        ("UV_OFFLINE", Some("1".into())),
        ("UV_PYTHON_DOWNLOADS", Some("never".into())),
        ("UV_LINK_MODE", Some("copy".into())),
    ]);
    let mut managers = Vec::new();
    let outcome = tokio::time::timeout(Duration::from_secs(240), async {
        let first = ensure_kernel_python(EnsureKernelPythonOptions::default()).await?;
        anyhow::ensure!(first == exact.join("bin/python"), "first bootstrap must use exact base");
        let original_marker = std::fs::read(exact.join(".bootstrap-version"))?;
        let first_manager = ReplKernelManager::new(KernelManagerOptions { python: Some(first.clone()), cwd: Some(root.clone()), ..Default::default() });
        managers.push(first_manager.clone());
        first_manager.start(KernelStartOptions::default()).await?;
        let (second, concurrent) = tokio::join!(ensure_kernel_python(options()), ensure_kernel_python(options()));
        let second = second?;
        anyhow::ensure!(second == concurrent? && second != first, "changed skill request must publish exactly one sibling");
        let second_manager = ReplKernelManager::new(KernelManagerOptions { python: Some(second.clone()), cwd: Some(root.clone()), ..Default::default() });
        managers.push(second_manager.clone());
        second_manager.start(KernelStartOptions::default()).await?;
        let mut receipts = Vec::new();
        let mut installed_hashes = Vec::new();
        for (manager, python, skill_expected) in [(&first_manager, &first, false), (&second_manager, &second, true)] {
            let expected = python.parent().unwrap().parent().unwrap();
            let check = format!("import sys, json, importlib.util, rlm\nfrom pathlib import Path\nassert Path(sys.prefix) == Path({:?})\nassert Path(rlm.__file__).is_relative_to(Path(sys.prefix))\nassert type(rlm.RLM_RUNTIME_ABI) is int and rlm.RLM_RUNTIME_ABI == 2\nassert rlm.ISSUE44_RUNTIME == 1\nassert (importlib.util.find_spec('issue44_skill') is not None) == {}\nprint(json.dumps(dict(prefix=sys.prefix, runtime=rlm.__file__, abi=rlm.RLM_RUNTIME_ABI, code=rlm.ISSUE44_RUNTIME)))", expected.display().to_string(), if skill_expected { "True" } else { "False" });
            let observed = manager.execute(&check, ExecuteOptions::default()).await?;
            anyhow::ensure!(observed.status == ExecuteStatus::Ok, "real kernel check failed: {:?}", observed.error);
            let observation: serde_json::Value = serde_json::from_str(observed.stdout.trim())?;
            let installed = PathBuf::from(observation["runtime"].as_str().ok_or_else(|| anyhow::anyhow!("missing actual module origin"))?);
            anyhow::ensure!(installed.starts_with(expected), "actual runtime module escaped selected environment");
            let hash = format!("{:x}", sha2::Sha256::digest(std::fs::read(&installed)?));
            installed_hashes.push((installed, hash));
            let pid = manager.process_id().ok_or_else(|| anyhow::anyhow!("missing owned kernel PID"))?;
            receipts.push(serde_json::json!({"python": python, "pid": pid, "start": pa_types::platform::process::process_start_id(u32::try_from(pid)?), "observed": observed.stdout}));
        }
        std::fs::write(root.join("receipts.json"), serde_json::to_vec_pretty(&receipts)?)?;
        let imported = second_manager.execute("import issue44_skill\nassert issue44_skill.VALUE == 44", ExecuteOptions::default()).await?;
        anyhow::ensure!(imported.status == ExecuteStatus::Ok, "actual skill import failed");
        std::fs::write(&runtime_init, format!("{original_init}\nISSUE44_RUNTIME = 2\n"))?;
        let third = ensure_kernel_python(options()).await?;
        anyhow::ensure!(third != second, "runtime update must select another generation");
        let third_manager = ReplKernelManager::new(KernelManagerOptions { python: Some(third.clone()), cwd: Some(root.clone()), ..Default::default() });
        managers.push(third_manager.clone());
        third_manager.start(KernelStartOptions::default()).await?;
        let third_pid = third_manager.process_id().ok_or_else(|| anyhow::anyhow!("missing third kernel PID"))?;
        receipts.push(serde_json::json!({"python": third, "pid": third_pid, "start": pa_types::platform::process::process_start_id(u32::try_from(third_pid)?)}));
        std::fs::write(root.join("receipts.json"), serde_json::to_vec_pretty(&receipts)?)?;
        for (manager, version) in [(&first_manager, 1), (&second_manager, 1), (&third_manager, 2)] {
            let observed = manager.execute(&format!("import importlib, rlm\nimportlib.reload(rlm)\nassert rlm.ISSUE44_RUNTIME == {version}"), ExecuteOptions::default()).await?;
            anyhow::ensure!(observed.status == ExecuteStatus::Ok, "active kernel code changed: {:?}", observed.error);
        }
        for (path, before) in &installed_hashes {
            anyhow::ensure!(format!("{:x}", sha2::Sha256::digest(std::fs::read(path)?)) == *before, "old installed runtime file changed: {}", path.display());
        }
        std::fs::write(root.join("installed-runtime-hashes.json"), serde_json::to_vec_pretty(&installed_hashes)?)?;
        let removed = ensure_kernel_python(EnsureKernelPythonOptions::default()).await?;
        anyhow::ensure!(removed != third, "removing a skill selects another environment");
        let removed_manager = ReplKernelManager::new(KernelManagerOptions { python: Some(removed.clone()), cwd: Some(root.clone()), ..Default::default() });
        managers.push(removed_manager.clone());
        removed_manager.start(KernelStartOptions::default()).await?;
        let removed_pid = removed_manager.process_id().ok_or_else(|| anyhow::anyhow!("missing removed-skill kernel PID"))?;
        receipts.push(serde_json::json!({"python": removed, "pid": removed_pid, "start": pa_types::platform::process::process_start_id(u32::try_from(removed_pid)?)}));
        let observed = removed_manager.execute("import importlib.util, rlm\nassert rlm.ISSUE44_RUNTIME == 2\nassert importlib.util.find_spec('issue44_skill') is None", ExecuteOptions::default()).await?;
        anyhow::ensure!(observed.status == ExecuteStatus::Ok, "removed skill is still installed: {:?}", observed.error);
        anyhow::ensure!(std::fs::read(exact.join(".bootstrap-version"))? == original_marker, "published base was mutated");
        anyhow::ensure!(ensure_kernel_python(options()).await? == third, "identical request must reuse matching published generation");
        std::fs::write(root.join("receipts.json"), serde_json::to_vec_pretty(&receipts)?)?;
        Ok::<_, anyhow::Error>(())
    }).await;
    let mut shutdown_errors = Vec::new();
    for manager in &managers {
        match tokio::time::timeout(Duration::from_secs(20), manager.shutdown(KernelShutdownOptions::default())).await {
            Ok(Ok(_)) => {}
            failure => { manager.kill(); shutdown_errors.push(format!("{failure:?}")); }
        }
    }
    assert!(shutdown_errors.is_empty(), "owned kernel shutdown failures: {shutdown_errors:?}");
    outcome.expect("bounded real-uv verifier").expect("real installs, imports, and generations must agree");
}

fn copy_runtime_fixture(source: &Path, target: &Path) {
    std::fs::create_dir_all(target).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if name == "__pycache__" || name == ".venv" { continue; }
        let destination = target.join(name);
        if entry.file_type().unwrap().is_dir() {
            copy_runtime_fixture(&entry.path(), &destination);
        } else if entry.path().extension().is_some_and(|extension| extension == "py" || extension == "toml") {
            std::fs::copy(entry.path(), destination).unwrap();
        }
    }
}

#[tokio::test]
async fn issue44_untrusted_pending_build_never_enrols_a_current_legacy_environment() {
    use std::os::unix::fs::PermissionsExt;
    let _serial = ENV_LOCK.lock().await;
    invalidate_runtime_probe_cache();
    let root = tempfile::tempdir().unwrap();
    let package = runtime_fixture(root.path());
    let (bin, log) = fake_uv(root.path());
    let exact = root.path().join("explicit-kernel-venv");
    let _env = explicit_env(&bin, &package, &log, &exact, None);
    ensure_kernel_python(EnsureKernelPythonOptions::default()).await.unwrap();
    let ownership = exact.with_file_name("explicit-kernel-venv.prime-agent-managed.json");
    std::fs::remove_file(&ownership).unwrap();
    let original_version = std::fs::read(exact.join(".bootstrap-version")).unwrap();
    let original_log = std::fs::read(&log).unwrap();
    let leases_before = std::fs::read_dir(exact.join(".leases")).unwrap().count();
    for (mode, binding) in [(0o666, "valid"), (0o664, "valid"), (0o644, "valid"), (0o600, "legacy"), (0o600, "wrong-parent")] {
        write_build_record(&exact, &exact, dead_pid());
        let pending = build_record_path(&exact);
        if binding != "valid" {
            let mut record: serde_json::Value = serde_json::from_slice(&std::fs::read(&pending).unwrap()).unwrap();
            if binding == "legacy" {
                record["version"] = serde_json::json!(2);
                record.as_object_mut().unwrap().remove("path");
                record.as_object_mut().unwrap().remove("parent_identity");
            } else {
                record["parent_identity"]["file"] = serde_json::json!(0);
            }
            std::fs::write(&pending, serde_json::to_vec(&record).unwrap()).unwrap();
        }
        std::fs::set_permissions(&pending, std::fs::Permissions::from_mode(mode)).unwrap();
        let before = std::fs::read(&pending).unwrap();
        let mtime = std::fs::metadata(&pending).unwrap().modified().unwrap();
        let _result = ensure_kernel_python(EnsureKernelPythonOptions::default()).await;
        assert!(!ownership.exists(), "untrusted pending record must not enrol a legacy root (mode {mode:o})");
        assert_eq!(std::fs::read(&pending).unwrap(), before);
        assert_eq!(std::fs::metadata(&pending).unwrap().modified().unwrap(), mtime);
        assert_eq!(std::fs::read(exact.join(".bootstrap-version")).unwrap(), original_version);
        assert_eq!(std::fs::read_dir(exact.join(".leases")).unwrap().count(), leases_before);
        assert_eq!(std::fs::read(&log).unwrap(), original_log);
    }
}
