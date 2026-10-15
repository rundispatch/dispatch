//! Two real `dispatch run --auto-apply` processes finishing against one
//! source (see `docs/plan-0.3-auto-apply-and-attach.md`, parts 5.2, 5.3 and
//! the validation matrix in part 9): one lands, the other is re-judged
//! against the new world, at most one apply per run, the source is never
//! half-patched, and the fence retry works.
//!
//! `Fixture` is modeled on `tests/coherence_recovery.rs`'s `Fixture`: a Git
//! source, a `dispatch.yml` whose `harnesses.codex.executable` points at a
//! shell script "agent", and FIFO gates so each agent blocks until the test
//! releases it. Unlike that fixture, this one needs two independent agents
//! per scenario, so the one `codex` script is parametrized by the task text
//! (the last argument, per `harness::build_prompt`) to choose which files it
//! edits and which gate it waits on.
#![cfg(unix)]

use anyhow::{Context, Result};
use dispatch::state::State;
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

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
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

/// Every non-`.git` file of a directory (relative name, bytes), for exact
/// byte-for-byte comparison of the source tree.
fn tree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.file_name().is_none_or(|name| name != ".git"))
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                out.push((
                    path.strip_prefix(root).unwrap().to_owned(),
                    fs::read(&path).unwrap(),
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
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

/// Kills a fixture process that outlived its supervisor (blocked on a FIFO
/// with nothing else to stop it).
struct OrphanGuard(Option<i32>);
impl Drop for OrphanGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            // SAFETY: the pid is a fixture process recorded in a `*.pid` file.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

/// What the shared `checks.verify` command does.
enum Verify {
    /// Always passes immediately; used by scenarios that do not need L2 to
    /// take any observable time.
    Trivial,
    /// Passes, but when run against a merged tree that already contains A's
    /// landed edit (detected by grepping `a.txt`) it first announces itself
    /// via a marker file and sleeps 2s, giving the test a window to edit the
    /// source while the check is in flight.
    GrepSleepOnMarkerA,
    /// Announces itself via a marker file, then blocks forever on a FIFO the
    /// test never releases (used to hold the owner alive mid-check so it can
    /// be killed).
    BlockOnFifo,
}

/// A Git source with a `codex` agent that is parametrized by its task text
/// (`ROLE_*` markers) to select which file it edits/creates and which named
/// FIFO gate (`a`, `b`, `q`) it blocks on before writing.
struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    source: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new(verify: Verify) -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().to_owned();
        let source = root.join("source");
        let state = root.join("state");
        fs::create_dir_all(&source)?;
        fs::create_dir(&state)?;
        fs::write(source.join("a.txt"), "baseline a\n")?;
        fs::write(source.join("c.txt"), "baseline c\n")?;

        for role in ["a", "b", "q"] {
            mkfifo(&root.join(format!("gate-{role}")))?;
        }

        let agent = root.join("codex");
        executable(&agent, &agent_script(&root))?;

        let verify_command = match verify {
            Verify::Trivial => "true".to_owned(),
            Verify::GrepSleepOnMarkerA => {
                let script = root.join("verify-grep-sleep.sh");
                executable(&script, &grep_sleep_script(&root))?;
                script.display().to_string()
            }
            Verify::BlockOnFifo => {
                mkfifo(&root.join("check-gate"))?;
                let script = root.join("verify-block.sh");
                executable(&script, &block_script(&root))?;
                script.display().to_string()
            }
        };

        fs::write(
            source.join("dispatch.yml"),
            format!(
                "execution:\n  timeout_secs: 600\nchecks:\n  verify: ['{verify_command}']\nharnesses:\n  codex:\n    executable: '{}'\n",
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

    /// Waits for the agent slot `role` (`a`, `b` or `q`) to reach its gate,
    /// then returns its pid.
    fn wait_started(&self, role: &str, child: &mut OwnedChild) -> Result<i32> {
        let marker = self.root.join(format!("started-{role}"));
        wait_until(|| Ok(marker.exists() || child.0.try_wait()?.is_some()))?;
        anyhow::ensure!(marker.exists(), "agent {role} never started");
        Ok(
            fs::read_to_string(self.root.join(format!("agent-{role}.pid")))?
                .trim()
                .parse()?,
        )
    }

    fn release(&self, role: &str) -> Result<()> {
        fs::write(self.root.join(format!("gate-{role}")), "go\n")?;
        Ok(())
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.state.join("dispatch.db")).unwrap()
    }

    fn event_count(&self, run_id: &str, kind: &str) -> i64 {
        self.db()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE run_id = ?1 AND event_type = ?2",
                rusqlite::params![run_id, kind],
                |row| row.get(0),
            )
            .unwrap_or(0)
    }

    /// The `timestamp` of the latest event of `kind` on `run_id`.
    fn event_timestamp(&self, run_id: &str, kind: &str) -> String {
        self.db()
            .query_row(
                "SELECT timestamp FROM events WHERE run_id = ?1 AND event_type = ?2 \
                 ORDER BY sequence DESC LIMIT 1",
                rusqlite::params![run_id, kind],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn only_run_id(&self) -> Result<String> {
        Ok(self
            .db()
            .query_row("SELECT id FROM runs", [], |row| row.get(0))?)
    }

    fn loaded(&self, id: &str) -> Result<dispatch::RunRecord> {
        State::discover(Some(self.state.clone()))?.load_run(id)
    }

    fn metadata(&self, run_id: &str) -> Value {
        let path = self.state.join("runs").join(run_id).join("metadata.json");
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
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

/// One `codex` fixture script for every scenario: the last argument is the
/// built prompt (preamble + task), and the task text carries exactly one
/// `ROLE_*` marker that selects both the gate it blocks on and the file
/// operation it performs once released.
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
  *ROLE_CONFLICT_A*) role=a ;;
  *ROLE_CONFLICT_B*) role=b ;;
  *ROLE_SAME_A*) role=a ;;
  *ROLE_SAME_B*) role=b ;;
  *ROLE_QUICK*) role=q ;;
  *) echo "fixture: unrecognized role in prompt" >&2; exit 1 ;;
esac
printf '%s\n' "$$" > "$root/agent-$role.pid"
: > "$root/started-$role"
read _line < "$root/gate-$role"
case "$prompt" in
  *ROLE_EDIT_A*) printf 'baseline a\nedited by A\n' > a.txt ;;
  *ROLE_CREATE_B*) printf 'b\n' > b.txt ;;
  *ROLE_CONFLICT_A*) printf 'from A\n' > shared.txt ;;
  *ROLE_CONFLICT_B*) printf 'from B\n' > shared.txt ;;
  *ROLE_SAME_A*) printf 'same content\n' > shared.txt ;;
  *ROLE_SAME_B*) printf 'same content\n' > shared.txt ;;
  *ROLE_QUICK*) printf 'delivered\n' > delivered.txt ;;
esac
: > "$root/finished-$role"
printf '{{"type":"result","model":"fixture"}}\n'
"#,
        root = root.display()
    )
}

/// The shared `checks.verify` command for
/// `edit_during_integration_checks_is_fenced_and_retried_once`: it always
/// passes, but only when run against a tree that carries *both* A's landed
/// edit to `a.txt` and B's own `b.txt` does it first announce itself and
/// sleep. That combination is unique to the coherence oracle's merged-tree
/// scratch copy for B (current source, which already has A's edit, plus B's
/// own patch applied) — A's own candidate workspace has the edited `a.txt`
/// but no `b.txt`, and B's own candidate workspace has `b.txt` but the
/// unmodified baseline `a.txt`.
fn grep_sleep_script(root: &Path) -> String {
    format!(
        r#"#!/bin/sh
if grep -q 'edited by A' a.txt 2>/dev/null && test -f b.txt; then
  : > '{root}/b-check-marker'
  sleep 2
fi
exit 0
"#,
        root = root.display()
    )
}

/// The `checks.verify` command for `owner_killed_after_ready_never_applies_later`.
/// The same `checks.verify` list is also run as a pre-flight baseline check
/// against a pristine copy of the source before the harness ever runs, so
/// this only announces itself and blocks on a FIFO the test never releases
/// when it can see `delivered.txt` (written by the agent into its own
/// candidate workspace): the real, post-harness candidate verification. The
/// baseline check, which never sees that file, passes immediately so the
/// harness gets to run at all.
fn block_script(root: &Path) -> String {
    format!(
        r#"#!/bin/sh
if [ -f delivered.txt ]; then
  printf '%s\n' "$$" > '{root}/check.pid'
  : > '{root}/check-started'
  read _line < '{root}/check-gate'
fi
exit 0
"#,
        root = root.display()
    )
}

#[test]
fn two_sessions_disjoint_files_both_auto_apply() -> Result<()> {
    let f = Fixture::new(Verify::Trivial)?;
    let mut a = f.spawn("ROLE_EDIT_A: give a.txt a companion edit")?;
    let mut b = f.spawn("ROLE_CREATE_B: create b.txt")?;
    f.wait_started("a", &mut a)?;
    f.wait_started("b", &mut b)?;

    f.release("a")?;
    let output_a = a.output()?;
    assert!(output_a.status.success(), "{}", Fixture::stderr(&output_a));
    let result_a = Fixture::result(&output_a)?;
    let run_a = result_a["run_id"].as_str().unwrap().to_owned();
    assert_eq!(result_a["auto_apply"]["outcome"], "applied", "{result_a}");
    assert!(
        result_a["auto_apply"].get("coherence").is_none(),
        "an unmoved world carries no coherence summary: {result_a}"
    );

    f.release("b")?;
    let output_b = b.output()?;
    assert!(output_b.status.success(), "{}", Fixture::stderr(&output_b));
    let result_b = Fixture::result(&output_b)?;
    let run_b = result_b["run_id"].as_str().unwrap().to_owned();
    assert_eq!(result_b["auto_apply"]["outcome"], "applied", "{result_b}");
    assert_eq!(
        result_b["auto_apply"]["coherence"]["analysis"], "integration",
        "{result_b}"
    );
    assert_eq!(
        result_b["auto_apply"]["coherence"]["changed_files"]
            .as_u64()
            .unwrap(),
        1
    );

    assert_eq!(
        fs::read_to_string(f.source.join("a.txt"))?,
        "baseline a\nedited by A\n"
    );
    assert_eq!(fs::read_to_string(f.source.join("b.txt"))?, "b\n");
    assert_eq!(f.event_count(&run_a, "result.applied"), 1);
    assert_eq!(f.event_count(&run_b, "result.applied"), 1);
    Ok(())
}

#[test]
fn two_sessions_conflicting_hunks_second_is_blocked() -> Result<()> {
    let f = Fixture::new(Verify::Trivial)?;
    let mut a = f.spawn("ROLE_CONFLICT_A: create shared.txt with A's content")?;
    let mut b = f.spawn("ROLE_CONFLICT_B: create shared.txt with B's content")?;
    f.wait_started("a", &mut a)?;
    f.wait_started("b", &mut b)?;

    f.release("a")?;
    let output_a = a.output()?;
    assert!(output_a.status.success(), "{}", Fixture::stderr(&output_a));
    assert_eq!(
        Fixture::result(&output_a)?["auto_apply"]["outcome"],
        "applied"
    );
    let source_after_a = tree(&f.source);

    f.release("b")?;
    let output_b = b.output()?;
    assert_eq!(
        output_b.status.code(),
        Some(6),
        "{}",
        Fixture::stderr(&output_b)
    );
    let result_b = Fixture::result(&output_b)?;
    let run_b = result_b["run_id"].as_str().unwrap().to_owned();
    assert_eq!(result_b["auto_apply"]["outcome"], "blocked", "{result_b}");
    assert_eq!(result_b["auto_apply"]["reason"], "verdict_refresh");
    assert_eq!(result_b["auto_apply"]["coherence"]["decision"], "refresh");
    assert_eq!(
        result_b["auto_apply"]["coherence"]["reasons"][0]["code"],
        "patch_conflict"
    );

    assert_eq!(
        tree(&f.source),
        source_after_a,
        "B must not touch the source"
    );
    let metadata = f.metadata(&run_b);
    assert_eq!(
        metadata["outcome"]["application"],
        "blocked_by_source_drift"
    );
    assert_eq!(metadata["outcome"]["review"], "pending");
    let run = f.loaded(&run_b)?;
    assert!(dispatch::coherence::is_ready_unapplied(&run));

    let check = f.command().args(["check", &run_b]).output()?;
    let check_stdout = String::from_utf8_lossy(&check.stdout).into_owned();
    assert!(
        check_stdout.contains("Coherence: REFRESH"),
        "{check_stdout}"
    );
    Ok(())
}

#[test]
fn two_sessions_identical_patch_second_is_stopped() -> Result<()> {
    let f = Fixture::new(Verify::Trivial)?;
    let mut a = f.spawn("ROLE_SAME_A: create shared.txt with identical content")?;
    let mut b = f.spawn("ROLE_SAME_B: create shared.txt with identical content")?;
    f.wait_started("a", &mut a)?;
    f.wait_started("b", &mut b)?;

    f.release("a")?;
    let output_a = a.output()?;
    assert!(output_a.status.success(), "{}", Fixture::stderr(&output_a));
    let source_after_a = tree(&f.source);

    f.release("b")?;
    let output_b = b.output()?;
    assert_eq!(
        output_b.status.code(),
        Some(6),
        "{}",
        Fixture::stderr(&output_b)
    );
    let result_b = Fixture::result(&output_b)?;
    assert_eq!(result_b["auto_apply"]["outcome"], "blocked", "{result_b}");
    assert_eq!(result_b["auto_apply"]["reason"], "verdict_stop");
    assert_eq!(result_b["auto_apply"]["coherence"]["decision"], "stop");
    assert_eq!(
        result_b["auto_apply"]["coherence"]["reasons"][0]["code"],
        "already_applied"
    );

    assert_eq!(
        tree(&f.source),
        source_after_a,
        "B must not touch the source"
    );
    Ok(())
}

#[test]
fn simultaneous_finish_serializes_on_the_source_lock() -> Result<()> {
    let f = Fixture::new(Verify::Trivial)?;
    let mut a = f.spawn("ROLE_EDIT_A: give a.txt a companion edit")?;
    let mut b = f.spawn("ROLE_CREATE_B: create b.txt")?;
    f.wait_started("a", &mut a)?;
    f.wait_started("b", &mut b)?;

    // Release both at the same moment: whichever wins the source lock is not
    // determined by this test.
    let gate_a = f.root.join("gate-a");
    let gate_b = f.root.join("gate-b");
    let release_a = thread::spawn(move || fs::write(gate_a, "go\n"));
    let release_b = thread::spawn(move || fs::write(gate_b, "go\n"));
    release_a.join().unwrap()?;
    release_b.join().unwrap()?;

    let output_a = a.output()?;
    let output_b = b.output()?;
    assert!(output_a.status.success(), "{}", Fixture::stderr(&output_a));
    assert!(output_b.status.success(), "{}", Fixture::stderr(&output_b));
    let result_a = Fixture::result(&output_a)?;
    let result_b = Fixture::result(&output_b)?;
    let run_a = result_a["run_id"].as_str().unwrap().to_owned();
    let run_b = result_b["run_id"].as_str().unwrap().to_owned();

    assert_eq!(result_a["auto_apply"]["outcome"], "applied", "{result_a}");
    assert_eq!(result_b["auto_apply"]["outcome"], "applied", "{result_b}");
    assert_eq!(f.event_count(&run_a, "result.applied"), 1);
    assert_eq!(f.event_count(&run_b, "result.applied"), 1);
    assert_eq!(f.event_count(&run_a, "application.failed"), 0);
    assert_eq!(f.event_count(&run_b, "application.failed"), 0);

    assert_eq!(
        fs::read_to_string(f.source.join("a.txt"))?,
        "baseline a\nedited by A\n"
    );
    assert_eq!(fs::read_to_string(f.source.join("b.txt"))?, "b\n");

    let a_has_coherence = result_a["auto_apply"].get("coherence").is_some();
    let b_has_coherence = result_b["auto_apply"].get("coherence").is_some();
    assert_ne!(
        a_has_coherence, b_has_coherence,
        "exactly one world was unmoved when its run applied: a={result_a} b={result_b}"
    );

    let (mover_id, follower_id, follower_result) = if a_has_coherence {
        (run_b.clone(), run_a.clone(), &result_a)
    } else {
        (run_a.clone(), run_b.clone(), &result_b)
    };
    assert_eq!(
        follower_result["auto_apply"]["coherence"]["analysis"], "integration",
        "{follower_result}"
    );
    assert!(
        follower_result["auto_apply"]["coherence"]["changed_files"]
            .as_u64()
            .unwrap()
            >= 1
    );

    let mover_ts = f.event_timestamp(&mover_id, "result.applied");
    let follower_ts = f.event_timestamp(&follower_id, "result.applied");
    let mover_dt = chrono::DateTime::parse_from_rfc3339(&mover_ts)?;
    let follower_dt = chrono::DateTime::parse_from_rfc3339(&follower_ts)?;
    assert!(
        follower_dt > mover_dt,
        "the follower ({follower_id}, {follower_ts}) must apply after the mover \
         ({mover_id}, {mover_ts})"
    );
    Ok(())
}

#[test]
fn edit_during_integration_checks_is_fenced_and_retried_once() -> Result<()> {
    let f = Fixture::new(Verify::GrepSleepOnMarkerA)?;

    // Both start from the same S0, like the disjoint-files scenario, so that
    // A's edit is a genuine world change from B's point of view once it
    // lands (not something already baked into B's own baseline).
    let mut a = f.spawn("ROLE_EDIT_A: give a.txt a companion edit")?;
    let mut b = f.spawn("ROLE_CREATE_B: create b.txt")?;
    f.wait_started("a", &mut a)?;
    f.wait_started("b", &mut b)?;

    // A lands first.
    f.release("a")?;
    let output_a = a.output()?;
    assert!(output_a.status.success(), "{}", Fixture::stderr(&output_a));

    f.release("b")?;

    // While B's L2 integration check runs against the merged tree (which
    // already carries A's edit), edit an unrelated third file exactly once.
    let marker = f.root.join("b-check-marker");
    wait_until(|| Ok(marker.exists()))?;
    fs::write(f.source.join("c.txt"), "edited during the check\n")?;

    let output_b = b.output()?;
    assert!(output_b.status.success(), "{}", Fixture::stderr(&output_b));
    let result_b = Fixture::result(&output_b)?;
    let run_b = result_b["run_id"].as_str().unwrap().to_owned();
    assert_eq!(result_b["auto_apply"]["outcome"], "applied", "{result_b}");
    assert_eq!(
        result_b["auto_apply"]["coherence"]["analysis"], "integration",
        "{result_b}"
    );
    assert_eq!(
        result_b["auto_apply"]["coherence"]["changed_files"]
            .as_u64()
            .unwrap(),
        2
    );

    let checks_dir = f.state.join("runs").join(&run_b).join("coherence-checks");
    let entries: Vec<_> = fs::read_dir(&checks_dir)?.collect::<std::io::Result<_>>()?;
    assert_eq!(
        entries.len(),
        2,
        "the fence must have forced exactly one retry (one gate() call per check directory)"
    );

    assert_eq!(fs::read_to_string(f.source.join("b.txt"))?, "b\n");
    Ok(())
}

#[test]
fn owner_killed_after_ready_never_applies_later() -> Result<()> {
    let f = Fixture::new(Verify::BlockOnFifo)?;
    let mut child = f.spawn("ROLE_QUICK: deliver something quickly")?;
    f.wait_started("q", &mut child)?;
    // Let the agent itself finish; the candidate's own verify check now
    // blocks, holding the owner process alive after the candidate workspace
    // exists.
    f.release("q")?;

    let check_started = f.root.join("check-started");
    wait_until(|| Ok(check_started.exists() || child.0.try_wait()?.is_some()))?;
    anyhow::ensure!(check_started.exists(), "verify check never started");
    let check_pid: i32 = fs::read_to_string(f.root.join("check.pid"))?
        .trim()
        .parse()?;
    let mut orphan = OrphanGuard(Some(check_pid));
    assert!(alive(check_pid));

    let run_id = f.only_run_id()?;
    let source_before = tree(&f.source);

    // kill -9 the owning `dispatch run`; the blocked check knows nothing of it.
    child.0.kill()?;
    child.0.wait()?;
    assert!(
        alive(check_pid),
        "kill -9 of the owner leaves the blocked check running"
    );

    let status = f.command().args(["status", &run_id]).output()?;
    assert!(
        !Fixture::stderr(&status).contains("panicked"),
        "{}",
        Fixture::stderr(&status)
    );

    assert_eq!(f.event_count(&run_id, "result.applied"), 0);
    assert_eq!(tree(&f.source), source_before, "no partial writes");
    assert!(f.loaded(&run_id).is_ok());

    // Cleanup: the orphaned check process would otherwise block forever.
    unsafe { libc::kill(check_pid, libc::SIGKILL) };
    orphan.0 = None;
    wait_until(|| Ok(!alive(check_pid)))?;
    Ok(())
}
