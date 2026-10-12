//! L2 integration oracle: when the source moved but the static layers see no
//! conflict, run the project's own `checks.verify` commands against the merged
//! result (current source plus the candidate's patch) in a scratch tree before
//! anything is applied. Static facts cannot see transitive or behavioral
//! breakage; the user's checks can, and they cost no tokens.
//!
//! The source is only read. The scratch tree omits ignored build output (see
//! `create_scratch_tree`) and is removed on every exit path; the
//! check logs are kept under the run directory as evidence.

use std::{
    fmt, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use ulid::Ulid;

use super::{MAX_DETAIL_CHARS, MAX_REASONS, find_candidate, patch_is_empty, run_config};
use crate::{
    AnalysisLevel, CheckPhase, CheckResult, CheckStatus, Decision, Reason, ReasonCode, RunMode,
    RunRecord, Validity,
    config::{AcceptMode, Config},
    executor::run_checks_with_config,
    source::{apply_patch_in_workspace, create_scratch_tree, fingerprint_tree},
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergedChecks {
    Skipped(ChecksSkipped),
    /// They ran and passed; their logs are in this directory.
    Passed(PathBuf),
    /// They ran and did not all pass, or could not run: the verdict's
    /// reasons name the command and its logs.
    Failed,
}

impl fmt::Display for MergedChecks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Skipped(why) => write!(f, "not run — {why}"),
            Self::Passed(logs) => write!(
                f,
                "passed on the current source with this patch; logs: {}",
                logs.display()
            ),
            Self::Failed => f.write_str("did not pass"),
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

/// Removes the scratch tree when dropped, whatever path leaves the function.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

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

    let id = Ulid::new().to_string();
    let output_dir = run_dir.join("coherence-checks").join(&id);
    let new_reasons = match run_checks(run, &patch, &config, run_dir, &id, &output_dir).await {
        Ok(results) => match results
            .iter()
            .find(|result| result.status != CheckStatus::Passed)
        {
            None => {
                let validity = Validity {
                    analysis: AnalysisLevel::Integration,
                    ..validity
                };
                return Ok((validity, MergedChecks::Passed(output_dir)));
            }
            Some(_) => failure_reasons(&results),
        },
        Err(error) => vec![Reason {
            code: ReasonCode::AnalysisUncertain,
            fact_id: None,
            path: None,
            detail: cap(format!(
                "{COULD_NOT_RUN}: {error:#}; set coherence.integration_checks: false to skip"
            )),
        }],
    };
    let mut reasons = validity.reasons;
    reasons.extend(new_reasons);
    reasons.truncate(MAX_REASONS);
    let validity = Validity {
        decision: Decision::Refresh,
        reasons,
        ..validity
    };
    Ok((validity, MergedChecks::Failed))
}

/// Build the merged tree in a scratch area and run the verify commands there.
async fn run_checks(
    run: &RunRecord,
    patch: &Path,
    config: &Config,
    run_dir: &Path,
    id: &str,
    output_dir: &Path,
) -> Result<Vec<CheckResult>> {
    let scratch = Scratch(run_dir.join(format!("coherence-scratch-{id}")));
    // The current source minus ignored build output, as a plain directory;
    // `git apply` does not need a repository.
    create_scratch_tree(&run.source_path, &run.source_kind, &scratch.0)
        .context("failed to copy the current source")?;
    apply_patch_in_workspace(&scratch.0, patch)?;
    fs::create_dir_all(output_dir)
        .with_context(|| format!("failed to create {}", output_dir.display()))?;
    let results = run_checks_with_config(
        &scratch.0,
        &config.checks.verify,
        CheckPhase::Verify,
        output_dir,
        config.execution.clone(),
    )
    .await;
    if results.len() != config.checks.verify.len() {
        bail!("verification checks did not all report a result");
    }
    Ok(results)
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
