//! Hermes Agent's worktrees and its session-end hook. Observed with Hermes
//! Agent 0.18.2 and read from its source: `hermes chat --worktree` makes
//! `<repo>/.worktrees/hermes-<8 hex>` on a new branch `hermes/hermes-<same
//! 8 hex>`, and locks it at once with `hermes pid=<pid>`, its own process,
//! with no start time. When the session ends Hermes runs its
//! `on_session_finalize` hooks, then removes the worktree and its branch,
//! uncommitted changes and all, unless it holds unpushed commits (`removes`).
//! The hook gets the main checkout as its payload's `cwd` and the worktree
//! only as `TERMINAL_CWD`. Nothing here lists or walks a directory: it tests
//! one path Git gave.
//!
//! `on_session_finalize` also fires on `/new` (reason `session_boundary`),
//! and from the TUI with no reason, while the worktree lives on: only the
//! `shutdown` of the process that locked the worktree, the hook's parent, is
//! the end before removal.

use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::Deserialize;

use super::{
    EventKind, Hook, RuntimeEvent,
    claude::{Liveness, Output},
};
use crate::{source, state::State};

/// The worktree's name, `hermes-<id>`, which is its session as far as
/// discovery can tell, if Hermes made the worktree at `worktree` (canonical,
/// as Git listed it, on `branch`) for the project at `root`.
pub fn session(root: &Path, worktree: &Path, branch: Option<&str>) -> Option<String> {
    let name = worktree.file_name()?.to_str()?;
    let id = name.strip_prefix("hermes-")?;
    (id.len() == 8
        && id
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        && worktree == root.join(".worktrees").join(name)
        && branch == Some(&format!("refs/heads/hermes/{name}")))
    .then(|| name.to_owned())
}

/// Whether Hermes made `worktree` for `root`, from the branch its Git
/// metadata names, read without running Git.
pub fn made(root: &Path, worktree: &Path) -> bool {
    let head = read_gitdir(worktree, "HEAD");
    let branch = head.as_deref().and_then(|head| head.strip_prefix("ref: "));
    session(root, worktree, branch).is_some()
}

/// The pid in a `hermes pid=<pid>` lock reason.
pub fn lock_pid(reason: &str) -> Option<u32> {
    let pid = reason.strip_prefix("hermes pid=")?;
    pid.bytes()
        .all(|byte| byte.is_ascii_digit())
        .then(|| pid.parse().ok())?
}

/// The pid that `worktree`'s lock names, if Hermes's lock is on it.
pub fn locked_pid(worktree: &Path) -> Option<u32> {
    lock_pid(&read_gitdir(worktree, "locked")?)
}

/// Whether Hermes's process `pid` still runs in `worktree`. The lock has no
/// start time, so a recycled pid is told apart by when its process started:
/// no later than Git made the worktree (its `.git` file's time), with a
/// second's slack for `ps`'s whole-second start times.
pub fn liveness(pid: u32, worktree: &Path) -> Liveness {
    if !crate::process::pid_exists(pid) {
        return Liveness::Gone;
    }
    let made = fs::metadata(worktree.join(".git")).and_then(|metadata| metadata.modified());
    let started = super::claude::process_start_utc(pid).and_then(|start| {
        NaiveDateTime::parse_from_str(&start, "%a %b %e %H:%M:%S %Y")
            .ok()
            .map(|start| start.and_utc())
    });
    match (started, made.map(DateTime::<Utc>::from)) {
        (Some(started), Ok(made)) if started <= made + Duration::from_secs(1) => Liveness::Running,
        (Some(_), Ok(_)) => Liveness::Gone,
        _ => Liveness::Unknown,
    }
}

fn read_gitdir(worktree: &Path, name: &str) -> Option<String> {
    let text = fs::read_to_string(super::claude::linked_gitdir(worktree)?.join(name)).ok()?;
    Some(text.trim().to_owned()).filter(|text| !text.is_empty())
}

#[derive(Deserialize)]
struct Input {
    hook_event_name: String,
    /// Where Hermes runs: the main checkout, not the worktree.
    cwd: PathBuf,
    #[serde(default)]
    extra: serde_json::Value,
}

/// `dispatch hook hermes`, with the hook's `TERMINAL_CWD`. Only the
/// `shutdown` `on_session_finalize` of the process that locked a Hermes
/// worktree of the payload's repository is acted on: the exact Δ is kept if
/// Hermes is about to remove the worktree, the session's end recorded if it
/// keeps it. Anything else is silent. Hermes removes the worktree whatever
/// the hook does, so a failure is reported on stderr with a failing status,
/// and the Δ the owner last saw is what remains.
pub fn handle(state: &State, input: &[u8], terminal_cwd: Option<&OsStr>) -> Output {
    let (stderr, code) = match finalize(state, input, terminal_cwd, parent_pid()) {
        Ok(()) => (String::new(), 0),
        Err(error) => (
            format!("Dispatch could not keep this Hermes worktree's changes: {error:#}"),
            1,
        ),
    };
    Output {
        stdout: String::new(),
        stderr,
        code,
    }
}

fn finalize(
    state: &State,
    input: &[u8],
    terminal_cwd: Option<&OsStr>,
    parent: Option<u32>,
) -> Result<()> {
    anyhow::ensure!(
        input.len() <= super::MAX_INPUT_BYTES,
        "hook input is larger than {} bytes",
        super::MAX_INPUT_BYTES
    );
    let input: Input = serde_json::from_slice(input).context("unreadable hook input")?;
    // `on_session_end` ends every turn, and `/new` finalizes a session with
    // reason `session_boundary`: only the process's shutdown comes before
    // the worktree's removal.
    if input.hook_event_name != "on_session_finalize" || input.extra["reason"] != "shutdown" {
        return Ok(());
    }
    let Some((worktree, name)) = terminal_cwd
        .map(Path::new)
        .map(|worktree| hermes_worktree(&input.cwd, worktree))
        .transpose()?
        .flatten()
    else {
        return Ok(());
    };
    // The process about to remove it is the one that locked it.
    if parent.is_none() || locked_pid(&worktree) != parent {
        return Ok(());
    }
    let kind = if removes(&worktree) {
        EventKind::WorkspaceRemoved
    } else {
        EventKind::End {
            reason: "shutdown".into(),
        }
    };
    let event = RuntimeEvent {
        provider: "hermes",
        session_id: name,
        cwd: worktree,
        model: None,
        kind,
    };
    super::ingest(state, event).map(|_| ())
}

/// The worktree at `worktree` and its name, if it is a Hermes worktree of the
/// repository at `cwd`. Both come from hook input: neither is trusted until
/// Git says they are one repository and the worktree has Hermes's shape.
fn hermes_worktree(cwd: &Path, worktree: &Path) -> Result<Option<(PathBuf, String)>> {
    if !cwd.is_absolute() || !worktree.is_absolute() {
        return Ok(None);
    }
    let (Ok(cwd), Ok(worktree)) = (fs::canonicalize(cwd), fs::canonicalize(worktree)) else {
        return Ok(None);
    };
    if !cwd.is_dir() || !worktree.is_dir() {
        return Ok(None);
    }
    let (Some(repository), Some(theirs)) = (
        source::repo_identity(&cwd)?,
        source::repo_identity(&worktree)?,
    ) else {
        return Ok(None);
    };
    if theirs.key != repository.key || source::checkout_top(&worktree)?.as_ref() != Some(&worktree)
    {
        return Ok(None);
    }
    let mut head = source::git_command(&worktree);
    head.args(["symbolic-ref", "-q", "HEAD"]);
    let output = head.output().context("failed to run Git")?;
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let branch = output.status.success().then_some(branch.as_str());
    Ok(session(&repository.main_worktree, &worktree, branch).map(|name| (worktree, name)))
}

#[cfg(unix)]
fn parent_pid() -> Option<u32> {
    Some(std::os::unix::process::parent_id())
}

#[cfg(not(unix))]
fn parent_pid() -> Option<u32> {
    None
}

/// Whether Hermes removes `worktree` once its hooks return, by Hermes's own
/// rule (Hermes Agent 0.18.2, `_worktree_has_unpushed_commits` and
/// `_cleanup_worktree` in its `cli.py`): it keeps a worktree whose `HEAD` has
/// commits on no remote-tracking ref. A repository with no remote-tracking
/// refs has none, and a Git command that fails counts as having some.
fn removes(worktree: &Path) -> bool {
    let printed = |args: &[&str]| {
        let mut git = source::git_command(worktree);
        git.args(args);
        let output = git.output().ok().filter(|output| output.status.success())?;
        Some(!output.stdout.trim_ascii().is_empty())
    };
    match printed(&["for-each-ref", "--format=%(refname)", "refs/remotes"]) {
        Some(false) => true,
        Some(true) => printed(&["log", "--oneline", "HEAD", "--not", "--remotes"]) == Some(false),
        None => false,
    }
}

/// Where Hermes keeps its configuration: `$HERMES_HOME/config.yaml`, or
/// `~/.hermes/config.yaml`.
pub fn config_path() -> Result<PathBuf> {
    let home = std::env::var("HERMES_HOME")
        .ok()
        .map(|home| home.trim().to_owned())
        .filter(|home| !home.is_empty());
    let home = match home {
        Some(home) => PathBuf::from(home),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".hermes"),
    };
    Ok(home.join("config.yaml"))
}

/// The command Hermes runs, quoted to survive its `shlex` split: this
/// Dispatch, this state, `hook hermes`.
pub fn hook_command(state: &State) -> Result<String> {
    let quote = |value: &Path| format!("'{}'", value.to_string_lossy().replace('\'', r"'\''"));
    Ok(format!(
        "{} --state-dir {} hook hermes",
        quote(&std::env::current_exe()?),
        quote(&state.root)
    ))
}

/// The first line of the block Dispatch appends, which is how it finds it.
const MARKER: &str = "# Added by dispatch setup, which removes it unless it is edited.";

/// The hook's timeout: Hermes's most, which covers the minute keeping a
/// removed worktree's Δ may wait for its registration and the minute it may
/// wait for the Work's lock.
const TIMEOUT: u64 = 300;

/// The lines under `hooks:` that run `command` when a session ends.
fn entry(command: &str) -> Result<String> {
    anyhow::ensure!(
        !command.chars().any(char::is_control),
        "the hook command has a control character"
    );
    let quoted = command.replace('\\', r"\\").replace('"', "\\\"");
    Ok(format!(
        "  on_session_finalize:\n    - command: \"{quoted}\"\n      timeout: {TIMEOUT}\n"
    ))
}

/// What Dispatch appends: a newline, so the file's last line ends whether it
/// did or not, then its block.
fn block(command: &str) -> Result<String> {
    Ok(format!("\n{MARKER}\nhooks:\n{}", entry(command)?))
}

/// What installing does to the configuration.
pub enum Addition {
    /// Appends this, and changes nothing else.
    Append(String),
    /// The configuration has a `hooks:` key of its own, so Dispatch leaves it
    /// alone: these are the lines to add under it.
    ByHand(String),
}

/// What installing the hook running `command` would do to the configuration
/// at `path`. It is read only to find a `hooks:` key, never rewritten.
pub fn addition(path: &Path, command: &str) -> Result<Addition> {
    let config = parse(read(path)?.as_deref().unwrap_or(""), path)?;
    if config.get("hooks").is_some() {
        return Ok(Addition::ByHand(entry(command)?));
    }
    anyhow::ensure!(
        config.is_mapping() || config.is_null(),
        "{} is not a YAML mapping; it was left untouched",
        path.display()
    );
    Ok(Addition::Append(block(command)?))
}

/// Dispatch's hook in the configuration at `path`: exact for the block
/// Dispatch appended, changed for any other `… hook hermes` hook.
pub fn hook(path: &Path) -> Result<Hook> {
    let text = read(path)?.unwrap_or_default();
    let config = parse(&text, path)?;
    if let Some((_, command)) = ours(&text) {
        return Ok(Hook::Exact(command));
    }
    let entries = config["hooks"]["on_session_finalize"].as_sequence();
    Ok(
        if entries.is_some_and(|entries| {
            entries.iter().any(|entry| {
                entry["command"]
                    .as_str()
                    .is_some_and(|command| command.ends_with(" hook hermes"))
            })
        }) {
            Hook::Changed
        } else {
            Hook::Absent
        },
    )
}

/// Append Dispatch's hook to the configuration at `path`, every byte of it
/// kept. Returns the backup of the previous configuration, if there was one.
pub fn install(path: &Path, command: &str) -> Result<Option<PathBuf>> {
    let Addition::Append(block) = addition(path, command)? else {
        anyhow::bail!(
            "{} has a hooks: key of its own; add the hook there by hand",
            path.display()
        );
    };
    let existing = read(path)?;
    let text = format!("{}{block}", existing.as_deref().unwrap_or(""));
    // What Hermes will read: the one hook, running exactly this command.
    let config = parse(&text, path)?;
    let added = &config["hooks"]["on_session_finalize"][0];
    anyhow::ensure!(
        added["command"].as_str() == Some(command) && added["timeout"].as_u64() == Some(TIMEOUT),
        "the hook would not read back as written; {} was left untouched",
        path.display()
    );
    write(path, existing.as_deref(), &text)
}

/// Change only the command in a hook block Dispatch previously appended.
/// Edited blocks and hooks the person added by hand remain theirs to update.
pub fn update(path: &Path, command: &str) -> Result<Option<PathBuf>> {
    let existing = read(path)?.unwrap_or_default();
    let (range, _) = ours(&existing).with_context(|| {
        format!(
            "Dispatch's hook in {} is not as Dispatch added it; update it there yourself",
            path.display()
        )
    })?;
    let mut text = existing.clone();
    text.replace_range(range, &block(command)?);
    let config = parse(&text, path)?;
    anyhow::ensure!(
        config["hooks"]["on_session_finalize"][0]["command"].as_str() == Some(command),
        "the hook would not read back as written; {} was left untouched",
        path.display()
    );
    write(path, Some(&existing), &text)
}

/// Remove exactly the block Dispatch appended from the configuration at
/// `path`, refusing if it was edited. Returns the backup.
pub fn uninstall(path: &Path) -> Result<Option<PathBuf>> {
    let existing = read(path)?.unwrap_or_default();
    let (range, _) = ours(&existing).with_context(|| {
        format!(
            "Dispatch's hook in {} is not as Dispatch added it; it was edited or added by \
             hand, so remove it there yourself",
            path.display()
        )
    })?;
    let mut text = existing.clone();
    text.replace_range(range, "");
    write(path, Some(&existing), &text)
}

/// Where in `text` the block Dispatch appended is, exactly as Dispatch wrote
/// it for some `… hook hermes` command, with nothing nested under it since;
/// and that command.
fn ours(text: &str) -> Option<(std::ops::Range<usize>, String)> {
    let header = format!("\n{MARKER}\nhooks:\n  on_session_finalize:\n    - command: ");
    let start = text.find(&header)?;
    let line = text[start + header.len()..].lines().next()?;
    let command: String = serde_yaml::from_str(line).ok()?;
    let block = block(&command).ok()?;
    let end = start + block.len();
    let after = text.get(end..)?;
    (command.ends_with(" hook hermes")
        && text[start..].starts_with(&block)
        && !after.starts_with([' ', '\t', '-']))
    .then_some((start..end, command))
}

/// The configuration's text, `None` when there is no file.
fn read(path: &Path) -> Result<Option<String>> {
    match fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes)
            .map(Some)
            .with_context(|| format!("{} is not UTF-8; it was left untouched", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn parse(text: &str, path: &Path) -> Result<serde_yaml::Value> {
    serde_yaml::from_str(text).with_context(|| {
        format!(
            "{} is not valid YAML; it was left untouched",
            path.display()
        )
    })
}

/// Write `text` over the configuration at `path`, through a symbolic link if
/// it is one, after keeping a backup of `existing` beside it.
fn write(path: &Path, existing: Option<&str>, text: &str) -> Result<Option<PathBuf>> {
    let backup = match existing {
        Some(existing) => {
            let backup = path.with_extension(format!(
                "yaml.dispatch-backup-{}",
                Utc::now().format("%Y%m%d%H%M%S")
            ));
            // Durably, and readable only by its owner, as Hermes keeps it.
            crate::state::write_durably(&backup, existing.as_bytes())
                .with_context(|| format!("failed to keep a backup at {}", backup.display()))?;
            Some(backup)
        }
        None => None,
    };
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    crate::state::write_durably(&target, text.as_bytes())?;
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_its_own_place_on_its_own_branch_is_a_hermes_worktree() {
        let root = Path::new("/code/repo");
        let made = root.join(".worktrees/hermes-dbc09083");
        let branch = Some("refs/heads/hermes/hermes-dbc09083");
        assert_eq!(
            session(root, &made, branch).as_deref(),
            Some("hermes-dbc09083")
        );
        assert_eq!(
            session(root, &made, Some("refs/heads/hermes-dbc09083")),
            None
        );
        assert_eq!(
            session(root, &made, Some("refs/heads/hermes/hermes-0bc09083")),
            None
        );
        assert_eq!(session(root, &made, None), None);
        // Another directory, or another repository's.
        for (worktree, branch) in [
            (root.join("worktrees/hermes-dbc09083"), branch),
            (root.join(".worktrees/x/hermes-dbc09083"), branch),
            (
                PathBuf::from("/code/other/.worktrees/hermes-dbc09083"),
                branch,
            ),
        ] {
            assert_eq!(session(root, &worktree, branch), None, "{worktree:?}");
        }
        // Exactly 8 lowercase hex digits.
        for id in ["dbc0908", "dbc090833", "DBC09083", "dbc0908g"] {
            let name = format!("hermes-{id}");
            let branch = format!("refs/heads/hermes/{name}");
            let worktree = root.join(".worktrees").join(&name);
            assert_eq!(session(root, &worktree, Some(&branch)), None, "{id}");
        }
    }

    #[test]
    fn a_hermes_lock_names_its_pid_and_anything_else_names_none() {
        assert_eq!(lock_pid("hermes pid=83166"), Some(83166));
        for other in [
            "hermes pid=",
            "hermes pid=+1",
            "hermes pid=83166 ",
            "hermes pid=x",
            "claude session x (pid 1 start y)",
            "moving it",
        ] {
            assert_eq!(lock_pid(other), None, "{other}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_live_process_older_than_the_worktree_runs_and_any_other_does_not() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path();
        let git = worktree.join(".git");
        fs::write(&git, "gitdir: /nowhere\n").unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("600")
            .spawn()
            .unwrap();
        // Made after the process started.
        let now = std::time::SystemTime::now() + Duration::from_secs(5);
        fs::File::options()
            .write(true)
            .open(&git)
            .unwrap()
            .set_modified(now)
            .unwrap();
        assert_eq!(liveness(child.id(), worktree), Liveness::Running);
        // Made well before it: its pid belongs to another process now.
        let then = std::time::SystemTime::now() - Duration::from_secs(3600);
        fs::File::options()
            .write(true)
            .open(&git)
            .unwrap()
            .set_modified(then)
            .unwrap();
        assert_eq!(liveness(child.id(), worktree), Liveness::Gone);
        // No `.git` time to compare with.
        fs::remove_file(&git).unwrap();
        assert_eq!(liveness(child.id(), worktree), Liveness::Unknown);
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(liveness(child.id(), worktree), Liveness::Gone);
    }

    const COMMAND: &str =
        "'/Applications/my \"apps\"/dispatch' --state-dir '/s/it'\\''s' hook hermes";

    const THEIRS: &str = "# Hermes config\nmodel:\n  default: gpt-6.1-sol  # theirs\n\
                          terminal:\n  cwd: .\n# trailing comment, no newline";

    #[test]
    fn installing_appends_one_block_and_removing_restores_every_byte() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.yaml");
        for original in [THEIRS, "model:\n  default: x\n", ""] {
            fs::write(&path, original).unwrap();
            assert_eq!(hook(&path).unwrap(), Hook::Absent);
            let backup = install(&path, COMMAND).unwrap().unwrap();
            assert_eq!(fs::read_to_string(&backup).unwrap(), original);
            fs::remove_file(backup).unwrap();
            let text = fs::read_to_string(&path).unwrap();
            assert!(text.starts_with(original), "{text}");
            assert_eq!(hook(&path).unwrap(), Hook::Exact(COMMAND.into()));
            // As Hermes reads it: one `on_session_finalize` hook, whose
            // command splits into this Dispatch's arguments.
            let config: serde_yaml::Value = serde_yaml::from_str(&text).unwrap();
            let hooks = config["hooks"]["on_session_finalize"]
                .as_sequence()
                .unwrap();
            assert_eq!(hooks.len(), 1);
            assert_eq!(hooks[0]["command"].as_str(), Some(COMMAND));
            assert_eq!(hooks[0]["timeout"].as_u64(), Some(300));
            assert_eq!(
                shell_words::split(COMMAND).unwrap(),
                [
                    "/Applications/my \"apps\"/dispatch",
                    "--state-dir",
                    "/s/it's",
                    "hook",
                    "hermes"
                ]
            );
            if !original.is_empty() {
                assert_eq!(
                    config["model"],
                    serde_yaml::from_str::<serde_yaml::Value>(original).unwrap()["model"]
                );
            }
            uninstall(&path).unwrap().unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), original);
        }
    }

    #[test]
    fn a_config_with_hooks_of_its_own_is_left_alone() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.yaml");
        let theirs = "hooks:\n  on_session_start:\n    - command: echo hi\n";
        fs::write(&path, theirs).unwrap();
        let Addition::ByHand(lines) = addition(&path, COMMAND).unwrap() else {
            panic!("a hooks: key is changed by hand");
        };
        assert!(lines.contains("on_session_finalize:") && lines.contains("hook hermes"));
        assert!(install(&path, COMMAND).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), theirs);
        // Added by hand as shown: installed, but not Dispatch's to remove.
        fs::write(&path, format!("{theirs}{lines}")).unwrap();
        assert_eq!(hook(&path).unwrap(), Hook::Changed);
        assert!(uninstall(&path).is_err());
        // Not YAML: untouched.
        fs::write(&path, "model: [\n").unwrap();
        assert!(
            install(&path, COMMAND)
                .unwrap_err()
                .to_string()
                .contains("left untouched")
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "model: [\n");
        assert!(fs::read_dir(temp.path()).unwrap().count() == 1, "no backup");
    }

    #[test]
    fn an_edited_block_is_not_removed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.yaml");
        fs::write(&path, "model: x\n").unwrap();
        install(&path, COMMAND).unwrap();
        let installed_text = fs::read_to_string(&path).unwrap();
        for edited in [
            installed_text.replace("timeout: 300", "timeout: 30"),
            format!("{installed_text}  on_session_start:\n    - command: echo hi\n"),
            format!("{installed_text}    - command: echo hi\n"),
        ] {
            fs::write(&path, &edited).unwrap();
            assert_eq!(hook(&path).unwrap(), Hook::Changed, "{edited}");
            assert!(uninstall(&path).is_err(), "{edited}");
            assert_eq!(fs::read_to_string(&path).unwrap(), edited);
        }
        // Something added after it at the top level is kept.
        fs::write(&path, format!("{installed_text}display:\n  skin: x\n")).unwrap();
        uninstall(&path).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "model: x\ndisplay:\n  skin: x\n"
        );
    }

    #[test]
    fn installing_into_no_config_creates_it_and_never_an_allowlist() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.yaml");
        assert!(install(&path, COMMAND).unwrap().is_none());
        assert_eq!(hook(&path).unwrap(), Hook::Exact(COMMAND.into()));
        let names: Vec<_> = fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["config.yaml"]);
    }
}
