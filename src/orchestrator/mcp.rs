//! `dispatch mcp`: a local stdio MCP server for one project, so an
//! orchestrating agent can see the project's Work, wait on it and get the
//! landing proposal without a person polling a screen. JSON-RPC 2.0, one
//! message per line on stdin and stdout; stdout carries nothing else. Its
//! tools only read: none of them lands, accepts, rejects, closes, refreshes
//! or starts anything, and nothing is uploaded.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use anyhow::{Result, bail};
use serde_json::{Map, Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::mpsc,
    task::AbortHandle,
    time::Instant,
};

use super::{
    background::{self, Watcher},
    batch, one_line, serve, waits_for_decision,
};
use crate::{RunRecord, source, state::State};

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
                person runs `dispatch start` there. It starts and lands nothing.",
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
                waits on every Work that `work` lists, and on Work that appears. While \
                nothing watches the project (`dispatch start`), nothing records needs-you \
                marks, or verdicts for attached Work and finished results: attached Work \
                then wakes it only by finishing. It starts and lands nothing.",
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
    ])
}

/// Calls in flight, by their request id as JSON, for `notifications/cancelled`,
/// each with whether it is a `wait`.
type Calls = Arc<Mutex<HashMap<String, (AbortHandle, bool)>>>;

/// The row of each Work this server last gave the client, by `work` or by
/// `wait`, by run id: what a `wait` reports changes from.
type Seen = Arc<Mutex<HashMap<String, Value>>>;

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
    let (calls, seen) = (Calls::default(), Seen::default());
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
        if let Some(answer) = handle(state, &root, &line, &out, &calls, &seen) {
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
            // Held while the call is spawned, so that it cannot leave the
            // table before it is in it.
            let mut calls = calls.lock().unwrap();
            let task = tokio::spawn(async move {
                let result = call(state, root, seen, &name, arguments, progress).await;
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
    name: &str,
    arguments: Map<String, Value>,
    progress: Option<(Out, Value)>,
) -> Value {
    let result = match name {
        "work" => {
            let work = blocking(move || {
                only(&arguments, &[])?;
                work(&state, &root)
            })
            .await;
            // Nothing awaits between here and the answer, so the rows of a
            // call cancelled meanwhile are not taken as given.
            if let Ok(work) = &work {
                remember(&seen, work["work"].as_array().into_iter().flatten());
            }
            work
        }
        "status" => blocking(move || status(&state, &root, &arguments)).await,
        "wait" => wait(state, root, seen, arguments, progress).await,
        "propose_landing" => {
            blocking(move || {
                only(&arguments, &[])?;
                propose_landing(&state, &root)
            })
            .await
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
                let run = named(&runs, id)?;
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
    // Named Work is read by its full id from here on.
    let ids = ids.map(|_| rows.iter().map(id).collect::<Vec<_>>());
    let mut stats = None;
    let mut poll = tokio::time::interval(Duration::from_millis(500));
    let every = Duration::from_secs(10);
    let mut report = tokio::time::interval_at(started + every, every);
    loop {
        if let Some(answer) = changed(&seen, &rows) {
            return Ok(answer);
        }
        let files = watched(&state, &rows, ids.is_none());
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

/// What a wait stats for `rows`: each run's `events.jsonl`, appended to
/// whenever its row changes, after the database and `metadata.json`. While
/// any Work that appears is waited on, also `runs/`, which a run joins, and
/// the `events.jsonl` of each run there with no `metadata.json` yet: one
/// that `dispatch run` still prepares has no row until it is published, and
/// what is written inside it does not change `runs/`.
fn watched(state: &State, rows: &[Value], any: bool) -> Vec<PathBuf> {
    let ids = rows.iter().filter_map(|row| row["run_id"].as_str());
    let mut files: Vec<PathBuf> = ids.map(|id| state.events_path(id)).collect();
    if any {
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
