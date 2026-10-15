//! Regression test for the A7 defect (found by real-agent dogfood on
//! 2026-09-22, see `AGENTS.md` and `docs/coherence.md`'s "World observation"
//! section): `initialize_internal_repository` used to force-track ignored
//! build output that happened to exist at snapshot time. A `checks.verify`
//! command that regenerates such an artifact then put it into Δ, and applying
//! Δ wrote bytecode into the user's source tree.
//!
//! `Fixture` is modeled on `tests/auto_apply_concurrency.rs`'s `Fixture`: a
//! Git source, a `dispatch.yml` whose `harnesses.codex.executable` points at
//! a shell script "agent", and FIFO gates so each agent blocks until the test
//! releases it. Here the source also carries a `.gitignore`'d `build/out.bin`
//! and a `checks.verify` script that rewrites it on every check run, and the
//! two agents (like `two_sessions_disjoint_files_both_auto_apply`) edit
//! disjoint files from the same S0 so both land.
#![cfg(unix)]

use anyhow::{Context, Result};
use serde_json::Value;
use std::{
    ffi::CString,
    fs,
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

fn executable(path: &Path, text: &str) -> Result<()> {
    fs::write(path, text)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

fn mkfifo(path: &Path) -> Result<()> {
    let fifo = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: `fifo` is a valid, NUL-terminated fixture path.
    anyhow::ensure!(
        unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) } == 0,
        "mkfifo failed"
    );
    Ok(())
}

fn git(dir: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
        ])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn wait_until(mut ready: impl FnMut() -> Result<bool>) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(300);
    while !ready()? {
        anyhow::ensure!(
            Instant::now() < deadline,
            "fixture did not reach its durable boundary"
        );
        thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

/// Kills and reaps the process it owns, even when an assertion panics.
struct OwnedChild(std::process::Child);
impl OwnedChild {
    fn output(mut self) -> Result<Output> {
        use std::io::Read;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        self.0.stdout.take().unwrap().read_to_end(&mut stdout)?;
        self.0.stderr.take().unwrap().read_to_end(&mut stderr)?;
        Ok(Output {
            status: self.0.wait()?,
            stdout,
            stderr,
        })
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A Git source that already has a `.gitignore`'d `build/out.bin`, a
/// `checks.verify` script that rewrites it on every run, and a `codex` agent
/// parametrized by its task text to edit `a.txt` or create `b.txt`.
struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    source: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().to_owned();
        let source = root.join("source");
        let state = root.join("state");
        fs::create_dir_all(&source)?;
        fs::create_dir(&state)?;
        fs::write(source.join(".gitignore"), "build/\n")?;
        fs::create_dir_all(source.join("build"))?;
        fs::write(source.join("build/out.bin"), b"original build output\n")?;
        fs::write(source.join("a.txt"), "baseline a\n")?;

        for role in ["a", "b"] {
            mkfifo(&root.join(format!("gate-{role}")))?;
        }

        let agent = root.join("codex");
        executable(&agent, &agent_script(&root))?;

        let verify = root.join("verify-rebuild.sh");
        executable(&verify, REBUILD_SCRIPT)?;

        fs::write(
            source.join("dispatch.yml"),
            format!(
                "execution:\n  timeout_secs: 600\nchecks:\n  verify: ['{}']\nharnesses:\n  codex:\n    executable: '{}'\n",
                verify.display(),
                agent.display()
            ),
        )?;
        git(&source, &["init", "--quiet"])?;
        git(&source, &["add", "-A"])?;
        git(&source, &["commit", "--quiet", "-m", "initial"])?;
        Ok(Self {
            _temp: temp,
            root,
            source,
            state,
        })
    }

    fn command(&self) -> Command {
        let mut command = Command::new(assert_cmd::cargo_bin!("dispatch"));
        command.arg("--state-dir").arg(&self.state);
        command
    }

    /// Starts `dispatch run --harnesses codex --allow-unsafe-local
    /// --auto-apply --json` with the given task text (which must carry
    /// exactly one `ROLE_*` marker the agent script recognizes).
    fn spawn(&self, task: &str) -> Result<OwnedChild> {
        Ok(OwnedChild(
            self.command()
                .arg("run")
                .arg(&self.source)
                .args([
                    "--task",
                    task,
                    "--allow-unsafe-local",
                    "--auto-apply",
                    "--json",
                    "--agent",
                    "codex",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?,
        ))
    }

    /// Waits for the agent slot `role` (`a` or `b`) to reach its gate.
    fn wait_started(&self, role: &str, child: &mut OwnedChild) -> Result<()> {
        let marker = self.root.join(format!("started-{role}"));
        wait_until(|| Ok(marker.exists() || child.0.try_wait()?.is_some()))?;
        anyhow::ensure!(marker.exists(), "agent {role} never started");
        Ok(())
    }

    fn release(&self, role: &str) -> Result<()> {
        fs::write(self.root.join(format!("gate-{role}")), "go\n")?;
        Ok(())
    }

    fn result(output: &Output) -> Result<Value> {
        serde_json::from_slice(&output.stdout).with_context(|| {
            format!(
                "stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        })
    }

    fn stderr(output: &Output) -> String {
        String::from_utf8_lossy(&output.stderr).into_owned()
    }
}

/// A `codex` fixture script parametrized by its task text: `ROLE_EDIT_A`
/// blocks on gate `a` then edits `a.txt`; `ROLE_CREATE_B` blocks on gate `b`
/// then creates `b.txt`. Disjoint files, same shape as
/// `auto_apply_concurrency.rs`'s `two_sessions_disjoint_files_both_auto_apply`.
fn agent_script(root: &Path) -> String {
    format!(
        r#"#!/bin/sh
if [ "$1" = '--version' ]; then echo 'codex fixture'; exit 0; fi
prompt=''
for arg in "$@"; do prompt="$arg"; done
root='{root}'
case "$prompt" in
  *ROLE_EDIT_A*) role=a ;;
  *ROLE_CREATE_B*) role=b ;;
  *) echo "fixture: unrecognized role in prompt" >&2; exit 1 ;;
esac
: > "$root/started-$role"
read _line < "$root/gate-$role"
case "$prompt" in
  *ROLE_EDIT_A*) printf 'baseline a\nedited by A\n' > a.txt ;;
  *ROLE_CREATE_B*) printf 'b\n' > b.txt ;;
esac
printf '{{"type":"result","model":"fixture"}}\n'
"#,
        root = root.display()
    )
}

/// The shared `checks.verify` command: it simulates a build step that
/// regenerates a `.gitignore`'d artifact on every check run (post-execution
/// candidate verification, and the merged-tree L2 integration check for
/// whichever run lands second), whether or not `build/` already exists in
/// the tree it runs against.
const REBUILD_SCRIPT: &str = r#"#!/bin/sh
mkdir -p build
head -c 64 /dev/urandom > build/out.bin
exit 0
"#;

#[test]
fn ignored_build_artifact_never_enters_the_delta_or_the_source() -> Result<()> {
    let f = Fixture::new()?;
    let original_build_output = fs::read(f.source.join("build/out.bin"))?;

    let mut a = f.spawn("ROLE_EDIT_A: give a.txt a companion edit")?;
    let mut b = f.spawn("ROLE_CREATE_B: create b.txt")?;
    f.wait_started("a", &mut a)?;
    f.wait_started("b", &mut b)?;

    f.release("a")?;
    let output_a = a.output()?;
    assert!(output_a.status.success(), "{}", Fixture::stderr(&output_a));
    let result_a = Fixture::result(&output_a)?;
    assert_eq!(result_a["auto_apply"]["outcome"], "applied", "{result_a}");

    f.release("b")?;
    let output_b = b.output()?;
    assert!(output_b.status.success(), "{}", Fixture::stderr(&output_b));
    let result_b = Fixture::result(&output_b)?;
    assert_eq!(result_b["auto_apply"]["outcome"], "applied", "{result_b}");
    assert_eq!(
        result_b["auto_apply"]["coherence"]["analysis"], "integration",
        "{result_b}"
    );

    assert_eq!(
        fs::read_to_string(f.source.join("a.txt"))?,
        "baseline a\nedited by A\n"
    );
    assert_eq!(fs::read_to_string(f.source.join("b.txt"))?, "b\n");

    let final_build_output = fs::read(f.source.join("build/out.bin"))?;
    assert_eq!(
        final_build_output, original_build_output,
        "the .gitignore'd build artifact regenerated by checks.verify must never be \
         written into the source; Dispatch only applies Δ, and the artifact was never \
         tracked into the baseline, so it can never be part of Δ"
    );
    Ok(())
}
