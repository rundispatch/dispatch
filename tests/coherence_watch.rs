#![cfg(unix)]
//! End-to-end tests for the mid-run coherence watcher. The fake agent writes
//! its work, announces itself, then blocks on a FIFO so that the test can move
//! the source underneath the running attempt and only then let it finish.
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

/// Launches that are not known to be over: none may outlast a run.
const LIVE_LAUNCHES: &str = "attempt_launches WHERE state IN ('intent', 'spawned', 'uncertain')";

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    source: PathBuf,
    state: PathBuf,
}

fn executable(path: &Path, text: &str) -> Result<()> {
    fs::write(path, text)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
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

impl Fixture {
    /// `coherence` is the YAML body of the `coherence:` block.
    fn new(coherence: &str) -> Result<Self> {
        Self::build(coherence, false)
    }

    /// The agent announces itself and waits before writing anything, then
    /// writes its change and waits again: the source can move while the work
    /// is still empty. Its second wait opens the gate again, maybe while the
    /// first release still holds it open; the end of that release is not a
    /// second one, so it waits for a line.
    fn new_late_write(coherence: &str) -> Result<Self> {
        Self::build(coherence, true)
    }

    fn build(coherence: &str, late_write: bool) -> Result<Self> {
        let work = if late_write {
            ": > '{root}/started'\nread line < '{root}/gate'\nprintf '// delivered\\n' > src/lib.rs\n: > '{root}/written'\nuntil read line < '{root}/gate'; do :; done"
        } else {
            "printf '// delivered\\n' > src/lib.rs\n: > '{root}/started'\nread line < '{root}/gate'"
        };
        let temp = tempfile::tempdir()?;
        let root = temp.path().to_owned();
        let source = root.join("source");
        let state = root.join("state");
        fs::create_dir_all(source.join("src"))?;
        fs::create_dir(&state)?;
        fs::write(source.join("src/lib.rs"), "// baseline\n")?;
        fs::write(source.join("result.txt"), "ok\n")?;
        let gate = root.join("gate");
        let gate_c = CString::new(gate.as_os_str().as_bytes())?;
        // SAFETY: the CString is a valid, terminated fixture path.
        anyhow::ensure!(
            unsafe { libc::mkfifo(gate_c.as_ptr(), 0o600) } == 0,
            "mkfifo failed"
        );
        let agent = root.join("codex");
        executable(
            &agent,
            &format!(
                r#"#!/bin/sh
if [ "$1" = '--version' ]; then echo 'codex fixture'; exit 0; fi
if [ "$1" = 'app-server' ]; then
while IFS= read -r line; do
case "$line" in
*'"id":0'*) printf '%s\n' '{{"id":0,"result":{{"userAgent":"fixture"}}}}' ;;
*'"id":1'*) printf '%s\n' '{{"id":1,"result":{{"account":{{"type":"chatgpt","planType":"plus","email":"fixture@example.invalid"}}}}}}' ;;
*'"id":2'*) printf '%s\n' '{{"id":2,"result":{{"rateLimitsByLimitId":{{}}}}}}'; exit 0 ;;
esac
done
exit 0
fi
model=''; previous=''
for arg in "$@"; do if [ "$previous" = '--model' ]; then model="$arg"; fi; previous="$arg"; done
printf '%s\n' "$model" >> '{root}/invocations'
printf '%s\n' "$$" > '{root}/agent.pid'
{work}
printf 'ok\n' > result.txt
: > '{root}/finished'
printf '{{"type":"result","model":"%s"}}\n' "$model"
"#,
                root = root.display(),
                work = work.replace("{root}", &root.display().to_string()),
            ),
        )?;
        let check = root.join("verify");
        executable(&check, "#!/bin/sh\ntest \"$(cat result.txt)\" = 'ok'\n")?;
        fs::write(
            source.join("dispatch.yml"),
            format!(
                "execution:\n  timeout_secs: 600\nchecks:\n  verify: ['{}']\nharnesses:\n  codex:\n    executable: '{}'\ncoherence:\n{coherence}",
                check.display(),
                agent.display()
            ),
        )?;
        git(&source, &["init", "--quiet"])?;
        git(&source, &["add", "-A"])?;
        git(&source, &["commit", "--quiet", "-m", "initial"])?;
        let profiles = [("light-model", "low", "light"), ("strong-model", "high", "strong")].map(|(m, e, t)| {
            format!("  - provider: openai\n    funding_source: chatgpt-plus\n    harness: codex\n    model: {m}\n    effort: {e}\n    runtime: local\n    service_mode: standard\n    pool: shared\n    provider_buckets: [codex]\n    tier: {t}\n    included: true\n    no_overage_verified: true\n    authorization_revision: 1\n    codex_account: {{\"account_sha256\":\"cc6d96611cffa9f02c3626f0b9ee897dc171e2d540a5cae349d4ec316104997b\",\"checked_at\":\"2026-01-01T00:00:00Z\"}}\n")
        }).concat();
        fs::write(
            state.join("resources.yml"),
            format!("version: 1\nallocation_enabled: true\nprofiles:\n{profiles}"),
        )?;
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

    fn start(&self) -> Result<OwnedChild> {
        Ok(OwnedChild(
            self.command()
                .arg("run")
                .arg(&self.source)
                .args([
                    "--task",
                    "Add a comment in src/lib.rs",
                    "--allow-unsafe-local",
                    "--json",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?,
        ))
    }

    fn wait_for_agent(&self, child: &mut OwnedChild) -> Result<i32> {
        wait_until(|| Ok(self.root.join("started").exists() || child.0.try_wait()?.is_some()))?;
        if !self.root.join("started").exists() {
            use std::io::Read;
            let (mut stdout, mut stderr) = (String::new(), String::new());
            child.0.stdout.take().unwrap().read_to_string(&mut stdout)?;
            child.0.stderr.take().unwrap().read_to_string(&mut stderr)?;
            anyhow::bail!("agent never started: stdout={stdout} stderr={stderr}");
        }
        Ok(fs::read_to_string(self.root.join("agent.pid"))?
            .trim()
            .parse()?)
    }

    fn release(&self) -> Result<()> {
        fs::write(self.root.join("gate"), "go\n")?;
        Ok(())
    }

    /// `release`, holding the gate open until `done`, as a release whose
    /// writer has yet to close it when the agent waits again.
    fn release_until(&self, done: impl FnMut() -> Result<bool>) -> Result<()> {
        use std::io::Write;
        let mut gate = fs::OpenOptions::new()
            .write(true)
            .open(self.root.join("gate"))?;
        gate.write_all(b"go\n")?;
        wait_until(done)
    }

    fn edit_source(&self, path: &str, contents: &str) -> Result<()> {
        fs::write(self.source.join(path), contents)?;
        Ok(())
    }

    fn event_count(&self, kind: &str) -> i64 {
        rusqlite::Connection::open(self.state.join("dispatch.db"))
            .and_then(|db| {
                db.query_row(
                    "SELECT COUNT(*) FROM events WHERE event_type = ?1",
                    [kind],
                    |row| row.get(0),
                )
            })
            .unwrap_or(0)
    }

    fn count(&self, table: &str) -> Result<i64> {
        let db = rusqlite::Connection::open(self.state.join("dispatch.db"))?;
        Ok(
            db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })?,
        )
    }

    fn loaded(&self, id: &str) -> Result<dispatch::RunRecord> {
        State::discover(Some(self.state.clone()))?.load_run(id)
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

    fn accept(&self, id: &str) -> Result<Output> {
        Ok(self.command().args(["accept", id]).output()?)
    }
}

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

/// The source moves while the agent has written nothing, then the agent edits
/// the same line. The watcher follows the work as well as the world, so the
/// conflict is recorded during the attempt, not only at accept.
#[test]
fn a_change_that_lands_before_the_agent_edits_the_same_code_is_seen_mid_run() -> Result<()> {
    let f = Fixture::new_late_write("  poll_secs: 1\n")?;
    let mut child = f.start()?;
    f.wait_for_agent(&mut child)?;
    f.edit_source("src/lib.rs", "// teammate\n")?;
    // Watcher ticks see the moved world against still-empty work: CONTINUE,
    // which is not recorded.
    thread::sleep(Duration::from_millis(2500));
    assert_eq!(f.event_count("coherence.invalidated"), 0);
    f.release_until(|| Ok(f.root.join("written").exists()))?;
    wait_until(|| Ok(f.event_count("coherence.invalidated") > 0))?;
    assert!(
        !f.root.join("finished").exists(),
        "recorded while the agent was still working"
    );
    f.release()?;
    let result = Fixture::result(&child.output()?)?;
    let id = result["run_id"].as_str().unwrap();
    let validity = f.loaded(id)?.coherence.unwrap().validity.unwrap();
    assert_eq!(validity.decision, dispatch::Decision::Refresh);
    Ok(())
}

#[test]
fn unrelated_edit_mid_run_records_nothing_and_accept_applies() -> Result<()> {
    let f = Fixture::new("  poll_secs: 1\n")?;
    let mut child = f.start()?;
    let pid = f.wait_for_agent(&mut child)?;
    f.edit_source("NOTES.md", "unrelated\n")?;
    // Several polls pass with the agent still running.
    thread::sleep(Duration::from_millis(3500));
    assert!(alive(pid));
    assert_eq!(f.event_count("coherence.invalidated"), 0);
    assert_eq!(f.event_count("coherence.checked"), 0);
    f.release()?;
    let output = child.output()?;
    let result = Fixture::result(&output)?;
    assert!(output.status.success(), "{result}");
    let id = result["run_id"].as_str().unwrap();
    assert_eq!(f.event_count("coherence.invalidated"), 0);
    assert!(
        f.loaded(id)?
            .coherence
            .is_none_or(|c| c.first_invalid_at.is_none())
    );
    let accepted = f.accept(id)?;
    assert!(
        accepted.status.success(),
        "{}",
        String::from_utf8_lossy(&accepted.stderr)
    );
    assert_eq!(
        fs::read_to_string(f.source.join("src/lib.rs"))?,
        "// delivered\n"
    );
    assert_eq!(
        fs::read_to_string(f.source.join("NOTES.md"))?,
        "unrelated\n"
    );
    Ok(())
}

#[test]
fn conflicting_edit_is_recorded_before_release_and_never_kills_in_observe_or_plain_stop_mode()
-> Result<()> {
    // `stop` without `stop_on_refresh` only stops work that is already applied.
    for coherence in [
        "  poll_secs: 1\n",
        "  poll_secs: 1\n  mid_run: observe\n  stop_on_refresh: true\n",
        "  poll_secs: 1\n  mid_run: stop\n",
    ] {
        let f = Fixture::new(coherence)?;
        let mut child = f.start()?;
        let pid = f.wait_for_agent(&mut child)?;
        f.edit_source("src/lib.rs", "// human edit\n")?;
        wait_until(|| Ok(f.event_count("coherence.invalidated") >= 1))?;
        // The verdict is durable while the agent is still running, untouched.
        assert!(alive(pid), "{coherence}");
        assert!(!f.root.join("finished").exists());
        assert_eq!(f.event_count("coherence.stopped"), 0);
        f.release()?;
        let output = child.output()?;
        let result = Fixture::result(&output)?;
        assert!(output.status.success(), "{coherence}: {result}");
        assert!(f.root.join("finished").exists());
        assert_eq!(f.event_count("coherence.invalidated"), 1);
        assert_eq!(f.event_count("coherence.stopped"), 0);
        let id = result["run_id"].as_str().unwrap();
        let run = f.loaded(id)?;
        let record = run.coherence.as_ref().expect("coherence recorded");
        assert!(record.first_invalid_at.is_some());
        assert_eq!(
            record.validity.as_ref().unwrap().decision,
            dispatch::Decision::Refresh
        );
        // The payload is exactly the validity, next to the injected outcome.
        let db = rusqlite::Connection::open(f.state.join("dispatch.db"))?;
        let payload: String = db.query_row(
            "SELECT payload_json FROM events WHERE event_type = 'coherence.invalidated'",
            [],
            |row| row.get(0),
        )?;
        let payload: Value = serde_json::from_str(&payload)?;
        assert_eq!(payload["coherence"]["decision"], "refresh");
        assert!(payload["outcome"].is_object());
        // The live delta was written outside the workspace.
        assert!(
            run.attempts[0]
                .detail
                .result
                .as_ref()
                .unwrap()
                .prompt_path
                .parent()
                .unwrap()
                .join("delta-live.patch")
                .is_file()
        );
        // Acceptance is blocked exactly as before and leaves the source alone.
        let accepted = f.accept(id)?;
        assert!(!accepted.status.success());
        assert_eq!(
            fs::read_to_string(f.source.join("src/lib.rs"))?,
            "// human edit\n"
        );
    }
    Ok(())
}

#[test]
fn stop_mode_kills_the_agent_when_the_work_is_stale() -> Result<()> {
    // A refresh stops only with `stop_on_refresh`; an already-applied patch
    // (a stop verdict) always stops in `stop` mode.
    for (coherence, edit) in [
        (
            "  poll_secs: 1\n  mid_run: stop\n  stop_on_refresh: true\n",
            "// human edit\n",
        ),
        ("  poll_secs: 1\n  mid_run: stop\n", "// delivered\n"),
    ] {
        let f = Fixture::new(coherence)?;
        let mut child = f.start()?;
        let pid = f.wait_for_agent(&mut child)?;
        f.edit_source("src/lib.rs", edit)?;
        // Nothing else ends the agent: it waits on its gate, which only the
        // watcher's stop gets it past. That the stop is not held back is the
        // watcher's own test; how long the kill takes here is the machine's.
        wait_until(|| Ok(!alive(pid)))?;
        let output = child.output()?;
        let result = Fixture::result(&output)?;
        assert!(!f.root.join("finished").exists());
        assert_eq!(output.status.code(), Some(1), "{result}");
        assert_eq!(result["execution"]["failure"], "stale_work");
        assert_eq!(result["outcome"]["work_result"], "cancelled");
        assert_eq!(f.event_count("coherence.invalidated"), 1);
        assert_eq!(f.event_count("coherence.stopped"), 1);
        let id = result["run_id"].as_str().unwrap();
        let run = f.loaded(id)?;
        assert_eq!(run.status, dispatch::RunStatus::Interrupted);
        assert_eq!(
            run.execution.as_ref().unwrap().failure,
            Some(dispatch::FailureKind::StaleWork)
        );
        assert!(run.coherence.unwrap().first_invalid_at.is_some());
        // The partial work is preserved.
        let diff = fs::read_to_string(&run.candidates[0].diff_path)?;
        assert!(diff.contains("+// delivered"), "{diff}");
        assert_eq!(f.count("attempts")?, 1);
        // The stop reason is durable and says why.
        let db = rusqlite::Connection::open(f.state.join("dispatch.db"))?;
        let payload: String = db.query_row(
            "SELECT payload_json FROM events WHERE event_type = 'run.stopped'",
            [],
            |row| row.get(0),
        )?;
        let payload: Value = serde_json::from_str(&payload)?;
        assert_eq!(payload["failure"], "stale_work");
        assert!(
            payload["reason"]
                .as_str()
                .unwrap()
                .starts_with("work stopped: the source changed underneath it (")
        );
        // Nothing counts against the agent and nothing is left leased.
        assert_eq!(f.count(LIVE_LAUNCHES)?, 0);
        assert_eq!(f.count("routing_observations")?, 0);
        assert_eq!(f.count("goal_feedback_revisions")?, 0);
        assert_eq!(f.count("evaluations")?, 0);
        // The stopped work cannot be accepted.
        assert!(!f.accept(id)?.status.success());
        assert_eq!(fs::read_to_string(f.source.join("src/lib.rs"))?, edit);
    }
    Ok(())
}

#[test]
fn a_second_run_after_a_stopped_one_works() -> Result<()> {
    // The stopped attempt's watcher must not linger or hold anything: a new
    // run in the same state directory proceeds normally.
    let f = Fixture::new("  poll_secs: 1\n  mid_run: stop\n  stop_on_refresh: true\n")?;
    let mut child = f.start()?;
    let pid = f.wait_for_agent(&mut child)?;
    f.edit_source("src/lib.rs", "// human edit\n")?;
    wait_until(|| Ok(!alive(pid)))?;
    let output = child.output()?;
    assert_eq!(
        Fixture::result(&output)?["execution"]["failure"],
        "stale_work"
    );
    fs::remove_file(f.root.join("started"))?;
    // A different source state with no conflicting edit completes.
    git(&f.source, &["checkout", "--quiet", "--", "src/lib.rs"])?;
    let mut child = f.start()?;
    f.wait_for_agent(&mut child)?;
    f.release()?;
    let output = child.output()?;
    assert!(output.status.success(), "{}", Fixture::result(&output)?);
    assert_eq!(f.count(LIVE_LAUNCHES)?, 0);
    Ok(())
}
