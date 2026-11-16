//! Claude Code's hooks (`SessionStart`, `SessionEnd`, the two ways a
//! worktree is deleted: `WorktreeRemove` at session exit, and `PreToolUse` of
//! the `ExitWorktree` tool with `action: remove`, which does not run
//! `WorktreeRemove`; and `Notification` and `Stop`, when the session waits on
//! the person) as `RuntimeEvent`s, and the reply Claude Code shows the
//! person. `SessionStart` and `PostToolUse` also deliver pending notes to the
//! agent, as `additionalContext`: Claude Code shows it to the model before
//! the first prompt, or right after the tool's result. A pending ask to catch
//! up is delivered at `Stop`, which it keeps going, or by the waiter, a second
//! `Stop` hook that wakes an idle session (`wait`). Everything Claude-specific
//! ends here.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use super::{EventKind, Hook, Reply, RuntimeEvent};
use crate::{NeedsYouKind, orchestrator::catchup, state::State};

#[derive(Deserialize)]
struct Input {
    hook_event_name: String,
    session_id: String,
    cwd: std::path::PathBuf,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    model: Option<serde_json::Value>,
    /// `WorktreeRemove`: the worktree about to be deleted.
    #[serde(default)]
    worktree_path: Option<std::path::PathBuf>,
    /// `PreToolUse`: which tool is about to run, with what.
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_input: serde_json::Value,
    /// `Notification`: what kind, and Claude's words for it.
    #[serde(default)]
    notification_type: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    transcript_path: Option<std::path::PathBuf>,
}

/// The `Notification` types that mean the session waits on the person, the
/// matcher setup installs; any other type is ignored here too.
const WAITING: [(&str, NeedsYouKind); 3] = [
    ("permission_prompt", NeedsYouKind::Permission),
    ("idle_prompt", NeedsYouKind::Input),
    ("agent_needs_input", NeedsYouKind::Input),
];

/// The event in a hook's input, or `None` for an event Dispatch does not use.
pub fn parse(input: &[u8]) -> Result<Option<RuntimeEvent>> {
    anyhow::ensure!(
        input.len() <= super::MAX_INPUT_BYTES,
        "hook input is larger than {} bytes",
        super::MAX_INPUT_BYTES
    );
    let input: Input = serde_json::from_slice(input).context("unreadable hook input")?;
    let kind = match input.hook_event_name.as_str() {
        "SessionStart" => {
            let source = input.source.unwrap_or_else(|| "startup".into());
            // A new session, or a fork into its own workspace, begins before
            // any edit; anything else may follow edits made earlier.
            let resumed = !matches!(source.as_str(), "startup" | "fork");
            EventKind::Start { source, resumed }
        }
        "SessionEnd" => EventKind::End {
            reason: input.reason.unwrap_or_else(|| "other".into()),
        },
        "WorktreeRemove" => EventKind::WorkspaceRemoved,
        // Leaving a worktree by removing it deletes it without WorktreeRemove.
        "PreToolUse"
            if input.tool_name.as_deref() == Some("ExitWorktree")
                && input.tool_input["action"] == "remove" =>
        {
            EventKind::WorkspaceRemoved
        }
        "Notification" => {
            let Some((_, kind)) = WAITING
                .iter()
                .find(|(name, _)| input.notification_type.as_deref() == Some(*name))
            else {
                return Ok(None);
            };
            EventKind::NeedsYou {
                kind: *kind,
                message: input.message.unwrap_or_default(),
                transcript: input.transcript_path,
            }
        }
        // The turn ended: Claude waits for the next prompt.
        "Stop" => EventKind::NeedsYou {
            kind: NeedsYouKind::TurnEnded,
            message: "waiting for your next prompt".into(),
            transcript: input.transcript_path,
        },
        _ => return Ok(None),
    };
    let cwd = match (&kind, input.worktree_path) {
        (EventKind::WorkspaceRemoved, Some(worktree)) => worktree,
        _ => input.cwd,
    };
    // Claude Code reports the model as a name or as an object with an id.
    let model = input.model.and_then(|model| match model {
        serde_json::Value::String(name) => Some(name),
        other => other["id"].as_str().map(str::to_owned),
    });
    Ok(Some(RuntimeEvent {
        provider: "claude",
        session_id: input.session_id,
        cwd,
        model,
        kind,
    }))
}

/// What `dispatch hook claude` prints, and its exit status.
pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

/// How long Dispatch may take to keep a removed worktree's Δ: below the
/// `WorktreeRemove` timeout setup installs, so Dispatch decides, not Claude.
const REMOVAL_DEADLINE: Duration = Duration::from_secs(240);

/// `dispatch hook claude`. A session is never held up or failed by Dispatch:
/// a problem is shown to the person. The one exception is a worktree about to
/// be deleted whose Δ Dispatch could not keep: the hook fails, so Claude Code
/// keeps the worktree and the work in it.
pub fn handle(state: &State, input: &[u8]) -> Output {
    // Asks and notes first, from the payload's head: a tool's output or the
    // last message can make the rest larger than Dispatch reads. Once claimed
    // they are always printed.
    let head = super::head(input);
    let text = |name: &str| head.get(name).and_then(Value::as_str);
    let (event, cwd) = (text("hook_event_name"), text("cwd").map(Path::new));
    // An ask to catch up keeps the turn going, as its reason, so the turn
    // has not ended. Never on a turn a `Stop` hook already kept going.
    if event == Some("Stop")
        && head.get("stop_hook_active") != Some(&Value::Bool(true))
        && let Some(reason) = cwd.and_then(|cwd| super::catch_up(state, cwd, "stop"))
    {
        return Output {
            stdout: serde_json::json!({"decision": "block", "reason": reason}).to_string(),
            stderr: String::new(),
            code: 0,
        };
    }
    let notes = match (event, cwd) {
        (Some(event @ ("SessionStart" | "PostToolUse")), Some(cwd)) => {
            super::deliver(state, cwd, event).map(|notes| (event, notes))
        }
        _ => None,
    };
    if event == Some("PostToolUse") {
        return notice(Ok(Reply::Silent), notes);
    }
    let event = match parse(input) {
        Ok(Some(event)) => event,
        Ok(None) => return notice(Ok(Reply::Silent), notes),
        Err(error) => return notice(Err(error), notes),
    };
    // Recorded only, never replied to: `Stop`'s stdout reaches Claude Code.
    if matches!(event.kind, EventKind::NeedsYou { .. }) {
        let _ = super::ingest(state, event);
        return notice(Ok(Reply::Silent), None);
    }
    if !matches!(event.kind, EventKind::WorkspaceRemoved) {
        return notice(super::ingest(state, event), notes);
    }
    // Run it where a deadline can be kept; if the deadline passes, this
    // process exits and the attempt with it, leaving nothing half-written
    // (the Δ is renamed into place and the removal committed atomically).
    let (sender, receiver) = mpsc::channel();
    let state = state.clone();
    std::thread::spawn(move || {
        let _ = sender.send(super::ingest(&state, event));
    });
    let outcome = receiver
        .recv_timeout(REMOVAL_DEADLINE)
        .unwrap_or_else(|_| Err(anyhow::anyhow!("keeping its changes took too long")));
    match outcome {
        Ok(reply) => notice(Ok(reply), None),
        Err(error) => Output {
            stdout: String::new(),
            stderr: format!(
                "Dispatch could not keep this worktree's changes, so it stopped the removal; \
                 the worktree is intact: {error:#}"
            ),
            code: 2,
        },
    }
}

/// The reply: `reply` shown to the person, and `notes` for the agent after
/// the hook event that delivered them.
fn notice(reply: Result<Reply>, notes: Option<(&str, String)>) -> Output {
    let mut output = serde_json::Map::new();
    let message = match reply {
        Ok(Reply::Silent) => None,
        Ok(Reply::Notice(text)) => Some(text),
        Err(error) => Some(format!("Dispatch could not track this session: {error:#}")),
    };
    if let Some(message) = message {
        output.insert("systemMessage".into(), message.into());
    }
    if let Some((event, notes)) = notes {
        output.insert(
            "hookSpecificOutput".into(),
            serde_json::json!({"hookEventName": event, "additionalContext": notes}),
        );
    }
    Output {
        stdout: if output.is_empty() {
            String::new()
        } else {
            serde_json::Value::Object(output).to_string()
        },
        stderr: String::new(),
        code: 0,
    }
}

/// How long Claude Code lets the waiter run, in seconds: its `timeout`.
const WAITER_TIMEOUT: u64 = 28_800;

/// How long the waiter waits: a minute less than Claude Code lets it run, so
/// that it ends by itself.
pub const MAX_WAIT: Duration = Duration::from_secs(WAITER_TIMEOUT - 60);

/// `dispatch hook claude-wait`, the waiter: the second `Stop` hook, which
/// Claude Code runs in the background (`asyncRewake`). Its exit status 2
/// gives the session its stderr, waking it when idle, or at the next tool
/// call when busy (observed with Claude Code 2.1.296). It waits for an ask
/// to catch up for its workspace, looking once a second with one `stat`, and
/// leaves at once in a project nobody watches or a workspace the owner keeps
/// no record for. It leaves when a newer waiter of its session starts, when
/// its session ends (its parent, the Claude Code process, changes), and
/// after `max_wait`. It never prints to stdout.
pub fn wait(state: &State, input: &[u8], max_wait: Duration) -> Output {
    let (began, claude) = (Instant::now(), parent());
    let quiet = Output {
        stdout: String::new(),
        stderr: String::new(),
        code: 0,
    };
    let head = super::head(input);
    let text = |name: &str| head.get(name).and_then(Value::as_str);
    let (Some(cwd), Some(session)) = (text("cwd").map(Path::new), text("session_id")) else {
        return quiet;
    };
    if !super::session_id(session) || !cwd.is_absolute() || super::unwatched(state, cwd) {
        return quiet;
    }
    let Some(workspace) = super::workspace(cwd).filter(|w| catchup::followed(state, w)) else {
        return quiet;
    };
    let Some(newest) = Newest::start(state, session) else {
        return quiet;
    };
    let pending = catchup::pending(state, &workspace);
    // A second first: an ask waiting at this `Stop` is the `Stop` hook's to
    // deliver, keeping the turn going rather than waking it once it ended.
    loop {
        std::thread::sleep(Duration::from_secs(1));
        if began.elapsed() >= max_wait || parent() != claude || !newest.still() {
            return quiet;
        }
        if pending.exists()
            && let Some(text) = catchup::claim(state, &workspace, "wake")
        {
            return Output {
                stdout: String::new(),
                stderr: text,
                code: 2,
            };
        }
    }
}

/// A waiter as the newest of its session's: `waiters/<session>` is its own
/// until a newer waiter of that session replaces it. Claude Code starts one
/// at every `Stop`.
struct Newest {
    path: PathBuf,
    written: SystemTime,
}

impl Newest {
    /// Make this waiter its session's newest. Files older than a day, which
    /// no waiter can own still, are removed.
    fn start(state: &State, session: &str) -> Option<Self> {
        let directory = state.root.join("waiters");
        fs::create_dir_all(&directory).ok()?;
        let stale = |entry: &fs::DirEntry| {
            let age = entry.metadata().and_then(|m| m.modified()).ok()?.elapsed();
            Some(age.ok()? > Duration::from_secs(24 * 60 * 60))
        };
        for entry in fs::read_dir(&directory).ok()?.flatten() {
            if stale(&entry) == Some(true) {
                let _ = fs::remove_file(entry.path());
            }
        }
        let mut file = tempfile::NamedTempFile::new_in(&directory).ok()?;
        std::io::Write::write_all(&mut file, std::process::id().to_string().as_bytes()).ok()?;
        // As written, before the rename that makes it the session's.
        let written = file.as_file().metadata().ok()?.modified().ok()?;
        let path = directory.join(session);
        file.persist(&path).ok()?;
        Some(Self { path, written })
    }

    fn still(&self) -> bool {
        fs::metadata(&self.path)
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|modified| modified == self.written)
    }
}

/// The process that started this one: for a hook, the Claude Code process
/// of its session, which starts hooks itself.
#[cfg(unix)]
fn parent() -> Option<u32> {
    Some(std::os::unix::process::parent_id())
}

#[cfg(not(unix))]
fn parent() -> Option<u32> {
    None
}

/// What the waiter's command adds to `hook_command`: `hook claude-wait`. A
/// runtime name, not an option, so a Dispatch that does not know it refuses
/// it with status 1; a usage error's status 2 would wake the session.
const WAIT: &str = "-wait";

/// The matcher of `Notification`.
const WAITING_TYPES: &str = "permission_prompt|idle_prompt|agent_needs_input";

/// Hooks as Dispatch writes them: event, matcher, timeout, what the command
/// adds to `hook_command`, and whether Claude Code runs it in the background
/// and lets its exit status 2 wake the session (`asyncRewake`).
type HookSet = [(&'static str, Option<&'static str>, u64, &'static str, bool)];

/// The hook events Dispatch installs, with the matcher and timeout each
/// gets. A `WorktreeRemove` hook outlasts `REMOVAL_DEADLINE`, so Dispatch
/// always decides before Claude Code gives up on it. Claude Code matches a
/// `Notification` matcher of plain words separated by `|` exactly against
/// `notification_type` (its hooks reference), so only the types in `WAITING`
/// run Dispatch at all. `PostToolUse`, with no matcher, runs after every
/// tool call, and does nothing but deliver notes. `Stop` runs twice: once to
/// record the turn's end or deliver an ask, and once as the waiter.
const INSTALLED: &HookSet = &[
    ("SessionStart", None, 60, "", false),
    ("SessionEnd", None, 5, "", false),
    ("WorktreeRemove", None, 300, "", false),
    ("PreToolUse", Some("ExitWorktree"), 300, "", false),
    ("Notification", Some(WAITING_TYPES), 5, "", false),
    ("Stop", None, 5, "", false),
    ("Stop", None, WAITER_TIMEOUT, WAIT, true),
    ("PostToolUse", None, 5, "", false),
];

/// The hooks earlier Dispatches installed: 0.4.6 to 0.4.16, 0.4.17, then
/// 0.4.18. Settings holding exactly one of these are Dispatch's own, to be
/// updated, not changed by hand.
const EARLIER: [&HookSet; 3] = [
    &[
        ("SessionStart", None, 60, "", false),
        ("SessionEnd", None, 5, "", false),
        ("WorktreeRemove", None, 300, "", false),
        ("PreToolUse", Some("ExitWorktree"), 300, "", false),
    ],
    &[
        ("SessionStart", None, 60, "", false),
        ("SessionEnd", None, 5, "", false),
        ("WorktreeRemove", None, 300, "", false),
        ("PreToolUse", Some("ExitWorktree"), 300, "", false),
        ("Notification", Some(WAITING_TYPES), 5, "", false),
        ("Stop", None, 5, "", false),
    ],
    &[
        ("SessionStart", None, 60, "", false),
        ("SessionEnd", None, 5, "", false),
        ("WorktreeRemove", None, 300, "", false),
        ("PreToolUse", Some("ExitWorktree"), 300, "", false),
        ("Notification", Some(WAITING_TYPES), 5, "", false),
        ("Stop", None, 5, "", false),
        ("PostToolUse", None, 5, "", false),
    ],
];

/// For Work in a `claude --worktree` workspace that began from source other
/// than the project's (`runtime::start_advice`). Advice only: Dispatch never
/// changes the setting, commits or pushes.
pub const START_ADVICE: &str = "Claude Code: local commits reach a claude --worktree workspace \
     only with worktree.baseRef: \"head\" in its settings; neither that setting nor a commit \
     copies uncommitted changes.";

/// Where Claude Code keeps the user's settings: `$CLAUDE_CONFIG_DIR`, or
/// `~/.claude`.
pub fn settings_path() -> Result<std::path::PathBuf> {
    let directory = match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(directory) => std::path::PathBuf::from(directory),
        None => std::path::PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?)
            .join(".claude"),
    };
    Ok(directory.join("settings.json"))
}

/// The command Claude Code runs: this Dispatch, this state, `hook claude`.
pub fn hook_command(state: &State) -> Result<String> {
    let quote =
        |value: &std::path::Path| format!("'{}'", value.to_string_lossy().replace('\'', r"'\''"));
    Ok(format!(
        "{} --state-dir {} hook claude",
        quote(&std::env::current_exe()?),
        quote(&state.root)
    ))
}

/// Whether `hook` is one Dispatch installed: a command running some Dispatch
/// executable with its state directory, `hook claude` or the waiter's `hook
/// claude-wait`. Only such a hook is ever replaced or removed, never an
/// entry or another hook beside it.
fn ours(hook: &serde_json::Value) -> bool {
    hook["type"] == "command"
        && hook["command"].as_str().is_some_and(|command| {
            command.starts_with('\'')
                && command.contains("' --state-dir '")
                && (command.ends_with("' hook claude")
                    || command.ends_with(&format!("' hook claude{WAIT}")))
        })
}

/// Dispatch's hooks in `settings`, under any event, as `(event, matcher,
/// hook)`, sorted by event, matcher and hook, so that two under one event
/// compare in any order.
fn installed(settings: &serde_json::Value) -> Vec<(String, Option<String>, serde_json::Value)> {
    let mut found = Vec::new();
    for (event, entries) in settings["hooks"].as_object().into_iter().flatten() {
        for entry in entries.as_array().into_iter().flatten() {
            for hook in entry["hooks"].as_array().into_iter().flatten() {
                if ours(hook) {
                    let matcher = entry["matcher"].as_str().map(str::to_owned);
                    found.push((event.clone(), matcher, hook.clone()));
                }
            }
        }
    }
    found.sort_by_cached_key(|(event, matcher, hook)| {
        (event.clone(), matcher.clone(), hook.to_string())
    });
    found
}

/// `settings` without Dispatch's hooks, under whichever event an earlier
/// Dispatch put them: every other setting and hook as it was. An entry is
/// removed only when nothing but Dispatch's hooks was in it.
pub fn without_hooks(mut settings: serde_json::Value) -> serde_json::Value {
    if let Some(hooks) = settings["hooks"].as_object_mut() {
        for entries in hooks.values_mut() {
            let Some(entries) = entries.as_array_mut() else {
                continue;
            };
            for entry in entries.iter_mut() {
                if let Some(list) = entry["hooks"].as_array_mut()
                    && list.iter().any(ours)
                {
                    list.retain(|hook| !ours(hook));
                    if list.is_empty() {
                        *entry = serde_json::Value::Null;
                    }
                }
            }
            entries.retain(|entry| !entry.is_null());
        }
        hooks.retain(|_, entries| entries.as_array().is_none_or(|e| !e.is_empty()));
        if hooks.is_empty() {
            settings.as_object_mut().map(|s| s.remove("hooks"));
        }
    }
    settings
}

/// `settings` with exactly one set of Dispatch's hooks, running `command`.
pub fn with_hooks(settings: serde_json::Value, command: &str) -> Result<serde_json::Value> {
    with_set(settings, INSTALLED, command)
}

fn with_set(
    settings: serde_json::Value,
    set: &HookSet,
    command: &str,
) -> Result<serde_json::Value> {
    let mut settings = without_hooks(settings);
    anyhow::ensure!(
        settings.is_object(),
        "Claude Code settings are not a JSON object"
    );
    if !settings["hooks"].is_object() {
        settings["hooks"] = serde_json::json!({});
    }
    for &(event, matcher, timeout, suffix, rewake) in set {
        let mut hook = serde_json::json!({
            "type": "command", "command": format!("{command}{suffix}"), "timeout": timeout
        });
        if rewake {
            hook["asyncRewake"] = true.into();
        }
        let mut entry = serde_json::json!({"hooks": [hook]});
        if let Some(matcher) = matcher {
            entry["matcher"] = serde_json::json!(matcher);
        }
        match settings["hooks"][event].as_array_mut() {
            Some(entries) => entries.push(entry),
            None => settings["hooks"][event] = serde_json::json!([entry]),
        }
    }
    Ok(settings)
}

/// Dispatch's hooks in the settings at `path`, hook by hook: exact when they
/// are just the hooks `with_hooks` writes, all running one command (the
/// waiter's as `claude-wait`), and earlier when they are just the hooks an
/// earlier Dispatch wrote.
pub fn hook(path: &std::path::Path) -> Result<Hook> {
    let Ok(bytes) = std::fs::read(path) else {
        return Ok(Hook::Absent);
    };
    let settings: serde_json::Value =
        serde_json::from_slice(&bytes).context("Claude Code settings are not valid JSON")?;
    let found = installed(&settings);
    let Some((_, _, first)) = found.first() else {
        return Ok(Hook::Absent);
    };
    let command = first["command"].as_str().unwrap_or_default();
    let command = command.strip_suffix(WAIT).unwrap_or(command).to_owned();
    let written = |set| with_set(serde_json::json!({}), set, &command).map(|s| installed(&s));
    if found == written(INSTALLED)? {
        return Ok(Hook::Exact(command));
    }
    for set in EARLIER {
        if found == written(set)? {
            return Ok(Hook::Earlier(command));
        }
    }
    Ok(Hook::Changed)
}

/// Rewrite the settings at `path` with `change`, after keeping a backup
/// beside them. Settings that are not valid JSON are left untouched.
fn rewrite(
    path: &std::path::Path,
    change: impl FnOnce(serde_json::Value) -> Result<serde_json::Value>,
) -> Result<Option<std::path::PathBuf>> {
    let existing = match std::fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    let settings = match &existing {
        Some(bytes) => serde_json::from_slice(bytes).with_context(|| {
            format!(
                "{} is not valid JSON; it was left untouched",
                path.display()
            )
        })?,
        None => serde_json::json!({}),
    };
    let changed = change(settings)?;
    let backup = match &existing {
        Some(bytes) => {
            let backup = path.with_extension(format!(
                "json.dispatch-backup-{}",
                chrono::Utc::now().format("%Y%m%d%H%M%S")
            ));
            std::fs::write(&backup, bytes)
                .with_context(|| format!("failed to keep a backup at {}", backup.display()))?;
            Some(backup)
        }
        None => None,
    };
    let mut text = serde_json::to_vec_pretty(&changed)?;
    text.push(b'\n');
    crate::state::write_durably(path, &text)?;
    Ok(backup)
}

/// Add Dispatch's hooks to Claude Code's settings at `path`. Returns the
/// backup of the previous settings, if there were any.
pub fn install_hooks(path: &std::path::Path, command: &str) -> Result<Option<std::path::PathBuf>> {
    rewrite(path, |settings| with_hooks(settings, command))
}

/// Remove Dispatch's hooks, and nothing else, from the settings at `path`.
pub fn uninstall_hooks(path: &std::path::Path) -> Result<Option<std::path::PathBuf>> {
    rewrite(path, |settings| Ok(without_hooks(settings)))
}

/// What installing adds, as the person approving it sees it.
pub fn hooks_preview(command: &str) -> String {
    let added = with_hooks(serde_json::json!({}), command).expect("an empty object takes hooks");
    serde_json::to_string_pretty(&added).expect("settings serialize")
}

/// What Claude Code records about a worktree it made, in the worktree's Git
/// metadata (`.git/worktrees/<name>/`), read without running Git. Observed
/// with Claude Code 2.1.296 (`docs/plan-0.4.13.md` §2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeSignals {
    /// `CLAUDE_BASE`: the commit Claude branched the worktree from. A hint of
    /// provenance, never S0.
    pub base: Option<String>,
    /// The worktree's `git worktree lock`, if any.
    pub lock: Option<Lock>,
}

/// A `git worktree lock` reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lock {
    /// `claude session <name> (pid <pid> start <start>)`, the lock a running
    /// `claude --worktree` session holds; `start` as `ps -o lstart=` prints it
    /// in UTC.
    Session {
        name: String,
        pid: u32,
        start: String,
    },
    /// Any other reason, verbatim: a person's lock, or one not yet observed.
    Other(String),
}

/// Whether the process a lock names still runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// That pid runs, and it started when the lock says.
    Running,
    /// That pid does not run, or a different process now has it.
    Gone,
    /// Not a session lock, or the start time cannot be read.
    Unknown,
}

/// The signals of the worktree at `worktree`, whose `.git` file names its
/// metadata directory. A worktree that is not a linked Git worktree has none.
pub fn worktree_signals(worktree: &std::path::Path) -> WorktreeSignals {
    let Some(gitdir) = linked_gitdir(worktree) else {
        return WorktreeSignals::default();
    };
    let read = |name: &str| {
        std::fs::read_to_string(gitdir.join(name))
            .ok()
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty())
    };
    WorktreeSignals {
        base: read("CLAUDE_BASE"),
        lock: read("locked").map(|reason| parse_lock(&reason)),
    }
}

/// Whether Claude Code made the worktree at `worktree` for the project at
/// `root`: it carries `CLAUDE_BASE` and lives where Claude documents it makes
/// worktrees.
pub fn made_by_claude(
    root: &std::path::Path,
    worktree: &std::path::Path,
    signals: &WorktreeSignals,
) -> bool {
    signals.base.is_some() && worktree.starts_with(root.join(".claude").join("worktrees"))
}

/// A lock reason, as `Lock`.
pub fn parse_lock(reason: &str) -> Lock {
    let session = || {
        let rest = reason.strip_prefix("claude session ")?;
        let (name, rest) = rest.split_once(" (pid ")?;
        let (pid, rest) = rest.split_once(" start ")?;
        let start = rest.strip_suffix(')')?;
        Some(Lock::Session {
            name: name.to_owned(),
            pid: pid.parse().ok()?,
            start: one_spaced(start),
        })
    };
    session().unwrap_or_else(|| Lock::Other(reason.to_owned()))
}

/// Whether the process `lock` names still runs: its pid exists and started at
/// the time the lock records, so a recycled pid is not mistaken for it.
pub fn liveness(lock: &Lock) -> Liveness {
    let Lock::Session { pid, start, .. } = lock else {
        return Liveness::Unknown;
    };
    if !crate::process::pid_exists(*pid) {
        return Liveness::Gone;
    }
    match process_start_utc(*pid) {
        Some(now) if now == *start => Liveness::Running,
        Some(_) => Liveness::Gone,
        None => Liveness::Unknown,
    }
}

/// `pid`'s start time as Claude Code writes it in a lock: `ps -o lstart=` in UTC.
pub(super) fn process_start_utc(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .env("TZ", "UTC0")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    let start = one_spaced(text.trim());
    (output.status.success() && !start.is_empty()).then_some(start)
}

/// `ps` pads single-digit days with a second space; compare words, not spacing.
fn one_spaced(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The metadata directory a linked worktree's `.git` file points to.
pub(crate) fn linked_gitdir(worktree: &std::path::Path) -> Option<std::path::PathBuf> {
    let file = std::fs::read_to_string(worktree.join(".git")).ok()?;
    let gitdir = std::path::PathBuf::from(file.strip_prefix("gitdir:")?.trim());
    Some(if gitdir.is_absolute() {
        gitdir
    } else {
        worktree.join(gitdir)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_lock_parses_and_anything_else_stays_verbatim() {
        assert_eq!(
            parse_lock("claude session trial-b (pid 87076 start Tue Oct 13 14:08:28 2026)"),
            Lock::Session {
                name: "trial-b".into(),
                pid: 87076,
                start: "Tue Oct 13 14:08:28 2026".into(),
            }
        );
        assert_eq!(
            parse_lock("claude session x (pid 1 start Sat Oct  3 09:00:00 2026)"),
            Lock::Session {
                name: "x".into(),
                pid: 1,
                start: "Sat Oct 3 09:00:00 2026".into(),
            }
        );
        for other in [
            "moving it",
            "claude session x (pid abc start y)",
            "claude agent a-1",
        ] {
            assert_eq!(parse_lock(other), Lock::Other(other.into()));
        }
    }

    #[test]
    fn liveness_is_exact_for_this_process_and_gone_for_a_wrong_start_or_pid() {
        let me = std::process::id();
        let start = process_start_utc(me).expect("ps reads this process's start");
        let lock = |pid, start: &str| Lock::Session {
            name: "x".into(),
            pid,
            start: start.into(),
        };
        assert_eq!(liveness(&lock(me, &start)), Liveness::Running);
        assert_eq!(
            liveness(&lock(me, "Thu Jan 1 00:00:00 1970")),
            Liveness::Gone
        );
        assert_eq!(liveness(&lock(u32::MAX - 1, &start)), Liveness::Gone);
        assert_eq!(liveness(&Lock::Other("x".into())), Liveness::Unknown);
    }

    #[test]
    fn signals_come_from_the_worktrees_git_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        let gitdir = root.join(".git").join("worktrees").join("a");
        let worktree = root.join(".claude").join("worktrees").join("a");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .unwrap();
        assert_eq!(worktree_signals(&worktree), WorktreeSignals::default());
        std::fs::write(gitdir.join("CLAUDE_BASE"), "0607b593\n").unwrap();
        std::fs::write(
            gitdir.join("locked"),
            "claude session a (pid 7 start Tue Oct 13 14:08:28 2026)",
        )
        .unwrap();
        let signals = worktree_signals(&worktree);
        assert_eq!(signals.base.as_deref(), Some("0607b593"));
        assert!(matches!(signals.lock, Some(Lock::Session { pid: 7, .. })));
        assert!(made_by_claude(&root, &worktree, &signals));
        assert!(!made_by_claude(&root, &root.join("elsewhere"), &signals));
        assert!(!made_by_claude(
            &root,
            &worktree,
            &WorktreeSignals::default()
        ));
        assert_eq!(
            worktree_signals(&temp.path().join("plain")),
            WorktreeSignals::default()
        );
    }

    const COMMAND: &str = "'/bin/dispatch' --state-dir '/s' hook claude";

    #[test]
    fn installing_adds_one_set_of_hooks_and_keeps_everything_else() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        let theirs = serde_json::json!({
            "model": "claude-sonnet-5",
            "hooks": {
                "SessionStart": [{"hooks": [{"type": "command", "command": "direnv export"}]}],
                "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "x"}]}],
            },
        });
        std::fs::write(&path, theirs.to_string()).unwrap();

        let backup = install_hooks(&path, COMMAND).unwrap().unwrap();
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            theirs.to_string()
        );
        install_hooks(&path, "'/moved/dispatch' --state-dir '/s' hook claude").unwrap();
        let settings: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(settings["model"], "claude-sonnet-5");
        let tools = settings["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(tools.len(), 2, "theirs plus exactly one of ours: {tools:?}");
        assert_eq!(tools[0], theirs["hooks"]["PreToolUse"][0]);
        assert_eq!(tools[1]["matcher"], "ExitWorktree");
        let starts = settings["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(
            starts.len(),
            2,
            "theirs plus exactly one of ours: {starts:?}"
        );
        assert_eq!(starts[0]["hooks"][0]["command"], "direnv export");
        assert_eq!(
            starts[1]["hooks"][0]["command"],
            "'/moved/dispatch' --state-dir '/s' hook claude"
        );
        assert_eq!(
            settings["hooks"]["WorktreeRemove"][0]["hooks"][0]["timeout"],
            300
        );
        assert_eq!(
            hook(&path).unwrap(),
            Hook::Exact("'/moved/dispatch' --state-dir '/s' hook claude".into())
        );
        // An entry edited by hand, or one of them missing, is no longer exact.
        let installed = settings.clone();
        let mut edited = installed.clone();
        edited["hooks"]["WorktreeRemove"][0]["hooks"][0]["timeout"] = 30.into();
        let mut missing = installed.clone();
        missing["hooks"]
            .as_object_mut()
            .unwrap()
            .remove("SessionEnd");
        for changed in [edited, missing] {
            std::fs::write(&path, changed.to_string()).unwrap();
            assert_eq!(hook(&path).unwrap(), Hook::Changed, "{changed}");
        }
        std::fs::write(&path, installed.to_string()).unwrap();

        uninstall_hooks(&path).unwrap();
        let settings: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(settings, theirs);
        assert_eq!(hook(&path).unwrap(), Hook::Absent);
    }

    /// The probe's payloads (`Notification`, `Stop`), paths replaced.
    fn probe(event: &str, extra: serde_json::Value) -> Vec<u8> {
        let mut payload = serde_json::json!({
            "session_id": "96fce6bd-fe23-4b1c-bd1e-b171635c421b",
            "transcript_path": "/home/u/.claude/projects/p/96fce6bd.jsonl",
            "cwd": "/repo/.claude/worktrees/probe-a",
            "scratchpad_dir": "/tmp/scratch",
            "prompt_id": "7e92d8c0-ed66-464a-845b-98f1cc4cff83",
            "hook_event_name": event,
        });
        for (key, value) in extra.as_object().unwrap() {
            payload[key] = value.clone();
        }
        payload.to_string().into_bytes()
    }

    #[test]
    fn notifications_that_wait_on_the_person_and_stop_are_marks_and_nothing_else_is() {
        let mark = |input: Vec<u8>| match parse(&input).unwrap().map(|event| event.kind) {
            Some(EventKind::NeedsYou {
                kind,
                message,
                transcript,
            }) => {
                assert_eq!(
                    transcript.as_deref(),
                    Some(std::path::Path::new(
                        "/home/u/.claude/projects/p/96fce6bd.jsonl"
                    ))
                );
                Some((kind, message))
            }
            Some(_) => panic!("not a mark"),
            None => None,
        };
        let notification = |kind: &str, message: &str| {
            probe(
                "Notification",
                serde_json::json!({"notification_type": kind, "message": message}),
            )
        };
        assert_eq!(
            mark(notification(
                "permission_prompt",
                "Claude needs your permission"
            )),
            Some((
                NeedsYouKind::Permission,
                "Claude needs your permission".into()
            ))
        );
        assert_eq!(
            mark(notification(
                "idle_prompt",
                "Claude is waiting for your input"
            )),
            Some((
                NeedsYouKind::Input,
                "Claude is waiting for your input".into()
            ))
        );
        assert_eq!(
            mark(notification("agent_needs_input", "x")).unwrap().0,
            NeedsYouKind::Input
        );
        let stop = probe(
            "Stop",
            serde_json::json!({
                "permission_mode": "default", "effort": {"level": "high"},
                "stop_hook_active": false, "last_assistant_message": "done",
                "background_tasks": [], "session_crons": [],
            }),
        );
        assert_eq!(
            mark(stop),
            Some((
                NeedsYouKind::TurnEnded,
                "waiting for your next prompt".into()
            ))
        );
        for other in ["auth_success", "elicitation_dialog", "agent_completed"] {
            assert_eq!(mark(notification(other, "x")), None, "{other}");
        }
        assert_eq!(mark(probe("Notification", serde_json::json!({}))), None);
        // The matcher setup installs names exactly the types parsed as marks.
        let names: Vec<&str> = WAITING.iter().map(|(name, _)| *name).collect();
        assert!(INSTALLED.contains(&("Notification", Some(&names.join("|")), 5, "", false)));
    }

    #[test]
    fn a_mark_is_never_replied_to() {
        let temp = tempfile::tempdir().unwrap();
        let state = State {
            root: temp.path().join("state"),
        };
        let output = handle(
            &state,
            &probe(
                "Notification",
                serde_json::json!({"notification_type": "permission_prompt", "message": "m"}),
            ),
        );
        assert_eq!((output.stdout.as_str(), output.code), ("", 0));
        // Malformed: still silent, as `Stop`'s stdout would reach Claude Code.
        let output = handle(
            &state,
            &probe(
                "Stop",
                serde_json::json!({"transcript_path": "relative.jsonl"}),
            ),
        );
        assert_eq!((output.stdout.as_str(), output.code), ("", 0));
    }

    #[test]
    fn the_new_hooks_get_five_seconds_and_removal_keeps_every_hook_not_dispatchs() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        // The person's own Notification and Stop hooks, one of them in the
        // same entry Dispatch will be found in after an upgrade below.
        let theirs = serde_json::json!({
            "hooks": {
                "Notification": [{"hooks": [{"type": "command", "command": "moshi-hook notify"}]}],
                "Stop": [{"hooks": [{"type": "command", "command": "afplay done.aiff", "timeout": 2}]}],
            },
        });
        std::fs::write(&path, theirs.to_string()).unwrap();
        install_hooks(&path, COMMAND).unwrap();
        let settings: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let notifications = settings["hooks"]["Notification"].as_array().unwrap();
        assert_eq!(notifications.len(), 2, "{notifications:?}");
        assert_eq!(notifications[0], theirs["hooks"]["Notification"][0]);
        assert_eq!(
            notifications[1],
            serde_json::json!({
                "matcher": "permission_prompt|idle_prompt|agent_needs_input",
                "hooks": [{"type": "command", "command": COMMAND, "timeout": 5}],
            })
        );
        // Theirs, Dispatch's, and Dispatch's waiter, in the background for
        // up to eight hours.
        let stops = settings["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stops.len(), 3, "{stops:?}");
        assert_eq!(stops[0], theirs["hooks"]["Stop"][0]);
        assert_eq!(
            stops[1],
            serde_json::json!({"hooks": [{"type": "command", "command": COMMAND, "timeout": 5}]})
        );
        assert_eq!(
            stops[2],
            serde_json::json!({"hooks": [{
                "type": "command", "command": format!("{COMMAND}-wait"),
                "timeout": 28_800, "asyncRewake": true,
            }]})
        );
        assert_eq!(hook(&path).unwrap(), Hook::Exact(COMMAND.into()));

        uninstall_hooks(&path).unwrap();
        let settings: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(settings, theirs);

        // A person's hook put beside Dispatch's in one entry stays.
        let shared = serde_json::json!({"hooks": {"Stop": [{"hooks": [
            {"type": "command", "command": COMMAND, "timeout": 5},
            {"type": "command", "command": "say done"},
        ]}]}});
        assert_eq!(
            without_hooks(shared),
            serde_json::json!({"hooks": {"Stop": [{"hooks": [
                {"type": "command", "command": "say done"},
            ]}]}})
        );
        // A command that only ends like Dispatch's is not Dispatch's.
        let lookalike = serde_json::json!({"hooks": {"Stop": [{"hooks": [
            {"type": "command", "command": "my-tool hook claude"},
        ]}]}});
        assert_eq!(without_hooks(lookalike.clone()), lookalike);
    }

    #[test]
    fn an_earlier_dispatchs_hooks_are_recognised_and_upgraded_once() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        // The four hooks 0.4.16 installed, from this install path or another.
        let old = "'/old/dispatch' --state-dir '/s' hook claude";
        let earlier = |command: &str| {
            let entry = |timeout: u64| serde_json::json!({"hooks": [{"type": "command", "command": command, "timeout": timeout}]});
            let mut tool = entry(300);
            tool["matcher"] = "ExitWorktree".into();
            serde_json::json!({"hooks": {
                "SessionStart": [entry(60)],
                "SessionEnd": [entry(5)],
                "WorktreeRemove": [entry(300)],
                "PreToolUse": [tool],
                "Stop": [{"hooks": [{"type": "command", "command": "afplay done.aiff"}]}],
            }})
        };
        let found = |settings: &serde_json::Value| {
            std::fs::write(&path, settings.to_string()).unwrap();
            hook(&path).unwrap()
        };
        assert_eq!(found(&earlier(COMMAND)), Hook::Earlier(COMMAND.into()));
        // Hook by hook: a person's hook beside one of Dispatch's is theirs.
        let mut shared = earlier(COMMAND);
        shared["hooks"]["SessionEnd"][0]["hooks"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"type": "command", "command": "say bye"}));
        assert_eq!(found(&shared), Hook::Earlier(COMMAND.into()));
        // Edited by hand, or two executables at once: not as any Dispatch
        // wrote it.
        let mut timeout = earlier(COMMAND);
        timeout["hooks"]["SessionEnd"][0]["hooks"][0]["timeout"] = 10.into();
        let mut key = earlier(COMMAND);
        key["hooks"]["SessionEnd"][0]["hooks"][0]["async"] = true.into();
        let mut mixed = earlier(COMMAND);
        mixed["hooks"]["SessionEnd"][0]["hooks"][0]["command"] = old.into();
        for changed in [timeout, key, mixed] {
            assert_eq!(found(&changed), Hook::Changed, "{changed}");
        }
        assert_eq!(found(&earlier(old)), Hook::Earlier(old.into()));

        install_hooks(&path, COMMAND).unwrap();
        install_hooks(&path, COMMAND).unwrap();
        assert_eq!(hook(&path).unwrap(), Hook::Exact(COMMAND.into()));
        let settings: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let text = settings.to_string();
        assert!(!text.contains("/old/dispatch"), "{text}");
        assert_eq!(text.matches(COMMAND).count(), INSTALLED.len(), "{text}");
        assert_eq!(
            settings["hooks"]["Stop"][0]["hooks"][0]["command"],
            "afplay done.aiff"
        );
    }

    #[test]
    fn settings_that_are_not_json_are_left_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        std::fs::write(&path, "{ not json").unwrap();
        let error = install_hooks(&path, COMMAND).unwrap_err().to_string();
        assert!(error.contains("left untouched"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[test]
    fn installing_into_no_settings_creates_them() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        assert!(install_hooks(&path, COMMAND).unwrap().is_none());
        assert_eq!(hook(&path).unwrap(), Hook::Exact(COMMAND.into()));
    }

    #[test]
    fn the_hooks_of_0_4_17_are_recognised_and_upgraded_to_deliver_notes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        let old = "'/old/dispatch' --state-dir '/s' hook claude";
        // The six hooks 0.4.17 installed, beside a hook of the person's.
        let mut settings = with_set(serde_json::json!({}), EARLIER[1], old).unwrap();
        settings["hooks"]["PostToolUse"] = serde_json::json!([{"matcher": "Bash", "hooks": [{"type": "command", "command": "lint"}]}]);
        std::fs::write(&path, settings.to_string()).unwrap();
        assert_eq!(hook(&path).unwrap(), Hook::Earlier(old.into()));
        install_hooks(&path, COMMAND).unwrap();
        assert_eq!(hook(&path).unwrap(), Hook::Exact(COMMAND.into()));
        let settings: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let tools = settings["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(tools[0]["hooks"][0]["command"], "lint");
        // Every tool, within Claude Code's five seconds.
        assert_eq!(
            tools[1],
            serde_json::json!({"hooks": [{"type": "command", "command": COMMAND, "timeout": 5}]})
        );
        // Anything between the earlier sets and this one is not as any
        // Dispatch wrote it.
        let partial = with_set(serde_json::json!({}), &INSTALLED[..5], COMMAND).unwrap();
        std::fs::write(&path, partial.to_string()).unwrap();
        assert_eq!(hook(&path).unwrap(), Hook::Changed);
    }

    #[test]
    fn notes_ride_with_the_reply_and_a_tool_call_with_none_prints_nothing() {
        let both = notice(
            Ok(Reply::Notice("Dispatch is tracking this worktree".into())),
            Some(("SessionStart", "Work 01ABC landed fn f.".into())),
        );
        let reply: serde_json::Value = serde_json::from_str(&both.stdout).unwrap();
        assert_eq!(
            reply,
            serde_json::json!({
                "systemMessage": "Dispatch is tracking this worktree",
                "hookSpecificOutput": {
                    "hookEventName": "SessionStart",
                    "additionalContext": "Work 01ABC landed fn f.",
                },
            })
        );
        assert_eq!(notice(Ok(Reply::Silent), None).stdout, "");
        // A tool call in no repository, with an output past what the rest of
        // the hook reads: nothing to deliver, and nothing said.
        let temp = tempfile::tempdir().unwrap();
        let state = State {
            root: temp.path().join("state"),
        };
        let mut input = probe(
            "PostToolUse",
            serde_json::json!({"cwd": temp.path(), "tool_name": "Read"}),
        );
        input.truncate(input.len() - 1);
        input.extend(format!(r#", "tool_response": "{}"}}"#, "x".repeat(100 * 1024)).bytes());
        let output = handle(&state, &input[..super::super::MAX_INPUT_BYTES + 1]);
        assert_eq!((output.stdout.as_str(), output.code), ("", 0));
        assert!(!state.root.exists());
    }

    const ASK: &str = "Dispatch: Work 01AAAAAA (Claude Code) landed a change this Work relies on.";

    /// A watched repository with a linked worktree in which an ask to catch
    /// up waits: the state, the worktree, and the lock that watches it.
    fn asked(temp: &Path) -> (State, PathBuf, crate::lock::OperationLock) {
        let temp = fs::canonicalize(temp).unwrap();
        let repo = temp.join("repo");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("a.txt"), "a\n").unwrap();
        let worktree = temp.join("wt");
        for args in [
            &["init", "--quiet"][..],
            &["add", "."],
            &["commit", "--quiet", "-m", "initial"],
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "wt",
                worktree.to_str().unwrap(),
            ],
        ] {
            let mut git = crate::source::git_command(&repo);
            git.args(["-c", "user.name=T", "-c", "user.email=t@example.invalid"]);
            assert!(git.args(args).status().unwrap().success(), "git {args:?}");
        }
        let state = State {
            root: temp.join("state"),
        };
        let serving = crate::orchestrator::background::acquire(&state, &repo).unwrap();
        let pending = catchup::pending(&state, &worktree);
        fs::create_dir_all(pending.with_file_name("delivered")).unwrap();
        let ask = serde_json::json!({"version": 1, "run_id": "r", "landings": [], "text": ASK});
        fs::write(&pending, ask.to_string()).unwrap();
        (state, worktree, serving)
    }

    fn files(directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(directory)
            .into_iter()
            .flatten()
            .filter_map(|entry| Some(entry.ok()?.file_name().to_string_lossy().into_owned()))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn an_ask_keeps_the_turn_going_once_and_that_stop_ends_no_turn() {
        let temp = tempfile::tempdir().unwrap();
        let (state, worktree, _serving) = asked(temp.path());
        let stop = |active: bool| {
            probe(
                "Stop",
                serde_json::json!({
                    "cwd": worktree, "permission_mode": "auto", "effort": {"level": "high"},
                    "stop_hook_active": active, "last_assistant_message": "done",
                    "background_tasks": [], "session_crons": [],
                }),
            )
        };
        let marks = || files(&state.root.join("marks")).len();
        // A turn a Stop hook already kept going is left to end: nothing is
        // claimed, and its end is recorded as before.
        let output = handle(&state, &stop(true));
        assert_eq!((output.stdout.as_str(), output.code), ("", 0));
        assert!(catchup::pending(&state, &worktree).exists());
        assert_eq!(marks(), 1);

        let output = handle(&state, &stop(false));
        let reply: Value = serde_json::from_str(&output.stdout).unwrap();
        assert_eq!(
            reply,
            serde_json::json!({"decision": "block", "reason": ASK})
        );
        assert_eq!((output.stderr.as_str(), output.code), ("", 0));
        assert_eq!(marks(), 1, "the turn did not end");
        let delivered = files(&state.root.join("catchup/delivered"));
        assert!(
            delivered.len() == 1 && delivered[0].ends_with(".stop.json"),
            "{delivered:?}"
        );

        // Asked once: the next Stop is as before.
        let output = handle(&state, &stop(false));
        assert_eq!((output.stdout.as_str(), output.code), ("", 0));
        assert_eq!(marks(), 2);
    }

    /// Settings with `hooks` as an earlier Dispatch wrote them after `theirs`:
    /// each in an entry of its own, running `command`.
    fn written_by(
        mut settings: Value,
        command: &str,
        hooks: &[(&str, Option<&str>, u64)],
    ) -> Value {
        for &(event, matcher, timeout) in hooks {
            let mut entry = serde_json::json!({
                "hooks": [{"type": "command", "command": command, "timeout": timeout}]
            });
            if let Some(matcher) = matcher {
                entry["matcher"] = matcher.into();
            }
            match settings["hooks"][event].as_array_mut() {
                Some(entries) => entries.push(entry),
                None => settings["hooks"][event] = serde_json::json!([entry]),
            }
        }
        settings
    }

    #[test]
    fn every_earlier_dispatchs_hooks_are_upgraded_to_the_waiter_and_removed_with_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        let old = "'/old/dispatch' --state-dir '/s' hook claude";
        let read = || -> Value { serde_json::from_slice(&fs::read(&path).unwrap()).unwrap() };
        let theirs = serde_json::json!({"model": "keep", "hooks": {
            "Stop": [{"hooks": [{"type": "command", "command": "afplay done.aiff", "timeout": 2}]}],
            "PostToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "lint"}]}],
        }});
        let waiting = Some("permission_prompt|idle_prompt|agent_needs_input");
        let removal = [
            ("SessionStart", None, 60),
            ("SessionEnd", None, 5),
            ("WorktreeRemove", None, 300),
            ("PreToolUse", Some("ExitWorktree"), 300),
        ];
        let marks = [("Notification", waiting, 5), ("Stop", None, 5)];
        let notes = [("PostToolUse", None, 5)];
        let releases = [
            ("0.4.6", removal.to_vec()),
            ("0.4.17", [&removal[..], &marks].concat()),
            ("0.4.18", [&removal[..], &marks, &notes].concat()),
        ];
        for (release, hooks) in releases {
            fs::write(&path, written_by(theirs.clone(), old, &hooks).to_string()).unwrap();
            assert_eq!(hook(&path).unwrap(), Hook::Earlier(old.into()), "{release}");
            install_hooks(&path, COMMAND).unwrap();
            assert_eq!(
                hook(&path).unwrap(),
                Hook::Exact(COMMAND.into()),
                "{release}"
            );
            let settings = read();
            assert_eq!(settings, with_hooks(theirs.clone(), COMMAND).unwrap());
            let stops = settings["hooks"]["Stop"].as_array().unwrap();
            assert_eq!(stops[0], theirs["hooks"]["Stop"][0], "{release}");
            assert_eq!(stops[1]["hooks"][0]["command"], COMMAND);
            let waiter = &stops[2]["hooks"][0];
            assert_eq!(waiter["command"], format!("{COMMAND}-wait"));
            assert_eq!(
                (&waiter["asyncRewake"], &waiter["timeout"]),
                (&true.into(), &28_800.into())
            );
            uninstall_hooks(&path).unwrap();
            assert_eq!(read(), theirs, "{release}: nothing of Dispatch's left");
            assert_eq!(hook(&path).unwrap(), Hook::Absent);
        }
    }

    #[test]
    fn the_two_stop_hooks_are_dispatchs_in_either_order_and_only_as_written() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        let found = |settings: &Value| {
            fs::write(&path, settings.to_string()).unwrap();
            hook(&path).unwrap()
        };
        let installed = with_hooks(serde_json::json!({}), COMMAND).unwrap();
        let mut swapped = installed.clone();
        swapped["hooks"]["Stop"].as_array_mut().unwrap().reverse();
        assert_eq!(found(&swapped), Hook::Exact(COMMAND.into()));
        // A waiter that does not wake the session, or waits longer, or a set
        // with only the waiter added: changed by hand.
        let mut sync = installed.clone();
        sync["hooks"]["Stop"][1]["hooks"][0]
            .as_object_mut()
            .unwrap()
            .remove("asyncRewake");
        let mut longer = installed.clone();
        longer["hooks"]["Stop"][1]["hooks"][0]["timeout"] = 86_400.into();
        let mut partial = installed.clone();
        partial["hooks"]
            .as_object_mut()
            .unwrap()
            .remove("PostToolUse");
        for changed in [sync, longer, partial] {
            assert_eq!(found(&changed), Hook::Changed, "{changed}");
        }
        // Someone else's command that only ends like the waiter's is theirs.
        let lookalike = serde_json::json!({"hooks": {"Stop": [{"hooks": [
            {"type": "command", "command": "my-tool hook claude-wait", "asyncRewake": true},
        ]}]}});
        assert_eq!(without_hooks(lookalike.clone()), lookalike);
    }
}
