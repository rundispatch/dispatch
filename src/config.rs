use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(skip_serializing_if = "CoherenceConfig::is_default")]
    pub coherence: CoherenceConfig,
    pub execution: ExecutionConfig,
    pub checks: ChecksConfig,
    pub harnesses: HarnessesConfig,
}

/// How completed work is checked against a source tree that moved underneath it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CoherenceConfig {
    pub accept: AcceptMode,
    pub mid_run: MidRunMode,
    pub stop_on_refresh: bool,
    pub poll_secs: u64,
    pub integration_checks: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptMode {
    /// Validate the patch against the moved tree before applying.
    #[default]
    Validate,
    /// Legacy behavior: refuse on any source drift.
    Strict,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MidRunMode {
    #[default]
    Observe,
    Stop,
}

impl Default for CoherenceConfig {
    fn default() -> Self {
        Self {
            accept: AcceptMode::default(),
            mid_run: MidRunMode::default(),
            stop_on_refresh: false,
            poll_secs: 10,
            integration_checks: true,
        }
    }
}

impl CoherenceConfig {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ExecutionConfig {
    pub backend: String,
    pub timeout_secs: u64,
    pub cpus: f64,
    pub memory: String,
    pub docker_image: String,
    pub forwarded_env: Vec<String>,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            backend: "local".into(),
            timeout_secs: 1800,
            cpus: 2.0,
            memory: "4g".into(),
            docker_image: "ubuntu:24.04".into(),
            forwarded_env: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChecksConfig {
    pub baseline: Vec<String>,
    pub verify: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HarnessesConfig {
    pub claude: HarnessConfig,
    pub codex: HarnessConfig,
    pub cursor: HarnessConfig,
    pub hermes: HarnessConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HarnessConfig {
    #[serde(skip)]
    pub read_only: bool,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub effort: Option<String>,
    pub executable: Option<PathBuf>,
    pub extra_args: Vec<String>,
    /// Selected allocation service mode. Project YAML cannot set this value.
    #[serde(skip)]
    pub allocation_service_mode: Option<String>,
    #[serde(skip)]
    pub claude_subscription: Option<crate::harness::claude::SubscriptionEvidence>,
    /// Bound from the selected profile: the authorized Codex account and the
    /// funding source the adapter preflight must observe. Never from YAML.
    #[serde(skip)]
    pub codex_account: Option<crate::harness::codex::AccountEvidence>,
    #[serde(skip)]
    pub funding_source: Option<String>,
}

impl HarnessesConfig {
    pub fn get(&self, harness: &str) -> &HarnessConfig {
        match harness {
            "claude" => &self.claude,
            "cursor" => &self.cursor,
            "hermes" => &self.hermes,
            _ => &self.codex,
        }
    }
    pub fn bind_profile(&mut self, profile: &ResourceProfile) {
        let config = match profile.harness.as_str() {
            "claude" => &mut self.claude,
            _ => &mut self.codex,
        };
        config.model = Some(profile.model.clone());
        config.effort = profile.effort.clone();
        config.allocation_service_mode = Some(profile.service_mode.clone());
        config.claude_subscription = profile.claude_subscription.clone();
        config.codex_account = profile.codex_account.clone();
        config.funding_source = Some(profile.funding_source.clone());
    }
    pub fn bind(&mut self, choice: &crate::ResourceChoice, resources: &ResourceConfig) {
        if let Some(profile) = resources.profiles.iter().find(|p| {
            p.enabled
                && p.harness == choice.harness
                && p.provider == choice.provider
                && p.funding_source == choice.funding_source
                && p.pool == choice.pool
                && p.model == choice.resolved_model
                && p.effort == choice.effort
        }) {
            self.bind_profile(profile);
        }
    }
}

impl Config {
    pub fn discover(source: &Path, explicit: Option<&Path>) -> Result<(Self, Option<PathBuf>)> {
        let path = explicit.map(Path::to_path_buf).or_else(|| {
            [
                "dispatch.yml",
                "dispatch.yaml",
                ".dispatch.yml",
                ".dispatch.yaml",
            ]
            .into_iter()
            .map(|name| source.join(name))
            .find(|p| p.is_file())
        });
        match path {
            Some(path) => {
                let raw = fs::read_to_string(&path)
                    .with_context(|| format!("failed to read config {}", path.display()))?;
                let config = serde_yaml::from_str(&raw)
                    .with_context(|| format!("failed to parse config {}", path.display()))?;
                Ok((config, Some(path)))
            }
            None => Ok((Self::default(), None)),
        }
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            matches!(self.execution.backend.as_str(), "local" | "docker"),
            "execution.backend must be local or docker"
        );
        anyhow::ensure!(
            self.execution.timeout_secs > 0,
            "execution.timeout_secs must be positive"
        );
        anyhow::ensure!(self.execution.cpus > 0.0, "execution.cpus must be positive");
        anyhow::ensure!(
            !self.execution.memory.trim().is_empty(),
            "execution.memory must not be empty"
        );
        anyhow::ensure!(
            self.coherence.poll_secs > 0,
            "coherence.poll_secs must be positive"
        );
        for (name, harness) in [
            ("claude", &self.harnesses.claude),
            ("codex", &self.harnesses.codex),
            ("cursor", &self.harnesses.cursor),
            ("hermes", &self.harnesses.hermes),
        ] {
            validate_model(harness.model.as_deref())
                .with_context(|| format!("invalid harnesses.{name}.model"))?;
            validate_effort(harness.effort.as_deref())
                .with_context(|| format!("invalid harnesses.{name}.effort"))?;
            if name == "hermes" {
                validate_identifier(harness.provider.as_deref(), "provider")
                    .context("invalid harnesses.hermes.provider")?;
            }
        }
        Ok(())
    }

    pub fn example_yaml() -> &'static str {
        "execution:\n  backend: local\n  timeout_secs: 1800\n  cpus: 2\n  memory: 4g\n  docker_image: ubuntu:24.04\n  forwarded_env: []\nchecks:\n  baseline: []\n  # Replace [] with your project's verification commands, such as [cargo test].\n  verify: []\nharnesses:\n  claude:\n    model: null\n    effort: null\n    extra_args: []\n  codex:\n    model: null\n    effort: null\n    extra_args: []\n  cursor:\n    model: null\n    effort: null\n    extra_args: []\n  hermes:\n    model: null\n    provider: null\n    extra_args: []\n"
    }
}

pub fn validate_model(model: Option<&str>) -> Result<()> {
    validate_identifier(model, "model")
}

/// One value passed to a harness as a single argument: not empty, not an
/// option, no whitespace. `what` names it in the error.
fn validate_identifier(value: Option<&str>, what: &str) -> Result<()> {
    if let Some(value) = value {
        anyhow::ensure!(!value.trim().is_empty(), "{what} must not be empty");
        anyhow::ensure!(
            !value.starts_with('-') && !value.chars().any(char::is_whitespace),
            "{what} must be one non-option identifier"
        );
    }
    Ok(())
}

pub fn validate_effort(effort: Option<&str>) -> Result<()> {
    if let Some(effort) = effort {
        anyhow::ensure!(
            matches!(effort, "minimal" | "low" | "medium" | "high" | "xhigh"),
            "effort must be one of minimal, low, medium, high, or xhigh"
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourceConfig {
    pub version: u32,
    pub allocation_enabled: bool,
    pub profiles: Vec<ResourceProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceProfile {
    #[serde(default = "enabled_by_default", skip_serializing_if = "is_enabled")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_subscription: Option<crate::harness::claude::SubscriptionEvidence>,
    /// The Codex account authorized at setup; the adapter preflight refuses to
    /// launch when it observes any other account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_account: Option<crate::harness::codex::AccountEvidence>,
    pub provider: String,
    pub funding_source: String,
    pub harness: String,
    pub model: String,
    pub effort: Option<String>,
    pub service_mode: String,
    pub runtime: String,
    pub pool: String,
    /// Opaque provider limit IDs known to constrain this shared pool.
    #[serde(default)]
    pub provider_buckets: Vec<String>,
    pub tier: crate::ResourceTier,
    #[serde(default)]
    pub included: bool,
    #[serde(default)]
    pub no_overage_verified: bool,
    /// User-controlled epoch for the time-bound no-overage authorization.
    /// Incrementing this is the explicit acknowledgement required after a
    /// conflicting account or funding observation invalidates an epoch.
    #[serde(default = "default_authorization_revision")]
    pub authorization_revision: u64,
}

impl ResourceProfile {
    /// A local display-name change cannot manufacture a new account allowance.
    pub fn funding_scope(&self) -> String {
        self.claude_subscription
            .as_ref()
            .filter(|_| self.harness == "claude")
            .map(|e| format!("claude-account:{}", e.account_sha256))
            .unwrap_or_else(|| self.funding_source.clone())
    }
    pub fn eligibility(&self) -> Result<()> {
        anyhow::ensure!(self.enabled, "resource profile disabled");
        anyhow::ensure!(
            self.included && self.no_overage_verified,
            "included-only funding is not validated"
        );
        if self.harness == "claude" {
            crate::harness::claude::eligibility(self)?;
        }
        if self.harness == "codex" {
            anyhow::ensure!(
                self.codex_account.is_some(),
                "Codex account evidence missing; run `dispatch setup codex` to revalidate"
            );
        }
        Ok(())
    }

    /// The funding identity a refusal is recorded against: provider, plan and
    /// authorized account. With `authorization_revision` it keys the durable
    /// funding refusals that setup clears by re-authorizing.
    pub fn funding_key(&self) -> String {
        let account = self
            .claude_subscription
            .as_ref()
            .map(|e| e.account_sha256.as_str())
            .or(self
                .codex_account
                .as_ref()
                .map(|e| e.account_sha256.as_str()))
            .unwrap_or("");
        format!(
            "{}/{}/{}/{account}",
            self.provider, self.harness, self.funding_source
        )
    }
}

fn enabled_by_default() -> bool {
    true
}
fn is_enabled(value: &bool) -> bool {
    *value
}

fn default_authorization_revision() -> u64 {
    1
}

impl ResourceConfig {
    pub fn load(state_root: &Path) -> Result<Self> {
        let path = state_root.join("resources.yml");
        if !path.is_file() {
            return Ok(Self {
                version: 1,
                ..Self::default()
            });
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read resource config {}", path.display()))?;
        let config: Self = serde_yaml::from_str(&raw)
            .with_context(|| format!("failed to parse resource config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.version == 1, "resources.yml version must be 1");
        for profile in &self.profiles {
            if !profile.enabled {
                continue;
            }
            anyhow::ensure!(
                profile.authorization_revision > 0,
                "resource profile authorization_revision must be positive"
            );
            anyhow::ensure!(
                matches!(
                    (profile.harness.as_str(), profile.provider.as_str()),
                    ("codex", "openai") | ("claude", "anthropic")
                ),
                "resource profile provider/harness must be openai/codex or anthropic/claude"
            );
            validate_model(Some(&profile.model))?;
            validate_effort(profile.effort.as_deref())?;
            if profile.included {
                anyhow::ensure!(
                    profile.service_mode == "standard",
                    "included allocation profiles must use standard service mode"
                );
            }
            if profile.harness == "claude" {
                crate::harness::claude::validate_profile(profile)?;
            }
            for (name, value) in [
                ("funding_source", &profile.funding_source),
                ("service_mode", &profile.service_mode),
                ("runtime", &profile.runtime),
                ("pool", &profile.pool),
            ] {
                anyhow::ensure!(
                    !value.trim().is_empty(),
                    "resource profile {name} must not be empty"
                );
            }
            anyhow::ensure!(
                profile
                    .provider_buckets
                    .iter()
                    .all(|bucket| !bucket.trim().is_empty()),
                "provider bucket IDs must not be empty"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn an_invalid_hermes_provider_is_named_as_a_provider() {
        let mut config = Config::default();
        config.harnesses.hermes.provider = Some("bad id!".into());
        let error = format!("{:#}", config.validate().unwrap_err());
        assert_eq!(
            error,
            "invalid harnesses.hermes.provider: provider must be one non-option identifier"
        );
    }

    #[test]
    fn parses_partial_config_with_defaults() {
        let config: Config = serde_yaml::from_str(
            "execution:\n  timeout_secs: 12\nchecks:\n  verify: [cargo test]\n",
        )
        .unwrap();
        assert_eq!(config.execution.timeout_secs, 12);
        assert_eq!(config.execution.backend, "local");
        assert_eq!(config.checks.verify, vec!["cargo test"]);
    }

    #[test]
    fn coherence_defaults_and_yaml() {
        let config = CoherenceConfig::default();
        assert_eq!(config.accept, AcceptMode::Validate);
        assert_eq!(config.mid_run, MidRunMode::Observe);
        assert!(!config.stop_on_refresh);
        assert_eq!(config.poll_secs, 10);
        assert!(config.integration_checks);

        let config: Config = serde_yaml::from_str(
            "coherence:\n  accept: strict\n  mid_run: stop\n  stop_on_refresh: true\n  poll_secs: 3\n  integration_checks: false\n",
        )
        .unwrap();
        assert_eq!(config.coherence.accept, AcceptMode::Strict);
        assert_eq!(config.coherence.mid_run, MidRunMode::Stop);
        assert!(config.coherence.stop_on_refresh);
        assert_eq!(config.coherence.poll_secs, 3);
        assert!(!config.coherence.integration_checks);
        config.validate().unwrap();
        assert!(
            serde_yaml::to_string(&config)
                .unwrap()
                .contains("coherence:")
        );

        let partial: Config = serde_yaml::from_str("coherence:\n  mid_run: stop\n").unwrap();
        assert_eq!(partial.coherence.accept, AcceptMode::Validate);
        assert_eq!(partial.coherence.poll_secs, 10);

        let zero: Config = serde_yaml::from_str("coherence:\n  poll_secs: 0\n").unwrap();
        assert!(zero.validate().is_err());
    }

    #[test]
    fn absent_coherence_block_is_not_serialized() {
        let config: Config = serde_yaml::from_str("checks:\n  verify: [cargo test]\n").unwrap();
        assert!(config.coherence.is_default());
        assert!(
            !serde_yaml::to_string(&config)
                .unwrap()
                .contains("coherence")
        );
        assert!(
            !serde_json::to_string(&Config::default())
                .unwrap()
                .contains("coherence")
        );
    }

    #[test]
    fn parses_chatgpt_and_claude_shaped_resource_profiles() {
        let config: ResourceConfig = serde_yaml::from_str(
            "version: 1\nallocation_enabled: false\nprofiles:\n  - provider: openai\n    funding_source: chatgpt-plus\n    harness: codex\n    model: codex-light\n    effort: low\n    service_mode: standard\n    runtime: local\n    pool: chatgpt-codex\n    tier: light\n    included: true\n    no_overage_verified: true\n  - provider: anthropic\n    funding_source: claude-pro\n    harness: claude\n    model: claude-example\n    effort: null\n    service_mode: standard\n    runtime: local\n    pool: claude-code\n    tier: strong\n    included: true\n    no_overage_verified: false\n",
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(config.profiles.len(), 2);
        assert_eq!(config.profiles[1].provider, "anthropic");
    }
}
