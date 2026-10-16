//! Claude Code's hooks (`SessionStart`, `SessionEnd`, and the two ways a
//! worktree is deleted: `WorktreeRemove` at session exit, and `PreToolUse` of
//! the `ExitWorktree` tool with `action: remove`, which does not run
//! `WorktreeRemove`) as `RuntimeEvent`s, and the reply Claude Code shows the
//! person. Everything Claude-specific ends here.

use std::{sync::mpsc, time::Duration};

use anyhow::{Context, Result};
use serde::Deserialize;

use super::{EventKind, Reply, RuntimeEvent};
use crate::state::State;

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
}

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
    let event = match parse(input) {
        Ok(Some(event)) => event,
        Ok(None) => return notice(Ok(Reply::Silent)),
        Err(error) => return notice(Err(error)),
    };
    if !matches!(event.kind, EventKind::WorkspaceRemoved) {
        return notice(super::ingest(state, event));
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
        Ok(reply) => notice(Ok(reply)),
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

fn notice(reply: Result<Reply>) -> Output {
    let stdout = match reply {
        Ok(Reply::Silent) => String::new(),
        Ok(Reply::Notice(text)) => serde_json::json!({"systemMessage": text}).to_string(),
        Err(error) => serde_json::json!({
            "systemMessage": format!("Dispatch could not track this session: {error:#}")
        })
        .to_string(),
    };
    Output {
        stdout,
        stderr: String::new(),
        code: 0,
    }
}

/// The hook events Dispatch installs, with the timeout each gets. A
/// `WorktreeRemove` hook outlasts `REMOVAL_DEADLINE`, so Dispatch always
/// decides before Claude Code gives up on it.
const INSTALLED: [(&str, Option<&str>, u64); 4] = [
    ("SessionStart", None, 60),
    ("SessionEnd", None, 5),
    ("WorktreeRemove", None, 300),
    ("PreToolUse", Some("ExitWorktree"), 300),
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

fn ours(entry: &serde_json::Value) -> bool {
    entry["hooks"].as_array().is_some_and(|hooks| {
        hooks.iter().any(|hook| {
            hook["command"]
                .as_str()
                .is_some_and(|command| command.ends_with(" hook claude"))
        })
    })
}

/// `settings` without Dispatch's hooks: every other setting and hook as it was.
pub fn without_hooks(mut settings: serde_json::Value) -> serde_json::Value {
    if let Some(hooks) = settings["hooks"].as_object_mut() {
        for (event, _, _) in INSTALLED {
            if let Some(entries) = hooks.get_mut(event).and_then(|e| e.as_array_mut()) {
                entries.retain(|entry| !ours(entry));
                if entries.is_empty() {
                    hooks.remove(event);
                }
            }
        }
        if hooks.is_empty() {
            settings.as_object_mut().map(|s| s.remove("hooks"));
        }
    }
    settings
}

/// `settings` with exactly one set of Dispatch's hooks, running `command`.
pub fn with_hooks(settings: serde_json::Value, command: &str) -> Result<serde_json::Value> {
    let mut settings = without_hooks(settings);
    anyhow::ensure!(
        settings.is_object(),
        "Claude Code settings are not a JSON object"
    );
    if !settings["hooks"].is_object() {
        settings["hooks"] = serde_json::json!({});
    }
    for (event, matcher, timeout) in INSTALLED {
        let mut entry = serde_json::json!({
            "hooks": [{"type": "command", "command": command, "timeout": timeout}]
        });
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

/// Whether Dispatch's hooks are in the settings at `path`.
pub fn hooks_installed(path: &std::path::Path) -> Result<bool> {
    let Ok(bytes) = std::fs::read(path) else {
        return Ok(false);
    };
    let settings: serde_json::Value =
        serde_json::from_slice(&bytes).context("Claude Code settings are not valid JSON")?;
    Ok(INSTALLED.iter().any(|(event, _, _)| {
        settings["hooks"][event]
            .as_array()
            .is_some_and(|entries| entries.iter().any(ours))
    }))
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
pub(super) fn linked_gitdir(worktree: &std::path::Path) -> Option<std::path::PathBuf> {
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
        assert!(hooks_installed(&path).unwrap());

        uninstall_hooks(&path).unwrap();
        let settings: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(settings, theirs);
        assert!(!hooks_installed(&path).unwrap());
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
        assert!(hooks_installed(&path).unwrap());
    }
}
