//! The verification memo: a pass of `checks.verify` on one exact tree, kept
//! so the same tree is not checked again with the same commands in the same
//! environment, whichever of finish, `check --merged` or accept asks.
//!
//! One JSON file per pass under the state directory, named by the digest of
//! its key: the Git tree id of the tree that was checked, the ordered
//! commands, and an execution fingerprint. Only passes are kept, for 24
//! hours; `dispatch clean` removes them all. It records results, not code:
//! nothing here indexes a tree's contents.
//!
//! The memo only ever saves a run. When a key cannot be computed (a plain
//! directory, a Docker image that cannot be identified, a Git failure), the
//! checks run as they would without it.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    RunRecord, SourceKind,
    config::{Config, ExecutionConfig},
    executor::{safe_local_environment, trusted_host_executable},
    source::{self, checked_output, git_command},
    state::State,
};

/// How long a pass may be reused.
const LIFETIME_HOURS: i64 = 24;

/// What identifies one check run: the tree, the commands and the
/// environment they ran in, and the digest of all three.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub digest: String,
    pub tree: String,
    commands: Vec<String>,
    fingerprint: serde_json::Value,
}

/// A recorded pass: which run produced it, where its logs are, and when.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub key: String,
    pub tree: String,
    pub commands: Vec<String>,
    pub fingerprint: serde_json::Value,
    pub run_id: String,
    pub logs: PathBuf,
    pub at: DateTime<Utc>,
}

impl Entry {
    /// "passed, reused from <run> at <time>".
    pub fn describe(&self) -> String {
        describe(&self.run_id, &self.at)
    }

    /// What `checks.reused` records for `by`, beside the caller's own fields.
    pub fn event_payload(&self, by: &str) -> serde_json::Value {
        serde_json::json!({
            "by": by,
            "reused_run": self.run_id,
            "logs": self.logs,
            "passed_at": self.at,
            "key": self.key,
            "tree": self.tree,
        })
    }
}

fn describe(run_id: &str, at: &DateTime<Utc>) -> String {
    format!(
        "passed, reused from {run_id} at {}",
        at.format("%Y-%m-%d %H:%M:%S UTC")
    )
}

/// `Entry::describe` for a recorded `checks.reused` payload.
pub fn describe_reuse(payload: &serde_json::Value) -> Option<String> {
    let at = serde_json::from_value::<DateTime<Utc>>(payload["passed_at"].clone()).ok()?;
    Some(describe(payload["reused_run"].as_str()?, &at))
}

/// The key for running `config`'s verify commands on the current source of
/// `run` with `patch` applied, or `None` when it cannot be known.
pub fn merged_key(run: &RunRecord, patch: &Path, config: &Config) -> Option<Key> {
    let tree = merged_tree(&run.source_path, &run.source_kind, patch)
        .ok()
        .flatten()?;
    key(tree, &config.checks.verify, &config.execution)
}

/// The key for running `commands` in `workspace` as it is, or `None` when it
/// cannot be known.
pub fn workspace_key(
    workspace: &Path,
    commands: &[String],
    execution: &ExecutionConfig,
) -> Option<Key> {
    key(workspace_tree(workspace).ok()??, commands, execution)
}

/// The Git tree id of `source`'s world (`source::world_commit`) with `patch`
/// applied, built in a temporary index; `None` for a plain directory.
fn merged_tree(source: &Path, kind: &SourceKind, patch: &Path) -> Result<Option<String>> {
    if matches!(kind, SourceKind::Directory) {
        return Ok(None);
    }
    let Some(world) = world(source)? else {
        return Ok(None);
    };
    let scratch = tempfile::Builder::new()
        .prefix("dispatch-memo-")
        .tempdir()
        .context("failed to create a temporary Git index")?;
    let index = scratch.path().join("index");
    let mut read_tree = git_command(source);
    read_tree
        .env("GIT_INDEX_FILE", &index)
        .args(["read-tree", &world]);
    checked_output(read_tree, "failed to read the source's world")?;
    if fs::metadata(patch)?.len() > 0 {
        let mut apply = git_command(source);
        apply
            .env("GIT_INDEX_FILE", &index)
            .args(["apply", "--cached", "--binary", "--whitespace=nowarn", "--"])
            .arg(patch);
        checked_output(apply, "the patch does not apply to the source's world")?;
    }
    let mut write_tree = git_command(source);
    write_tree.env("GIT_INDEX_FILE", &index).arg("write-tree");
    let tree = checked_output(write_tree, "failed to write the merged tree")?.stdout;
    Ok(Some(String::from_utf8_lossy(&tree).trim().to_owned()))
}

/// The Git tree id of `workspace`'s files, ignored files left out.
fn workspace_tree(workspace: &Path) -> Result<Option<String>> {
    let Some(commit) = world(workspace)? else {
        return Ok(None);
    };
    let mut tree = git_command(workspace);
    tree.args(["rev-parse", &format!("{commit}^{{tree}}")]);
    let tree = checked_output(tree, "failed to read the workspace's tree")?.stdout;
    Ok(Some(String::from_utf8_lossy(&tree).trim().to_owned()))
}

/// `source::world_commit` of `checkout`, or `None` when its tree would not
/// name all the checks see: an untracked nested repository (the world leaves
/// those out while the checks may see them), or Git converting the bytes it
/// stores (`converts`), so that two different files make one tree.
fn world(checkout: &Path) -> Result<Option<String>> {
    let mut list = git_command(checkout);
    list.args(["ls-files", "-co", "--exclude-standard", "-z"]);
    let listed = checked_output(list, "failed to list the files")?.stdout;
    if listed
        .split(|byte| *byte == 0)
        .any(|raw| raw.ends_with(b"/"))
        || converts(checkout, &listed)?
    {
        return Ok(None);
    }
    source::world_commit(checkout).map(Some)
}

/// The attributes with which Git stores other bytes than the file's.
const CONVERTING_ATTRIBUTES: [&str; 6] = [
    "text",
    "crlf",
    "eol",
    "ident",
    "filter",
    "working-tree-encoding",
];

/// Whether Git converts any of the NUL-separated `paths` of `checkout` when
/// it stores them: `core.autocrlf` or `core.eol` set, or a converting
/// attribute that is set for one of them, from any attributes file.
fn converts(checkout: &Path, paths: &[u8]) -> Result<bool> {
    let mut config = git_command(checkout);
    config.args(["config", "--get-regexp", r"^core\.(autocrlf|eol)$"]);
    let config = source::run_git(config, None, "failed to read the Git config")?;
    let configured = String::from_utf8_lossy(&config.stdout).lines().any(|line| {
        line.split_once(' ').is_none_or(|(name, value)| {
            name != "core.autocrlf" || !value.eq_ignore_ascii_case("false")
        })
    });
    if configured {
        return Ok(true);
    }
    let mut attributes = git_command(checkout);
    attributes
        .args(["check-attr", "--stdin", "-z"])
        .args(CONVERTING_ATTRIBUTES);
    let attributes = source::run_git(
        attributes,
        Some(paths),
        "failed to read the files' attributes",
    )?;
    anyhow::ensure!(
        attributes.status.success(),
        "failed to read the files' attributes"
    );
    // Records of path, attribute, value; `-text` is unset, which stores the
    // bytes as they are.
    Ok(attributes
        .stdout
        .split(|byte| *byte == 0)
        .skip(2)
        .step_by(3)
        .any(|value| value != b"unspecified" && value != b"unset" && !value.is_empty()))
}

/// The key for `commands` on `tree`. The fingerprint is what the commands
/// see besides the tree: the execution settings and Docker image, the
/// environment a check gets (`PATH`, `HOME`, `TMPDIR`, `LANG`), where each
/// command's program resolves, and this Dispatch.
fn key(tree: String, commands: &[String], execution: &ExecutionConfig) -> Option<Key> {
    let image = match execution.backend.as_str() {
        "docker" => Some(image_id(&execution.docker_image)?),
        _ => None,
    };
    let environment = safe_local_environment();
    let path = environment.get("PATH").cloned().unwrap_or_default();
    let programs = commands
        .iter()
        .map(|command| (execution.backend == "local").then(|| program(command, &path)))
        .collect::<Vec<_>>();
    let fingerprint = serde_json::json!({
        "dispatch": env!("CARGO_PKG_VERSION"),
        "execution": execution,
        "image": image,
        "environment": environment,
        "programs": programs,
    });
    let keyed = serde_json::json!({
        "tree": tree,
        "commands": commands,
        "fingerprint": fingerprint,
    });
    Some(Key {
        digest: hex::encode(Sha256::digest(keyed.to_string())),
        tree,
        commands: commands.to_owned(),
        fingerprint,
    })
}

/// The program a shell command runs, as `PATH` resolves it, symlinks
/// followed; a path with a slash as written (a relative one is part of the
/// tree, an absolute one with symlinks followed). `None` when nothing
/// resolves, such as a shell builtin.
fn program(command: &str, path: &str) -> Option<PathBuf> {
    let word = command.split_whitespace().find(|word| {
        // Leading `NAME=value` assignments are not the program.
        !word.split_once('=').is_some_and(|(name, _)| {
            !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
    })?;
    if word.starts_with('/') {
        return Some(fs::canonicalize(word).unwrap_or_else(|_| PathBuf::from(word)));
    }
    if word.contains('/') {
        return Some(PathBuf::from(word));
    }
    let found = which::which_in(word, Some(path), "/").ok()?;
    Some(fs::canonicalize(&found).unwrap_or(found))
}

/// The local Docker image's id, which changes whenever the tag is moved.
fn image_id(image: &str) -> Option<String> {
    let output = std::process::Command::new(trusted_host_executable("docker").ok()?)
        .args(["image", "inspect", "--format", "{{.Id}}", "--", image])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    let id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (output.status.success() && !id.is_empty()).then_some(id)
}

/// The memo's directory, beside the runs that `run_dir` is one of.
fn dir(run_dir: &Path) -> PathBuf {
    run_dir
        .parent()
        .and_then(Path::parent)
        .unwrap_or(run_dir)
        .join(MEMO_DIRECTORY)
}

const MEMO_DIRECTORY: &str = "memo";

fn entry_path(run_dir: &Path, key: &Key) -> PathBuf {
    dir(run_dir).join(format!("{}.json", key.digest))
}

/// A pass recorded for exactly `key` in the last 24 hours whose logs are
/// still there.
pub fn lookup(run_dir: &Path, key: &Key) -> Option<Entry> {
    let entry: Entry = serde_json::from_slice(&fs::read(entry_path(run_dir, key)).ok()?).ok()?;
    let same = entry.key == key.digest
        && entry.tree == key.tree
        && entry.commands == key.commands
        && entry.fingerprint == key.fingerprint;
    let fresh = Utc::now() - entry.at < Duration::hours(LIFETIME_HOURS);
    (same && fresh && entry.logs.is_dir()).then_some(entry)
}

/// Record that the run `run_id` passed on `key`, its logs in `logs`. Only
/// call this for a pass.
pub fn store(run_dir: &Path, key: &Key, run_id: &str, logs: &Path) -> Result<()> {
    let entry = Entry {
        key: key.digest.clone(),
        tree: key.tree.clone(),
        commands: key.commands.clone(),
        fingerprint: key.fingerprint.clone(),
        run_id: run_id.to_owned(),
        logs: logs.to_owned(),
        at: Utc::now(),
    };
    crate::state::write_durably(&entry_path(run_dir, key), &serde_json::to_vec(&entry)?)
}

/// The memo's directory and how many passes it holds, or `None` when it
/// holds none.
pub fn recorded(state: &State) -> Result<Option<(PathBuf, usize)>> {
    let dir = state.root.join(MEMO_DIRECTORY);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut passes = 0;
    for entry in entries {
        if entry?.path().extension().is_some_and(|ext| ext == "json") {
            passes += 1;
        }
    }
    Ok((passes > 0).then_some((dir, passes)))
}

/// Remove every recorded pass.
pub fn clear(state: &State) -> Result<()> {
    match fs::remove_dir_all(state.root.join(MEMO_DIRECTORY)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyed(tree: &str, commands: &[&str]) -> Key {
        let commands = commands.iter().map(|c| (*c).to_owned()).collect::<Vec<_>>();
        key(tree.into(), &commands, &ExecutionConfig::default()).unwrap()
    }

    #[test]
    fn the_key_covers_the_tree_and_the_exact_ordered_commands() {
        let key = keyed("t1", &["true", "test -f a"]);
        assert_eq!(key, keyed("t1", &["true", "test -f a"]));
        for other in [
            keyed("t2", &["true", "test -f a"]),
            keyed("t1", &["test -f a", "true"]),
            keyed("t1", &["true", "test -f b"]),
            keyed("t1", &["true"]),
        ] {
            assert_ne!(other.digest, key.digest);
        }
        let fingerprint = &key.fingerprint;
        assert_eq!(fingerprint["dispatch"], env!("CARGO_PKG_VERSION"));
        assert_eq!(fingerprint["execution"]["backend"], "local");
        assert!(fingerprint["environment"].get("PATH").is_some());
        assert!(
            fingerprint["programs"][0]
                .as_str()
                .unwrap()
                .ends_with("true")
        );
    }

    #[test]
    fn a_program_resolves_past_assignments_and_keeps_paths_as_written() {
        let temp = tempfile::tempdir().unwrap();
        let bin = fs::canonicalize(temp.path()).unwrap();
        let tool = bin.join("tool");
        fs::write(&tool, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = bin.to_str().unwrap();
        assert_eq!(program("A=1 B_2=x tool --flag", path), Some(tool.clone()));
        assert_eq!(
            program("./scripts/check.sh", path),
            Some("./scripts/check.sh".into())
        );
        assert_eq!(program("missing-tool", path), None);
        assert_eq!(program("", path), None);
    }

    #[test]
    fn an_untracked_nested_repository_leaves_the_tree_unknown() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            assert!(git_command(dir).args(args).status().unwrap().success());
        };
        fs::write(root.join("a.txt"), "a\n").unwrap();
        git(&root, &["init", "--quiet"]);
        let empty = root.join("empty.patch");
        fs::write(&empty, "").unwrap();
        let tree = merged_tree(&root, &SourceKind::Git, &empty).unwrap();
        assert!(tree.is_some());
        assert_eq!(workspace_tree(&root).unwrap(), tree);

        let nested = root.join("vendor");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("b.txt"), "b\n").unwrap();
        git(&nested, &["init", "--quiet"]);
        assert_eq!(merged_tree(&root, &SourceKind::Git, &empty).unwrap(), None);
        assert_eq!(workspace_tree(&root).unwrap(), None);
    }

    #[test]
    fn converting_attributes_or_config_leave_the_tree_unknown() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let git = |args: &[&str]| {
            assert!(git_command(&root).args(args).status().unwrap().success());
        };
        fs::write(root.join("a.txt"), "a\n").unwrap();
        git(&["init", "--quiet"]);
        let tree = || workspace_tree(&root).unwrap();
        for (attributes, known) in [
            ("*.png binary\n", true),
            ("*.txt -text\n", true),
            ("*.png text\n", true),
            ("*.txt text\n", false),
            ("* text=auto\n", false),
            ("* eol=lf\n", false),
            ("*.txt filter=lfs\n", false),
            ("* ident\n", false),
        ] {
            fs::write(root.join(".gitattributes"), attributes).unwrap();
            assert_eq!(tree().is_some(), known, "{attributes}");
        }
        fs::remove_file(root.join(".gitattributes")).unwrap();
        for (setting, known) in [
            (["core.autocrlf", "false"], true),
            (["core.autocrlf", "input"], false),
            (["core.autocrlf", "true"], false),
        ] {
            git(&["config", setting[0], setting[1]]);
            assert_eq!(tree().is_some(), known, "{setting:?}");
        }
        git(&["config", "--unset", "core.autocrlf"]);
        git(&["config", "core.eol", "crlf"]);
        assert_eq!(tree(), None);
    }

    #[test]
    fn only_a_fresh_entry_for_the_same_key_with_its_logs_is_reused() {
        let temp = tempfile::tempdir().unwrap();
        let run_dir = temp.path().join("runs").join("R1");
        let logs = run_dir.join("checks");
        fs::create_dir_all(&logs).unwrap();
        let key = keyed("t1", &["true"]);
        assert!(lookup(&run_dir, &key).is_none());

        store(&run_dir, &key, "R1", &logs).unwrap();
        assert!(
            temp.path()
                .join("memo")
                .join(format!("{}.json", key.digest))
                .is_file()
        );
        let entry = lookup(&run_dir, &key).unwrap();
        assert_eq!(
            (entry.run_id.as_str(), entry.logs.as_path()),
            ("R1", logs.as_path())
        );
        assert!(entry.describe().starts_with("passed, reused from R1 at "));
        assert!(lookup(&run_dir, &keyed("t1", &["false"])).is_none());

        // Expired.
        let path = entry_path(&run_dir, &key);
        let mut stale = entry.clone();
        stale.at = Utc::now() - Duration::hours(LIFETIME_HOURS) - Duration::minutes(1);
        fs::write(&path, serde_json::to_vec(&stale).unwrap()).unwrap();
        assert!(lookup(&run_dir, &key).is_none());

        // Its logs are gone.
        store(&run_dir, &key, "R1", &logs).unwrap();
        fs::remove_dir_all(&logs).unwrap();
        assert!(lookup(&run_dir, &key).is_none());

        clear(&State {
            root: temp.path().to_owned(),
        })
        .unwrap();
        assert!(!temp.path().join("memo").exists());
    }
}
