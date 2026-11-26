//! Dispatch's MCP server, `dispatch mcp`, in each agent's own configuration:
//! one server named `dispatch` for the user, not a project, since it serves
//! the project the agent works in. Each agent here owns only where its
//! configuration is and its format, as observed with Claude Code 2.1.296,
//! Codex 0.170.0, Cursor Agent 2026.11.13 and Hermes 0.23.2:
//!
//! - Claude Code owns `.claude.json` and rewrites it all the time, so its
//!   own `claude mcp` changes it; Dispatch only reads it.
//! - Codex's `config.toml` and Hermes's `config.yaml` get a marked block at
//!   their end, every other byte kept, and only that block, exactly as some
//!   Dispatch wrote it, is ever changed. Dispatch parses no TOML, and
//!   `hermes config set` rewrites the whole file (`hermes mcp add` would
//!   first install Hermes's runtime and start the server).
//! - Cursor's `mcp.json` is rewritten with its other settings kept.
//!
//! A file Dispatch writes keeps a backup beside it, and one it cannot read
//! is left untouched.

use std::{
    fs,
    ops::Range,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::hermes::{self, MARKER};
use crate::state::State;

/// Dispatch's server in an agent's configuration.
#[derive(Debug, PartialEq, Eq)]
pub enum Server {
    Absent,
    /// As setup writes it, running this command line (`Server::of`).
    Ours(Vec<String>),
    /// A server named `dispatch` that setup did not write, or one Dispatch
    /// cannot tell from such: the person's.
    Changed,
}

impl Server {
    /// `Ours` when `argv` is a command line setup writes, for any Dispatch:
    /// an absolute executable, `--state-dir` and an absolute directory or
    /// nothing, then `mcp`. Any other is the person's.
    fn of(argv: Vec<String>) -> Self {
        let absolute = |path: &str| Path::new(path).is_absolute();
        let ours = match argv.iter().map(String::as_str).collect::<Vec<_>>()[..] {
            [command, "mcp"] => absolute(command),
            [command, "--state-dir", state, "mcp"] => absolute(command) && absolute(state),
            _ => false,
        };
        if ours {
            Self::Ours(argv)
        } else {
            Self::Changed
        }
    }
}

/// The command line an agent runs for this Dispatch's server: its absolute
/// path, `--state-dir` when the state is not the default `~/.dispatch`, so
/// that the server reads this state, then `mcp`.
pub fn command(state: &State) -> Result<Vec<String>> {
    // Without a control character, a JSON string is also a TOML and a YAML
    // string.
    let text = |path: &Path| {
        path.to_str()
            .filter(|text| !text.chars().any(char::is_control))
            .map(str::to_owned)
            .with_context(|| {
                format!(
                    "{} cannot be written in an agent's configuration",
                    path.display()
                )
            })
    };
    let mut argv = vec![text(&std::env::current_exe()?)?];
    if dirs::home_dir().map(|home| home.join(".dispatch")).as_ref() != Some(&state.root) {
        argv.extend(["--state-dir".to_owned(), text(&state.root)?]);
    }
    argv.push("mcp".to_owned());
    Ok(argv)
}

/// How Claude Code's CLI adds the server for the user, before its command
/// line.
pub const ADD: [&str; 6] = ["mcp", "add", "--scope", "user", "dispatch", "--"];
/// How Claude Code's CLI removes it.
pub const REMOVE: [&str; 5] = ["mcp", "remove", "--scope", "user", "dispatch"];

/// An agent whose own configuration takes Dispatch's server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
    Cursor,
    Hermes,
}

impl Agent {
    pub const ALL: [Self; 4] = [Self::Claude, Self::Codex, Self::Cursor, Self::Hermes];

    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::Cursor => "Cursor",
            Self::Hermes => "Hermes",
        }
    }

    /// Its executable, as Dispatch finds it on PATH.
    pub fn program(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Cursor => "cursor-agent",
            Self::Hermes => "hermes",
        }
    }

    /// Its user configuration: `.claude.json` in `$CLAUDE_CONFIG_DIR`, else
    /// in the home directory; `config.toml` in `$CODEX_HOME`, else
    /// `~/.codex`; `~/.cursor/mcp.json`; and Hermes's `config.yaml`.
    pub fn path(self) -> Result<PathBuf> {
        let home = || {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .context("HOME is not set")
        };
        Ok(match self {
            Self::Claude => super::claude::config_dir()
                .map_or_else(home, Ok)?
                .join(".claude.json"),
            Self::Codex => std::env::var_os("CODEX_HOME")
                .filter(|home| !home.is_empty())
                .map(PathBuf::from)
                .map_or_else(|| home().map(|home| home.join(".codex")), Ok)?
                .join("config.toml"),
            Self::Cursor => home()?.join(".cursor").join("mcp.json"),
            Self::Hermes => hermes::config_path()?,
        })
    }

    /// Dispatch's server in the configuration at `path`.
    pub fn server(self, path: &Path) -> Result<Server> {
        match hermes::read(path)? {
            Some(text) => self.found(&text, path),
            None => Ok(Server::Absent),
        }
    }

    /// Exactly what connecting runs (Claude Code) or writes, where
    /// Dispatch's server is `found` now.
    pub fn lines(self, found: &Server, argv: &[String]) -> String {
        match self {
            Self::Claude => {
                let add = format!("claude {} {}", ADD.join(" "), shell_words::join(argv));
                match found {
                    Server::Ours(_) => format!("claude {}\n{add}", REMOVE.join(" ")),
                    _ => add,
                }
            }
            Self::Cursor => {
                let added = json!({"mcpServers": {"dispatch": entry_json(argv)}});
                serde_json::to_string_pretty(&added).expect("JSON serializes")
            }
            Self::Codex | Self::Hermes => self.block(argv).trim_start_matches('\n').to_owned(),
        }
    }

    /// Whether Dispatch's server at `path` can be changed where it is:
    /// always for Claude Code and Cursor, whose entry is changed by value,
    /// and for Codex and Hermes while their block is there as some Dispatch
    /// wrote it.
    pub fn as_written(self, path: &Path) -> Result<bool> {
        Ok(match self {
            Self::Claude | Self::Cursor => true,
            _ => self
                .ours(&hermes::read(path)?.unwrap_or_default())
                .is_some(),
        })
    }

    /// For Hermes, when its configuration has an `mcp_servers:` key of its
    /// own and no block of Dispatch's as written: the lines to add under that
    /// key by hand, since Dispatch then leaves the file alone.
    pub fn by_hand(self, path: &Path, argv: &[String]) -> Result<Option<String>> {
        if self != Self::Hermes || self.as_written(path)? {
            return Ok(None);
        }
        let text = hermes::read(path)?.unwrap_or_default();
        Ok((!self.appends(&text, path)?).then(|| entry_yaml(argv)))
    }

    /// Make the agent run `argv` as Dispatch's server, in place of one setup
    /// wrote before, or with `None` remove Dispatch's server and nothing
    /// else. Returns the backup of the file Dispatch wrote.
    pub fn change(self, path: &Path, argv: Option<&[String]>) -> Result<Option<PathBuf>> {
        if self == Self::Claude {
            let cli = which::which(self.program()).context("claude is not on PATH")?;
            return claude(&cli, path, argv).map(|()| None);
        }
        let existing = hermes::read(path)?;
        let text = existing.as_deref().unwrap_or_default();
        let found = match &existing {
            Some(text) => self.found(text, path)?,
            None => Server::Absent,
        };
        match (found, argv) {
            (Server::Changed, _) => anyhow::bail!(theirs(path)),
            (Server::Absent, None) => return Ok(None),
            _ => {}
        }
        let changed = match self {
            Self::Cursor => {
                // Its shape was checked as it was read (`found`).
                let mut config = match &existing {
                    Some(text) => json_value(text, path)?,
                    None => json!({}),
                };
                match argv {
                    Some(argv) => config["mcpServers"]["dispatch"] = entry_json(argv),
                    None => {
                        if let Some(servers) = config["mcpServers"].as_object_mut() {
                            servers.remove("dispatch");
                        }
                    }
                }
                serde_json::to_string_pretty(&config)? + "\n"
            }
            _ => self.marked(text, path, argv)?,
        };
        let wanted = argv.map_or(Server::Absent, |argv| Server::Ours(argv.to_vec()));
        anyhow::ensure!(
            self.found(&changed, path)? == wanted,
            "Dispatch's server would not read back as written; {} was left untouched",
            path.display()
        );
        save(path, existing.as_deref(), &changed)
    }

    /// Dispatch's server in the configuration `text`, as the agent reads it,
    /// or for Codex as Dispatch can without a TOML parser (`codex`). JSON
    /// that is not an object with an object or no `mcpServers` is not as the
    /// agent reads it, so Dispatch leaves it alone.
    fn found(self, text: &str, path: &Path) -> Result<Server> {
        Ok(match self {
            Self::Claude | Self::Cursor => {
                let config = json_value(text, path)?;
                let servers = &config["mcpServers"];
                anyhow::ensure!(
                    config.is_object() && (servers.is_null() || servers.is_object()),
                    "{} is not as {} reads it; it was left untouched",
                    path.display(),
                    self.name()
                );
                self.read_entry(&servers["dispatch"])
            }
            Self::Codex => codex(text),
            Self::Hermes => {
                let config = hermes::parse(text, path)?;
                match serde_json::to_value(&config["mcp_servers"]["dispatch"]) {
                    Ok(entry) => self.read_entry(&entry),
                    Err(_) => Server::Changed,
                }
            }
        })
    }

    /// What `entry` runs, when it is exactly an entry as written for it:
    /// `command` and `args`, and for Claude Code also `type` `"stdio"` and
    /// `env` `{}`, either or neither, as its CLI may write them.
    fn read_entry(self, entry: &Value) -> Server {
        if entry.is_null() {
            return Server::Absent;
        }
        let mut entry = entry.clone();
        if self == Self::Claude
            && let Some(fields) = entry.as_object_mut()
        {
            fields.retain(|key, value| match key.as_str() {
                "type" => value != "stdio",
                "env" => *value != json!({}),
                _ => true,
            });
        }
        let argv = || -> Option<Vec<String>> {
            let mut argv = vec![entry["command"].as_str()?.to_owned()];
            argv.extend(serde_json::from_value::<Vec<String>>(entry["args"].clone()).ok()?);
            Some(argv)
        };
        match argv() {
            Some(argv) if entry == entry_json(&argv) => Server::of(argv),
            _ => Server::Changed,
        }
    }

    /// The block Dispatch appends to Codex's or Hermes's configuration: a
    /// newline, so the file's last line ends whether it did or not, then the
    /// marker and the server. Codex calls its tools only with
    /// `default_tools_approval_mode = "approve"`: without it, `codex exec`
    /// refuses every call ("MCP tool call requires approval, but approval
    /// policy is never").
    fn block(self, argv: &[String]) -> String {
        match self {
            Self::Codex => format!(
                "\n{MARKER}\n[mcp_servers.dispatch]\ncommand = {}\nargs = {}\n\
                 default_tools_approval_mode = \"approve\"\n",
                string(&argv[0]),
                list(&argv[1..])
            ),
            _ => format!("\n{MARKER}\nmcp_servers:\n{}", entry_yaml(argv)),
        }
    }

    /// Where in `text` Dispatch's block is, exactly as some Dispatch wrote it
    /// for some command line, with nothing after it that would join it; and
    /// that command line.
    fn ours(self, text: &str) -> Option<(Range<usize>, Vec<String>)> {
        let (server, args) = match self {
            Self::Codex => ("[mcp_servers.dispatch]\ncommand = ", "args = "),
            _ => ("mcp_servers:\n  dispatch:\n    command: ", "    args: "),
        };
        let header = format!("\n{MARKER}\n{server}");
        let start = text.find(&header)?;
        let mut lines = text[start + header.len()..].lines();
        let mut argv = vec![serde_json::from_str::<String>(lines.next()?).ok()?];
        argv.extend(serde_json::from_str::<Vec<String>>(lines.next()?.strip_prefix(args)?).ok()?);
        let block = self.block(&argv);
        let end = start + block.len();
        let after = text[start..].starts_with(&block).then(|| &text[end..])?;
        let joins = match self {
            // A key before the next table header would be in Dispatch's table.
            Self::Codex => after
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty() && !line.starts_with('#'))
                .is_some_and(|line| !line.starts_with('[')),
            // Anything indented would be nested under Dispatch's keys.
            _ => after.starts_with([' ', '\t', '-']),
        };
        (!joins).then_some((start..end, argv))
    }

    /// Whether Dispatch's block can be appended to `text`: Hermes's
    /// configuration then must have no `mcp_servers:` key of its own.
    fn appends(self, text: &str, path: &Path) -> Result<bool> {
        Ok(self != Self::Hermes || hermes::parse(text, path)?.get("mcp_servers").is_none())
    }

    /// `text` with Dispatch's block running `argv`, or without it for `None`:
    /// the block as some Dispatch wrote it replaced, or a new one appended.
    fn marked(self, text: &str, path: &Path, argv: Option<&[String]>) -> Result<String> {
        let block = argv.map(|argv| self.block(argv)).unwrap_or_default();
        if let Some((range, _)) = self.ours(text) {
            let mut changed = text.to_owned();
            changed.replace_range(range, &block);
            return Ok(changed);
        }
        anyhow::ensure!(
            argv.is_some() && self.appends(text, path)?,
            "{} has an mcp_servers: key that is not as Dispatch added it (hermes config set \
             rewrites the whole file), so Dispatch leaves the file alone; change dispatch: under \
             it yourself",
            path.display()
        );
        Ok(format!("{text}{block}"))
    }
}

/// What Codex's configuration `text` holds of Dispatch's, as far as Dispatch
/// can tell without a TOML parser: its block, as some Dispatch wrote it, when
/// no other line names a `dispatch` server (`names`).
fn codex(text: &str) -> Server {
    match Agent::Codex.ours(text) {
        Some((range, argv)) if !names(&text[..range.start]) && !names(&text[range.end..]) => {
            Server::of(argv)
        }
        None if !names(text) => Server::Absent,
        _ => Server::Changed,
    }
}

/// Whether a line of Codex's configuration `text` may define a `dispatch`
/// server, or make `mcp_servers` an inline table, which no block can add to.
/// Each line is read alone, with whitespace and quotes dropped: no table is
/// tracked, since a line in a multi-line string or array is no header. A
/// line that only might be one counts too, so Dispatch never gives the file
/// a second server.
fn names(text: &str) -> bool {
    text.lines().any(|line| {
        let line: String = line
            .chars()
            .filter(|c| !c.is_whitespace() && !matches!(c, '"' | '\''))
            .collect();
        line.contains("mcp_servers.dispatch")
            || ["mcp_servers=", "dispatch=", "dispatch."]
                .iter()
                .any(|start| line.starts_with(start))
    })
}

/// Have Claude Code's CLI `cli` remove the server setup wrote, then, with
/// `argv`, add one running `argv`. Its configuration at `path` must then
/// read so.
fn claude(cli: &Path, path: &Path, argv: Option<&[String]>) -> Result<()> {
    let run = |args: &[&str]| -> Result<()> {
        let output = Command::new(cli)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("failed to run {}", cli.display()))?;
        anyhow::ensure!(
            output.status.success(),
            "claude {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(())
    };
    match Agent::Claude.server(path)? {
        Server::Changed => anyhow::bail!(theirs(path)),
        Server::Ours(_) => run(&REMOVE)?,
        Server::Absent => {}
    }
    if let Some(argv) = argv {
        let mut add = ADD.to_vec();
        add.extend(argv.iter().map(String::as_str));
        run(&add)?;
    }
    let wanted = argv.map_or(Server::Absent, |argv| Server::Ours(argv.to_vec()));
    anyhow::ensure!(
        Agent::Claude.server(path)? == wanted,
        "claude mcp returned, but {} does not show Dispatch's server as asked",
        path.display()
    );
    Ok(())
}

fn theirs(path: &Path) -> String {
    format!(
        "{} has a dispatch server that setup did not write; it was left untouched",
        path.display()
    )
}

/// The entry for `argv` under Cursor's `mcpServers`, and what Claude Code's
/// and Hermes's entries must read as.
fn entry_json(argv: &[String]) -> Value {
    json!({"command": argv[0], "args": argv[1..]})
}

/// Hermes's entry for `argv`, under `mcp_servers:`.
fn entry_yaml(argv: &[String]) -> String {
    format!(
        "  dispatch:\n    command: {}\n    args: {}\n",
        string(&argv[0]),
        list(&argv[1..])
    )
}

fn string(text: &str) -> String {
    serde_json::to_string(text).expect("a string serializes")
}

fn list(args: &[String]) -> String {
    let args: Vec<String> = args.iter().map(|arg| string(arg)).collect();
    format!("[{}]", args.join(", "))
}

fn json_value(text: &str, path: &Path) -> Result<Value> {
    serde_json::from_str(text).with_context(|| {
        format!(
            "{} is not valid JSON; it was left untouched",
            path.display()
        )
    })
}

/// Write `text` over the configuration at `path`, through a symbolic link if
/// it is one, after keeping `existing` beside it as
/// `<name>.dispatch-backup-<time>`, never over an earlier backup: setup may
/// change one file for a hook and for the server within a second. Both are
/// written durably, and readable only by their owner, as Hermes keeps its
/// configuration. Hermes's hooks are written here too.
pub(super) fn save(path: &Path, existing: Option<&str>, text: &str) -> Result<Option<PathBuf>> {
    let backup = match existing {
        Some(existing) => {
            let name = path.file_name().context("a configuration has a name")?;
            let stamp = chrono::Utc::now().format("%Y%m%d%H%M%S");
            let backup = (1..)
                .map(|n| {
                    let suffix = if n == 1 {
                        String::new()
                    } else {
                        format!("-{n}")
                    };
                    path.with_file_name(format!(
                        "{}.dispatch-backup-{stamp}{suffix}",
                        name.to_string_lossy()
                    ))
                })
                .find(|backup| !backup.exists())
                .expect("a name is free");
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
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::runtime::Hook;

    /// An earlier Dispatch, and this one with a state of its own, at paths a
    /// shell, TOML and YAML each need quoted.
    const OLD: &[&str] = &["/old/dispatch", "mcp"];
    const NEW: &[&str] = &[
        "/Applications/my \"apps\"/dispatch",
        "--state-dir",
        "/s/it's",
        "mcp",
    ];

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_owned()).collect()
    }

    fn ours(parts: &[&str]) -> Server {
        Server::Ours(argv(parts))
    }

    /// Each case is written as the file; `agent` must then read the person's
    /// own server there and refuse to change it, leaving every byte.
    fn left_alone(agent: Agent, path: &Path, cases: &[&str]) {
        for case in cases {
            fs::write(path, case).unwrap();
            assert_eq!(agent.server(path).unwrap(), Server::Changed, "{case}");
            for result in [
                agent.change(path, Some(&argv(NEW))),
                agent.change(path, None),
            ] {
                let error = result.unwrap_err().to_string();
                assert!(error.contains("setup did not write"), "{error}");
            }
            assert_eq!(fs::read_to_string(path).unwrap(), *case);
        }
    }

    #[test]
    fn a_command_line_is_dispatchs_only_in_the_form_setup_writes() {
        assert_eq!(Server::of(argv(OLD)), ours(OLD));
        assert_eq!(Server::of(argv(NEW)), ours(NEW));
        for theirs in [
            &["dispatch", "mcp"][..],
            &["/bin/dispatch", "mcp", "--root", "/code/p"],
            &["/bin/dispatch", "--state-dir", "state", "mcp"],
            &["/bin/dispatch", "serve"],
            &["/bin/dispatch"],
        ] {
            assert_eq!(Server::of(argv(theirs)), Server::Changed, "{theirs:?}");
        }
        let exe = std::env::current_exe().unwrap();
        let exe = exe.to_str().unwrap();
        let state = State::discover(Some(dirs::home_dir().unwrap().join(".dispatch"))).unwrap();
        assert_eq!(command(&state).unwrap(), [exe, "mcp"]);
        let temp = tempfile::tempdir().unwrap();
        let state = State::discover(Some(temp.path().to_owned())).unwrap();
        let dir = temp.path().to_str().unwrap();
        assert_eq!(command(&state).unwrap(), [exe, "--state-dir", dir, "mcp"]);
        assert_eq!(
            shell_words::join(argv(NEW)),
            r#"'/Applications/my "apps"/dispatch' --state-dir '/s/it'\''s' mcp"#
        );
    }

    #[test]
    fn codex_gets_one_marked_block_and_only_that_block_changes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let theirs = "model = \"o3\"\n\n[mcp_servers.time]\ncommand = \"uvx\"\nargs = [\"mcp-server-time\"]\n";
        fs::write(&path, theirs).unwrap();
        assert_eq!(Agent::Codex.server(&path).unwrap(), Server::Absent);
        let backup = Agent::Codex
            .change(&path, Some(&argv(OLD)))
            .unwrap()
            .unwrap();
        assert_eq!(fs::read_to_string(backup).unwrap(), theirs);
        let installed = format!(
            "{theirs}\n# Added by dispatch setup, which removes it unless it is edited.\n\
             [mcp_servers.dispatch]\ncommand = \"/old/dispatch\"\nargs = [\"mcp\"]\n\
             default_tools_approval_mode = \"approve\"\n"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), installed);
        assert_eq!(Agent::Codex.server(&path).unwrap(), ours(OLD));
        assert_eq!(
            Agent::Codex.lines(&Server::Absent, &argv(OLD)),
            installed[theirs.len() + 1..]
        );
        // Codex adds a table of its own after the block when a project is
        // trusted; the block stays Dispatch's.
        let trusted = "\n[projects.\"/code/p\"]\ntrust_level = \"trusted\"\n";
        fs::write(&path, format!("{installed}{trusted}")).unwrap();
        assert_eq!(Agent::Codex.server(&path).unwrap(), ours(OLD));
        // An earlier path is updated in place, quoted as TOML reads it.
        Agent::Codex
            .change(&path, Some(&argv(NEW)))
            .unwrap()
            .unwrap();
        let updated = fs::read_to_string(&path).unwrap();
        assert!(
            updated.starts_with(theirs) && updated.ends_with(trusted),
            "{updated}"
        );
        assert!(updated.contains(
            "command = \"/Applications/my \\\"apps\\\"/dispatch\"\n\
             args = [\"--state-dir\", \"/s/it's\", \"mcp\"]\n"
        ));
        assert_eq!(Agent::Codex.server(&path).unwrap(), ours(NEW));
        Agent::Codex.change(&path, None).unwrap().unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("{theirs}{trusted}")
        );
        assert_eq!(Agent::Codex.server(&path).unwrap(), Server::Absent);
        assert_eq!(Agent::Codex.change(&path, None).unwrap(), None);

        // A server the person wrote, in any form Codex reads; a key that
        // would join Dispatch's table; an inline mcp_servers, which no table
        // can add to.
        left_alone(
            Agent::Codex,
            &path,
            &[
                &format!("{theirs}[mcp_servers.dispatch]\ncommand = \"dispatch\"\n"),
                &format!("{theirs}[ mcp_servers . \"dispatch\" . env ]\nX = \"1\"\n"),
                "[mcp_servers]\ndispatch = { command = \"/bin/dispatch\", args = [\"mcp\"] }\n",
                "mcp_servers.dispatch.command = \"/bin/dispatch\"\n",
                "mcp_servers = { time = { command = \"uvx\" } }\n",
                "notes = \"\"\"\n[x]\n\"\"\"\nmcp_servers.dispatch.command = \"/bin/mine\"\n",
                "[mcp_servers]\ntime.args = [\n  [\"a\"],\n]\ndispatch.command = \"/bin/mine\"\n",
                &format!("{installed}startup_timeout_sec = 20\n"),
                &format!("{installed}{installed}"),
            ],
        );
        // A configuration that is not UTF-8 is left untouched.
        fs::write(&path, b"model = \"\xff\"\n").unwrap();
        let error = Agent::Codex.change(&path, Some(&argv(NEW))).unwrap_err();
        assert!(error.to_string().contains("left untouched"), "{error}");
        assert_eq!(fs::read(&path).unwrap(), b"model = \"\xff\"\n");
    }

    #[test]
    fn cursor_gets_its_entry_beside_the_persons_servers() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".cursor/mcp.json");
        let read = || serde_json::from_str::<Value>(&fs::read_to_string(&path).unwrap()).unwrap();
        // No file: one is made, with no backup.
        assert_eq!(Agent::Cursor.change(&path, Some(&argv(OLD))).unwrap(), None);
        assert_eq!(
            read(),
            json!({"mcpServers": {"dispatch": {"command": "/old/dispatch", "args": ["mcp"]}}})
        );
        let theirs = json!({"mcpServers": {"time": {"command": "uvx", "args": ["mcp-server-time"]}}, "x": 1});
        fs::write(&path, theirs.to_string()).unwrap();
        assert_eq!(Agent::Cursor.server(&path).unwrap(), Server::Absent);
        let backup = Agent::Cursor
            .change(&path, Some(&argv(OLD)))
            .unwrap()
            .unwrap();
        assert_eq!(fs::read_to_string(backup).unwrap(), theirs.to_string());
        assert_eq!(Agent::Cursor.server(&path).unwrap(), ours(OLD));
        Agent::Cursor.change(&path, Some(&argv(NEW))).unwrap();
        let mut wanted = theirs.clone();
        wanted["mcpServers"]["dispatch"] = json!({"command": NEW[0], "args": NEW[1..]});
        assert_eq!(read(), wanted);
        assert_eq!(Agent::Cursor.server(&path).unwrap(), ours(NEW));
        let added = json!({"mcpServers": {"dispatch": wanted["mcpServers"]["dispatch"]}});
        assert_eq!(
            Agent::Cursor.lines(&Server::Absent, &argv(NEW)),
            serde_json::to_string_pretty(&added).unwrap()
        );
        Agent::Cursor.change(&path, None).unwrap();
        assert_eq!(read(), theirs);
        // An entry with anything more or less, or another command line.
        left_alone(
            Agent::Cursor,
            &path,
            &[
                r#"{"mcpServers": {"dispatch": {"command": "/bin/dispatch", "args": ["mcp"], "env": {"X": "1"}}}}"#,
                r#"{"mcpServers": {"dispatch": {"command": "/bin/dispatch"}}}"#,
                r#"{"mcpServers": {"dispatch": {"command": "dispatch", "args": ["mcp"]}}}"#,
                r#"{"mcpServers": {"dispatch": {"url": "http://localhost:1"}}}"#,
            ],
        );
        for unreadable in ["{", "[]", r#"{"mcpServers": []}"#] {
            fs::write(&path, unreadable).unwrap();
            assert!(
                Agent::Cursor.change(&path, Some(&argv(NEW))).is_err(),
                "{unreadable}"
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), unreadable);
        }
    }

    #[test]
    fn hermes_gets_one_marked_block_beside_dispatchs_hooks() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.yaml");
        let theirs = "# mine\nmodel:\n  default: keep  # theirs\n";
        fs::write(&path, theirs).unwrap();
        let hook = "'/old/dispatch' --state-dir '/s' hook hermes";
        let original = hermes::install(&path, hook).unwrap().unwrap();
        let hooked = fs::read_to_string(&path).unwrap();
        assert_eq!(Agent::Hermes.server(&path).unwrap(), Server::Absent);
        assert_eq!(Agent::Hermes.by_hand(&path, &argv(NEW)).unwrap(), None);
        let backup = Agent::Hermes
            .change(&path, Some(&argv(OLD)))
            .unwrap()
            .unwrap();
        assert_eq!(fs::read_to_string(&backup).unwrap(), hooked);
        assert_eq!(fs::read_to_string(&original).unwrap(), theirs);
        let installed = format!(
            "{hooked}\n# Added by dispatch setup, which removes it unless it is edited.\n\
             mcp_servers:\n  dispatch:\n    command: \"/old/dispatch\"\n    args: [\"mcp\"]\n"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), installed);
        assert_eq!(Agent::Hermes.server(&path).unwrap(), ours(OLD));
        assert_eq!(hermes::hook(&path).unwrap(), Hook::Exact(hook.into()));
        // As Hermes reads it, and as its example writes it.
        let config: serde_yaml::Value = serde_yaml::from_str(&installed).unwrap();
        let example = "dispatch:\n  command: /old/dispatch\n  args: [\"mcp\"]\n";
        assert_eq!(
            config["mcp_servers"],
            serde_yaml::from_str::<serde_yaml::Value>(example).unwrap()
        );
        Agent::Hermes.change(&path, Some(&argv(NEW))).unwrap();
        assert_eq!(Agent::Hermes.server(&path).unwrap(), ours(NEW));
        assert!(fs::read_to_string(&path).unwrap().starts_with(&hooked));
        Agent::Hermes.change(&path, None).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), hooked);
        // Dispatch's hooks come out as they went in, the server's block left.
        Agent::Hermes.change(&path, Some(&argv(NEW))).unwrap();
        let both = fs::read_to_string(&path).unwrap();
        hermes::uninstall(&path).unwrap();
        assert_eq!(Agent::Hermes.server(&path).unwrap(), ours(NEW));
        Agent::Hermes.change(&path, None).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), theirs);

        // Rewritten by `hermes config set`: still Dispatch's server by what
        // Hermes reads, but only the person changes it now.
        let rewritten = both.replace(
            "# Added by dispatch setup, which removes it unless it is edited.\n",
            "",
        );
        fs::write(&path, &rewritten).unwrap();
        assert_eq!(Agent::Hermes.server(&path).unwrap(), ours(NEW));
        assert!(Agent::Hermes.by_hand(&path, &argv(OLD)).unwrap().is_some());
        for result in [
            Agent::Hermes.change(&path, Some(&argv(OLD))),
            Agent::Hermes.change(&path, None),
        ] {
            let error = result.unwrap_err().to_string();
            assert!(error.contains("hermes config set"), "{error}");
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), rewritten);
        // An mcp_servers: of the person's own: the lines to add by hand.
        let time = format!("{theirs}mcp_servers:\n  time:\n    command: uvx\n");
        fs::write(&path, &time).unwrap();
        assert_eq!(Agent::Hermes.server(&path).unwrap(), Server::Absent);
        assert_eq!(
            Agent::Hermes.by_hand(&path, &argv(OLD)).unwrap().as_deref(),
            Some("  dispatch:\n    command: \"/old/dispatch\"\n    args: [\"mcp\"]\n")
        );
        assert!(Agent::Hermes.change(&path, Some(&argv(OLD))).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), time);
        left_alone(
            Agent::Hermes,
            &path,
            &[
                "mcp_servers:\n  dispatch:\n    command: /bin/dispatch\n    args: [mcp]\n    enabled: false\n",
                "mcp_servers:\n  dispatch:\n    command: dispatch\n    args: [mcp]\n",
                &format!("{installed}    env: {{}}\n"),
            ],
        );
        fs::write(&path, "model: [\n").unwrap();
        let error = Agent::Hermes.server(&path).unwrap_err();
        assert!(error.to_string().contains("not valid YAML"), "{error}");
        assert!(Agent::Hermes.change(&path, Some(&argv(OLD))).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "model: [\n");
    }

    /// Found in review: a line in a multi-line string or a nested array is no
    /// table header, and must not hide the person's own server, which a
    /// second `[mcp_servers.dispatch]` would make Codex reject.
    #[test]
    fn a_codex_string_or_nested_array_hides_no_server_of_the_persons() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        for theirs in [
            "notes = \"\"\"\n[x]\n\"\"\"\nmcp_servers.dispatch.command = \"/bin/mine\"\n",
            "[mcp_servers]\ntime.args = [\n  [\"a\"],\n]\ndispatch.command = \"/bin/mine\"\n",
        ] {
            fs::write(&path, theirs).unwrap();
            assert_eq!(
                Agent::Codex.server(&path).unwrap(),
                Server::Changed,
                "{theirs}"
            );
        }
    }

    /// Claude Code's CLI may write its entry with or without `type` and
    /// `env`: either way it is Dispatch's, and Remove is offered. Any other
    /// value or field is the person's.
    #[test]
    fn claude_codes_entry_is_dispatchs_with_or_without_type_and_env() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".claude.json");
        let write = |extra: Value| {
            let mut entry = json!({"command": "/old/dispatch", "args": ["mcp"]});
            entry
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            fs::write(
                &path,
                json!({"mcpServers": {"dispatch": entry}}).to_string(),
            )
            .unwrap();
        };
        for extra in [
            json!({}),
            json!({"type": "stdio"}),
            json!({"env": {}}),
            json!({"type": "stdio", "env": {}}),
        ] {
            write(extra.clone());
            assert_eq!(Agent::Claude.server(&path).unwrap(), ours(OLD), "{extra}");
        }
        for extra in [
            json!({"type": "sse"}),
            json!({"env": {"X": "1"}}),
            json!({"type": "stdio", "env": {}, "timeout": 30}),
        ] {
            write(extra.clone());
            assert_eq!(
                Agent::Claude.server(&path).unwrap(),
                Server::Changed,
                "{extra}"
            );
        }
    }

    /// JSON that is not an object, or whose `mcpServers` is not one, is not
    /// as Claude Code or Cursor reads it: Dispatch leaves it alone rather
    /// than offer a Connect that fails.
    #[test]
    fn json_of_another_shape_is_left_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        for agent in [Agent::Claude, Agent::Cursor] {
            for text in [
                "[]",
                "3",
                "\"x\"",
                "null",
                r#"{"mcpServers": []}"#,
                r#"{"mcpServers": "x"}"#,
            ] {
                fs::write(&path, text).unwrap();
                let error = agent.server(&path).unwrap_err().to_string();
                assert!(
                    error.contains("left untouched"),
                    "{agent:?} {text}: {error}"
                );
            }
        }
    }

    /// An empty `CLAUDE_CONFIG_DIR` is unset, for the hooks' settings and the
    /// server's `.claude.json` alike, never a relative path in the working
    /// directory.
    #[test]
    fn an_empty_claude_config_dir_is_unset() {
        let temp = tempfile::tempdir().unwrap();
        // In a process of its own: the environment is the whole process's.
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "--ignored",
                "runtime::mcp::tests::an_empty_claude_config_dir_is_unset_here",
            ])
            .env("HOME", temp.path())
            .env("CLAUDE_CONFIG_DIR", "")
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && said.contains("1 passed"),
            "{said}{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    #[ignore = "subprocess helper, exercised by an_empty_claude_config_dir_is_unset"]
    fn an_empty_claude_config_dir_is_unset_here() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(
            crate::runtime::claude::settings_path().unwrap(),
            home.join(".claude").join("settings.json")
        );
        assert_eq!(Agent::Claude.path().unwrap(), home.join(".claude.json"));
    }

    /// The server, then the hooks, change one Hermes configuration, most
    /// likely within the same second: each change keeps a backup of its own.
    #[test]
    fn the_server_and_the_hooks_keep_each_others_backups() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.yaml");
        fs::write(&path, "model: x\n").unwrap();
        let server = Agent::Hermes
            .change(&path, Some(&argv(OLD)))
            .unwrap()
            .unwrap();
        let connected = fs::read_to_string(&path).unwrap();
        let hooks = hermes::install(&path, "'/old/dispatch' --state-dir '/s' hook hermes")
            .unwrap()
            .unwrap();
        assert_ne!(server, hooks);
        assert_eq!(fs::read_to_string(&server).unwrap(), "model: x\n");
        assert_eq!(fs::read_to_string(&hooks).unwrap(), connected);
    }

    #[test]
    fn a_backup_never_replaces_an_earlier_one() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.yaml");
        let backups: Vec<_> = ["a: 1\n", "a: 2\n", "a: 3\n"]
            .into_iter()
            .map(|before| {
                (
                    save(&path, Some(before), "a: 4\n").unwrap().unwrap(),
                    before,
                )
            })
            .collect();
        for (backup, before) in &backups {
            assert_eq!(fs::read_to_string(backup).unwrap(), *before);
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), "a: 4\n");
    }

    /// Only `cp` ever writes `cli`, so no forked child holds it open for writing (ETXTBSY).
    fn executable(cli: &Path, script: &str) {
        let staged = cli.with_extension("script");
        fs::write(&staged, script).unwrap();
        let copied = Command::new("cp").arg(&staged).arg(cli).status().unwrap();
        assert!(copied.success());
        fs::set_permissions(cli, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A stand-in for Claude Code's CLI: `mcp add` and `mcp remove` change
    /// `.claude.json` as Claude Code 2.1.296 does, refusing to add over a
    /// server or to remove none; any other arguments fail. Every call is
    /// logged.
    fn stand_in(dir: &Path, config: &Path) -> (PathBuf, PathBuf) {
        let cli = dir.join("claude");
        let log = dir.join("claude.log");
        executable(
            &cli,
            &format!(
                r#"#!/usr/bin/env python3
import json, sys
from pathlib import Path
config, args = Path({config:?}), sys.argv[1:]
with open({log:?}, 'a') as log: log.write(json.dumps(args) + '\n')
data = json.loads(config.read_text()) if config.exists() else {{}}
servers = data.setdefault('mcpServers', {{}})
if args[:6] == ['mcp', 'add', '--scope', 'user', 'dispatch', '--'] and len(args) > 6:
    if 'dispatch' in servers: sys.exit('MCP server dispatch already exists in user config')
    servers['dispatch'] = {{'type': 'stdio', 'command': args[6], 'args': args[7:], 'env': {{}}}}
elif args == ['mcp', 'remove', '--scope', 'user', 'dispatch']:
    if servers.pop('dispatch', None) is None: sys.exit('No MCP server named dispatch')
else:
    sys.exit('unexpected arguments')
config.write_text(json.dumps(data, indent=2))
"#
            ),
        );
        (cli, log)
    }

    #[test]
    fn claude_code_is_changed_by_its_own_cli_and_only_read_by_dispatch() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".claude.json");
        let (cli, log) = stand_in(temp.path(), &path);
        let theirs = json!({"numStartups": 3, "mcpServers": {
            "time": {"type": "stdio", "command": "uvx", "args": ["mcp-server-time"], "env": {}}}});
        fs::write(&path, theirs.to_string()).unwrap();
        let read = || serde_json::from_str::<Value>(&fs::read_to_string(&path).unwrap()).unwrap();
        claude(&cli, &path, Some(&argv(OLD))).unwrap();
        assert_eq!(Agent::Claude.server(&path).unwrap(), ours(OLD));
        assert_eq!(read()["numStartups"], 3);
        claude(&cli, &path, Some(&argv(NEW))).unwrap();
        assert_eq!(Agent::Claude.server(&path).unwrap(), ours(NEW));
        assert_eq!(
            read()["mcpServers"]["dispatch"],
            json!({"type": "stdio", "command": NEW[0], "args": NEW[1..], "env": {}})
        );
        claude(&cli, &path, None).unwrap();
        assert_eq!(read(), theirs);
        // Nothing to remove runs nothing.
        claude(&cli, &path, None).unwrap();
        let add = |parts: &[&str]| argv(&[&ADD[..], parts].concat());
        let calls: Vec<Vec<String>> = fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(calls, [add(OLD), argv(&REMOVE), add(NEW), argv(&REMOVE)]);
        assert_eq!(
            Agent::Claude.lines(&ours(OLD), &argv(NEW)),
            format!(
                "claude mcp remove --scope user dispatch\nclaude mcp add --scope user dispatch -- {}",
                shell_words::join(argv(NEW))
            )
        );

        // The person's own server, and a file that is not JSON, are never
        // handed to the CLI.
        fs::remove_file(&log).unwrap();
        for (text, said) in [
            (
                r#"{"mcpServers": {"dispatch": {"type": "stdio", "command": "/bin/dispatch", "args": ["mcp"], "env": {"X": "1"}}}}"#,
                "setup did not write",
            ),
            (
                r#"{"mcpServers": {"dispatch": {"type": "http", "url": "http://localhost:1"}}}"#,
                "setup did not write",
            ),
            ("{", "not valid JSON"),
        ] {
            fs::write(&path, text).unwrap();
            for argv in [Some(&argv(NEW)[..]), None] {
                let error = claude(&cli, &path, argv).unwrap_err().to_string();
                assert!(error.contains(said), "{error}");
            }
            assert_eq!(fs::read_to_string(&path).unwrap(), text);
        }
        assert!(!log.exists());
        // A CLI that fails says why.
        let old = json!({"mcpServers": {"dispatch": {
            "type": "stdio", "command": "/old/dispatch", "args": ["mcp"], "env": {}}}});
        fs::write(&path, old.to_string()).unwrap();
        executable(&cli, "#!/bin/sh\necho 'config is locked' >&2\nexit 1\n");
        let error = claude(&cli, &path, Some(&argv(NEW)))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("claude mcp remove --scope user dispatch failed: config is locked"),
            "{error}"
        );
    }
}
