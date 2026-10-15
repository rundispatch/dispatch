//! End-to-end scenario tests for attached external work and `dispatch serve`
//! (work package S6, `docs/plan-0.3-auto-apply-and-attach.md` parts 6.1-6.11,
//! 9 and 14). Every scenario below names its match in part 9's validation
//! matrix / part 6's named scenarios in its doc comment.
//!
//! One Git repository (`root`) is shared by the fixture: `src/lib.rs`,
//! `src/other.rs`, a `.gitignore`'d `build/out.bin` that already exists at
//! commit time (never tracked), a `checks.verify` script that regenerates it
//! with random bytes on every run (so it must never enter Δ or the source;
//! see `tests/ignored_artifacts.rs`), `coherence.poll_secs: 1`, and a
//! `harnesses.codex.executable` fixture agent for native runs, FIFO-gated as
//! in `tests/auto_apply_concurrency.rs`. Wrapped attach uses a second,
//! file-gated fixture "external agent" script, as in `tests/attach_wrapped.rs`.
//! Every worktree is a real `git worktree add`; every process is the real
//! `dispatch` binary; every wait is bounded (30s outer, 5s where the plan
//! says so) and gated on FIFOs, marker files or DB polling, never a bare
//! sleep. Every spawned child is killed on drop or in explicit cleanup.
#![cfg(unix)]

use std::{
    collections::HashSet,
    ffi::CString,
    fs,
    io::{BufRead, BufReader},
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;

// ---------------------------------------------------------------------
// Shared fixture mechanics (modeled on tests/auto_apply_concurrency.rs,
// tests/attach_wrapped.rs and tests/serve.rs).
// ---------------------------------------------------------------------

fn executable(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn mkfifo(path: &Path) {
    let fifo = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: `fifo` is a valid, NUL-terminated fixture path.
    assert_eq!(
        unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) },
        0,
        "mkfifo failed"
    );
}

fn git(dir: &Path, args: &[&str]) -> String {
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
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks whether the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Poll `ready` every 50ms until it returns true or `timeout` elapses.
fn wait_for(timeout: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if ready() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    ready()
}

fn wait_until(timeout: Duration, ready: impl FnMut() -> bool, message: &str) {
    assert!(wait_for(timeout, ready), "{message}");
}

/// Assert `condition` never becomes true for the whole `window`: used to show
/// an absence (e.g. no invalidation) is not just a race that has not yet
/// resolved.
fn assert_never(window: Duration, mut condition: impl FnMut() -> bool, message: &str) {
    let deadline = Instant::now() + window;
    while Instant::now() < deadline {
        assert!(!condition(), "{message}");
        thread::sleep(Duration::from_millis(50));
    }
}

/// A spawned child process, killed and reaped on drop so a failing assertion
/// never leaves an orphaned `dispatch` process (or its agent) running.
struct OwnedChild(Child);

impl OwnedChild {
    fn output(mut self) -> Output {
        use std::io::Read;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        self.0
            .stdout
            .take()
            .unwrap()
            .read_to_end(&mut stdout)
            .unwrap();
        self.0
            .stderr
            .take()
            .unwrap()
            .read_to_end(&mut stderr)
            .unwrap();
        Output {
            status: self.0.wait().unwrap(),
            stdout,
            stderr,
        }
    }

    fn kill_and_wait(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Kills a fixture process that outlived its supervisor.
struct OrphanGuard(Option<i32>);
impl Drop for OrphanGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            // SAFETY: the pid is a fixture process this test recorded.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

fn result_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid JSON stdout: {error}; stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A background `dispatch serve` process: stdout lines are streamed to a
/// channel by a reader thread so the test can poll with a deadline instead of
/// blocking forever. Killed and reaped on drop, so a panicking assertion
/// never leaves a `serve` process (and its lock) behind.
struct ServeProcess {
    child: Child,
    lines: mpsc::Receiver<String>,
}

impl ServeProcess {
    fn spawn(fixture: &Fixture) -> Self {
        let mut command = Command::new(assert_cmd::cargo_bin!("dispatch"));
        command
            .arg("--state-dir")
            .arg(&fixture.state)
            .arg("serve")
            .arg("--root")
            .arg(&fixture.root)
            .arg("--json")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("failed to start dispatch serve");
        let stdout = child.stdout.take().expect("serve stdout is piped");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Self { child, lines: rx }
    }

    fn next_line(&self, timeout: Duration) -> Option<String> {
        self.lines.recv_timeout(timeout).ok()
    }

    /// The first line matching `predicate`, waiting up to `timeout` total.
    fn wait_for(
        &self,
        timeout: Duration,
        mut predicate: impl FnMut(&Value) -> bool,
    ) -> Option<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let line = self.next_line(remaining)?;
            if let Ok(value) = serde_json::from_str::<Value>(&line)
                && predicate(&value)
            {
                return Some(value);
            }
        }
    }

    fn kill_and_wait(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for ServeProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One `codex` fixture script for the native (dispatch-launched) side of
/// every scenario, parametrized by task text: `ROLE_LIB` edits `src/lib.rs`
/// to value 2; `ROLE_CONFLICT` edits `src/lib.rs` to value 30 (a conflicting
/// edit of the same line another party also edits); `ROLE_OTHER` edits
/// `src/other.rs` to value 2. Each role blocks on its own named FIFO
/// (`gate-<role>`) until the test releases it, after announcing itself via
/// `started-<role>`.
fn codex_agent_script(root: &Path) -> String {
    format!(
        r#"#!/bin/sh
if [ "$1" = '--version' ]; then echo 'codex fixture'; exit 0; fi
prompt=''
for arg in "$@"; do prompt="$arg"; done
root='{root}'
case "$prompt" in
  *ROLE_LIB*) role=lib ;;
  *ROLE_CONFLICT*) role=conflict ;;
  *ROLE_OTHER*) role=other ;;
  *) echo "fixture: unrecognized role in prompt" >&2; exit 1 ;;
esac
: > "$root/started-$role"
read _line < "$root/gate-$role"
case "$prompt" in
  *ROLE_LIB*) printf 'pub fn f() -> i32 {{\n    2\n}}\n' > src/lib.rs ;;
  *ROLE_CONFLICT*) printf 'pub fn f() -> i32 {{\n    30\n}}\n' > src/lib.rs ;;
  *ROLE_OTHER*) printf 'pub fn g() -> i32 {{\n    2\n}}\n' > src/other.rs ;;
esac
printf '{{"type":"result","model":"fixture"}}\n'
"#,
        root = root.display()
    )
}

/// The `checks.verify` script: regenerates the `.gitignore`'d `build/out.bin`
/// with random bytes on every run, whether or not `build/` already exists
/// (see `tests/ignored_artifacts.rs`'s `REBUILD_SCRIPT`).
const VERIFY_SCRIPT: &str = r#"#!/bin/sh
mkdir -p build
head -c 64 /dev/urandom > build/out.bin
exit 0
"#;

/// The external-agent fixture for wrapped attach: writes `$EDIT_CONTENT` to
/// `$EDIT_FILE` (relative to its cwd, the attached workspace), announces
/// itself via `$READY`, then optionally blocks on a `$GATE` marker file
/// (polled, as in `tests/attach_wrapped.rs`) before exiting with `$EXIT_CODE`
/// (default 0). Forwards SIGTERM as exit code 143, mirroring a real agent
/// terminating cleanly on the wrapper's forwarded signal.
const EXTERNAL_AGENT_SCRIPT: &str = r#"#!/bin/sh
trap 'exit 143' TERM
printf '%s' "$EDIT_CONTENT" > "$EDIT_FILE"
: > "$READY"
if [ -n "$GATE" ]; then
    while [ ! -f "$GATE" ]; do sleep 0.1; done
fi
exit "${EXIT_CODE:-0}"
"#;

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
    external_agent_script: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn f() -> i32 {\n    1\n}\n").unwrap();
        fs::write(root.join("src/other.rs"), "pub fn g() -> i32 {\n    1\n}\n").unwrap();
        fs::write(root.join(".gitignore"), "build/\n").unwrap();
        fs::create_dir_all(root.join("build")).unwrap();
        fs::write(root.join("build/out.bin"), b"original build output\n").unwrap();

        let verify = root.join("verify.sh");
        executable(&verify, VERIFY_SCRIPT);

        let codex_script = temp.path().join("codex");
        executable(&codex_script, &codex_agent_script(temp.path()));
        for role in ["lib", "conflict", "other"] {
            mkfifo(&temp.path().join(format!("gate-{role}")));
        }

        let external_agent_script = temp.path().join("external-agent.sh");
        executable(&external_agent_script, EXTERNAL_AGENT_SCRIPT);

        fs::write(
            root.join("dispatch.yml"),
            format!(
                "execution:\n  timeout_secs: 600\nchecks:\n  verify: ['sh verify.sh']\ncoherence:\n  poll_secs: 1\nharnesses:\n  codex:\n    executable: '{}'\n",
                codex_script.display()
            ),
        )
        .unwrap();

        git(&root, &["init", "--quiet"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "--quiet", "-m", "initial"]);

        let state = temp.path().join("state");
        Self {
            root: fs::canonicalize(&root).unwrap(),
            state,
            external_agent_script,
            _temp: temp,
        }
    }

    /// A new linked worktree of the root repository on its own branch.
    fn worktree(&self, name: &str) -> PathBuf {
        let path = self._temp.path().join(name);
        git(
            &self.root,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                &format!("{name}-branch"),
                path.to_str().unwrap(),
            ],
        );
        fs::canonicalize(&path).unwrap()
    }

    fn dispatch(&self, args: &[&str]) -> Output {
        Command::new(assert_cmd::cargo_bin!("dispatch"))
            .arg("--state-dir")
            .arg(&self.state)
            .args(args)
            .output()
            .unwrap()
    }

    fn dispatch_ok(&self, args: &[&str]) -> String {
        let output = self.dispatch(args);
        assert!(
            output.status.success(),
            "dispatch {args:?} failed: {}",
            stderr_of(&output)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// `dispatch attach --workspace <workspace> --root <root>
    /// --allow-unsafe-local <extra...>` (foreign form), returning the new
    /// run's ID.
    fn attach_foreign(&self, workspace: &Path, extra: &[&str]) -> String {
        let mut args = vec![
            "attach",
            "--workspace",
            workspace.to_str().unwrap(),
            "--root",
            self.root.to_str().unwrap(),
            "--allow-unsafe-local",
        ];
        args.extend_from_slice(extra);
        let stdout = self.dispatch_ok(&args);
        stdout
            .lines()
            .find_map(|line| line.strip_prefix("ATTACHED "))
            .expect("attach prints ATTACHED <id>")
            .to_owned()
    }

    fn finish(&self, id: &str) -> Output {
        self.dispatch(&["finish", id])
    }

    /// A `dispatch attach --workspace <workspace> --root <root>
    /// --allow-unsafe-local <extra...> -- sh <external_agent_script>`
    /// command (wrapped form), not yet spawned, stdio piped.
    fn attach_wrapped_command(&self, workspace: &Path, extra: &[&str]) -> Command {
        let mut command = Command::new(assert_cmd::cargo_bin!("dispatch"));
        command
            .arg("--state-dir")
            .arg(&self.state)
            .arg("attach")
            .arg("--workspace")
            .arg(workspace)
            .arg("--root")
            .arg(&self.root)
            .arg("--allow-unsafe-local")
            .args(extra)
            .arg("--")
            .arg("sh")
            .arg(&self.external_agent_script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// Starts `dispatch run --harnesses codex --allow-unsafe-local
    /// --auto-apply --json` with the given task text (which must carry
    /// exactly one `ROLE_*` marker `codex_agent_script` recognizes).
    fn spawn_native(&self, task: &str) -> OwnedChild {
        OwnedChild(
            Command::new(assert_cmd::cargo_bin!("dispatch"))
                .arg("--state-dir")
                .arg(&self.state)
                .arg("run")
                .arg(&self.root)
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
                .spawn()
                .unwrap(),
        )
    }

    /// Waits for the native agent slot `role` (`lib`, `conflict` or `other`)
    /// to reach its gate.
    fn wait_native_started(&self, role: &str, child: &mut OwnedChild) {
        let marker = self._temp.path().join(format!("started-{role}"));
        wait_until(
            Duration::from_secs(300),
            || marker.exists() || child.0.try_wait().unwrap().is_some(),
            &format!("native agent {role} never started"),
        );
        assert!(marker.exists(), "native agent {role} never started");
    }

    fn release_native(&self, role: &str) {
        fs::write(self._temp.path().join(format!("gate-{role}")), "go\n").unwrap();
    }

    fn metadata_path(&self, id: &str) -> PathBuf {
        self.state.join("runs").join(id).join("metadata.json")
    }

    fn metadata(&self, id: &str) -> Value {
        serde_json::from_slice(&fs::read(self.metadata_path(id)).unwrap()).unwrap()
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.state.join("dispatch.db")).unwrap()
    }

    fn event_count(&self, id: &str, kind: &str) -> i64 {
        self.db()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE run_id = ?1 AND event_type = ?2",
                rusqlite::params![id, kind],
                |row| row.get(0),
            )
            .unwrap_or(0)
    }

    fn event_timestamp(&self, id: &str, kind: &str) -> String {
        self.db()
            .query_row(
                "SELECT timestamp FROM events WHERE run_id = ?1 AND event_type = ?2 \
                 ORDER BY sequence DESC LIMIT 1",
                rusqlite::params![id, kind],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// Empty before the first `dispatch` invocation has created and migrated
    /// `dispatch.db` (the state directory itself does not exist yet).
    fn all_run_ids(&self) -> HashSet<String> {
        let Ok(db) = rusqlite::Connection::open(self.state.join("dispatch.db")) else {
            return HashSet::new();
        };
        let Ok(mut statement) = db.prepare("SELECT id FROM runs") else {
            return HashSet::new();
        };
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
    }

    /// Waits for exactly one run ID to appear beyond `before`, returning it.
    /// Used after spawning a wrapped attach, which (unlike the foreign form)
    /// prints nothing while the agent runs.
    fn wait_new_run_id(&self, before: &HashSet<String>) -> String {
        let mut found = None;
        wait_until(
            Duration::from_secs(300),
            || {
                let ids = self.all_run_ids();
                let mut diff = ids.difference(before);
                match (diff.next(), diff.next()) {
                    (Some(id), None) => {
                        found = Some(id.clone());
                        true
                    }
                    _ => false,
                }
            },
            "no new run id appeared",
        );
        found.unwrap()
    }
}

// ---------------------------------------------------------------------
// Scenario 1: ATTACHED CLAUDE (part 6, 6.1-6.9; matrix row "Wrapped attach
// exit -> finish -> verification -> auto-apply" plus the disjoint-file
// coexistence row).
// ---------------------------------------------------------------------

/// A wrapped attach (playing "Claude") edits `src/other.rs` and blocks on its
/// gate; concurrently a native `dispatch run --auto-apply` (a scripted
/// agent) edits the disjoint `src/lib.rs` and lands while the attached agent
/// is still running. Because the files are disjoint, the attached run must
/// never see `coherence.invalidated`; once released, it finishes `ready`, and
/// `dispatch check` on it reports CONTINUE with at least one changed file
/// (the native landing).
#[test]
fn attached_external_agent_stays_coherent_while_native_work_lands() {
    let f = Fixture::new();
    let workspace = f.worktree("wt1");
    let ready = f._temp.path().join("ready-1");
    let gate = f._temp.path().join("gate-attach-1");

    let before = f.all_run_ids();
    let wrapped = OwnedChild(
        f.attach_wrapped_command(&workspace, &[])
            .env("EDIT_FILE", "src/other.rs")
            .env("EDIT_CONTENT", "pub fn g() -> i32 {\n    2\n}\n")
            .env("READY", &ready)
            .env("GATE", &gate)
            .env("EXIT_CODE", "0")
            .spawn()
            .unwrap(),
    );
    wait_until(
        Duration::from_secs(300),
        || ready.exists(),
        "attached agent never reached its gate",
    );
    let attached_id = f.wait_new_run_id(&before);

    // Native work lands on a disjoint file while the attached agent is still
    // running (gate not yet released).
    let mut native = f.spawn_native("ROLE_LIB: give lib.rs a companion edit");
    f.wait_native_started("lib", &mut native);
    f.release_native("lib");
    let native_output = native.output();
    assert!(
        native_output.status.success(),
        "{}",
        stderr_of(&native_output)
    );
    let native_result = result_json(&native_output);
    assert_eq!(
        native_result["auto_apply"]["outcome"], "applied",
        "{native_result}"
    );
    assert_eq!(
        fs::read_to_string(f.root.join("src/lib.rs")).unwrap(),
        "pub fn f() -> i32 {\n    2\n}\n"
    );

    // Disjoint files: the attached run must not be invalidated by the
    // landing, even after giving the watcher a full window to notice it.
    assert_never(
        Duration::from_secs(3),
        || f.event_count(&attached_id, "coherence.invalidated") > 0,
        "a disjoint-file native landing must not invalidate the attached run",
    );

    fs::write(&gate, "go\n").unwrap();
    let output = wrapped.output();
    assert!(output.status.success(), "{}", stderr_of(&output));

    let metadata = f.metadata(&attached_id);
    assert_eq!(metadata["outcome"]["work_result"], "ready");
    assert_eq!(metadata["outcome"]["lifecycle"], "finished");

    let check = f.dispatch(&["check", &attached_id]);
    assert!(check.status.success(), "{}", stderr_of(&check));
    let stdout = String::from_utf8_lossy(&check.stdout);
    assert!(stdout.contains("Coherence: CONTINUE"), "{stdout}");
    let changed_files: u64 = stdout
        .lines()
        .find_map(|line| line.strip_prefix("World changed files: "))
        .and_then(|n| n.trim().parse().ok())
        .expect("check prints World changed files: N");
    assert!(changed_files >= 1, "{stdout}");
}

// ---------------------------------------------------------------------
// Scenario 2: matrix row "Wrapped attach fixture agent editing the
// worktree; another Work lands; verdict recorded for the attached Work"
// (coherence.invalidated on a fact break).
// ---------------------------------------------------------------------

/// As above, but both the wrapped agent and the native run edit the same
/// line of `src/lib.rs`. Once the native run lands, the attached run must be
/// invalidated; after it finishes, `dispatch check` must report
/// REFRESH with `patch_conflict`.
#[test]
fn attached_work_invalidated_by_native_landing() {
    let f = Fixture::new();
    let workspace = f.worktree("wt2");
    let ready = f._temp.path().join("ready-2");
    let gate = f._temp.path().join("gate-attach-2");

    let before = f.all_run_ids();
    let wrapped = OwnedChild(
        f.attach_wrapped_command(&workspace, &[])
            .env("EDIT_FILE", "src/lib.rs")
            .env("EDIT_CONTENT", "pub fn f() -> i32 {\n    20\n}\n")
            .env("READY", &ready)
            .env("GATE", &gate)
            .env("EXIT_CODE", "0")
            .spawn()
            .unwrap(),
    );
    wait_until(
        Duration::from_secs(300),
        || ready.exists(),
        "attached agent never reached its gate",
    );
    let attached_id = f.wait_new_run_id(&before);

    let mut native = f.spawn_native("ROLE_CONFLICT: give lib.rs a conflicting edit");
    f.wait_native_started("conflict", &mut native);
    f.release_native("conflict");
    let native_output = native.output();
    assert!(
        native_output.status.success(),
        "{}",
        stderr_of(&native_output)
    );
    assert_eq!(
        fs::read_to_string(f.root.join("src/lib.rs")).unwrap(),
        "pub fn f() -> i32 {\n    30\n}\n"
    );

    wait_until(
        Duration::from_secs(300),
        || f.event_count(&attached_id, "coherence.invalidated") > 0,
        "no coherence.invalidated event was recorded for the conflicting landing",
    );

    fs::write(&gate, "go\n").unwrap();
    let output = wrapped.output();
    assert!(output.status.success(), "{}", stderr_of(&output));

    let check = f.dispatch(&["check", &attached_id]);
    assert!(check.status.success(), "{}", stderr_of(&check));
    let stdout = String::from_utf8_lossy(&check.stdout);
    assert!(stdout.contains("Coherence: REFRESH"), "{stdout}");
    assert!(stdout.contains("patch_conflict"), "{stdout}");
}

// ---------------------------------------------------------------------
// Scenario 3: ATTACHED CODEX + DISPATCH-NATIVE WORK (part 6, matrix row
// "Native run and attached Work on one root, both auto-apply").
// ---------------------------------------------------------------------

/// A foreign-attached worktree (playing "Codex", edited directly by the
/// test) with `--auto-apply`, finished before anything else runs, and a
/// native `dispatch run --auto-apply` on a disjoint file, with `serve`
/// applying the foreign work. Both must end up applied with
/// `applied_by: auto_apply`, exactly one `result.applied` each, and the
/// later applier (deterministically the native run here, since it is held on
/// its gate until the foreign work is already applied) must see
/// `coherence.analysis: integration`. The root must carry both edits, and
/// the `.gitignore`'d `build/out.bin` must be byte-identical to what it was
/// before (checks regenerate their own copies inside candidate workspaces,
/// never the root's).
#[test]
fn native_and_attached_work_share_one_world() {
    let f = Fixture::new();
    let original_build_output = fs::read(f.root.join("build/out.bin")).unwrap();
    let workspace = f.worktree("wt3");

    let foreign_id = f.attach_foreign(&workspace, &["--auto-apply"]);
    fs::write(
        workspace.join("src/lib.rs"),
        "pub fn f() -> i32 {\n    2\n}\n",
    )
    .unwrap();
    let finish_output = f.finish(&foreign_id);
    assert!(
        finish_output.status.success(),
        "{}",
        stderr_of(&finish_output)
    );

    let mut native = f.spawn_native("ROLE_OTHER: create a companion edit to other.rs");
    f.wait_native_started("other", &mut native);

    let mut serve = ServeProcess::spawn(&f);
    serve.wait_for(Duration::from_secs(300), |value| value["type"] == "world");

    wait_until(
        Duration::from_secs(300),
        || {
            let metadata = f.metadata(&foreign_id);
            metadata["outcome"]["application"] == "applied"
                && metadata["outcome"]["applied_by"] == "auto_apply"
        },
        "serve never applied the finished, integrate-capable foreign work",
    );

    f.release_native("other");
    let native_output = native.output();
    assert!(
        native_output.status.success(),
        "{}",
        stderr_of(&native_output)
    );
    let native_result = result_json(&native_output);
    let native_id = native_result["run_id"].as_str().unwrap().to_owned();
    assert_eq!(
        native_result["auto_apply"]["outcome"], "applied",
        "{native_result}"
    );
    assert_eq!(
        native_result["auto_apply"]["coherence"]["analysis"], "integration",
        "{native_result}"
    );

    assert_eq!(f.event_count(&foreign_id, "result.applied"), 1);
    assert_eq!(f.event_count(&native_id, "result.applied"), 1);

    assert_eq!(
        fs::read_to_string(f.root.join("src/lib.rs")).unwrap(),
        "pub fn f() -> i32 {\n    2\n}\n"
    );
    assert_eq!(
        fs::read_to_string(f.root.join("src/other.rs")).unwrap(),
        "pub fn g() -> i32 {\n    2\n}\n"
    );
    let final_build_output = fs::read(f.root.join("build/out.bin")).unwrap();
    assert_eq!(
        final_build_output, original_build_output,
        "the root's own build/out.bin must never be touched by an apply"
    );

    serve.kill_and_wait();
}

// ---------------------------------------------------------------------
// Scenario 4: SERVICE RESTART (part 6.9, matrix row "serve restart with
// active Work; wrapper alive vs gone").
// ---------------------------------------------------------------------

/// A wrapped attach gated on a marker file: `serve` sees it, is killed,
/// restarted, and must see it again with nothing adopted, applied or
/// finished while the wrapper is alive. Then the wrapper itself (not the
/// agent) is killed; a fresh `serve` must adopt the orphaned Work exactly
/// once, marking `owner_state: adopted`, and must still apply or finish
/// nothing. The orphaned agent process is killed in cleanup.
#[test]
fn serve_restart_recovers_active_work_honestly() {
    let f = Fixture::new();
    let workspace = f.worktree("wt4");
    let ready = f._temp.path().join("ready-4");
    let gate = f._temp.path().join("gate-attach-4");

    let before = f.all_run_ids();
    let mut wrapped = OwnedChild(
        f.attach_wrapped_command(&workspace, &[])
            .env("EDIT_FILE", "src/lib.rs")
            .env("EDIT_CONTENT", "pub fn f() -> i32 {\n    2\n}\n")
            .env("READY", &ready)
            .env("GATE", &gate)
            .env("EXIT_CODE", "0")
            .spawn()
            .unwrap(),
    );
    let wrapper_pid = wrapped.0.id() as i32;
    wait_until(
        Duration::from_secs(300),
        || ready.exists(),
        "attached agent never reached its gate",
    );
    let attached_id = f.wait_new_run_id(&before);
    // The wrapper records `attachment.agent_process` and syncs it to disk
    // right after spawning the child, before the child could plausibly reach
    // its own READY marker; under heavy parallel load the wrapper's own
    // commit can still lag the child briefly, so poll rather than assume.
    let mut agent_pid = None;
    wait_until(
        Duration::from_secs(300),
        || {
            agent_pid = f.metadata(&attached_id)["attachment"]["agent_process"]["pid"].as_u64();
            agent_pid.is_some()
        },
        "attachment.agent_process.pid was never recorded",
    );
    let agent_pid = agent_pid.unwrap() as i32;
    let mut orphan = OrphanGuard(Some(agent_pid));

    // First serve: sees the live-owned Work.
    let mut serve1 = ServeProcess::spawn(&f);
    let work1 = serve1.wait_for(Duration::from_secs(300), |value| {
        value["type"] == "work" && value["run_id"] == attached_id
    });
    assert!(
        work1.is_some(),
        "serve never printed a work line for the attached run"
    );
    serve1.kill_and_wait();

    // Second serve, after a restart: sees it again, adopts nothing (the
    // wrapper is still alive).
    let mut serve2 = ServeProcess::spawn(&f);
    let work2 = serve2.wait_for(Duration::from_secs(300), |value| {
        value["type"] == "work" && value["run_id"] == attached_id
    });
    assert!(
        work2.is_some(),
        "a restarted serve never printed a work line for the attached run"
    );
    // Give it a couple of ticks to prove the absence is not just a race.
    assert_never(
        Duration::from_secs(3),
        || f.event_count(&attached_id, "attach.adopted") > 0,
        "serve must not adopt Work whose wrapper is still alive",
    );
    assert_eq!(f.event_count(&attached_id, "result.applied"), 0);
    assert_eq!(f.event_count(&attached_id, "run.finished"), 0);
    let metadata = f.metadata(&attached_id);
    assert_eq!(metadata["outcome"]["lifecycle"], "working");
    assert_eq!(metadata["attachment"]["owner_state"], "live");
    serve2.kill_and_wait();

    // Kill the wrapper only; the agent, orphaned, stays alive and blocked on
    // its own gate.
    assert_eq!(unsafe { libc::kill(wrapper_pid, libc::SIGKILL) }, 0);
    wrapped.kill_and_wait();
    assert!(alive(agent_pid), "the orphaned agent must still be running");

    // Third serve: adopts the orphaned Work exactly once.
    let mut serve3 = ServeProcess::spawn(&f);
    wait_until(
        Duration::from_secs(300),
        || f.event_count(&attached_id, "attach.adopted") > 0,
        "serve never adopted the orphaned attached Work",
    );
    assert_eq!(f.event_count(&attached_id, "attach.adopted"), 1);
    let metadata = f.metadata(&attached_id);
    assert_eq!(metadata["attachment"]["owner_state"], "adopted");
    assert_eq!(f.event_count(&attached_id, "result.applied"), 0);
    assert_eq!(f.event_count(&attached_id, "run.finished"), 0);
    assert_eq!(metadata["outcome"]["lifecycle"], "working");
    serve3.kill_and_wait();

    // Cleanup: the orphaned agent would otherwise block on its gate forever.
    unsafe { libc::kill(agent_pid, libc::SIGKILL) };
    orphan.0 = None;
    wait_until(
        Duration::from_secs(300),
        || !alive(agent_pid),
        "the orphaned agent was not reaped",
    );
}

// ---------------------------------------------------------------------
// Scenario 5: SIMULTANEOUS INTEGRATION (matrix row "Simultaneous finish").
// ---------------------------------------------------------------------

/// A foreign-attached worktree with `--auto-apply` (not yet finished) and a
/// native `dispatch run --auto-apply` on a disjoint file, both racing for the
/// source lock: with `serve` running, the native agent's gate is released
/// and `dispatch finish` on the attached work is issued at the same moment.
/// Exactly one `result.applied` must land for each, whichever lands second
/// must carry `coherence.analysis: integration`, and neither must see
/// `application.failed`.
#[test]
fn simultaneous_integration_serializes_attached_and_native() {
    let f = Fixture::new();
    let workspace = f.worktree("wt5");

    let foreign_id = f.attach_foreign(&workspace, &["--auto-apply"]);
    fs::write(
        workspace.join("src/other.rs"),
        "pub fn g() -> i32 {\n    3\n}\n",
    )
    .unwrap();

    let mut native = f.spawn_native("ROLE_LIB: give lib.rs a companion edit");
    f.wait_native_started("lib", &mut native);

    let mut serve = ServeProcess::spawn(&f);
    serve.wait_for(Duration::from_secs(300), |value| value["type"] == "world");

    let gate = f._temp.path().join("gate-lib");
    let release = thread::spawn(move || fs::write(gate, "go\n"));
    let finish_result = f.finish(&foreign_id);
    release.join().unwrap().unwrap();
    assert!(
        finish_result.status.success(),
        "{}",
        stderr_of(&finish_result)
    );

    let native_output = native.output();
    assert!(
        native_output.status.success(),
        "{}",
        stderr_of(&native_output)
    );
    let native_result = result_json(&native_output);
    let native_id = native_result["run_id"].as_str().unwrap().to_owned();
    assert_eq!(
        native_result["auto_apply"]["outcome"], "applied",
        "{native_result}"
    );

    wait_until(
        Duration::from_secs(300),
        || {
            let metadata = f.metadata(&foreign_id);
            metadata["outcome"]["application"] == "applied"
        },
        "serve never applied the foreign work",
    );

    assert_eq!(f.event_count(&foreign_id, "result.applied"), 1);
    assert_eq!(f.event_count(&native_id, "result.applied"), 1);
    assert_eq!(f.event_count(&foreign_id, "application.failed"), 0);
    assert_eq!(f.event_count(&native_id, "application.failed"), 0);

    let foreign_metadata = f.metadata(&foreign_id);
    let foreign_is_follower =
        foreign_metadata["coherence"]["validity"]["analysis"] == "integration";
    let native_is_follower = native_result["auto_apply"].get("coherence").is_some();
    assert_ne!(
        foreign_is_follower, native_is_follower,
        "exactly one side's world was unmoved when it applied: foreign={foreign_metadata} native={native_result}"
    );
    if native_is_follower {
        assert_eq!(
            native_result["auto_apply"]["coherence"]["analysis"], "integration",
            "{native_result}"
        );
    } else {
        assert_eq!(
            foreign_metadata["coherence"]["validity"]["analysis"], "integration",
            "{foreign_metadata}"
        );
    }

    let foreign_ts = f.event_timestamp(&foreign_id, "result.applied");
    let native_ts = f.event_timestamp(&native_id, "result.applied");
    let foreign_dt = chrono::DateTime::parse_from_rfc3339(&foreign_ts).unwrap();
    let native_dt = chrono::DateTime::parse_from_rfc3339(&native_ts).unwrap();
    if foreign_is_follower {
        assert!(
            foreign_dt > native_dt,
            "foreign applied second but its timestamp is not later"
        );
    } else {
        assert!(
            native_dt > foreign_dt,
            "native applied second but its timestamp is not later"
        );
    }

    serve.kill_and_wait();
}

// ---------------------------------------------------------------------
// Scenario 6: MANUAL SOURCE CHANGE (part 6.9's "manual edits detected";
// matrix row for the same).
// ---------------------------------------------------------------------

/// Two foreign-attached worktrees, both observed by `serve`: one edits
/// `src/lib.rs`, the other the unrelated `src/other.rs`. The test then edits
/// `src/lib.rs` in the root by hand, conflicting with the first attach's own
/// edit. Only the first must be invalidated; the second must see no
/// invalidation within the same 5s window (a `coherence.checked` is
/// tolerated).
#[test]
fn manual_source_change_reevaluates_only_relevant_work() {
    let f = Fixture::new();
    let workspace_lib = f.worktree("wt6-lib");
    let workspace_other = f.worktree("wt6-other");

    let id_lib = f.attach_foreign(&workspace_lib, &[]);
    fs::write(
        workspace_lib.join("src/lib.rs"),
        "pub fn f() -> i32 {\n    2\n}\n",
    )
    .unwrap();
    let id_other = f.attach_foreign(&workspace_other, &[]);
    fs::write(
        workspace_other.join("src/other.rs"),
        "pub fn g() -> i32 {\n    2\n}\n",
    )
    .unwrap();

    let mut serve = ServeProcess::spawn(&f);
    serve.wait_for(Duration::from_secs(300), |value| value["type"] == "world");

    // Manual edit to the root, conflicting with the first attach's own edit
    // to the same line.
    fs::write(
        f.root.join("src/lib.rs"),
        "pub fn f() -> i32 {\n    99\n}\n",
    )
    .unwrap();

    wait_until(
        Duration::from_secs(300),
        || f.event_count(&id_lib, "coherence.invalidated") > 0,
        "the lib.rs attach was never invalidated by the conflicting manual edit",
    );
    assert_never(
        Duration::from_secs(5),
        || f.event_count(&id_other, "coherence.invalidated") > 0,
        "the unrelated other.rs attach must not be invalidated by an edit to lib.rs",
    );

    serve.kill_and_wait();
}

// ---------------------------------------------------------------------
// Scenario 7: plain-directory wrapped attach (part 6.2/6.4's plain-directory
// row; matrix row "Attach: same repo required; ... plain dir foreign
// refused").
// ---------------------------------------------------------------------

/// A wrapped attach of a plain directory workspace whose `--root` is *also* a
/// plain directory (no `.git` anywhere): must carry
/// `provenance: snapshot_at_attach` and `confidence: partial`. If `attach`
/// instead refuses a plain-directory root, the exact refusal message is
/// asserted and reported instead (this is an escalation path, not a bug this
/// test papers over).
#[test]
fn plain_directory_wrapped_attach_is_partial_confidence() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("plain-root");
    let workspace = temp.path().join("plain-workspace");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&workspace).unwrap();
    fs::write(root.join("dispatch.yml"), "coherence:\n  poll_secs: 1\n").unwrap();

    let agent_script = temp.path().join("external-agent.sh");
    executable(&agent_script, EXTERNAL_AGENT_SCRIPT);

    let state = temp.path().join("state");
    let ready = temp.path().join("ready-7");

    let mut command = Command::new(assert_cmd::cargo_bin!("dispatch"));
    command
        .arg("--state-dir")
        .arg(&state)
        .arg("attach")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--root")
        .arg(&root)
        .arg("--allow-unsafe-local")
        .arg("--")
        .arg("sh")
        .arg(&agent_script)
        .env("EDIT_FILE", "note.txt")
        .env("EDIT_CONTENT", "hello\n")
        .env("READY", &ready)
        .env_remove("GATE")
        .env("EXIT_CODE", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = OwnedChild(command.spawn().unwrap()).output();

    if !output.status.success() {
        // Escalation path: attach refused a plain-directory root. Assert and
        // surface the exact message rather than papering over it.
        let stderr = stderr_of(&output);
        panic!(
            "dispatch attach refused a plain-directory root; exact message: {stderr:?} \
             (report this to the integrator per S6's escalation clause; the test asserts \
             the refusal happened but does not know the expected wording ahead of time)"
        );
    }

    let db = rusqlite::Connection::open(state.join("dispatch.db")).unwrap();
    let id: String = db
        .query_row("SELECT id FROM runs", [], |row| row.get(0))
        .expect("attach created exactly one run");
    let metadata_path = state.join("runs").join(&id).join("metadata.json");
    let metadata: Value = serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();

    assert_eq!(
        metadata["attachment"]["provenance"],
        serde_json::json!("snapshot_at_attach")
    );
    assert_eq!(metadata["attachment"]["confidence"], "partial");
    assert_eq!(metadata["attachment"]["repo_key"], Value::Null);
}
