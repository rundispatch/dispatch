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
//!
//! `pre_llm_call` runs once per turn, before the model is called: its
//! `{"context": …}` reply is added to that turn's user message, which is how
//! pending notes reach a Hermes agent. Its payload carries the whole
//! conversation.
//!
//! `pre_verify` runs at the end of a turn that edited files, with the turn's
//! `extra.attempt`, 0 the first time: a `{"action": "continue", "message":
//! …}` reply keeps the turn going with the message, at most
//! `agent.max_verify_nudges` times (3 by default). Its payload carries the
//! turn's final response. An ask to catch up that no edited turn takes comes
//! with the next turn's `pre_llm_call`.

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

/// `dispatch hook hermes`, with the hook's `TERMINAL_CWD`. `pre_llm_call`
/// and `pre_verify` deliver what waits for the worktree. Otherwise only the
/// `shutdown` `on_session_finalize` of the process that locked a Hermes
/// worktree of the payload's repository is acted on: the exact Δ is kept if
/// Hermes is about to remove the worktree, the session's end recorded if it
/// keeps it. Anything else is silent. Hermes removes the worktree whatever
/// the hook does, so a failure is reported on stderr with a failing status,
/// and the Δ the owner last saw is what remains.
pub fn handle(state: &State, input: &[u8], terminal_cwd: Option<&OsStr>) -> Output {
    // From the payload's head alone, which names the event before its
    // conversation or final response, however long that has grown.
    let event = super::head(input).remove("hook_event_name");
    let worktree = terminal_cwd.map(Path::new);
    let reply = match event.as_ref().and_then(serde_json::Value::as_str) {
        Some("pre_llm_call") => {
            // An ask no edited turn took comes after the notes.
            let notes = worktree.and_then(|cwd| super::deliver(state, cwd, "pre_llm_call"));
            let ask = worktree.and_then(|cwd| super::catch_up(state, cwd, "turn"));
            let context: Vec<String> = notes.into_iter().chain(ask).collect();
            (!context.is_empty()).then(|| serde_json::json!({"context": context.join("\n")}))
        }
        // Only the turn's first verify: the nudges after it are the person's.
        Some("pre_verify") if attempt(input) == Some(0) => worktree
            .and_then(|cwd| super::catch_up(state, cwd, "verify"))
            .map(|ask| serde_json::json!({"action": "continue", "message": ask})),
        Some("pre_verify") => None,
        _ => {
            let (stderr, code) = match finalize(state, input, terminal_cwd, parent_pid()) {
                Ok(()) => (String::new(), 0),
                Err(error) => (
                    format!("Dispatch could not keep this Hermes worktree's changes: {error:#}"),
                    1,
                ),
            };
            return Output {
                stdout: String::new(),
                stderr,
                code,
            };
        }
    };
    Output {
        stdout: reply.map_or_else(String::new, |reply| reply.to_string()),
        stderr: String::new(),
        code: 0,
    }
}

/// `extra.attempt` of a `pre_verify` payload, read without what follows it:
/// Hermes sends it before the turn's final response, however long that is.
fn attempt(input: &[u8]) -> Option<u64> {
    use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, MapAccess, Visitor};
    /// The number at the path `.0` in the JSON object being read, put in
    /// `.1`; the fields before each step are skipped, and reading stops there.
    struct At<'a>(&'a [&'a str], &'a mut Option<u64>);
    impl<'de> DeserializeSeed<'de> for At<'_> {
        type Value = ();
        fn deserialize<D: Deserializer<'de>>(self, input: D) -> Result<(), D::Error> {
            if self.0.is_empty() {
                *self.1 = Some(u64::deserialize(input)?);
                return Ok(());
            }
            input.deserialize_map(self)
        }
    }
    impl<'de> Visitor<'de> for At<'_> {
        type Value = ();
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a JSON object")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
            let (field, rest) = self.0.split_first().expect("a field to read");
            while let Some(key) = map.next_key::<String>()? {
                if key == *field {
                    return map.next_value_seed(At(rest, self.1));
                }
                map.next_value::<IgnoredAny>()?;
            }
            Ok(())
        }
    }
    let mut found = None;
    let mut input = serde_json::Deserializer::from_slice(input);
    // Stopping inside an object reads as an error once the field is read.
    let _ = At(&["extra", "attempt"], &mut found).deserialize(&mut input);
    found
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

/// The hooks Dispatch installs, each with its timeout. `on_session_finalize`
/// has Hermes's most, which covers the minute keeping a removed worktree's Δ
/// may wait for its registration and the minute it may wait for the Work's
/// lock; `pre_llm_call` and `pre_verify` only rename files.
const INSTALLED: &[(&str, u64)] = &[
    ("on_session_finalize", 300),
    ("pre_llm_call", 5),
    ("pre_verify", 5),
];

/// The hooks earlier Dispatches installed, the most first: 0.4.18's, and the
/// one 0.4.15 to 0.4.17 installed.
const EARLIER: &[&[(&str, u64)]] = &[
    &[("on_session_finalize", 300), ("pre_llm_call", 5)],
    &[("on_session_finalize", 300)],
];

/// The lines under `hooks:` that run `command` for each hook of `set`.
fn entries(command: &str, set: &[(&str, u64)]) -> Result<String> {
    anyhow::ensure!(
        !command.chars().any(char::is_control),
        "the hook command has a control character"
    );
    let quoted = command.replace('\\', r"\\").replace('"', "\\\"");
    Ok(set
        .iter()
        .map(|(event, timeout)| {
            format!("  {event}:\n    - command: \"{quoted}\"\n      timeout: {timeout}\n")
        })
        .collect())
}

/// What Dispatch appends: a newline, so the file's last line ends whether it
/// did or not, then its block.
fn block(command: &str, set: &[(&str, u64)]) -> Result<String> {
    Ok(format!("\n{MARKER}\nhooks:\n{}", entries(command, set)?))
}

/// What installing does to the configuration.
pub enum Addition {
    /// Appends this, and changes nothing else.
    Append(String),
    /// The configuration has a `hooks:` key of its own, so Dispatch leaves it
    /// alone: these are the lines to add under it.
    ByHand(String),
}

/// What installing the hooks running `command` would do to the configuration
/// at `path`. It is read only to find a `hooks:` key, never rewritten.
pub fn addition(path: &Path, command: &str) -> Result<Addition> {
    let config = parse(read(path)?.as_deref().unwrap_or(""), path)?;
    if config.get("hooks").is_some() {
        return Ok(Addition::ByHand(entries(command, INSTALLED)?));
    }
    anyhow::ensure!(
        config.is_mapping() || config.is_null(),
        "{} is not a YAML mapping; it was left untouched",
        path.display()
    );
    Ok(Addition::Append(block(command, INSTALLED)?))
}

/// Dispatch's entries under `hooks:`, running `command`, as they should be.
pub fn wanted(command: &str) -> Result<String> {
    entries(command, INSTALLED)
}

/// Whether the block Dispatch appended to the configuration at `path` is
/// there as some Dispatch wrote it, so that `update` and `uninstall` can
/// change it. Hermes's own `hermes config set` rewrites the whole file, and
/// then Dispatch's hooks are the person's to change.
pub fn as_written(path: &Path) -> Result<bool> {
    Ok(ours(&read(path)?.unwrap_or_default()).is_some())
}

/// Dispatch's hooks in the configuration at `path`, by what Hermes reads
/// there: exact when they are just the hooks Dispatch installs, earlier when
/// they are just those an earlier Dispatch did, each running one command
/// with nothing else in its entry, and changed for any other `… hook hermes`
/// hook. Hermes rewriting the file in its own form changes none of that.
pub fn hook(path: &Path) -> Result<Hook> {
    found(&read(path)?.unwrap_or_default(), path)
}

fn found(text: &str, path: &Path) -> Result<Hook> {
    let config = parse(text, path)?;
    let mut ours = serde_yaml::Mapping::new();
    for (event, hooks) in config["hooks"].as_mapping().into_iter().flatten() {
        let mine: Vec<serde_yaml::Value> = hooks
            .as_sequence()
            .into_iter()
            .flatten()
            .filter(|hook| {
                hook["command"]
                    .as_str()
                    .is_some_and(|command| command.ends_with(" hook hermes"))
            })
            .cloned()
            .collect();
        if !mine.is_empty() {
            ours.insert(event.clone(), mine.into());
        }
    }
    let Some(command) = ours
        .values()
        .next()
        .map(|hooks| hooks[0]["command"].clone())
    else {
        return Ok(Hook::Absent);
    };
    let command = command.as_str().unwrap_or_default().to_owned();
    let ours = serde_yaml::Value::Mapping(ours);
    let written = |set| -> Result<bool> {
        let hooks = parse(&format!("hooks:\n{}", entries(&command, set)?), path)?;
        Ok(hooks["hooks"] == ours)
    };
    if written(INSTALLED)? {
        return Ok(Hook::Exact(command));
    }
    for set in EARLIER {
        if written(set)? {
            return Ok(Hook::Earlier(command));
        }
    }
    Ok(Hook::Changed)
}

/// Append Dispatch's hooks to the configuration at `path`, every byte of it
/// kept. Returns the backup of the previous configuration, if there was one.
pub fn install(path: &Path, command: &str) -> Result<Option<PathBuf>> {
    let Addition::Append(block) = addition(path, command)? else {
        anyhow::bail!(
            "{} has a hooks: key of its own; add the hooks there by hand",
            path.display()
        );
    };
    let existing = read(path)?;
    let text = format!("{}{block}", existing.as_deref().unwrap_or(""));
    write(path, existing.as_deref(), &text, command)
}

/// Make the block Dispatch previously appended, the current one or an
/// earlier Dispatch's, run `command` with the hooks Dispatch installs now.
/// Edited blocks and hooks the person added by hand remain theirs to update.
pub fn update(path: &Path, command: &str) -> Result<Option<PathBuf>> {
    let existing = read(path)?.unwrap_or_default();
    let (range, _) = ours(&existing).with_context(|| {
        format!(
            "Dispatch's hooks in {} are not as Dispatch added them; update them there yourself",
            path.display()
        )
    })?;
    let mut text = existing.clone();
    text.replace_range(range, &block(command, INSTALLED)?);
    write(path, Some(&existing), &text, command)
}

/// Remove exactly the block Dispatch appended, the current one or an earlier
/// Dispatch's, from the configuration at `path`, refusing if it was edited.
/// Returns the backup.
pub fn uninstall(path: &Path) -> Result<Option<PathBuf>> {
    let existing = read(path)?.unwrap_or_default();
    let (range, _) = ours(&existing).with_context(|| {
        format!(
            "Dispatch's hooks in {} are not as Dispatch added them; they were edited or \
             added by hand, so remove them there yourself",
            path.display()
        )
    })?;
    let mut text = existing.clone();
    text.replace_range(range, "");
    write(path, Some(&existing), &text, "")
}

/// Where in `text` the block Dispatch appended is, exactly as some Dispatch
/// wrote it for some `… hook hermes` command, with nothing nested under it
/// since; and that command.
fn ours(text: &str) -> Option<(std::ops::Range<usize>, String)> {
    let header = format!("\n{MARKER}\nhooks:\n  on_session_finalize:\n    - command: ");
    let start = text.find(&header)?;
    let line = text[start + header.len()..].lines().next()?;
    let command: String = serde_yaml::from_str(line).ok()?;
    if !command.ends_with(" hook hermes") {
        return None;
    }
    // The most hooks first: a block with fewer hooks begins one with more.
    std::iter::once(INSTALLED)
        .chain(EARLIER.iter().copied())
        .find_map(|set| {
            let block = block(&command, set).ok()?;
            let end = start + block.len();
            let after = text.get(end..)?;
            (text[start..].starts_with(&block) && !after.starts_with([' ', '\t', '-']))
                .then(|| (start..end, command.clone()))
        })
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
/// it is one, after keeping a backup of `existing` beside it. Only if Hermes
/// will read back exactly Dispatch's hooks running `command`, or none for an
/// empty `command`.
fn write(
    path: &Path,
    existing: Option<&str>,
    text: &str,
    command: &str,
) -> Result<Option<PathBuf>> {
    let wanted = match command {
        "" => Hook::Absent,
        command => Hook::Exact(command.to_owned()),
    };
    anyhow::ensure!(
        found(text, path)? == wanted,
        "the hooks would not read back as written; {} was left untouched",
        path.display()
    );
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

    /// `pre_verify` as Hermes Agent 0.18.2 sent it (P0), in its order, paths
    /// replaced: the turn's `attempt`, then its final `response`.
    fn pre_verify(attempt: u64, response: &str) -> Vec<u8> {
        format!(
            "{{\"hook_event_name\": \"pre_verify\", \"tool_name\": null, \"tool_input\": null, \
             \"session_id\": \"20261109_203614_66344e\", \"cwd\": \"/r/repo\", \"extra\": \
             {{\"platform\": \"cli\", \"model\": \"gpt-6.1-sol\", \"coding\": false, \
             \"attempt\": {attempt}, \"final_response\": {}, \"changed_paths\": \
             [\"/r/repo/.worktrees/hermes-c1f297f3/notes.txt\"], \
             \"telemetry_schema_version\": \"hermes.observer.v1\"}}}}",
            serde_json::Value::from(response)
        )
        .into_bytes()
    }

    #[test]
    fn a_verifys_attempt_is_read_before_its_final_response_however_long() {
        let recorded = pre_verify(0, "Created notes.txt containing the single line: hello.");
        let parsed: serde_json::Value = serde_json::from_slice(&recorded).unwrap();
        assert_eq!(parsed["extra"]["attempt"], 0);
        assert_eq!(attempt(&recorded), Some(0));
        assert_eq!(attempt(&pre_verify(1, "Added ASPEN-5.")), Some(1));
        // Cut inside the response, as Dispatch's bounded read cuts a long one.
        let long = pre_verify(0, &"x".repeat(1024 * 1024));
        assert_eq!(attempt(&long[..super::super::MAX_INPUT_BYTES + 1]), Some(0));
        for other in [
            &b""[..],
            b"not json",
            b"{}",
            br#"{"extra": null}"#,
            br#"{"attempt": 0, "extra": {"platform": "cli"}}"#,
            br#"{"extra": {"attempt": "0"}}"#,
            br#"{"extra": {"attempt": -1}}"#,
        ] {
            assert_eq!(attempt(other), None, "{}", String::from_utf8_lossy(other));
        }
    }

    const ASK: &str = "Dispatch: Work 01AAAAAA (Claude Code) landed a change this Work relies on.";

    #[test]
    fn an_ask_keeps_only_the_first_verify_of_an_edited_turn_going() {
        let temp = tempfile::tempdir().unwrap();
        let state = State {
            root: temp.path().join("state"),
        };
        state.initialize().unwrap();
        let worktree = fs::canonicalize(temp.path())
            .unwrap()
            .join("repo/.worktrees/hermes-c1f297f3");
        fs::create_dir_all(worktree.join(".git")).unwrap();
        fs::create_dir_all(worktree.join("src")).unwrap();
        let verify = |attempt, response: &str, cwd: Option<&Path>| {
            let output = handle(
                &state,
                &pre_verify(attempt, response),
                cwd.map(Path::as_os_str),
            );
            assert_eq!((output.stderr.as_str(), output.code), ("", 0));
            output.stdout
        };
        let pending = crate::orchestrator::catchup::pending(&state, &worktree);
        let delivered = pending.with_file_name("delivered");
        fs::create_dir_all(&delivered).unwrap();
        // Nothing waits: the turn ends as Hermes would end it.
        assert_eq!(verify(0, "Done.", Some(&worktree)), "");
        let ask = serde_json::json!({"version": 1, "run_id": "r", "landings": [], "text": ASK});
        fs::write(&pending, ask.to_string()).unwrap();
        // Hermes gives the worktree only in `TERMINAL_CWD`, and the nudges
        // after a turn's first verify are the person's.
        assert_eq!(verify(0, "Done.", None), "");
        assert_eq!(verify(1, "Done.", Some(&worktree)), "");
        assert!(pending.exists());
        // The first, however long the turn's response: kept going, once.
        let long = "x".repeat(1024 * 1024);
        let reply = verify(0, &long, Some(&worktree.join("src")));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&reply).unwrap(),
            serde_json::json!({"action": "continue", "message": ASK})
        );
        assert_eq!(verify(0, "Done.", Some(&worktree)), "");
        let taken: Vec<String> = fs::read_dir(&delivered)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            matches!(&taken[..], [one] if one.ends_with(".verify.json")),
            "{taken:?}"
        );
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
            // As Hermes reads it: one hook for each event, whose command
            // splits into this Dispatch's arguments.
            let config: serde_yaml::Value = serde_yaml::from_str(&text).unwrap();
            let hooks = [
                ("on_session_finalize", 300),
                ("pre_llm_call", 5),
                ("pre_verify", 5),
            ];
            for (event, timeout) in hooks {
                let hooks = config["hooks"][event].as_sequence().unwrap();
                assert_eq!(hooks.len(), 1);
                assert_eq!(hooks[0]["command"].as_str(), Some(COMMAND));
                assert_eq!(hooks[0]["timeout"].as_u64(), Some(timeout));
            }
            assert_eq!(config["hooks"].as_mapping().unwrap().len(), 3);
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
        for event in ["on_session_finalize:", "pre_llm_call:", "pre_verify:"] {
            assert!(lines.contains(event), "{lines}");
        }
        assert!(install(&path, COMMAND).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), theirs);
        // Added by hand as shown: Dispatch's hooks, but not its bytes to
        // change.
        fs::write(&path, format!("{theirs}{lines}")).unwrap();
        assert_eq!(hook(&path).unwrap(), Hook::Exact(COMMAND.into()));
        assert!(!as_written(&path).unwrap());
        assert!(uninstall(&path).is_err() && update(&path, COMMAND).is_err());
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
        // A timeout of Dispatch's changed is changed. A hook of the person's
        // under Dispatch's `hooks:` leaves Dispatch's hooks as written, but
        // the block no longer Dispatch's alone to change.
        let exact = || Hook::Exact(COMMAND.into());
        for (edited, found) in [
            (
                installed_text.replace("timeout: 300", "timeout: 30"),
                Hook::Changed,
            ),
            (
                format!("{installed_text}  on_session_start:\n    - command: echo hi\n"),
                exact(),
            ),
            (format!("{installed_text}    - command: echo hi\n"), exact()),
        ] {
            fs::write(&path, &edited).unwrap();
            assert_eq!(hook(&path).unwrap(), found, "{edited}");
            assert!(uninstall(&path).is_err(), "{edited}");
            assert!(update(&path, COMMAND).is_err(), "{edited}");
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

    #[test]
    fn every_earlier_dispatchs_block_is_updated_to_the_three_hooks_or_removed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.yaml");
        // 0.4.18's and 0.4.15 to 0.4.17's, as they wrote them.
        let blocks = [
            "\n# Added by dispatch setup, which removes it unless it is edited.\nhooks:\n  \
             on_session_finalize:\n    - command: \"OLD\"\n      timeout: 300\n  \
             pre_llm_call:\n    - command: \"OLD\"\n      timeout: 5\n",
            "\n# Added by dispatch setup, which removes it unless it is edited.\nhooks:\n  \
             on_session_finalize:\n    - command: \"OLD\"\n      timeout: 300\n",
        ];
        let old = "'/old/dispatch' --state-dir '/s' hook hermes";
        for written in blocks {
            for (theirs, after) in [(THEIRS, ""), ("model: x\n", "display:\n  skin: x\n")] {
                let earlier = format!("{theirs}{}{after}", written.replace("OLD", old));
                fs::write(&path, &earlier).unwrap();
                assert_eq!(hook(&path).unwrap(), Hook::Earlier(old.into()));
                assert!(as_written(&path).unwrap());
                update(&path, COMMAND).unwrap().unwrap();
                let installed = block(COMMAND, INSTALLED).unwrap();
                assert_eq!(
                    fs::read_to_string(&path).unwrap(),
                    format!("{theirs}{installed}{after}")
                );
                assert_eq!(hook(&path).unwrap(), Hook::Exact(COMMAND.into()));
                // Removed from the earlier block, and from the updated one.
                fs::write(&path, &earlier).unwrap();
                uninstall(&path).unwrap().unwrap();
                assert_eq!(
                    fs::read_to_string(&path).unwrap(),
                    format!("{theirs}{after}")
                );
                update(&path, COMMAND).unwrap_err();
                install(&path, COMMAND).unwrap().unwrap();
                uninstall(&path).unwrap().unwrap();
                assert_eq!(
                    fs::read_to_string(&path).unwrap(),
                    format!("{theirs}{after}")
                );
            }
        }
    }

    /// The user's configuration after `hermes config set` (2026-11-04),
    /// paths replaced: Hermes's dumper dropped Dispatch's marker and quoted
    /// the command its own way, across two lines.
    const REWRITTEN: &str = r"model:
  default: gpt-6.1-sol
agent:
  reasoning_effort: xhigh
hooks:
  on_session_finalize:
  - command: '''/Users/u/.local/bin/dispatch'' --state-dir ''/Users/u/.dispatch''
      hook hermes'
    timeout: 300
";

    const USERS: &str =
        "'/Users/u/.local/bin/dispatch' --state-dir '/Users/u/.dispatch' hook hermes";

    /// Dispatch's `event` hook as Hermes's dumper would write it, with
    /// `timeout`.
    fn dumped(event: &str, timeout: u64) -> String {
        format!(
            "  {event}:\n  - command: '{}'\n    timeout: {timeout}\n",
            USERS.replace('\'', "''")
        )
    }

    fn pre_llm_call(timeout: u64) -> String {
        dumped("pre_llm_call", timeout)
    }

    #[test]
    fn hooks_hermes_rewrote_are_read_as_dispatchs_and_changed_only_by_hand() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.yaml");
        fs::write(&path, REWRITTEN).unwrap();
        // What Hermes runs is the command Dispatch wrote.
        assert_eq!(hook(&path).unwrap(), Hook::Earlier(USERS.into()));
        assert!(!as_written(&path).unwrap());
        // Adding `pre_llm_call`, or removing them, is the person's to do,
        // from the lines setup shows; the file is never written.
        assert!(update(&path, USERS).is_err());
        assert!(uninstall(&path).is_err());
        assert!(install(&path, USERS).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), REWRITTEN);
        let shown: serde_yaml::Value =
            serde_yaml::from_str(&format!("hooks:\n{}", wanted(USERS).unwrap())).unwrap();
        // 0.4.18's hooks, added by hand in Hermes's form, are an earlier
        // Dispatch's; with `pre_verify` too, exactly Dispatch's hooks.
        let added = format!("{REWRITTEN}{}", pre_llm_call(5));
        fs::write(&path, &added).unwrap();
        assert_eq!(hook(&path).unwrap(), Hook::Earlier(USERS.into()));
        let added = format!("{added}{}", dumped("pre_verify", 5));
        fs::write(&path, &added).unwrap();
        assert_eq!(hook(&path).unwrap(), Hook::Exact(USERS.into()));
        let config: serde_yaml::Value = serde_yaml::from_str(&added).unwrap();
        assert_eq!(config["hooks"], shown["hooks"]);
        assert_eq!(config["agent"]["reasoning_effort"], "xhigh");
        // The person's own hooks beside them are theirs, and change nothing.
        let theirs = "  - command: afplay done.aiff\n    timeout: 2\n";
        let beside = added.replace("    timeout: 300\n", &format!("    timeout: 300\n{theirs}"));
        let beside = format!("{beside}  on_session_start:\n{theirs}");
        fs::write(&path, &beside).unwrap();
        assert_eq!(hook(&path).unwrap(), Hook::Exact(USERS.into()));
        assert!(update(&path, USERS).is_err() && uninstall(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), beside);
        // Removed by hand.
        let removed = &REWRITTEN[..REWRITTEN.find("hooks:").unwrap()];
        fs::write(&path, removed).unwrap();
        assert_eq!(hook(&path).unwrap(), Hook::Absent);
    }

    #[test]
    fn an_edited_timeout_command_or_event_in_hermess_form_is_changed_and_left_alone() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.yaml");
        for edited in [
            REWRITTEN.replace("timeout: 300", "timeout: 30"),
            // One of the two run another command; both doing so would be an
            // earlier install of another Dispatch, to update.
            format!("{REWRITTEN}{}", pre_llm_call(5)).replacen(
                "''/Users/u/.dispatch''",
                "''/Users/u/elsewhere''",
                1,
            ),
            REWRITTEN.replace("    timeout: 300\n", "    timeout: 300\n    matcher: x\n"),
            REWRITTEN.replace("on_session_finalize", "on_session_end"),
            format!("{REWRITTEN}{}", pre_llm_call(60)),
        ] {
            fs::write(&path, &edited).unwrap();
            assert_eq!(hook(&path).unwrap(), Hook::Changed, "{edited}");
            assert!(update(&path, USERS).is_err() && uninstall(&path).is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), edited);
        }
    }
}
