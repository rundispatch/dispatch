#![cfg(unix)]
//! Crash and interruption safety of the coherence feature: a killed supervisor
//! with the mid-run watcher active, a source that moves between the coherence
//! verdict and the real `git apply`, and the lineage after a `mid_run: stop`.
//!
//! The mid-run fixtures use a fake agent that writes its work, announces itself
//! and then blocks on a FIFO, so the test decides when the run may finish.
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
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready()? {
        anyhow::ensure!(
            Instant::now() < deadline,
            "fixture did not reach its durable boundary"
        );
        thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

/// Every non-`.git` file of a directory (relative name, bytes), for exact comparison.
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

/// Kills a fixture agent that outlived its supervisor (it is blocked on the
/// FIFO and nothing else would ever stop it).
struct OrphanGuard(Option<i32>);
impl Drop for OrphanGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            // SAFETY: the pid is the fixture agent recorded in `agent.pid`.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

/// A Git source whose `codex` agent blocks on a FIFO mid-run.
/// Launches that are not known to be over: none may outlast a run.
const LIVE_LAUNCHES: &str = "attempt_launches WHERE state IN ('intent', 'spawned', 'uncertain')";

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    source: PathBuf,
    state: PathBuf,
}

impl Fixture {
    /// `coherence` is the YAML body of the `coherence:` block.
    fn new(coherence: &str) -> Result<Self> {
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
printf '// delivered\n' > src/lib.rs
: > '{root}/started'
read line < '{root}/gate'
printf 'ok\n' > result.txt
: > '{root}/finished'
printf '{{"type":"result","model":"%s"}}\n' "$model"
"#,
                root = root.display()
            ),
        )?;
        let check = root.join("verify");
        executable(&check, "#!/bin/sh\ntest \"$(cat result.txt)\" = 'ok'\n")?;
        fs::write(
            source.join("dispatch.yml"),
            format!(
                "execution:\n  timeout_secs: 60\nchecks:\n  verify: ['{}']\nharnesses:\n  codex:\n    executable: '{}'\ncoherence:\n{coherence}",
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
        anyhow::ensure!(self.root.join("started").exists(), "agent never started");
        Ok(fs::read_to_string(self.root.join("agent.pid"))?
            .trim()
            .parse()?)
    }

    fn release(&self) -> Result<()> {
        fs::write(self.root.join("gate"), "go\n")?;
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

    fn only_run_id(&self) -> Result<String> {
        let db = rusqlite::Connection::open(self.state.join("dispatch.db"))?;
        Ok(db.query_row("SELECT id FROM runs", [], |row| row.get(0))?)
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

    fn stderr(output: &Output) -> String {
        String::from_utf8_lossy(&output.stderr).into_owned()
    }
}

#[test]
fn killed_supervisor_mid_run_leaves_a_loadable_run_that_cannot_be_accepted() -> Result<()> {
    let f = Fixture::new("  poll_secs: 1\n")?;
    let mut child = f.start()?;
    let pid = f.wait_for_agent(&mut child)?;
    let mut orphan = OrphanGuard(Some(pid));
    // The watcher has judged the conflicting edit and made it durable.
    fs::write(f.source.join("src/lib.rs"), "// human edit\n")?;
    wait_until(|| Ok(f.event_count("coherence.invalidated") >= 1))?;
    assert!(alive(pid));

    // kill -9 the supervising `dispatch run`; the agent knows nothing of it.
    child.0.kill()?;
    child.0.wait()?;
    let source_before = tree(&f.source);

    // The run loads without panicking and is recorded as never having finished.
    let id = f.only_run_id()?;
    let run = f.loaded(&id)?;
    assert_ne!(
        run.outcome.lifecycle,
        dispatch::LifecycleState::Finished,
        "an unfinished attempt must not be reported as a finished result"
    );
    assert_ne!(run.outcome.work_result, dispatch::WorkResult::Ready);
    // The verdict the watcher stored before the crash survived it.
    let record = run.coherence.as_ref().expect("stored verdict survives");
    assert!(record.first_invalid_at.is_some());
    assert_eq!(
        record.validity.as_ref().unwrap().decision,
        dispatch::Decision::Refresh
    );

    // `check` and `accept` refuse cleanly and touch nothing.
    let check = f.command().args(["check", &id]).output()?;
    assert_eq!(check.status.code(), Some(1), "{}", Fixture::stderr(&check));
    assert!(
        Fixture::stderr(&check).contains("not a ready, unapplied result"),
        "{}",
        Fixture::stderr(&check)
    );
    let accept = f.command().args(["accept", &id]).output()?;
    assert!(!accept.status.success());
    let message = Fixture::stderr(&accept);
    assert!(!message.contains("panicked"), "{message}");
    assert!(!message.is_empty());
    let apply = f.command().args(["accept", &id]).output()?;
    assert!(!apply.status.success());
    assert!(!Fixture::stderr(&apply).contains("panicked"));
    assert_eq!(tree(&f.source), source_before, "no partial writes");
    assert_eq!(
        fs::read_to_string(f.source.join("src/lib.rs"))?,
        "// human edit\n"
    );
    let db = rusqlite::Connection::open(f.state.join("dispatch.db"))?;
    let accepted: i64 = db.query_row(
        "SELECT COUNT(*) FROM events WHERE event_type = 'review.accepted'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(accepted, 0);
    drop(db);
    assert_eq!(f.count("evaluations")?, 0);

    // Cleanup: the orphaned agent is stopped before the next run starts.
    assert!(alive(pid), "kill -9 of the supervisor leaves the agent");
    // SAFETY: the pid is the fixture agent recorded in `agent.pid`.
    unsafe { libc::kill(pid, libc::SIGKILL) };
    orphan.0 = None;
    wait_until(|| Ok(!alive(pid)))?;
    fs::remove_file(f.root.join("started"))?;
    git(&f.source, &["checkout", "--quiet", "--", "src/lib.rs"])?;
    let mut next = f.start()?;
    f.wait_for_agent(&mut next)?;
    f.release()?;
    let output = next.output()?;
    assert!(output.status.success(), "{}", Fixture::result(&output)?);
    // The interrupted run is still not acceptable, and is still loadable.
    assert!(!f.command().args(["accept", &id]).output()?.status.success());
    assert!(f.loaded(&id).is_ok());
    Ok(())
}

/// A ready `fake-good` run over a plain directory: it creates the one empty
/// file `dispatch-fake-good.txt`.
struct FakeGood {
    _temp: tempfile::TempDir,
    source: PathBuf,
    state: PathBuf,
    run_id: String,
}

impl FakeGood {
    fn new() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source");
        let state = temp.path().join("state");
        fs::create_dir_all(&source)?;
        fs::write(source.join("original.txt"), "baseline\n")?;
        fs::write(
            source.join("dispatch.yml"),
            "execution:\n  timeout_secs: 30\n",
        )?;
        let output = Command::new(assert_cmd::cargo_bin!("dispatch"))
            .arg("--state-dir")
            .arg(&state)
            .arg("run")
            .arg(&source)
            .args([
                "--allow-unsafe-local",
                "--task",
                "Create the fake artifact.",
                "--agent",
                "fake-good",
            ])
            .output()?;
        anyhow::ensure!(output.status.success(), "{}", Fixture::stderr(&output));
        let run_id = String::from_utf8(output.stdout)?
            .lines()
            .find_map(|line| line.strip_prefix("RUN "))
            .context("run output includes an ID")?
            .to_owned();
        Ok(Self {
            source: fs::canonicalize(source)?,
            _temp: temp,
            state,
            run_id,
        })
    }

    fn loaded(&self) -> Result<dispatch::RunRecord> {
        State::discover(Some(self.state.clone()))?.load_run(&self.run_id)
    }
}

// There is no hook between the coherence verdict and the real `git apply` in
// the CLI, so this exercises `source::apply_validated` directly with the digest
// a verdict produced before the source moved. The orchestrator passes exactly
// this digest (`Validity::world_digest`) when it applies.
#[test]
fn apply_validated_aborts_when_the_source_moves_after_the_verdict() -> Result<()> {
    let f = FakeGood::new()?;
    let run = f.loaded()?;
    let label = run.candidates[0].label.clone();
    let verdict = dispatch::coherence::evaluate_run(&run, &label, None)?;
    assert_eq!(verdict.decision, dispatch::Decision::Continue);
    let stale_digest = verdict.world_digest;

    // Another writer lands a change after the verdict and before the apply.
    fs::write(f.source.join("landed-later.txt"), "not judged\n")?;
    let before = tree(&f.source);

    let error = dispatch::source::apply_validated(&run, &label, &stale_digest)
        .expect_err("a world the verdict never saw must not be patched");
    assert!(
        format!("{error:#}").contains("source has changed"),
        "{error:#}"
    );
    assert_eq!(tree(&f.source), before, "the source is byte-identical");
    assert!(!f.source.join("dispatch-fake-good.txt").exists());

    // Nothing was recorded and nothing was half done: re-judging the new world
    // and applying with its digest succeeds, keeping the other writer's file.
    let now = dispatch::coherence::evaluate_run(&run, &label, None)?;
    assert_eq!(now.decision, dispatch::Decision::Continue);
    assert_ne!(now.world_digest, stale_digest);
    dispatch::source::apply_validated(&run, &label, &now.world_digest)?;
    assert!(f.source.join("dispatch-fake-good.txt").is_file());
    assert_eq!(
        fs::read_to_string(f.source.join("landed-later.txt"))?,
        "not judged\n"
    );
    Ok(())
}

#[test]
fn apply_validated_digest_names_content_not_time() -> Result<()> {
    // The digest names content, not time: an edit that is undone before the apply
    // leaves the judged world in place, so the same digest is still valid.
    let f = FakeGood::new()?;
    let run = f.loaded()?;
    let label = run.candidates[0].label.clone();
    let digest = dispatch::coherence::evaluate_run(&run, &label, None)?.world_digest;
    fs::write(f.source.join("scratch.txt"), "transient\n")?;
    assert!(dispatch::source::apply_validated(&run, &label, &digest).is_err());
    fs::remove_file(f.source.join("scratch.txt"))?;
    dispatch::source::apply_validated(&run, &label, &digest)?;
    assert!(f.source.join("dispatch-fake-good.txt").is_file());
    Ok(())
}

#[test]
fn stopped_run_cannot_be_refreshed_and_a_fresh_run_still_works() -> Result<()> {
    let f = Fixture::new("  poll_secs: 1\n  mid_run: stop\n  stop_on_refresh: true\n")?;
    let mut child = f.start()?;
    let pid = f.wait_for_agent(&mut child)?;
    fs::write(f.source.join("src/lib.rs"), "// human edit\n")?;
    wait_until(|| Ok(!alive(pid)))?;
    let output = child.output()?;
    let result = Fixture::result(&output)?;
    assert_eq!(result["execution"]["failure"], "stale_work", "{result}");
    let old = result["run_id"].as_str().unwrap().to_owned();
    assert_eq!(
        f.loaded(&old)?.status,
        dispatch::RunStatus::Interrupted,
        "the stop is recorded"
    );
    let runs_before = f.count("runs")?;
    let source_before = tree(&f.source);

    // A stopped run is not a ready result, so refresh says so and starts nothing.
    let refresh = f
        .command()
        .args(["refresh", &old, "--allow-unsafe-local"])
        .output()?;
    assert_eq!(refresh.status.code(), Some(1));
    let message = Fixture::stderr(&refresh);
    assert!(
        message.contains(&format!(
            "run {old} is not a ready, unapplied result; nothing to refresh"
        )),
        "{message}"
    );
    assert_eq!(f.count("runs")?, runs_before, "no run may be created");
    assert_eq!(f.count("attempts")?, 1);
    assert_eq!(f.count(LIVE_LAUNCHES)?, 0);
    assert_eq!(tree(&f.source), source_before);
    let check = f.command().args(["check", &old]).output()?;
    assert!(Fixture::stderr(&check).contains("not a ready, unapplied result"));

    // A fresh `dispatch run` on the current source is the way forward.
    fs::remove_file(f.root.join("started"))?;
    git(&f.source, &["checkout", "--quiet", "--", "src/lib.rs"])?;
    let mut next = f.start()?;
    f.wait_for_agent(&mut next)?;
    f.release()?;
    let output = next.output()?;
    let fresh = Fixture::result(&output)?;
    assert!(output.status.success(), "{fresh}");
    let fresh_id = fresh["run_id"].as_str().unwrap();
    assert_ne!(fresh_id, old);
    assert!(
        f.loaded(fresh_id)?
            .coherence
            .is_none_or(|c| { c.refreshed_from.is_none() && c.first_invalid_at.is_none() })
    );
    let checked = f.command().args(["check", fresh_id]).output()?;
    assert!(
        String::from_utf8_lossy(&checked.stdout).contains("Coherence: CONTINUE"),
        "{}",
        String::from_utf8_lossy(&checked.stdout)
    );
    assert_eq!(f.count(LIVE_LAUNCHES)?, 0);
    Ok(())
}
