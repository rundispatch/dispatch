//! `dispatch mcp`: a local stdio MCP server for one project, so an
//! orchestrating agent can see the project's Work, launch agents, message
//! their live sessions, wait on them and get the landing proposal without a
//! person polling a screen. JSON-RPC 2.0, one message per line on stdin and
//! stdout; stdout carries nothing else. None of its tools lands, accepts,
//! rejects, closes, refreshes or retries anything, and nothing is uploaded.
//! `start` launches only what the person's launch consent for the project
//! allows.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{mpsc, oneshot},
    task::AbortHandle,
    time::Instant,
};
use ulid::Ulid;

use super::{
    background::{self, Watcher},
    batch, inbox, notify, one_line, serve, waits_for_decision,
};
use crate::{
    Config, LifecycleState, NeedsYouKind, RunMode, RunRecord, RuntimeSession, consent,
    db::Database,
    process::{IdentityState, identity_state},
    runtime::claude,
    source,
    state::State,
};

/// The protocol versions answered as asked; any other gets the first.
const VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// `wait`'s default and longest wait, in seconds: the default is under
/// every client's limit, and a client that allows more may ask for it.
const WAIT_DEFAULT: f64 = 50.0;
const WAIT_MOST: f64 = 900.0;

/// The tools as `tools/list` gives them: what the orchestrating model reads.
fn tools() -> Value {
    let nothing = json!({"type": "object", "properties": {}, "additionalProperties": false});
    json!([
        {
            "name": "work",
            "description": "List this project's Work, one row each: Work in progress, \
                results waiting for review, and Work that finished in the last hour. A row \
                gives its run_id, task, agent, origin, subagent, workspace and state; its \
                verdict (continue, refresh, stop, or null before it is checked) and reason; \
                needs_you; asked_to_catch_up; its interactions with other Work; \
                files_changed (null until the Work finished with a Δ); ready_for_review; \
                verification and review. watched is false while nothing watches the \
                project: verdicts, needs-you and interactions are then not kept until the \
                person runs `dispatch start` there. launch_failed, when there is one, \
                lists each run the start tool launched that exited before it was created \
                (id, agent, task, log, exit, last_lines), each reported once, by work, \
                wait or start. It lands nothing, and starts nothing: only the start tool \
                launches, as the person's launch consent allows.",
            "inputSchema": nothing,
        },
        {
            "name": "status",
            "description": "One Work in full, exactly as `dispatch status <id> --json` \
                prints it. It starts and lands nothing.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "The Work's run_id, or a unique prefix of it."},
                },
                "required": ["id"],
                "additionalProperties": false,
            },
        },
        {
            "name": "wait",
            "description": "Wait until Work changes from the rows this server last gave, \
                by work or by wait, then return {woke, work}, where work holds the rows \
                that woke it and woke is: needs_you (its needs_you was set, or its state \
                became question); verdict (its verdict changed, other than to a first \
                continue); finished (its state became ready, blocked, finished, applied, \
                rejected or closed); or timeout. Work this server has not given yet \
                wakes it at once if it needs the person or is finished. Without ids it \
                waits on every Work that `work` lists, and on Work that appears. An id the \
                start tool gave is waited on before its run is created too, and a run it \
                launched that exits before it is created wakes it as launch_failed, with \
                work [] and launch_failed as work gives it. While nothing watches the \
                project (`dispatch start`), nothing records needs-you marks, or verdicts \
                for attached Work and finished results: attached Work then wakes it only by \
                finishing. It starts and lands nothing.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "ids": {
                        "type": "array",
                        "items": {"type": "string"},
                        "minItems": 1,
                        "description": "The Work to wait on, each by run_id or a unique prefix; leave it out to wait on any.",
                    },
                    "timeout_s": {
                        "type": "number",
                        "minimum": 1,
                        "maximum": WAIT_MOST,
                        "default": WAIT_DEFAULT,
                        "description": "The most seconds to wait.",
                    },
                },
                "additionalProperties": false,
            },
        },
        {
            "name": "start",
            "description": format!("Launch an agent on a task in this project, detached, \
                as `dispatch run --agent <agent>` would: it works in its own snapshot of the \
                project, and the project's checks run on what it makes. Returns {{id, agent, \
                log, note}} once its run is created, or after at most {} s while it is still \
                being prepared; wait on it with wait {{ids: [id]}}. Only an agent the person's \
                launch consent here names may be launched, with its command and the project's \
                checks as they were when the person allowed it, and only while fewer of the \
                project's native runs are running than that consent allows at once, a run \
                started elsewhere counting only once it is created. The person gives that \
                consent once, with `dispatch setup` in a terminal. A launch that exits before \
                its run is created is reported once, here or by the next work or wait, and \
                never launched again. It lands nothing: only the person lands what the agent \
                makes, with `dispatch accept`.", START_WAIT.as_secs()),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agent": {
                        "type": "string",
                        "description": "The agent to launch, as the person's launch consent names it: claude, codex, cursor or hermes.",
                    },
                    "task": {
                        "type": "string",
                        "description": "What the agent is to do. It is given this text verbatim.",
                    },
                },
                "required": ["agent", "task"],
                "additionalProperties": false,
            },
        },
        {
            "name": "propose_landing",
            "description": "What `dispatch accept` would land now: members, in the \
                order they would land as one decision, each with why; batch, their \
                digest; accept, the exact command for the person to type in the project \
                to land them; and left_out, each with its verdict (decision, null when it \
                cannot be judged now), reasons, and accept. A left_out result judged with \
                the members has no accept; any other has the command that lands it alone \
                when it can land on its own now, else null. A lone result that can land \
                is the member. It lands and accepts nothing and records no \
                proposal: only the person lands Work, by typing one of these commands. \
                While checks or a landing hold the source, it says so at once and \
                waits for nothing.",
            "inputSchema": nothing,
        },
        {
            "name": "send",
            "description": "Send a message to the live agent session of a Work: a \
                Claude Code session or subagent, or a Hermes session. Its hooks deliver \
                it once and verbatim, after any of Dispatch's notes waiting there, under \
                the line \"Message from the orchestrating session (<client>):\". \
                delivered_by says when: next_tool_call (with the result of the session's \
                next tool call; an idle session with no waiter, such as one started by \
                `dispatch attach -- claude`, gets it in its next turn), waiter (the \
                Claude Code session is idle, and its waiter wakes it with the message \
                within about a second) or next_turn (Hermes, before its next turn). The \
                text holds 8,000 characters less its label line and a newline (7,946 \
                for the client claude-code), counted as Claude Code counts them, in \
                UTF-16 code units, two for most emoji; a longer one is refused, never \
                cut. Returns {id, message, delivered_by}. It is refused for Codex, \
                Cursor and native runs, which no hook carries messages to; for Work \
                that is finished or closed, or whose session ended; and for a subagent \
                that stopped, which is not woken. It starts and lands nothing.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "The Work's run_id, or a unique prefix of it."},
                    "text": {"type": "string", "minLength": 1, "description": "The message, delivered exactly as written."},
                },
                "required": ["id", "text"],
                "additionalProperties": false,
            },
        },
    ])
}

/// Calls in flight, by their request id as JSON, for `notifications/cancelled`,
/// each with whether it is a `wait`.
type Calls = Arc<Mutex<HashMap<String, (AbortHandle, bool)>>>;

/// The row of each Work this server last gave the client, by `work` or by
/// `wait`, by run id: what a `wait` reports changes from.
type Seen = Arc<Mutex<HashMap<String, Value>>>;

/// The client's name, as its `initialize` gave it.
type Client = Arc<OnceLock<String>>;

/// A run `start` launched, by its id in `LAUNCHES`.
struct Launch {
    agent: String,
    task: String,
    log: PathBuf,
    /// Whether its `dispatch run` still runs.
    running: bool,
    /// How it exited before its run was created, until that is reported.
    failed: Option<String>,
}

/// Every run this server launched. One server serves one client, so they
/// are this process's; a launch reported as failed leaves.
static LAUNCHES: Mutex<BTreeMap<String, Launch>> = Mutex::new(BTreeMap::new());

/// Held by a `start` from counting what runs to recording its launch, so
/// that two cannot both take the last place.
static STARTING: Mutex<()> = Mutex::new(());

/// Where whole messages go to be written to stdout, one line each.
type Out = mpsc::UnboundedSender<Value>;

/// Serve the project at `root` (by default the main worktree of the current
/// directory) until stdin ends or stdout is gone. Each call runs on a task
/// of its own, so a `wait` holds up nothing else; one writer puts whole
/// lines on stdout. When stdin ends, the waits in flight stop and answer
/// nothing, and what the reads already running answer is written, briefly
/// bounded, before the server returns.
pub async fn serve(state: &State, root: Option<PathBuf>) -> Result<()> {
    let root = project(root)?;
    let (out, mut messages) = mpsc::unbounded_channel::<Value>();
    let mut writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(message) = messages.recv().await {
            let mut line = message.to_string().into_bytes();
            line.push(b'\n');
            stdout.write_all(&line).await?;
            stdout.flush().await?;
        }
        std::io::Result::Ok(())
    });
    let (calls, seen, client) = (Calls::default(), Seen::default(), Client::default());
    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut line = Vec::new();
    let gone = loop {
        line.clear();
        tokio::select! {
            read = stdin.read_until(b'\n', &mut line) => match read {
                Ok(0) | Err(_) => break false,
                Ok(_) => {}
            },
            _ = &mut writer => break true,
        }
        if line.trim_ascii().is_empty() {
            continue;
        }
        if let Some(answer) = handle(state, &root, &line, &out, &calls, &seen, &client) {
            let _ = out.send(answer);
        }
    };
    for (call, wait) in calls.lock().unwrap().values() {
        if *wait || gone {
            call.abort();
        }
    }
    // The writer ends once every call still running has answered.
    drop(out);
    if !gone {
        let _ = tokio::time::timeout(Duration::from_secs(5), writer).await;
    }
    Ok(())
}

/// The main worktree of `--root`, or else of the current directory, as
/// hooks find a session's project: Work is matched by its exact root.
fn project(root: Option<PathBuf>) -> Result<PathBuf> {
    let dir = source::resolve_source(root.as_deref())?;
    match crate::runtime::main_worktree(&dir) {
        Some(Some(main)) => source::resolve_source(Some(&main)),
        _ => Ok(dir),
    }
}

/// The answer to one message, or `None`: a notification is never answered,
/// and a tool call answers from its own task.
fn handle(
    state: &State,
    root: &Path,
    line: &[u8],
    out: &Out,
    calls: &Calls,
    seen: &Seen,
    client: &Client,
) -> Option<Value> {
    let Ok(message) = serde_json::from_slice::<Value>(line) else {
        return Some(error(Value::Null, -32700, "Parse error"));
    };
    // A batch, an array, is not part of the versions answered.
    let Some(message) = message.as_object() else {
        return Some(error(Value::Null, -32600, "Invalid Request"));
    };
    let id = message.get("id");
    let valid_id = id.is_none_or(|id| id.is_string() || id.is_number());
    let method = message.get("method").and_then(Value::as_str);
    let version = message.get("jsonrpc").and_then(Value::as_str);
    let (Some(method), true, Some("2.0")) = (method, valid_id, version) else {
        let id = id.filter(|_| valid_id).cloned().unwrap_or(Value::Null);
        return Some(error(id, -32600, "Invalid Request"));
    };
    let params = message.get("params").unwrap_or(&Value::Null);
    let Some(id) = id.cloned() else {
        if method == "notifications/cancelled"
            && let Some((call, _)) = calls
                .lock()
                .unwrap()
                .remove(&params["requestId"].to_string())
        {
            call.abort();
        }
        return None;
    };
    let result = match method {
        "initialize" => {
            let name = params["clientInfo"]["name"].as_str().unwrap_or("unknown");
            let _ = client.set(name.to_owned());
            let asked = params["protocolVersion"].as_str();
            let version = VERSIONS.into_iter().find(|&known| Some(known) == asked);
            json!({
                "protocolVersion": version.unwrap_or(VERSIONS[0]),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "dispatch", "version": crate::VERSION},
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools()}),
        "tools/call" => {
            let tools = tools();
            let listed = |name: &&str| tools.as_array().unwrap().iter().any(|t| t["name"] == *name);
            let name = params["name"].as_str().filter(listed);
            let arguments = match params.get("arguments") {
                None => Some(Map::new()),
                Some(arguments) => arguments.as_object().cloned(),
            };
            let (Some(name), Some(arguments)) = (name, arguments) else {
                let message = "Invalid params: name a tool that tools/list gives, with arguments as an object";
                return Some(error(id, -32602, message));
            };
            let token = &params["_meta"]["progressToken"];
            let progress =
                (token.is_string() || token.is_number()).then(|| (out.clone(), token.clone()));
            let (state, root, name) = (state.clone(), root.to_owned(), name.to_owned());
            let (key, out, done) = (id.to_string(), out.clone(), Arc::clone(calls));
            let (seen, wait) = (Arc::clone(seen), name == "wait");
            let client = Arc::clone(client);
            // Held while the call is spawned, so that it cannot leave the
            // table before it is in it.
            let mut calls = calls.lock().unwrap();
            let task = tokio::spawn(async move {
                let result = call(state, root, seen, client, &name, arguments, progress).await;
                done.lock().unwrap().remove(&id.to_string());
                let _ = out.send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
            });
            calls.insert(key, (task.abort_handle(), wait));
            return None;
        }
        _ => return Some(error(id, -32601, "Method not found")),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// One tool call's result: its value as compact JSON text, or one sentence
/// that says what to do, as an error.
async fn call(
    state: State,
    root: PathBuf,
    seen: Seen,
    client: Client,
    name: &str,
    arguments: Map<String, Value>,
    progress: Option<(Out, Value)>,
) -> Value {
    let result = match name {
        "work" => {
            let mut work = blocking(move || {
                only(&arguments, &[])?;
                work(&state, &root)
            })
            .await;
            // Nothing awaits between here and the answer, so the rows of a
            // call cancelled meanwhile are not taken as given, and no failed
            // launch is taken as reported.
            if let Ok(work) = &mut work {
                remember(&seen, work["work"].as_array().into_iter().flatten());
                let failed = failures(None);
                if !failed.is_empty() {
                    work["launch_failed"] = failed.into();
                }
            }
            work
        }
        "status" => blocking(move || status(&state, &root, &arguments)).await,
        "wait" => wait(state, root, seen, arguments, progress).await,
        "start" => start(state, root, client, arguments).await,
        "propose_landing" => {
            blocking(move || {
                only(&arguments, &[])?;
                propose_landing(&state, &root)
            })
            .await
        }
        "send" => {
            let client = client.get().map_or("unknown", String::as_str).to_owned();
            blocking(move || send(&state, &root, &client, &arguments)).await
        }
        _ => unreachable!("only a listed tool is called"),
    };
    match result {
        Ok(value) => json!({"content": [{"type": "text", "text": value.to_string()}]}),
        Err(error) => json!({
            "content": [{"type": "text", "text": format!("{error:#}")}],
            "isError": true,
        }),
    }
}

/// `read` on a thread of its own: it reads SQLite, Git and files.
async fn blocking<T: Send + 'static>(
    read: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(read).await?
}

/// Refuse any argument but those `takes` names.
fn only(arguments: &Map<String, Value>, takes: &[&str]) -> Result<()> {
    match arguments.keys().find(|key| !takes.contains(&key.as_str())) {
        Some(key) => bail!(
            "This tool takes no argument {}; tools/list gives the arguments it takes.",
            shown(key)
        ),
        None => Ok(()),
    }
}

/// Text from the client, as an answer quotes it.
fn shown(text: &str) -> String {
    format!("{:?}", one_line(text, 40))
}

fn work(state: &State, root: &Path) -> Result<Value> {
    let watched = matches!(background::watcher(state, root), Ok(Watcher::Watched(_)));
    let mut result = json!({"watched": watched, "work": rows(state, root, None)?});
    if !watched {
        result["note"] = "Nothing watches this project, so verdicts, needs-you and interactions \
                          are not kept until the person runs `dispatch start` in it."
            .into();
    }
    Ok(result)
}

/// `work`'s rows (`serve::work_row`): the project view's, or else one for
/// each Work that `ids` names, wherever it is.
fn rows(state: &State, root: &Path, ids: Option<&[String]>) -> Result<Vec<Value>> {
    let interactions = background::interactions(state, root);
    let runs = state.load_source_runs(root)?;
    let shown = match ids {
        None => serve::view_rows(&runs),
        Some(ids) => {
            let mut named_runs: Vec<&RunRecord> = Vec::new();
            for id in ids {
                let run = match named(&runs, id) {
                    // A run `start` launched has no row until it is created.
                    Err(_) if launched(id) => continue,
                    run => run?,
                };
                if !named_runs.iter().any(|named| named.id == run.id) {
                    named_runs.push(run);
                }
            }
            named_runs
        }
    };
    let rows = shown
        .into_iter()
        .map(|run| serve::work_row(state, run, interactions.as_ref()));
    Ok(rows.collect())
}

/// The Work of `runs` that `id` names, in full or by a unique prefix, as
/// the command line takes it.
fn named<'a>(runs: &'a [RunRecord], id: &str) -> Result<&'a RunRecord> {
    let prefix = id.to_ascii_uppercase();
    let mut matching = runs
        .iter()
        .filter(|run| !id.is_empty() && run.id.starts_with(&prefix));
    match (matching.next(), matching.next()) {
        (Some(run), None) => Ok(run),
        (None, _) => bail!(
            "No Work {} in this project; work lists its Work.",
            shown(id)
        ),
        (Some(_), Some(_)) => bail!(
            "More than one Work here has an id that starts with {}; give more of the id.",
            shown(id)
        ),
    }
}

fn status(state: &State, root: &Path, arguments: &Map<String, Value>) -> Result<Value> {
    only(arguments, &["id"])?;
    let Some(id) = arguments.get("id").and_then(Value::as_str) else {
        bail!("status needs id: a Work's run_id as work gives it, or a unique prefix of it.");
    };
    let id = named(&state.load_source_runs(root)?, id)?.id.clone();
    super::status_value(state, &state.load_run(&id)?)
}

/// A row's run id.
fn id(row: &Value) -> String {
    row["run_id"].as_str().unwrap_or_default().to_owned()
}

/// Note `rows` as the rows last given to the client.
fn remember<'a>(seen: &Seen, rows: impl IntoIterator<Item = &'a Value>) {
    let mut seen = seen.lock().unwrap();
    for row in rows {
        seen.insert(id(row), row.clone());
    }
}

/// Read the rows, and answer once one of them changed from the row last
/// given (`changed`); else stat what is written when they change, at most
/// every 500 ms, and read them again only once a stat changed: no SQLite
/// while nothing changes.
async fn wait(
    state: State,
    root: PathBuf,
    seen: Seen,
    arguments: Map<String, Value>,
    progress: Option<(Out, Value)>,
) -> Result<Value> {
    only(&arguments, &["ids", "timeout_s"])?;
    // A null is an argument left out, as models often send it.
    let given = |key: &str| arguments.get(key).filter(|value| !value.is_null());
    let ids = match given("ids") {
        None => None,
        Some(Value::Array(ids)) if !ids.is_empty() && ids.iter().all(Value::is_string) => Some(
            ids.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>(),
        ),
        Some(_) => {
            bail!("ids is a list of one or more Work ids; leave it out to wait on any Work.")
        }
    };
    let timeout = match given("timeout_s").map(Value::as_f64) {
        None => WAIT_DEFAULT,
        Some(Some(seconds)) if (1.0..=WAIT_MOST).contains(&seconds) => seconds,
        Some(_) => bail!("timeout_s is a number of seconds from 1 to 900."),
    };
    let started = Instant::now();
    let deadline = tokio::time::sleep(Duration::from_secs_f64(timeout));
    tokio::pin!(deadline);
    let read = |ids: Option<Vec<String>>| {
        let (state, root) = (state.clone(), root.clone());
        blocking(move || rows(&state, &root, ids.as_deref()))
    };
    let mut rows = read(ids.clone()).await?;
    // Named Work is read by its full id from here on, and so is a run
    // `start` launched, which has no row before it is created.
    let ids = ids.map(|ids| {
        let mut full: Vec<String> = rows.iter().map(id).collect();
        for launch in ids.iter().filter(|id| launched(id)) {
            let launch = launch.to_ascii_uppercase();
            if !full.contains(&launch) {
                full.push(launch);
            }
        }
        full
    });
    let mut stats = None;
    let mut poll = tokio::time::interval(Duration::from_millis(500));
    let every = Duration::from_secs(10);
    let mut report = tokio::time::interval_at(started + every, every);
    loop {
        if let Some(answer) = changed(&seen, &rows) {
            return Ok(answer);
        }
        let files = watched(&state, &rows, ids.as_deref());
        loop {
            tokio::select! {
                _ = &mut deadline => return Ok(json!({"woke": "timeout", "work": []})),
                _ = report.tick(), if progress.is_some() => {
                    if let Some((out, token)) = &progress {
                        let _ = out.send(json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/progress",
                            "params": {
                                "progressToken": token,
                                "progress": started.elapsed().as_secs(),
                                "total": timeout,
                            },
                        }));
                    }
                    continue;
                }
                _ = poll.tick() => {}
            }
            let failed = failures(ids.as_deref());
            if !failed.is_empty() {
                return Ok(json!({"woke": "launch_failed", "work": [], "launch_failed": failed}));
            }
            let now: Vec<_> = files.iter().map(|file| stat(file)).collect();
            if stats.as_ref() != Some(&now) {
                stats = Some(now);
                break;
            }
        }
        rows = read(ids.clone()).await?;
    }
}

/// `wait`'s answer once any of `rows` woke it (`woke`) from its row last
/// given, or from none: the rows that woke it, for the first of needs_you,
/// verdict and finished that one of them woke it for. They are then the
/// rows last given; rows that changed without waking it stay as they were.
fn changed(seen: &Seen, rows: &[Value]) -> Option<Value> {
    let mut seen = seen.lock().unwrap();
    let woke: Vec<(&str, &Value)> = rows
        .iter()
        .filter_map(|row| woke(seen.get(&id(row)), row).map(|why| (why, row)))
        .collect();
    let why = ["needs_you", "verdict", "finished"]
        .into_iter()
        .find(|why| woke.iter().any(|(woke, _)| woke == why))?;
    let rows: Vec<Value> = woke.into_iter().map(|(_, row)| row.clone()).collect();
    for row in &rows {
        seen.insert(id(row), row.clone());
    }
    Some(json!({"woke": why, "work": rows}))
}

/// What a wait stats for `rows`, or for the Work `ids` names: each run's
/// `events.jsonl`, appended to whenever its row changes, after the database
/// and `metadata.json`. While any Work that appears is waited on, also
/// `runs/`, which a run joins, and the `events.jsonl` of each run there with
/// no `metadata.json` yet: one that `dispatch run` still prepares has no row
/// until it is published, and what is written inside it does not change
/// `runs/`.
fn watched(state: &State, rows: &[Value], ids: Option<&[String]>) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = match ids {
        Some(ids) => ids.iter().map(|id| state.events_path(id)).collect(),
        None => rows
            .iter()
            .filter_map(|row| row["run_id"].as_str())
            .map(|id| state.events_path(id))
            .collect(),
    };
    if ids.is_none() {
        files.push(state.runs_dir());
        let entries = fs::read_dir(state.runs_dir()).into_iter().flatten();
        for entry in entries.flatten() {
            let id = entry.file_name().to_string_lossy().into_owned();
            if !state.metadata_path(&id).exists() {
                files.push(state.events_path(&id));
            }
        }
    }
    files
}

fn stat(path: &Path) -> Option<(u64, SystemTime)> {
    let metadata = fs::metadata(path).ok()?;
    Some((metadata.len(), metadata.modified().ok()?))
}

/// The states of a row whose Work is finished (`WorkLine::state`).
const FINISHED: [&str; 6] = [
    "ready", "blocked", "finished", "applied", "rejected", "closed",
];

/// Why `now` wakes a wait whose client was last given its row as `then`
/// (`None`: never), if it does: it came to need the person, its verdict
/// changed other than to a first `continue`, or it finished.
fn woke(then: Option<&Value>, now: &Value) -> Option<&'static str> {
    let was = |key: &str| then.map_or(&Value::Null, |then| &then[key]);
    let state = now["state"].as_str();
    if (!now["needs_you"].is_null() && now["needs_you"] != *was("needs_you"))
        || (state == Some("question") && was("state") != "question")
    {
        Some("needs_you")
    } else if then.is_some()
        && now["verdict"] != *was("verdict")
        && !(was("verdict").is_null() && now["verdict"] == "continue")
    {
        Some("verdict")
    } else if now["state"] != *was("state") && state.is_some_and(|state| FINISHED.contains(&state))
    {
        Some("finished")
    } else {
        None
    }
}

/// How long `start` waits for its run to be created, or for its child to
/// exit first, before it answers with the run still being prepared.
const START_WAIT: Duration = Duration::from_secs(5);

/// Launch an agent as the project's launch consent allows (`launch`), and
/// answer once its run is created, its child exited, or `START_WAIT`
/// passed. A child that exited before its run was created is reported
/// here, once; it is never launched again.
async fn start(
    state: State,
    root: PathBuf,
    client: Client,
    arguments: Map<String, Value>,
) -> Result<Value> {
    only(&arguments, &["agent", "task"])?;
    let text = |key: &str| {
        arguments
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let Some(agent) = text("agent") else {
        bail!("start needs agent: one the person's launch consent names, such as claude.");
    };
    let Some(task) = text("task").filter(|task| !task.trim().is_empty()) else {
        bail!("start needs task: what the agent is to do, as text.");
    };
    let client = client.get().map_or("unknown", String::as_str).to_owned();
    let (id, log, mut exited) = {
        let (state, root) = (state.clone(), root.clone());
        let (agent, task) = (agent.clone(), task.clone());
        blocking(move || launch(&state, &root, &agent, &task, &client)).await?
    };
    let created = state.metadata_path(&id);
    let deadline = tokio::time::sleep(START_WAIT);
    tokio::pin!(deadline);
    let mut poll = tokio::time::interval(Duration::from_millis(50));
    while !created.exists() {
        tokio::select! {
            _ = &mut exited => break,
            _ = &mut deadline => break,
            _ = poll.tick() => {}
        }
    }
    if let [failed] = &failures(Some(std::slice::from_ref(&id)))[..] {
        bail!(
            "The launch exited before its run was created ({}), and is not launched again; \
             its log, {}, ends:\n{}",
            failed["exit"].as_str().unwrap_or_default(),
            log.display(),
            failed["last_lines"].as_str().unwrap_or_default()
        );
    }
    let note = format!(
        "Wait on it with wait {{\"ids\": [\"{id}\"]}}; work lists it once its run is created, \
         and only the person lands what it makes, with dispatch accept."
    );
    Ok(json!({"id": id, "agent": agent, "log": log, "note": note}))
}

/// Launch `agent` on `task` in the project at `root`, as its launch consent
/// allows: a `dispatch run` of its own, detached, on an id chosen here, the
/// consent being its `--allow-unsafe-local`. A thread reaps it and notes how
/// it ended in `LAUNCHES`. Returns the id, the log and what fires when the
/// child exits.
fn launch(
    state: &State,
    root: &Path,
    agent: &str,
    task: &str,
    client: &str,
) -> Result<(String, PathBuf, oneshot::Receiver<()>)> {
    let Some(allowed) = consent::launch_consent(state, &consent::project_root(root)?)? else {
        bail!(
            "Nothing may be launched here until the person allows it, with `dispatch setup` in a terminal."
        );
    };
    let Some(pinned) = allowed.agents.get(agent) else {
        let names: Vec<&str> = allowed.agents.keys().map(String::as_str).collect();
        bail!(
            "The launch consent here names {}, not {}; start one of those, or ask the person to change it with `dispatch setup`.",
            names.join(", "),
            shown(agent)
        );
    };
    let (config, _) = Config::discover(root, None)?;
    if !config.execution.forwarded_env.is_empty() {
        bail!(
            "This project's dispatch.yml forwards environment variables to its runs, so only the person can launch one, from a terminal, with --allow-forwarded-env."
        );
    }
    let now = tokio::runtime::Handle::current()
        .block_on(consent::native_commands(root, agent, &config))?;
    if now != *pinned {
        bail!(
            "This project's command or checks for {} changed since the person allowed launches; ask them to allow it again with `dispatch setup`.",
            super::harness_name(agent)
        );
    }
    let _starting = STARTING.lock().unwrap();
    let running = running(state, root)?;
    if running >= allowed.max_running {
        let are = if running == 1 { "is" } else { "are" };
        bail!(
            "{running} of this project's native runs {are} running, and its launch consent allows {} at once; wait for one to finish, then start again.",
            allowed.max_running
        );
    }
    let id = Ulid::new().to_string();
    state.initialize()?;
    let started = state.root.join("started");
    fs::create_dir_all(&started)?;
    let task_file = started.join(format!("{id}.task"));
    private(&task_file)?.write_all(task.as_bytes())?;
    let log = started.join(format!("{id}.log"));
    let mut child = background::detached(state, root, &private(&log)?)?
        .arg("run")
        .arg(format!("--agent={agent}"))
        .arg("--task-file")
        .arg(&task_file)
        .arg("--allow-unsafe-local")
        .arg(format!("--run-id={id}"))
        .args(["--requested-by", "mcp"])
        .arg(format!("--client={client}"))
        .spawn()
        .context("failed to start dispatch run")?;
    let launch = Launch {
        agent: agent.to_owned(),
        task: task.to_owned(),
        log: log.clone(),
        running: true,
        failed: None,
    };
    LAUNCHES.lock().unwrap().insert(id.clone(), launch);
    let (exit, exited) = oneshot::channel();
    let (reaped, created) = (id.clone(), state.metadata_path(&id));
    std::thread::spawn(move || {
        let status = child.wait();
        if let Some(launch) = LAUNCHES.lock().unwrap().get_mut(&reaped) {
            launch.running = false;
            if !created.exists() {
                launch.failed = Some(status.map_or_else(|e| e.to_string(), |s| s.to_string()));
            }
        }
        let _ = exit.send(());
    });
    Ok((id, log, exited))
}

/// A new file only its owner can read.
fn private(path: &Path) -> Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))
}

/// How many of the project's native runs count against its launch consent:
/// this server's launches whose `dispatch run` still runs, and native Work
/// not finished whose supervisor still runs or whose agent may. A run
/// started elsewhere counts only once it is created.
fn running(state: &State, root: &Path) -> Result<usize> {
    let mut ids: HashSet<String> = LAUNCHES
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, launch)| launch.running)
        .map(|(id, _)| id.clone())
        .collect();
    let db = Database::open(state.db_path())?;
    for run in state.load_source_runs(root)? {
        if run.mode != RunMode::Native || run.outcome.lifecycle == LifecycleState::Finished {
            continue;
        }
        let supervisor = run.execution.as_ref().and_then(|e| e.supervisor.as_ref());
        let gone = supervisor.is_some_and(|owner| {
            matches!(
                identity_state(owner),
                IdentityState::Gone | IdentityState::Reused
            )
        });
        if !gone || super::native::any_launch_alive(&db, &run.id)? {
            ids.insert(run.id);
        }
    }
    Ok(ids.len())
}

/// Whether `id` names a run `start` launched, not yet reported as failed.
fn launched(id: &str) -> bool {
    LAUNCHES
        .lock()
        .unwrap()
        .contains_key(&id.to_ascii_uppercase())
}

/// The launches of `ids`, or of any without ids, whose child exited before
/// their run was created, each reported now and never again: its id, agent,
/// task, log, exit and the log's last lines, where `dispatch run` says why.
fn failures(ids: Option<&[String]>) -> Vec<Value> {
    let failed: Vec<(String, Launch)> = {
        let mut launches = LAUNCHES.lock().unwrap();
        let named = |id: &String| ids.is_none_or(|ids| ids.contains(id));
        let failed: Vec<String> = launches
            .iter()
            .filter(|(id, launch)| launch.failed.is_some() && named(id))
            .map(|(id, _)| id.clone())
            .collect();
        failed
            .iter()
            .filter_map(|id| launches.remove_entry(id))
            .collect()
    };
    failed
        .into_iter()
        .map(|(id, launch)| {
            let log = fs::read(&launch.log).unwrap_or_default();
            let log = String::from_utf8_lossy(&log);
            let lines: Vec<&str> = log.lines().collect();
            json!({
                "id": id,
                "agent": launch.agent,
                "task": launch.task,
                "log": launch.log,
                "exit": launch.failed,
                "last_lines": lines[lines.len().saturating_sub(20)..].join("\n"),
            })
        })
        .collect()
}

/// What `dispatch accept` without an id would land now, and the commands
/// that land it: a batch when two or more results can land together, else
/// the one result waiting for review when it can land; every other result
/// is judged on its own (`batch::alone`). Nothing is recorded or printed,
/// and no lock is waited for.
fn propose_landing(state: &State, root: &Path) -> Result<Value> {
    let mut proposal = match batch::eligible(state, root)? {
        Some(eligible) => {
            let mut proposal = batch::proposal(state, &eligible)?;
            proposal["accept"] = match proposal["batch"].as_str() {
                Some(digest) => format!("dispatch accept --expect {digest}").into(),
                None => Value::Null,
            };
            proposal
        }
        None => {
            let runs = state.load_source_runs(root)?;
            let alone: Vec<(&RunRecord, Value)> = runs
                .iter()
                .filter(|run| waits_for_decision(run))
                .map(|run| (run, batch::alone(run, None)))
                .collect();
            match &alone[..] {
                [(run, judged)] if !judged["accept"].is_null() => json!({
                    "batch": null,
                    "members": [{
                        "id": run.id,
                        "task": run.task,
                        "why": "it is the only result here waiting for review",
                    }],
                    "left_out": [],
                    "accept": judged["accept"],
                }),
                _ => json!({
                    "batch": null,
                    "members": [],
                    "left_out": alone.iter().map(|(_, judged)| judged).collect::<Vec<_>>(),
                    "accept": null,
                }),
            }
        }
    };
    let left_out = proposal["left_out"].as_array().cloned().unwrap_or_default();
    let alone = left_out.iter().any(|entry| !entry["accept"].is_null());
    let at = root.display();
    proposal["note"] = match (!proposal["accept"].is_null(), alone) {
        (true, false) => format!(
            "Nothing was landed or decided: only the person lands Work, by typing the accept command in {at}."
        ),
        (true, true) => format!(
            "Nothing was landed or decided: only the person lands Work, in {at}, by typing accept to land the members as one decision, or a left_out entry's accept to land that result alone."
        ),
        (false, true) => format!(
            "Nothing was landed or decided: nothing here lands as one decision, and only the person lands Work, in {at}, by typing a left_out entry's accept to land that result alone."
        ),
        (false, false) if left_out.is_empty() => "No result waits for review here.".to_owned(),
        (false, false) => "Nothing here can land now; left_out says why.".to_owned(),
    }
    .into();
    Ok(proposal)
}

/// Relay `text` from the orchestrating session of `client` to the live
/// session of the Work `id` names, through its inbox, and say which hook
/// delivers it (`carrier`).
fn send(state: &State, root: &Path, client: &str, arguments: &Map<String, Value>) -> Result<Value> {
    only(arguments, &["id", "text"])?;
    let given = |key: &str| arguments.get(key).and_then(Value::as_str);
    let (Some(id), Some(text)) = (given("id"), given("text")) else {
        bail!("send needs id, a Work's run_id as work gives it, and text, the message.");
    };
    if text.trim().is_empty() {
        let blank = if text.is_empty() { "empty" } else { "blank" };
        bail!("text is {blank}; give the message to send.");
    }
    let runs = state.load_source_runs(root)?;
    let run = named(&runs, id)?;
    let delivered_by = carrier(state, run)?;
    // A finish or a close may come before the lock: judged again under it.
    let Some(message) = inbox::send(state, &run.id, client, text, in_progress)? else {
        bail!("Work {} is busy; call again shortly.", run.id);
    };
    Ok(json!({"id": run.id, "message": message, "delivered_by": delivered_by}))
}

/// What ends `send`'s refusals of Work that is not live.
const LIVE: &str = "only a live session gets a message";

/// Refuse `run` once it is closed or finished.
fn in_progress(run: &RunRecord) -> Result<()> {
    if run.outcome.closed.is_some() {
        bail!("Work {} is closed; {LIVE}.", run.id);
    }
    if run.outcome.lifecycle != LifecycleState::Working {
        bail!("Work {} is finished; {LIVE}.", run.id);
    }
    Ok(())
}

/// The hook that delivers a message to `run`'s agent next, or why none
/// will: only Claude Code's and Hermes's hooks deliver an inbox (as they do
/// notes), to a session still open in Work in progress, and a subagent that
/// stopped is not woken. An idle Claude Code session's waiter delivers it
/// within about a second, when one waits (`claude::waiting`).
fn carrier(state: &State, run: &RunRecord) -> Result<&'static str> {
    let hooked =
        |session: &RuntimeSession| matches!(session.provider.as_str(), "claude" | "hermes");
    let attached = run.attachment.as_ref();
    let Some(attachment) = attached.filter(|a| a.sessions.iter().any(hooked)) else {
        let runtime = notify::runtime(run).unwrap_or("its agent");
        if attached.is_some() && runtime == "Claude Code" {
            bail!(
                "No Claude Code hook has reported a session for Work {}.",
                run.id
            );
        }
        let native = attached.map_or(" in a native run", |_| "");
        bail!("No hook carries messages to {runtime}{native}.");
    };
    in_progress(run)?;
    let id = &run.id;
    if attachment.workspace_removed.is_some() {
        bail!("Work {id}'s workspace was removed; {LIVE}.");
    }
    let open = |session: &&RuntimeSession| hooked(session) && session.ended_at.is_none();
    let Some(session) = attachment.sessions.iter().rfind(open) else {
        bail!("Work {id}'s session ended; {LIVE}.");
    };
    if let Some(subagent) = &session.subagent {
        if subagent.stopped_at.is_some() {
            bail!("Work {id} is a subagent that stopped, and a stopped subagent is not woken.");
        }
        return Ok("next_tool_call");
    }
    let idle = attachment.needs_you.as_ref().is_some_and(|mark| {
        mark.session_id == session.session_id && mark.kind != NeedsYouKind::Permission
    });
    Ok(match session.provider.as_str() {
        "hermes" => "next_turn",
        _ if idle && claude::waiting(state, &session.session_id) => "waiter",
        _ => "next_tool_call",
    })
}
