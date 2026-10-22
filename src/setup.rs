//! Shared local resource setup. Discovery is read-only; only an explicitly
//! confirmed proposal can write configuration. Launch authorization stays in core.
use crate::{
    ResourceTier,
    config::{ExecutionConfig, ResourceConfig, ResourceProfile},
    harness::ModelOption,
    state::State,
};
use anyhow::{Context, Result, ensure};
use chrono::Utc;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

pub struct Discovery {
    pub executable: PathBuf,
    pub version: String,
    pub funding: String,
    pub account: String,
    pub claude: Option<crate::harness::claude::SubscriptionEvidence>,
    pub codex: Option<crate::harness::codex::AccountEvidence>,
    /// Models setup offers for selection: listed by the installed CLI when it
    /// can list them, otherwise the adapter's suggestions.
    pub models: Vec<ModelOption>,
}
impl Discovery {
    pub fn summary(&self) -> String {
        format!(
            "{}\nAccount fingerprint: {}\nFunding: {} · quota may be unknown\nExecutable: {}",
            self.version,
            &self.account[..self.account.len().min(12)],
            self.funding,
            self.executable.display()
        )
    }
}

pub fn executable(provider: &str) -> Result<PathBuf> {
    ensure!(
        matches!(provider, "codex" | "claude"),
        "choose codex or claude"
    );
    // Setup never executes a repository's configured arbitrary executable.
    let path = which::which(provider).with_context(|| format!("{provider} is not installed on PATH. Install the provider CLI, then return to Resources."))?;
    let path = fs::canonicalize(path)?;
    Ok(path)
}

pub async fn discover(provider: &str, executable: PathBuf) -> Result<Discovery> {
    let version = crate::harness::probe_version(&executable)
        .await?
        .context("provider version unavailable; install a supported CLI")?;
    if provider == "claude" {
        let temp = tempfile::tempdir()?;
        let request = crate::harness::HarnessRunRequest::new(temp.path(), "", temp.path());
        let account = crate::harness::claude::discover_account(
            &executable,
            &crate::executor::Executor::new(ExecutionConfig::default()),
            &request,
        )
        .await?;
        let now = Utc::now();
        let evidence = crate::harness::claude::SubscriptionEvidence {
            contract_version: 1,
            cli_version: version.clone(),
            executable_sha256: hex::encode(Sha256::digest(fs::read(&executable)?)),
            account_sha256: account.clone(),
            checked_at: now,
            valid_until: now + chrono::TimeDelta::hours(24),
            // Discovery cannot assert inclusion, credits or account management.
            print_mode_included: false,
            usage_credits_disabled: false,
            unmanaged_account: false,
        };
        Ok(Discovery {
            executable,
            version,
            funding: "claude-subscription".into(),
            account,
            claude: Some(evidence),
            codex: None,
            models: crate::harness::claude::models(),
        })
    } else {
        ensure!(provider == "codex", "unsupported provider");
        use crate::harness::codex;
        let observed = codex::Funding::from_probe(
            &codex::read_account(&executable, crate::harness::PROBE_TIMEOUT).await?,
        );
        ensure!(
            observed.auth_mode.as_deref() == Some("chatgpt"),
            "ChatGPT subscription login was not confirmed. Choose Login, then revalidate; API and unknown authentication cannot be included."
        );
        let Some(plan) = &observed.plan else {
            anyhow::bail!("subscription plan is unknown; no profile was authorized")
        };
        let funding = format!("chatgpt-{plan}");
        let Some(account) = observed.account_sha256.clone() else {
            anyhow::bail!("account identity is unknown; no profile was authorized")
        };
        let evidence = codex::AccountEvidence {
            account_sha256: account.clone(),
            checked_at: Utc::now(),
        };
        ensure!(
            codex::refusal(&observed, Some(&evidence), &funding).is_none(),
            "paid credits or unsupported service detected; no profile was authorized"
        );
        // An unlisted model list is not a funding fact; setup still offers
        // typing a model ID.
        let models = codex::list_models(&executable, crate::harness::PROBE_TIMEOUT)
            .await
            .unwrap_or_default();
        Ok(Discovery {
            executable,
            version,
            funding,
            codex: Some(evidence),
            account,
            claude: None,
            models,
        })
    }
}

pub fn status(state: &State) -> Result<String> {
    let config = ResourceConfig::load(&state.root)?;
    let mut lines = vec![
        "Accounts / Resources".to_owned(),
        "Local operation · no Dispatch login required".into(),
    ];
    if !config.allocation_enabled {
        lines.push(
            "No resource is set up. One is needed only for Dispatch to launch an agent; attach, check, serve and review work without one."
                .into(),
        );
    }
    for provider in ["codex", "claude"] {
        lines.push(format!(
            "{provider}: {}",
            if which::which(provider).is_ok() {
                "installed · auth checked on revalidation"
            } else {
                "not found on PATH"
            }
        ));
    }
    for (i, p) in config.profiles.iter().enumerate() {
        let eligibility = p
            .eligibility()
            .map(|_| "funding configured · launch revalidation required".into())
            .unwrap_or_else(|e| format!("needs attention: {e}"));
        lines.push(format!(
            "[{}] {} / {} · {}\n    {eligibility}",
            i + 1,
            p.harness,
            p.model,
            p.effort.as_deref().unwrap_or("default effort")
        ));
    }
    if config.profiles.is_empty() {
        lines.push("No resource profiles. Add Codex or Claude to begin.".into());
    }
    Ok(crate::presenter::sanitize(&lines.join("\n")))
}

pub struct Proposal {
    pub config: ResourceConfig,
    pub index: usize,
    original: Option<Vec<u8>>,
    pub discovery: Discovery,
}
impl Proposal {
    pub fn prepare(
        state: &State,
        discovery: Discovery,
        provider: &str,
        existing: Option<usize>,
        model: String,
        effort: String,
    ) -> Result<Self> {
        let path = state.root.join("resources.yml");
        let original = read_config(&path)?;
        let mut config: ResourceConfig = match &original {
            Some(bytes) => serde_yaml::from_slice(bytes)?,
            None => ResourceConfig {
                version: 1,
                ..Default::default()
            },
        };
        config.validate()?;
        let highest = config
            .profiles
            .iter()
            .map(|p| p.authorization_revision)
            .max()
            .unwrap_or(0);
        // A new revision must be above every refused one: a funding refusal
        // is sticky per revision, so reusing a refused revision would leave
        // the re-authorized profile refused.
        let retained: u64 = if state.db_path().exists() {
            crate::db::Database::open_read_only(state.db_path())?.max_refused_revision()?
        } else {
            0
        };
        let revision = highest
            .max(retained)
            .checked_add(1)
            .context("authorization revision exhausted")?;
        let index = existing.unwrap_or(config.profiles.len());
        let profile = if let Some(index) = existing {
            let mut profile = config
                .profiles
                .get(index)
                .context("resource selection changed")?
                .clone();
            ensure!(
                profile.harness == provider
                    && profile.runtime == "local"
                    && profile.service_mode == "standard",
                "guided revalidation supports the selected provider in local standard mode only"
            );
            // Account scope cannot be silently moved into another shared pool.
            if let Some(old) = &profile.claude_subscription {
                ensure!(
                    old.account_sha256 == discovery.account,
                    "account changed: add a new resource explicitly; existing account scope was preserved"
                );
            }
            if let Some(old) = &profile.codex_account {
                ensure!(
                    old.account_sha256 == discovery.account,
                    "account changed: add a new resource explicitly; existing account scope was preserved"
                );
            }
            ensure!(
                profile.funding_source == discovery.funding || provider == "claude",
                "subscription plan changed: add a new resource explicitly"
            );
            profile.authorization_revision = revision;
            profile.claude_subscription = discovery.claude.clone();
            profile.codex_account = discovery.codex.clone();
            profile
        } else {
            let shared = config.profiles.iter().find(|p| {
                p.enabled
                    && p.harness == provider
                    && (provider == "codex" && p.funding_source == discovery.funding
                        || p.claude_subscription
                            .as_ref()
                            .is_some_and(|e| e.account_sha256 == discovery.account))
            });
            ResourceProfile {
                enabled: true,
                provider: if provider == "codex" {
                    "openai"
                } else {
                    "anthropic"
                }
                .into(),
                harness: provider.into(),
                funding_source: discovery.funding.clone(),
                model,
                effort: Some(effort),
                service_mode: "standard".into(),
                runtime: "local".into(),
                pool: shared
                    .map(|p| p.pool.clone())
                    .unwrap_or_else(|| format!("{provider}-included-{}", &discovery.account[..12])),
                provider_buckets: vec![],
                // Nothing selects on tier since 0.4.1; it is written so the
                // file still loads in earlier versions.
                tier: ResourceTier::Standard,
                included: false,
                no_overage_verified: false,
                authorization_revision: revision,
                claude_subscription: discovery.claude.clone(),
                codex_account: discovery.codex.clone(),
            }
        };
        ensure!(
            profile.enabled,
            "disabled resources stay disabled; enable them explicitly in advanced configuration"
        );
        if existing.is_some() {
            config.profiles[index] = profile;
        } else {
            config.profiles.push(profile);
        }
        config.validate()?;
        Ok(Self {
            config,
            index,
            original,
            discovery,
        })
    }
    /// What authorizing this proposal asserts, shown in full before the
    /// Authorize / Cancel choice.
    pub fn summary(&self) -> String {
        let p = &self.config.profiles[self.index];
        let claude = p.harness == "claude";
        format!(
            "{}\n\n{} / {} / {}\nLocal runtime · standard service · included only\n{}\n\nBy authorizing, you confirm:\n- this exact model and effort are included in your subscription;\n- paid overage and usage credits are disabled.{}",
            self.discovery.summary(),
            p.harness,
            p.model,
            p.effort.as_deref().unwrap_or("default"),
            if claude {
                "The authorization expires in 24 hours."
            } else {
                "Each launch checks the current account and funding again."
            },
            if claude {
                "\n- controlled print mode is included;\n- this account is unmanaged."
            } else {
                ""
            }
        )
    }
    /// Call only for the displayed, explicitly confirmed proposal. No trust is
    /// renewed by discovery, cancellation, or possession of provider credentials.
    pub fn confirm(mut self, state: &State) -> Result<()> {
        let p = &mut self.config.profiles[self.index];
        p.included = true;
        p.no_overage_verified = true;
        if let Some(e) = &mut p.claude_subscription {
            e.print_mode_included = true;
            e.usage_credits_disabled = true;
            e.unmanaged_account = true;
        }
        p.eligibility()?;
        self.config.allocation_enabled = true;
        self.config.validate()?;
        state.initialize()?;
        atomic_config(
            &state.root.join("resources.yml"),
            self.original.as_deref(),
            serde_yaml::to_string(&self.config)?.as_bytes(),
        )
    }
}

fn read_config(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            ensure!(
                meta.is_file() && !meta.file_type().is_symlink(),
                "configuration must be a regular file"
            );
            Ok(Some(fs::read(path)?))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
/// Serialize cooperating writers, refuse stale proposals, then replace atomically.
/// Unknown advanced YAML fields survive project-check updates (via Value below).
fn atomic_config(path: &Path, expected: Option<&[u8]>, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("configuration parent missing")?;
    // The lock is the configuration's directory itself, so setup leaves no
    // file in a project but the configuration it was asked to save, and
    // writers with different state directories still take turns.
    let lock = fs::File::open(parent)?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // Another writer holds the lock only for one small write, but a
        // process forked meanwhile holds it with them until it execs, which a
        // loaded machine delays: wait for it, up to a hang.
        let started = std::time::Instant::now();
        while unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            ensure!(
                started.elapsed() < std::time::Duration::from_secs(30),
                "another setup write is in progress; re-open Resources"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    ensure!(
        read_config(path)?.as_deref() == expected,
        "configuration changed while awaiting confirmation; reopen setup to compare again"
    );
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    lock.sync_all()?;
    drop(lock);
    Ok(())
}

pub fn check_choices(source: &Path) -> Vec<&'static str> {
    let mut choices = Vec::new();
    if source.join("verify.sh").is_file() {
        choices.push("sh ./verify.sh");
    }
    if source.join("Cargo.toml").is_file() {
        choices.push("cargo test --locked");
    }
    if source.join("Makefile").is_file() {
        choices.push("make test");
    }
    if source.join("package.json").is_file() {
        choices.push("npm test");
    }
    if source.join("go.mod").is_file() {
        choices.push("go test ./...");
    }
    let pytest = source.join("pytest.ini").is_file()
        || source.join("conftest.py").is_file()
        || fs::read_to_string(source.join("pyproject.toml"))
            .is_ok_and(|text| text.contains("[tool.pytest"));
    if pytest {
        choices.push("python3 -m pytest");
    } else if [source.to_path_buf(), source.join("tests")]
        .iter()
        .any(|dir| {
            fs::read_dir(dir).is_ok_and(|entries| {
                entries.flatten().any(|entry| {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    name.starts_with("test_") && name.ends_with(".py")
                })
            })
        })
    {
        choices.push("python3 -m unittest");
    }
    choices
}
/// Save detected checks. Each must still be detected, so a changed project
/// cannot turn a stale menu choice into an approval.
pub fn save_checks(source: &Path, commands: &[&str], expected: Option<Vec<u8>>) -> Result<()> {
    let detected = check_choices(source);
    ensure!(
        commands.iter().all(|command| detected.contains(command)),
        "check choice is no longer available"
    );
    write_checks(source, commands, expected).map(drop)
}
/// Save checks the person typed, exactly and in order; typing them is the
/// approval. Returns where they were saved.
pub fn save_typed_checks(
    source: &Path,
    commands: &[String],
    expected: Option<Vec<u8>>,
) -> Result<PathBuf> {
    let commands: Vec<&str> = commands.iter().map(|c| c.trim()).collect();
    ensure!(
        !commands.is_empty()
            && commands
                .iter()
                .all(|c| !c.is_empty() && !c.contains(['\n', '\r'])),
        "a check is one non-empty command line"
    );
    write_checks(source, &commands, expected)
}
fn write_checks(source: &Path, commands: &[&str], expected: Option<Vec<u8>>) -> Result<PathBuf> {
    let (_, path) = crate::Config::discover(source, None)?;
    let path = path.unwrap_or_else(|| source.join("dispatch.yml"));
    let mut value: serde_yaml::Value = expected
        .as_ref()
        .map(|b| serde_yaml::from_slice(b))
        .transpose()?
        .unwrap_or(serde_yaml::Value::Mapping(Default::default()));
    value["checks"]["verify"] = serde_yaml::to_value(commands)?;
    let bytes = serde_yaml::to_string(&value)?;
    let config: crate::Config = serde_yaml::from_str(&bytes)?;
    config.validate()?;
    atomic_config(&path, expected.as_deref(), bytes.as_bytes())?;
    Ok(path)
}
pub fn project_config_bytes(source: &Path) -> Result<Option<Vec<u8>>> {
    let (_, path) = crate::Config::discover(source, None)?;
    path.map(|p| read_config(&p))
        .transpose()
        .map(Option::flatten)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_setup_write_waits_for_the_lock_to_be_let_go() -> Result<()> {
        use std::os::fd::AsRawFd;
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("resources.yml");
        // Held a little longer than at once, as by a process forked while a
        // writer held it, not yet exec'd on a loaded machine.
        let held = fs::File::open(temp.path())?;
        assert_eq!(
            unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let written = path.clone();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(1));
            assert!(!written.exists(), "written while the lock was held");
            drop(held);
        });
        atomic_config(&path, None, b"version: 1\n")?;
        release.join().unwrap();
        assert_eq!(fs::read(&path)?, b"version: 1\n");
        Ok(())
    }
    fn discovery() -> Discovery {
        Discovery {
            executable: "/fixture/codex".into(),
            version: "fixture only".into(),
            funding: "chatgpt-plus".into(),
            account: "a".repeat(64),
            claude: None,
            codex: Some(crate::harness::codex::AccountEvidence {
                account_sha256: "a".repeat(64),
                checked_at: Utc::now(),
            }),
            models: vec![],
        }
    }
    fn proposal(state: &State) -> Proposal {
        Proposal::prepare(
            state,
            discovery(),
            "codex",
            None,
            "fixed-model".into(),
            "medium".into(),
        )
        .unwrap()
    }
    #[test]
    fn cancellation_writes_nothing_and_confirmation_is_atomic_and_private() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let state = State {
            root: temp.path().join("state"),
        };
        drop(proposal(&state));
        assert!(!state.root.exists());
        proposal(&state).confirm(&state)?;
        let config = ResourceConfig::load(&state.root)?;
        assert!(config.allocation_enabled && config.profiles[0].eligibility().is_ok());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(state.root.join("resources.yml"))?
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        Ok(())
    }
    #[test]
    fn concurrent_and_manually_changed_configuration_cannot_be_overwritten() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let state = State {
            root: temp.path().into(),
        };
        let first = proposal(&state);
        let stale = proposal(&state);
        first.confirm(&state)?;
        let before = fs::read(state.root.join("resources.yml"))?;
        assert!(stale.confirm(&state).is_err());
        assert_eq!(fs::read(state.root.join("resources.yml"))?, before);
        let current =
            Proposal::prepare(&state, discovery(), "codex", Some(0), "".into(), "".into())?;
        drop(current);
        assert_eq!(fs::read(state.root.join("resources.yml"))?, before);
        Ok(())
    }
    #[test]
    fn revalidation_does_not_change_model_tier_pool_or_disabled_policy() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let state = State {
            root: temp.path().into(),
        };
        proposal(&state).confirm(&state)?;
        let update = Proposal::prepare(
            &state,
            discovery(),
            "codex",
            Some(0),
            "other-model".into(),
            "low".into(),
        )?;
        assert_eq!(update.config.profiles[0].model, "fixed-model");
        assert_eq!(update.config.profiles[0].tier, ResourceTier::Standard);
        assert_eq!(update.config.profiles[0].authorization_revision, 2);
        update.confirm(&state)?;
        let mut config = ResourceConfig::load(&state.root)?;
        config.profiles[0].enabled = false;
        fs::write(
            state.root.join("resources.yml"),
            serde_yaml::to_string(&config)?,
        )?;
        assert!(
            Proposal::prepare(&state, discovery(), "codex", Some(0), "".into(), "".into(),)
                .is_err()
        );
        Ok(())
    }
    /// Setup against a state that already has a database, at the current
    /// schema. A new authorization revision is above every refused revision,
    /// so a refused funding identity is never re-authorized under a revision
    /// that is still refused.
    #[test]
    fn setup_with_an_existing_database_authorizes_above_refused_revisions() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let state = State {
            root: temp.path().into(),
        };
        state.initialize()?;
        crate::db::Database::open(state.db_path())?.record_funding_refusal(
            "codex:plus:account",
            4,
            "fixture refusal",
        )?;
        let proposal = proposal(&state);
        assert_eq!(proposal.config.profiles[0].authorization_revision, 5);
        proposal.confirm(&state)?;
        let update =
            Proposal::prepare(&state, discovery(), "codex", Some(0), "".into(), "".into())?;
        assert_eq!(update.config.profiles[0].authorization_revision, 6);
        Ok(())
    }
    #[test]
    fn checks_are_explicit_choices_preserve_configuration_and_refuse_drift() -> Result<()> {
        let t = tempfile::tempdir()?;
        fs::write(t.path().join("verify.sh"), "exit 0")?;
        fs::write(
            t.path().join("dispatch.yml"),
            "execution:\n  timeout_secs: 123\nprivate_evidence:\n  shadow: false\n",
        )?;
        let before = project_config_bytes(t.path())?;
        save_checks(t.path(), &["sh ./verify.sh"], before.clone())?;
        let (config, _) = crate::Config::discover(t.path(), None)?;
        assert_eq!(config.execution.timeout_secs, 123);
        assert_eq!(config.checks.verify, vec!["sh ./verify.sh"]);
        assert!(save_checks(t.path(), &["sh ./verify.sh"], before).is_err());
        let current = project_config_bytes(t.path())?;
        assert!(save_checks(t.path(), &["sh ./verify.sh", "curl attacker"], current).is_err());
        Ok(())
    }
    #[test]
    fn checks_are_detected_from_the_project_and_typed_checks_are_one_line() -> Result<()> {
        let t = tempfile::tempdir()?;
        assert!(check_choices(t.path()).is_empty());
        fs::write(t.path().join("test_calc.py"), "")?;
        assert_eq!(check_choices(t.path()), ["python3 -m unittest"]);
        fs::write(t.path().join("conftest.py"), "")?;
        fs::write(t.path().join("package.json"), "{}")?;
        fs::write(t.path().join("go.mod"), "module x")?;
        assert_eq!(
            check_choices(t.path()),
            ["npm test", "go test ./...", "python3 -m pytest"]
        );
        // A typed check is saved as typed, but never a detected-only choice.
        assert!(save_checks(t.path(), &["make lint"], None).is_err());
        save_typed_checks(
            t.path(),
            &["  make lint  ".into(), "make test".into()],
            None,
        )?;
        let (config, _) = crate::Config::discover(t.path(), None)?;
        assert_eq!(config.checks.verify, vec!["make lint", "make test"]);
        let current = project_config_bytes(t.path())?;
        for typed in [&[][..], &["".into()], &["a".into(), "a\nb".into()]] {
            assert!(save_typed_checks(t.path(), typed, current.clone()).is_err());
        }
        assert_eq!(project_config_bytes(t.path())?, current);
        Ok(())
    }
    #[test]
    fn symlink_configuration_is_never_replaced() -> Result<()> {
        #[cfg(unix)]
        {
            let t = tempfile::tempdir()?;
            let outside = t.path().join("original");
            fs::write(&outside, "private")?;
            std::os::unix::fs::symlink(&outside, t.path().join("resources.yml"))?;
            assert!(read_config(&t.path().join("resources.yml")).is_err());
            assert_eq!(fs::read_to_string(outside)?, "private");
        }
        Ok(())
    }
}
