//! The durable launch record (0.4.1 S6a; the gate for deleting admission in
//! S6b, which must leave this file unchanged). A supervisor killed with
//! SIGKILL while its agent runs leaves a record naming that agent, and the
//! run is not closed while the agent may still be writing.
#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;
use dispatch::{db::Database, launch::launches_for_run};

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

fn wait_until(what: &str, mut ready: impl FnMut() -> Result<bool>) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(300);
    while !ready()? {
        anyhow::ensure!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(25));
    }
    Ok(())
}

impl Fixture {
    /// `hold`: the agent records its pid and then sleeps until killed.
    fn new(hold: bool) -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().to_owned();
        let source = root.join("source");
        let state = root.join("state");
        fs::create_dir_all(source.join("src"))?;
        fs::create_dir(&state)?;
        fs::write(source.join("src/lib.rs"), "// baseline\n")?;
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
printf '%s\n' "$$" > '{root}/agent.pid'
{hold}printf '// delivered\n' > src/lib.rs
printf '{{"type":"result"}}\n'
"#,
                root = root.display(),
                hold = if hold { "sleep 60\n" } else { "" },
            ),
        )?;
        fs::write(
            source.join("dispatch.yml"),
            format!(
                "execution:\n  timeout_secs: 600\nharnesses:\n  codex:\n    executable: '{}'\n",
                agent.display()
            ),
        )?;
        fs::write(
            state.join("resources.yml"),
            "version: 1\nallocation_enabled: true\ncapacity:\n  codex_probe: false\nprofiles:\n  - provider: openai\n    funding_source: chatgpt-plus\n    harness: codex\n    model: fixture-model\n    effort: low\n    runtime: local\n    service_mode: standard\n    pool: shared\n    tier: light\n    included: true\n    no_overage_verified: true\n    authorization_revision: 1\n    codex_account: {\"account_sha256\":\"cc6d96611cffa9f02c3626f0b9ee897dc171e2d540a5cae349d4ec316104997b\",\"checked_at\":\"2026-01-01T00:00:00Z\"}\n",
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

    fn start(&self) -> Result<Child> {
        Ok(self
            .command()
            .arg("run")
            .arg(&self.source)
            .args([
                "--task",
                "Deliver the fixture change.",
                "--allow-unsafe-local",
                "--json",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?)
    }

    fn run_id(&self) -> Result<String> {
        let runs = fs::read_dir(self.state.join("runs"))?.collect::<Result<Vec<_>, _>>()?;
        anyhow::ensure!(runs.len() == 1, "expected exactly one run");
        Ok(runs[0].file_name().to_string_lossy().into_owned())
    }

    fn launches(&self, run_id: &str) -> Result<Vec<dispatch::launch::LaunchRecord>> {
        launches_for_run(&Database::open(self.state.join("dispatch.db"))?, run_id)
    }

    fn agent_pid(&self) -> Result<Option<i32>> {
        Ok(fs::read_to_string(self.root.join("agent.pid"))
            .ok()
            .and_then(|text| text.trim().parse().ok()))
    }
}

#[test]
fn a_completed_native_attempt_leaves_a_cleaned_launch_record() -> Result<()> {
    let f = Fixture::new(false)?;
    let status = f.start()?.wait()?;
    assert!(status.success());
    let launches = f.launches(&f.run_id()?)?;
    assert_eq!(launches.len(), 1, "{launches:?}");
    assert_eq!(launches[0].state, dispatch::launch::LaunchState::Cleaned);
    assert_eq!(
        launches[0].child.as_ref().map(|child| child.pid as i32),
        f.agent_pid()?
    );
    assert!(!launches[0].possibly_alive());
    Ok(())
}

#[test]
fn a_killed_supervisor_leaves_its_running_agent_recorded_and_the_run_open() -> Result<()> {
    let f = Fixture::new(true)?;
    let mut supervisor = f.start()?;
    // Kill point: after the child's identity is durable. (An earlier kill
    // leaves only `intent`, which is treated as possibly alive; see the unit
    // tests in `dispatch::launch`.)
    wait_until("the agent's launch to be recorded", || {
        Ok(f.agent_pid()?.is_some()
            && fs::read_dir(f.state.join("runs"))?.count() == 1
            && f.launches(&f.run_id()?)?
                .iter()
                .any(|l| l.state == dispatch::launch::LaunchState::Spawned))
    })?;
    let agent = f.agent_pid()?.unwrap();
    // SAFETY: signals only the supervisor this test started.
    unsafe { libc::kill(supervisor.id() as i32, libc::SIGKILL) };
    supervisor.wait()?;

    let id = f.run_id()?;
    let launches = f.launches(&id)?;
    assert_eq!(launches.len(), 1, "{launches:?}");
    assert_eq!(launches[0].state, dispatch::launch::LaunchState::Spawned);
    assert_eq!(
        launches[0].child.as_ref().map(|child| child.pid as i32),
        Some(agent)
    );
    assert!(
        launches[0].possibly_alive(),
        "the orphaned agent is still running"
    );

    // Reading the run runs crash repair; it must not close work whose agent
    // may still be writing.
    let status = f.command().args(["status", &id, "--json"]).output()?;
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&status.stdout)?;
    assert_ne!(result["outcome"]["lifecycle"], "finished", "{result}");

    // SAFETY: the agent leads its own process group; stop only that group.
    unsafe { libc::kill(-agent, libc::SIGKILL) };
    wait_until("the agent group to exit", || {
        Ok(!f.launches(&id)?[0].possibly_alive())
    })?;
    Ok(())
}
