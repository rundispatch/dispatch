use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, CommandFactory, Parser, Subcommand};
use dispatch::{orchestrator, state::State};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "dispatch",
    version,
    about = "Dispatch — keep coding-agent work valid while the code moves"
)]
struct Cli {
    /// Override ~/.dispatch (also available as DISPATCH_HOME).
    #[arg(long, global = true, env = "DISPATCH_HOME")]
    state_dir: Option<PathBuf>,

    /// Show internal diagnostics. Repeat for more detail.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Use ordinary line input and scrollback instead of the live viewport.
    #[arg(long, global = true)]
    plain: bool,
    /// Use ASCII graph characters.
    #[arg(long, global = true)]
    ascii: bool,
    /// Keep native terminal colors (also respects NO_COLOR).
    #[arg(long, global = true)]
    no_color: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect local resource funding configuration without model calls.
    Resources,
    /// Guided local setup / explicit funding revalidation (requires a terminal).
    Setup {
        /// Choose and explicitly approve project verification commands.
        #[arg(long, conflicts_with = "provider")]
        checks: bool,
        #[arg(value_parser = ["codex", "claude"])]
        provider: Option<String>,
    },
    /// Follow committed semantic events (advanced, read-only JSON Lines).
    #[command(hide = true)]
    Events {
        run_id: String,
        #[arg(long, default_value_t = 0)]
        after: u64,
        #[arg(long, value_enum)]
        until: Option<dispatch::follow::Until>,
        #[arg(long, default_value_t = 30)]
        timeout: u64,
    },
    /// Create a small project-local Dispatch configuration.
    #[command(hide = true)]
    Init {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Replace an existing dispatch.yml.
        #[arg(long)]
        force: bool,
    },
    /// Check local dependencies and harness availability.
    #[command(hide = true)]
    Doctor {
        #[arg(default_value = ".")]
        source: PathBuf,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Work on a software task with your configured coding agent (or --agent).
    Run(RunArgs),
    /// Answer a durable clarification and continue within the existing goal limit.
    Answer {
        run_id: String,
        question_id: String,
        #[arg(long)]
        revision: u64,
        #[arg(long, default_value_t = 1)]
        generation: u32,
        #[arg(long)]
        answer: String,
        #[arg(long)]
        json: bool,
    },
    /// Cancel a goal awaiting a durable clarification.
    Cancel {
        run_id: String,
        question_id: String,
        #[arg(long)]
        revision: u64,
        #[arg(long, default_value_t = 1)]
        generation: u32,
        #[arg(long)]
        json: bool,
    },
    /// Show the state of one run (or the latest run).
    Status {
        run_id: Option<String>,
        /// Print the versioned result projection as JSON.
        #[arg(long)]
        json: bool,
        /// Replay committed events and the result as JSON Lines.
        #[arg(long, conflicts_with = "json")]
        jsonl: bool,
    },
    /// List recent runs.
    History {
        #[arg(short, long, default_value_t = 20)]
        limit: usize,
    },
    /// Explain which agent and resource the latest task used.
    Explain { run_id: Option<String> },
    /// Check whether a finished result is still valid against the source as it is now.
    Check {
        run_id: Option<String>,
        /// Print the verdict as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Redo a stale result as a new run on the current source (a new agent launch).
    Refresh(RefreshArgs),
    /// Attach external work Dispatch did not launch (an existing worktree).
    Attach(AttachArgs),
    /// Finish attached external work: freeze the delta, verify, become ready.
    Finish {
        run_id: String,
        /// Required when the integration root configures checks.verify.
        #[arg(long)]
        allow_unsafe_local: bool,
    },
    /// Watch this project in the foreground: show each Work item's coherence
    /// state, observe attached work whose owner has exited, and auto-apply
    /// attached work allowed to integrate. Stops when you stop it.
    Serve {
        /// Defaults to the current directory.
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        json: bool,
        /// The owner `dispatch start` runs: no view, a log on stderr.
        #[arg(long, hide = true, conflicts_with = "json")]
        background: bool,
    },
    /// Watch this project in the background and return: Work it knows about
    /// stays checked against the source as it moves. No agent is needed.
    Start {
        /// Defaults to the current directory.
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Show this project's Work live, as the watcher keeps it. Leaving the
    /// view (Ctrl+C) does not stop watching.
    Watch {
        /// Defaults to the current directory.
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Stop watching this project in the background.
    Stop {
        /// Defaults to the current directory.
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Remove the workspaces Dispatch made for Work that is over (rejected,
    /// closed, or applied and not yet released), after you confirm.
    Clean {
        /// Only list what would be removed.
        #[arg(long)]
        dry_run: bool,
        /// Remove without asking (required without a terminal).
        #[arg(long, conflicts_with = "dry_run")]
        yes: bool,
    },
    /// Show a run and its persisted signals.
    #[command(hide = true)]
    Show { run_id: String },
    /// An agent runtime's lifecycle hook (installed by `dispatch setup`):
    /// reads the runtime's event on stdin and never fails the runtime.
    #[command(hide = true)]
    Hook { provider: String },
    /// Print a candidate's patch.
    Diff {
        run_id: Option<String>,
        candidate: Option<String>,
        #[arg(long, conflicts_with = "name_only")]
        stat: bool,
        #[arg(long, conflicts_with = "stat")]
        name_only: bool,
    },
    /// Accept and safely apply the latest single result.
    Accept(AcceptArgs),
    /// Reject the latest single result without changing the source tree.
    Reject(ReviewArgs),
    /// Print the Dispatch version.
    Version,
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Desired software change. The source defaults to the current directory.
    task_or_legacy_source: Option<String>,

    /// Work on a source tree other than the current directory.
    #[arg(long)]
    source: Option<PathBuf>,

    /// Legacy task form retained for scripts using `run <source> --task ...`.
    #[arg(long, conflicts_with = "task_file", hide = true)]
    task: Option<String>,

    /// Read the task verbatim from a file, or '-' for stdin.
    #[arg(long, conflicts_with = "task")]
    task_file: Option<PathBuf>,

    /// Deliberately choose one supported coding agent.
    #[arg(long, value_parser = agent_id)]
    agent: Option<String>,

    /// Select a configured model resource.
    #[arg(long)]
    model: Option<String>,

    /// Select the configured provider-specific effort for this attempt.
    #[arg(long, value_parser = ["minimal", "low", "medium", "high", "xhigh"], hide = true)]
    effort: Option<String>,

    #[arg(long, hide = true)]
    config: Option<PathBuf>,

    #[arg(long, value_parser = ["local", "docker"], hide = true)]
    backend: Option<String>,

    #[arg(long, hide = true)]
    timeout: Option<u64>,

    /// Explicitly allow real agents or project checks to execute on the host.
    #[arg(long, hide = true)]
    allow_unsafe_local: bool,

    /// Forward only the environment variable names allowlisted in dispatch.yml.
    #[arg(long, hide = true)]
    allow_forwarded_env: bool,

    /// Print only the final versioned result projection.
    #[arg(long, conflicts_with = "jsonl")]
    json: bool,

    /// Stream committed events followed by the final result as JSON Lines.
    #[arg(long, conflicts_with = "json")]
    jsonl: bool,

    /// Apply the result automatically when verification passed and the work
    /// is coherent with the current source. Records no human review.
    #[arg(long)]
    auto_apply: bool,
}

#[derive(Debug, Args)]
struct RefreshArgs {
    run_id: Option<String>,

    /// Required again if the original run executed on the host.
    #[arg(long)]
    allow_unsafe_local: bool,

    /// Required again if the original run forwarded environment variables.
    #[arg(long)]
    allow_forwarded_env: bool,

    #[arg(long, hide = true)]
    config: Option<PathBuf>,

    /// Print only the final versioned result projection.
    #[arg(long, conflicts_with = "jsonl")]
    json: bool,

    /// Stream committed events followed by the final result as JSON Lines.
    #[arg(long, conflicts_with = "json")]
    jsonl: bool,

    /// Apply the result automatically when verification passed and the work
    /// is coherent with the current source. Records no human review.
    #[arg(long)]
    auto_apply: bool,
}

#[derive(Debug, Args)]
struct AttachArgs {
    /// Existing worktree to observe. Defaults to the current directory.
    #[arg(long)]
    workspace: Option<PathBuf>,

    /// The integration root. Defaults to the repository's main worktree when
    /// `--workspace` is a linked Git worktree; required for a plain directory.
    #[arg(long)]
    root: Option<PathBuf>,

    /// Describes the attached work; never sent to the agent.
    #[arg(long)]
    task: Option<String>,

    /// Free-text label for the external agent; never guessed.
    #[arg(long)]
    agent: Option<String>,

    /// The external agent's process ID, for liveness only; never signaled.
    #[arg(long)]
    pid: Option<u32>,

    /// Explicitly allow `dispatch finish` to run checks.verify on the host.
    #[arg(long)]
    allow_unsafe_local: bool,

    /// Apply automatically once the work is ready and coherent.
    #[arg(long)]
    auto_apply: bool,

    /// Wrapped form: the agent command to run after `--`.
    #[arg(last = true)]
    command: Vec<String>,
}

#[derive(Debug, Args)]
struct FeedbackArgs {
    /// Optional structured reason; repeat the flag for multiple labels.
    #[arg(long = "reason")]
    reasons: Vec<String>,

    /// Optional unrestricted explanation, stored verbatim.
    #[arg(long, conflicts_with = "explanation_file")]
    explanation: Option<String>,

    /// Read unrestricted explanation verbatim from this path, or '-' for stdin.
    #[arg(long, conflicts_with = "explanation")]
    explanation_file: Option<PathBuf>,
}

impl FeedbackArgs {
    fn read_explanation(&self) -> Result<Option<String>> {
        match &self.explanation_file {
            Some(path) => orchestrator::read_verbatim(path).map(Some),
            None => Ok(self.explanation.clone()),
        }
    }
}

#[derive(Debug, Args)]
struct ReviewArgs {
    run_id: Option<String>,

    #[command(flatten)]
    feedback: FeedbackArgs,
}

#[derive(Args, Debug)]
struct AcceptArgs {
    #[command(flatten)]
    review: ReviewArgs,

    /// Apply a REFRESH caused only by the file and symbol analysis, because you
    /// checked the work still holds. Requires an explanation; the project's
    /// checks must still pass on the merged tree. Never overrides STOP, a patch
    /// that no longer applies, or a failing check.
    #[arg(long)]
    despite_refresh: bool,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let filter = match cli.verbose {
        0 => EnvFilter::new("warn"),
        1 => EnvFilter::new("dispatch=info"),
        _ => EnvFilter::new("dispatch=debug"),
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_target(false)
        .init();

    let state = State::discover(cli.state_dir)?;
    let Some(command) = cli.command else {
        if !dispatch::presenter::suitable() {
            Cli::command().print_help()?;
            println!();
            std::process::exit(2);
        }
        return dispatch::presenter::session(
            &state,
            dispatch::presenter::Options {
                plain: cli.plain,
                ascii: cli.ascii,
                no_color: cli.no_color,
            },
        )
        .await;
    };
    match command {
        Command::Resources => {
            println!("{}", dispatch::setup::status(&state)?);
            Ok(())
        }
        Command::Setup { provider, checks } => {
            dispatch::presenter::resource_setup(
                &state,
                provider,
                checks,
                dispatch::presenter::Options {
                    plain: cli.plain,
                    ascii: cli.ascii,
                    no_color: cli.no_color,
                },
            )
            .await
        }
        Command::Events {
            run_id,
            after,
            until,
            timeout,
        } => {
            let reached = dispatch::follow::events(
                &state,
                &run_id,
                after,
                until,
                std::time::Duration::from_secs(timeout),
                std::io::stdout(),
            )
            .await?;
            if !reached {
                std::process::exit(124);
            }
            Ok(())
        }
        Command::Init { path, force } => orchestrator::init(&state, &path, force),
        Command::Doctor { source, config } => {
            orchestrator::doctor(&state, &source, config.as_deref()).await
        }
        Command::Answer {
            run_id,
            question_id,
            revision,
            generation,
            answer,
            json,
        } => {
            let output = if json {
                orchestrator::RunOutputMode::Json
            } else {
                orchestrator::RunOutputMode::Human
            };
            let run = orchestrator::answer_question(
                &state,
                orchestrator::QuestionCommand {
                    run_id,
                    question_id,
                    revision,
                    generation,
                },
                answer,
                output,
            )
            .await?;
            let code = orchestrator::run_result(&run).exit_code;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Command::Cancel {
            run_id,
            question_id,
            revision,
            generation,
            json,
        } => {
            orchestrator::cancel_question(
                &state,
                orchestrator::QuestionCommand {
                    run_id,
                    question_id,
                    revision,
                    generation,
                },
                if json {
                    orchestrator::RunOutputMode::Json
                } else {
                    orchestrator::RunOutputMode::Human
                },
            )?;
            Ok(())
        }
        Command::Run(args) => {
            let auto_apply = args.auto_apply;
            let json = args.json;
            let jsonl = args.jsonl;
            let (source, task) = read_run_input(&args)?;
            let request = orchestrator::RunRequest {
                source,
                task,
                agent: args.agent,
                model: args.model,
                effort: args.effort,
                config_path: args.config,
                backend: args.backend,
                timeout_secs: args.timeout,
                allow_unsafe_local: args.allow_unsafe_local,
                allow_forwarded_env: args.allow_forwarded_env,
                output: run_output_mode(json, jsonl, auto_apply),
                refreshed_from: None,
            };
            let run = orchestrator::run_dispatch(&state, request).await?;
            finish_run(&state, run, auto_apply, json, jsonl)
        }
        Command::Status {
            run_id,
            json,
            jsonl,
        } => {
            if json {
                orchestrator::status_json(&state, run_id.as_deref(), &std::env::current_dir()?)
            } else if jsonl {
                orchestrator::status_jsonl(&state, run_id.as_deref(), &std::env::current_dir()?)
            } else {
                orchestrator::status(&state, run_id.as_deref(), &std::env::current_dir()?)
            }
        }
        Command::History { limit } => orchestrator::history(&state, limit),
        Command::Explain { run_id } => {
            orchestrator::explain(&state, run_id.as_deref(), &std::env::current_dir()?)
        }
        Command::Check { run_id, json } => {
            orchestrator::check(&state, run_id.as_deref(), &std::env::current_dir()?, json)
        }
        Command::Refresh(args) => {
            let auto_apply = args.auto_apply;
            let json = args.json;
            let jsonl = args.jsonl;
            let output = run_output_mode(json, jsonl, auto_apply);
            let request = orchestrator::refresh_request(
                &state,
                args.run_id.as_deref(),
                &std::env::current_dir()?,
                orchestrator::RefreshOptions {
                    allow_unsafe_local: args.allow_unsafe_local,
                    allow_forwarded_env: args.allow_forwarded_env,
                    config_path: args.config,
                    output,
                },
            )?;
            let old = request.refreshed_from.clone().unwrap_or_default();
            let run = orchestrator::run_dispatch(&state, request).await?;
            if output == orchestrator::RunOutputMode::Human {
                println!("Refreshed from {old}; new run {}", run.id);
            }
            finish_run(&state, run, auto_apply, json, jsonl)
        }
        Command::Attach(args) => {
            let workspace = args.workspace.unwrap_or_else(|| PathBuf::from("."));
            let wrapped = !args.command.is_empty();
            let request = orchestrator::attach::AttachRequest {
                workspace,
                root: args.root,
                task: args.task,
                agent: args.agent,
                pid: args.pid,
                command: wrapped.then_some(args.command),
                allow_unsafe_local: args.allow_unsafe_local,
                auto_apply: args.auto_apply,
                runtime: None,
            };
            if wrapped {
                let code = orchestrator::attach::run_wrapped(&state, request).await?;
                if code != 0 {
                    std::process::exit(code);
                }
                Ok(())
            } else {
                orchestrator::attach::create(&state, request)?;
                Ok(())
            }
        }
        Command::Finish {
            run_id,
            allow_unsafe_local,
        } => {
            orchestrator::attach::finish(&state, &run_id, allow_unsafe_local).await?;
            Ok(())
        }
        Command::Serve {
            root,
            json,
            background,
        } => orchestrator::serve::serve(&state, root, json, background).await,
        Command::Start { root } => orchestrator::background::start(&state, root, cli.verbose),
        Command::Stop { root } => orchestrator::background::stop(&state, root),
        Command::Watch { root, json } => {
            use std::io::IsTerminal;
            // On a terminal, the view is where the person acts; piped, or
            // as JSON, or plain, it only reports.
            if json
                || cli.plain
                || !std::io::stdout().is_terminal()
                || !std::io::stdin().is_terminal()
            {
                orchestrator::serve::watch(&state, root, json).await
            } else {
                let root = dispatch::source::resolve_source(root.as_deref())?;
                dispatch::presenter::watch(
                    &state,
                    root,
                    dispatch::presenter::Options {
                        plain: cli.plain,
                        ascii: cli.ascii,
                        no_color: cli.no_color,
                    },
                )
                .await
            }
        }
        Command::Show { run_id } => orchestrator::show(&state, &run_id),
        Command::Clean { dry_run, yes } => {
            dispatch::presenter::clean(
                &state,
                dry_run,
                yes,
                dispatch::presenter::Options {
                    plain: cli.plain,
                    ascii: cli.ascii,
                    no_color: cli.no_color,
                },
            )
            .await
        }
        Command::Hook { provider } => {
            use std::io::Read;
            anyhow::ensure!(provider == "claude", "unknown agent runtime: {provider}");
            let mut input = Vec::new();
            std::io::stdin()
                .take(dispatch::runtime::MAX_INPUT_BYTES as u64 + 1)
                .read_to_end(&mut input)?;
            let output = dispatch::runtime::claude::handle(&state, &input);
            if !output.stdout.is_empty() {
                println!("{}", output.stdout);
            }
            if !output.stderr.is_empty() {
                eprintln!("{}", output.stderr);
            }
            if output.code != 0 {
                std::process::exit(output.code);
            }
            Ok(())
        }
        Command::Diff {
            run_id,
            candidate,
            stat,
            name_only,
        } => orchestrator::diff(
            &state,
            run_id.as_deref(),
            candidate.as_deref(),
            &std::env::current_dir()?,
            stat,
            name_only,
        ),
        Command::Accept(args) => {
            let explanation = args.review.feedback.read_explanation()?;
            orchestrator::accept_or_reject_latest(
                &state,
                args.review.run_id.as_deref(),
                &std::env::current_dir()?,
                orchestrator::ReviewDecision::Accept {
                    despite_refresh: args.despite_refresh,
                },
                args.review.feedback.reasons,
                explanation,
            )
        }
        Command::Reject(args) => {
            let explanation = args.feedback.read_explanation()?;
            orchestrator::accept_or_reject_latest(
                &state,
                args.run_id.as_deref(),
                &std::env::current_dir()?,
                orchestrator::ReviewDecision::Reject,
                args.feedback.reasons,
                explanation,
            )
        }
        Command::Version => {
            println!("dispatch {}", dispatch::VERSION);
            Ok(())
        }
    }
}

/// After `run_dispatch` returns for `run`/`refresh`: report the result, and
/// when `--auto-apply` was set and the run reached Ready, apply it
/// automatically and report that outcome too, on every output mode.
///
/// Without the flag (or when the run is not Ready) this reproduces exactly
/// today's behavior: the exit code from `run_result`, nothing else. `--json`
/// only ever needed `RunOutputMode::Silent` (instead of `Json`) to defer its
/// print until the auto-apply decision is known; the not-Ready path below
/// prints the identical JSON itself so that deferral is invisible from the
/// outside. `--jsonl` never changes its `run_dispatch` output mode: the
/// auto-apply attempt's own events are streamed by the existing publisher
/// because the global output mode set by `run_dispatch` is still JSONL.
///
/// Exit code: `0` when applied; `6` when the run was Ready but the outcome
/// is skipped, blocked or failed and the run would otherwise have exited
/// `0`; otherwise the run's own exit code, unchanged (a run that already
/// failed for its own reason, for example verification, is not relabeled
/// "not applied automatically").
/// With `--json --auto-apply` the run stays silent: `finish_run` prints the
/// one result after the automatic application.
fn run_output_mode(json: bool, jsonl: bool, auto_apply: bool) -> orchestrator::RunOutputMode {
    if json && auto_apply {
        orchestrator::RunOutputMode::Silent
    } else if json {
        orchestrator::RunOutputMode::Json
    } else if jsonl {
        orchestrator::RunOutputMode::Jsonl
    } else {
        orchestrator::RunOutputMode::Human
    }
}

fn finish_run(
    state: &State,
    run: dispatch::RunRecord,
    auto_apply: bool,
    json: bool,
    jsonl: bool,
) -> Result<()> {
    let result = orchestrator::run_result(&run);
    let base_exit_code = result.exit_code;
    if !auto_apply || run.outcome.work_result != dispatch::WorkResult::Ready {
        if auto_apply && json {
            println!("{}", serde_json::to_string(&result)?);
        }
        if base_exit_code != 0 {
            std::process::exit(base_exit_code);
        }
        return Ok(());
    }

    let outcome = orchestrator::auto_apply(state, &run.id)?;
    let exit_code = match &outcome {
        orchestrator::ApplyOutcome::Applied { .. } => 0,
        _ if base_exit_code == 0 => 6,
        _ => base_exit_code,
    };

    if json {
        let reloaded = state.load_run(&run.id)?;
        let mut result = orchestrator::run_result(&reloaded);
        result.auto_apply = Some(outcome.summary());
        println!("{}", serde_json::to_string(&result)?);
    } else if jsonl {
        println!(
            "{}",
            serde_json::json!({
                "type": "auto_apply",
                "run_id": run.id,
                "auto_apply": outcome.summary(),
            })
        );
    } else if base_exit_code == 0 {
        match &outcome {
            orchestrator::ApplyOutcome::Applied { report, .. } => println!(
                "Auto-applied Candidate {} to {} ({} file(s) changed). Review not performed.",
                report.candidate_label,
                run.source_path.display(),
                report.files_changed
            ),
            orchestrator::ApplyOutcome::Blocked { reason, .. } => println!(
                "Not applied automatically: {reason}. Review with dispatch check {0} or dispatch accept {0}.",
                run.id
            ),
            orchestrator::ApplyOutcome::Skipped { reason } => println!(
                "Not applied automatically: {reason}. Review with dispatch check {0} or dispatch accept {0}.",
                run.id
            ),
            orchestrator::ApplyOutcome::Failed { error } => println!(
                "Not applied automatically: {error}. Review with dispatch check {0} or dispatch accept {0}.",
                run.id
            ),
        }
    }

    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

/// A supported coding agent. The deterministic `fake-*` adapters are accepted
/// but not advertised; they make the complete workflow testable offline.
fn agent_id(value: &str) -> std::result::Result<String, String> {
    match value {
        "claude" | "codex" | "cursor" => Ok(value.to_owned()),
        fake if fake.starts_with("fake-") => Ok(fake.to_owned()),
        _ => Err("possible values: claude, codex, cursor".into()),
    }
}

fn read_run_input(args: &RunArgs) -> Result<(PathBuf, String)> {
    let legacy_task = match (&args.task, &args.task_file) {
        (Some(task), None) => Some(task.clone()),
        (None, Some(path)) => Some(orchestrator::read_verbatim(path)?),
        (None, None) => None,
        (Some(_), Some(_)) => unreachable!("clap rejects multiple task inputs"),
    };
    match legacy_task {
        Some(task) => Ok((
            args.source.clone().unwrap_or_else(|| {
                args.task_or_legacy_source
                    .as_deref()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("."))
            }),
            task,
        )),
        None => Ok((
            args.source.clone().unwrap_or_else(|| PathBuf::from(".")),
            args.task_or_legacy_source
                .clone()
                .context("a task is required; use `dispatch run \"<task>\"`")?,
        )),
    }
}
