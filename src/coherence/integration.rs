//! L2 integration oracle: when the source moved but the static layers see no
//! conflict, run the project's own `checks.verify` commands against the merged
//! result (current source plus the candidate's patch) in a private check tree before
//! anything is applied. Static facts cannot see transitive or behavioral
//! breakage; the user's checks can, and they cost no tokens.
//!
//! The source is only read. The check tree keeps ignored build output between
//! runs; check logs are kept under the run directory as evidence.

use std::{
    fmt, fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use ulid::Ulid;

use super::{MAX_DETAIL_CHARS, MAX_REASONS, find_candidate, patch_is_empty, run_config};
use crate::{
    AnalysisLevel, CheckPhase, CheckResult, CheckStatus, Decision, Reason, ReasonCode, RunMode,
    RunRecord, SourceKind, Validity,
    config::{AcceptMode, Config},
    executor::run_checks_with_config,
    lock::OperationLock,
    orchestrator::source_lock_path,
    source::{
        check_workspace_path, checked_output, fingerprint_tree, git_command, inspect_source,
        prepare_check_workspace,
    },
    state::State,
};

/// Why accept does not run `checks.verify` on the merged tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksSkipped {
    /// The run's frozen configuration cannot be read, so accept is strict.
    ConfigUnreadable,
    /// `coherence.accept: strict`.
    Strict,
    /// The source has not moved since S0: the merged tree is the Work's own.
    Unmoved,
    /// `coherence.integration_checks: false`.
    Disabled,
    /// `checks.verify` is empty.
    NotConfigured,
    /// Local execution, and the run never recorded `--allow-unsafe-local`.
    NoAuthority,
    /// The patch is empty.
    EmptyPatch,
}

impl fmt::Display for ChecksSkipped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ConfigUnreadable => {
                "the run's frozen configuration cannot be read, so accept is strict and applies only onto a source unchanged since the Work began"
            }
            Self::Strict => {
                "accept is strict (coherence.accept: strict) and applies only onto a source unchanged since the Work began"
            }
            Self::Unmoved => {
                "the source has not moved since the Work began, so the merged result is the Work's own tree"
            }
            Self::Disabled => "they are turned off (coherence.integration_checks: false)",
            Self::NotConfigured => "no checks are configured (checks.verify is empty)",
            Self::NoAuthority => {
                "this Work has no recorded permission to run commands on the host (--allow-unsafe-local)"
            }
            Self::EmptyPatch => "the patch is empty",
        })
    }
}

/// Why accept will not run `checks.verify` on the merged tree for
/// `candidate_label` of `run`, or `None` when it will. The one place these
/// conditions live: the accept gate and its oracle act on the answer, and
/// finish, check and accept report it. `world_changed` is a fresh
/// evaluation's; before one (`None`), only a run Dispatch launched can show
/// an unmoved source, by its whole-tree fingerprint.
pub fn checks_skipped(
    run: &RunRecord,
    candidate_label: &str,
    run_dir: &Path,
    world_changed: Option<bool>,
) -> Result<Option<ChecksSkipped>> {
    let Some(config) = run_config(run_dir) else {
        return Ok(Some(ChecksSkipped::ConfigUnreadable));
    };
    if config.coherence.accept == AcceptMode::Strict {
        return Ok(Some(ChecksSkipped::Strict));
    }
    let unmoved = match world_changed {
        Some(changed) => !changed,
        // For a run Dispatch launched, the whole-tree fingerprint was taken
        // from the same tree S0 was copied from, so an equal fingerprint
        // means an unmoved world. Attached work's S0 is a merge-base commit
        // while its fingerprint is the root's working tree at attach time,
        // which may already differ from S0, so only an evaluation can say.
        None => {
            run.mode != RunMode::Attached
                && fingerprint_tree(&run.source_path)? == run.source_fingerprint
        }
    };
    let skipped = if unmoved {
        ChecksSkipped::Unmoved
    } else if !config.coherence.integration_checks {
        ChecksSkipped::Disabled
    } else if config.checks.verify.is_empty() {
        ChecksSkipped::NotConfigured
    } else if config.execution.backend == "local" && !run.environment.unsafe_local {
        // Never widen authority at accept time: local commands need the
        // acknowledgement the run itself recorded.
        ChecksSkipped::NoAuthority
    } else if patch_is_empty(&find_candidate(run, candidate_label)?.diff_path)? {
        ChecksSkipped::EmptyPatch
    } else {
        return Ok(None);
    };
    Ok(Some(skipped))
}

/// What became of the checks on the merged tree at accept.
#[derive(Debug, Clone)]
pub enum MergedChecks {
    Skipped(ChecksSkipped),
    /// They ran and passed.
    Passed(MergedRun),
    /// They ran and did not all pass, or could not run (`None`): the
    /// verdict's reasons name the command and its logs.
    Failed(Option<MergedRun>),
}

impl MergedChecks {
    /// The run, when the checks ran.
    pub fn ran(&self) -> Option<&MergedRun> {
        match self {
            Self::Passed(ran) | Self::Failed(Some(ran)) => Some(ran),
            Self::Skipped(_) | Self::Failed(None) => None,
        }
    }
}

impl fmt::Display for MergedChecks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Skipped(why) => write!(f, "not run — {why}"),
            Self::Passed(ran) => write!(
                f,
                "passed on the current source with this patch; logs: {}",
                ran.logs.display()
            ),
            Self::Failed(_) => f.write_str("did not pass"),
        }
    }
}

/// One run of `checks.verify` on a merged tree: the source state it was
/// built from, each command's result, and the directory with their logs.
#[derive(Debug, Clone)]
pub struct MergedRun {
    pub source: SourceState,
    pub logs: PathBuf,
    pub results: Vec<CheckResult>,
    pub tree_id: String,
    pub load_average: Option<[f64; 3]>,
}

impl MergedRun {
    pub fn passed(&self) -> bool {
        self.results
            .iter()
            .all(|result| result.status == CheckStatus::Passed)
    }

    /// What the merged-check event records, for accept and `check --merged`
    /// (`by`): the source state, the Δ digest, and each command's result
    /// with its duration. Test-runner counts are not parsed.
    pub fn event_payload(&self, by: &str, delta_digest: &str) -> serde_json::Value {
        serde_json::json!({
            "by": by,
            "source": self.source,
            "delta_digest": delta_digest,
            "passed": self.passed(),
            "duration_ms": self.results.iter().map(|result| result.duration_ms).sum::<u64>(),
            "results": self.results,
            "logs": self.logs,
            "tree_id": self.tree_id,
            "warm": true,
            "load_average": self.load_average,
        })
    }
}

/// The source a merged tree was built from: a Git checkout's HEAD (`None`
/// before its first commit) and whether its files differ from it, or a plain
/// directory, which has no version to name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceState {
    Git { head: Option<String>, dirty: bool },
    PlainDirectory,
}

impl SourceState {
    pub fn observe(source: &Path, kind: &SourceKind) -> Result<Self> {
        if matches!(kind, SourceKind::Directory) {
            return Ok(Self::PlainDirectory);
        }
        let (_, head) = inspect_source(source)?;
        let mut status = git_command(source);
        status.args(["status", "--porcelain", "--untracked-files=normal"]);
        let status = checked_output(status, "failed to read the source's Git status")?;
        Ok(Self::Git {
            head,
            dirty: !status.stdout.is_empty(),
        })
    }
}

impl fmt::Display for SourceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Git { head: None, .. } => f.write_str("Git, no commit yet"),
            Self::Git {
                head: Some(head),
                dirty,
            } => write!(
                f,
                "Git HEAD {}{}",
                &head[..head.len().min(12)],
                if *dirty {
                    " with uncommitted changes"
                } else {
                    ""
                }
            ),
            Self::PlainDirectory => f.write_str("plain directory"),
        }
    }
}

/// Longest slice of a configured command quoted in a reason, so the log path
/// after it survives the overall detail cap.
const MAX_COMMAND_CHARS: usize = 80;
/// Integration reasons carry a command, what failed, an excerpt and a log
/// path, so they get more room than a fact reason.
const MAX_INTEGRATION_DETAIL_CHARS: usize = 600;
const MAX_EXCERPT_CHARS: usize = 240;
/// How much of each output log is searched for the excerpt.
const EXCERPT_SCAN_BYTES: u64 = 256 * 1024;

/// Verify `candidate_label` against the merged tree. Returns `validity`
/// unchanged, with the reason, when `checks_skipped` says the checks do not
/// run. A failing check downgrades the verdict to Refresh; so does an
/// infrastructure error, because a false Refresh costs a rerun while a false
/// Continue would corrupt the source.
pub async fn verify_integration(
    run: &RunRecord,
    candidate_label: &str,
    validity: Validity,
    run_dir: &Path,
) -> Result<(Validity, MergedChecks)> {
    if let Some(skipped) =
        checks_skipped(run, candidate_label, run_dir, Some(validity.world_changed))?
    {
        return Ok((validity, MergedChecks::Skipped(skipped)));
    }
    let config = run_config(run_dir).context("the run's frozen configuration cannot be read")?;
    let patch = find_candidate(run, candidate_label)?.diff_path.clone();

    let (new_reasons, ran) = match run_merged(run, &patch, &config, run_dir, &new_logs(run_dir))
        .await
    {
        Ok(ran) if ran.passed() => {
            let validity = Validity {
                analysis: AnalysisLevel::Integration,
                ..validity
            };
            return Ok((validity, MergedChecks::Passed(ran)));
        }
        Ok(ran) => (failure_reasons(&ran.results), Some(ran)),
        Err(error) => (
            vec![Reason {
                code: ReasonCode::AnalysisUncertain,
                fact_id: None,
                path: None,
                detail: cap(format!(
                    "{COULD_NOT_RUN}: {error:#}; set coherence.integration_checks: false to skip"
                )),
            }],
            None,
        ),
    };
    let mut reasons = validity.reasons;
    reasons.extend(new_reasons);
    reasons.truncate(MAX_REASONS);
    let validity = Validity {
        decision: Decision::Refresh,
        reasons,
        ..validity
    };
    Ok((validity, MergedChecks::Failed(ran)))
}

/// A new directory under `run_dir` for one merged-check run's logs.
pub fn new_logs(run_dir: &Path) -> PathBuf {
    run_dir
        .join("coherence-checks")
        .join(Ulid::new().to_string())
}

/// Build the merged tree (the current source with `patch`) in its private,
/// persistent check workspace and run `config`'s verify commands there.
/// The one merged-check runner: accept's gate and `dispatch check --merged`.
pub async fn run_merged(
    run: &RunRecord,
    patch: &Path,
    config: &Config,
    run_dir: &Path,
    logs: &Path,
) -> Result<MergedRun> {
    let state_root = run_dir
        .parent()
        .and_then(Path::parent)
        .context("merged checks need a run directory under the state directory")?;
    let state = State {
        root: state_root.to_owned(),
    };
    let lock_path = source_lock_path(&state, run);
    if OperationLock::is_held(&lock_path)? {
        eprintln!(
            "Merged checks: waiting for the source lock on {}",
            run.source_path.display()
        );
    }
    let _source_lock = tokio::task::spawn_blocking(move || {
        loop {
            match OperationLock::acquire_wait(
                &lock_path,
                "another operation is using this source",
                Duration::from_secs(30),
            ) {
                Ok(lock) => return Ok(lock),
                Err(error) if error.to_string().contains("still busy after waiting") => continue,
                Err(error) => return Err(error),
            }
        }
    })
    .await
    .context("source lock waiter stopped")??;
    let source = SourceState::observe(&run.source_path, &run.source_kind)?;
    let workspace = check_workspace_path(state_root, &run.source_path);
    let tree_id = prepare_check_workspace(&run.source_path, &run.source_kind, &workspace, patch)
        .context("failed to build the merged check workspace")?;
    fs::create_dir_all(logs).with_context(|| format!("failed to create {}", logs.display()))?;
    let load_average = load_average();
    let results = run_checks_with_config(
        &workspace,
        &config.checks.verify,
        CheckPhase::Verify,
        logs,
        config.execution.clone(),
    )
    .await;
    if results.len() != config.checks.verify.len() {
        bail!("verification checks did not all report a result");
    }
    Ok(MergedRun {
        source,
        logs: logs.to_owned(),
        results,
        tree_id,
        load_average,
    })
}

#[cfg(target_os = "linux")]
fn load_average() -> Option<[f64; 3]> {
    let text = fs::read_to_string("/proc/loadavg").ok()?;
    let values = text
        .split_whitespace()
        .filter_map(|word| word.parse::<f64>().ok())
        .take(3)
        .collect::<Vec<_>>();
    values.try_into().ok()
}

#[cfg(target_os = "macos")]
fn load_average() -> Option<[f64; 3]> {
    let mut values = [0.0_f64; 3];
    // SAFETY: `values` holds three writable doubles for getloadavg.
    (unsafe { libc::getloadavg(values.as_mut_ptr(), values.len() as i32) } == values.len() as i32)
        .then_some(values)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn load_average() -> Option<[f64; 3]> {
    None
}

/// One bounded reason per check that did not pass.
fn failure_reasons(results: &[CheckResult]) -> Vec<Reason> {
    results
        .iter()
        .filter(|result| result.status != CheckStatus::Passed)
        .map(|result| {
            let command = result
                .command
                .chars()
                .take(MAX_COMMAND_CHARS)
                .collect::<String>();
            let logs = result.stderr_path.display();
            let (code, what) = match result.status {
                CheckStatus::Failed => (
                    ReasonCode::IntegrationCheckFailed,
                    match result.exit_code {
                        Some(code) => format!("failed (exit {code})"),
                        None => "failed (no exit code)".to_owned(),
                    },
                ),
                CheckStatus::TimedOut => {
                    (ReasonCode::IntegrationCheckFailed, "timed out".to_owned())
                }
                _ => (ReasonCode::AnalysisUncertain, "could not run".to_owned()),
            };
            let excerpt = if result.status == CheckStatus::Failed {
                excerpt(result)
                    .map(|line| format!(": {line}"))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            Reason {
                code,
                fact_id: None,
                path: None,
                detail: format!("check `{command}` {what}{excerpt}; logs: {logs}")
                    .chars()
                    .take(MAX_INTEGRATION_DETAIL_CHARS)
                    .collect(),
            }
        })
        .collect()
}

/// What failed, in the check's own words: the first two lines that read like a
/// failure (containing `FAIL`, `ERROR`, `Error`, `error:` or `panicked`, or
/// starting with `assert`), searching stderr then stdout, else the last
/// non-empty line. A test's own source line (`self.assertEqual(...)`) is not a
/// failure message, so `assert` counts only at the start of a line. `None` when the check printed
/// nothing. No language is special-cased.
fn excerpt(result: &CheckResult) -> Option<String> {
    use std::io::Read;
    let read = |path: &Path| -> Option<String> {
        let mut bytes = Vec::new();
        fs::File::open(path)
            .ok()?
            .take(EXCERPT_SCAN_BYTES)
            .read_to_end(&mut bytes)
            .ok()?;
        Some(String::from_utf8_lossy(&bytes).into_owned())
    };
    let outputs = [&result.stderr_path, &result.stdout_path]
        .into_iter()
        .filter_map(|path| read(path))
        .collect::<Vec<_>>();
    let lines = || {
        outputs
            .iter()
            .flat_map(|output| output.lines())
            .map(str::trim)
    };
    let marked = lines()
        .filter(|line| {
            line.starts_with("assert")
                || ["FAIL", "ERROR", "Error", "error:", "panicked"]
                    .iter()
                    .any(|marker| line.contains(marker))
        })
        .take(2)
        .collect::<Vec<_>>();
    let text = if marked.is_empty() {
        lines().rfind(|line| !line.is_empty())?.to_owned()
    } else {
        marked.join(" / ")
    };
    Some(text.chars().take(MAX_EXCERPT_CHARS).collect())
}

const COULD_NOT_RUN: &str = "integration checks could not run";

/// Whether `reason` came from running the checks on the merged tree, which only
/// accept does; a file and symbol evaluation can never reproduce it.
pub fn is_integration_reason(reason: &Reason) -> bool {
    reason.code == ReasonCode::IntegrationCheckFailed || reason.detail.starts_with(COULD_NOT_RUN)
}

fn cap(detail: String) -> String {
    detail.chars().take(MAX_DETAIL_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed_check(dir: &Path, stderr: &str, stdout: &str) -> CheckResult {
        let (err, out) = (dir.join("err.log"), dir.join("out.log"));
        fs::write(&err, stderr).unwrap();
        fs::write(&out, stdout).unwrap();
        CheckResult {
            name: "verify 1".into(),
            phase: crate::CheckPhase::Verify,
            command: "run tests".into(),
            status: CheckStatus::Failed,
            exit_code: Some(1),
            duration_ms: 1,
            stdout_path: out,
            stderr_path: err,
        }
    }

    #[test]
    fn a_failed_check_names_what_failed_in_its_own_words() {
        let temp = tempfile::tempdir().unwrap();
        let unittest = "..F\n======\nFAIL: test_whoami_admin (test_handlers.HandlerTests)\n------\nTraceback (most recent call last):\n  File \"t.py\", line 9\nAssertionError: Tuples differ: (200, 'alice <admin>') != (200, 'alice (admin)')\n\nFAILED (failures=1)\n";
        assert_eq!(
            excerpt(&failed_check(temp.path(), unittest, "")).unwrap(),
            "FAIL: test_whoami_admin (test_handlers.HandlerTests) / AssertionError: Tuples differ: (200, 'alice <admin>') != (200, 'alice (admin)')"
        );
        // The trial's real case: a test source line must not be the excerpt.
        let unittest_error = "E\n======\nERROR: test_admin_token (test_admin.RequireAdminTests.test_admin_token)\n------\nTraceback (most recent call last):\n  File \"tests/test_admin.py\", line 9, in test_admin_token\n    self.assertEqual(require_admin(\"t-alice\")[\"name\"], \"alice\")\nTypeError: validate() missing 1 required positional argument: 'token'\n";
        assert_eq!(
            excerpt(&failed_check(temp.path(), unittest_error, "")).unwrap(),
            "ERROR: test_admin_token (test_admin.RequireAdminTests.test_admin_token) / TypeError: validate() missing 1 required positional argument: 'token'"
        );
        let cargo = "running 1 test\ntest tests::adds ... FAILED\n\n---- tests::adds stdout ----\nthread 'tests::adds' panicked at src/lib.rs:5:9:\nassertion `left == right` failed\n";
        assert_eq!(
            excerpt(&failed_check(temp.path(), "", cargo)).unwrap(),
            "test tests::adds ... FAILED / thread 'tests::adds' panicked at src/lib.rs:5:9:"
        );
        assert!(excerpt(&failed_check(temp.path(), "", "")).is_none());
        assert_eq!(
            excerpt(&failed_check(
                temp.path(),
                "",
                "3 files differ\nsee report\n"
            ))
            .unwrap(),
            "see report"
        );
        let reasons = failure_reasons(&[failed_check(temp.path(), unittest, "")]);
        let detail = &reasons[0].detail;
        assert!(detail.starts_with("check `run tests` failed (exit 1): FAIL: test_whoami_admin"));
        assert!(detail.ends_with("err.log"), "{detail}");
    }
}
