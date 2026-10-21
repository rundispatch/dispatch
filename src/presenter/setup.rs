//! CLI and in-session setup use this same prompt/controller and shared service.
use super::*;
use crate::setup::{self, Proposal};

/// `verify`: exact check commands the person gave (`--verify`). Without a
/// terminal they are saved as given; on one they pre-fill the checks menu.
pub async fn standalone(
    state: &State,
    provider: Option<String>,
    project_checks: bool,
    verify: Vec<String>,
    mut options: Options,
) -> Result<()> {
    if project_checks && !suitable() {
        anyhow::ensure!(
            !verify.is_empty(),
            "choosing checks needs a terminal; without one, save exact commands with \
             dispatch setup --checks --verify '<command>'"
        );
        return save_given_checks(state, &std::env::current_dir()?, &verify);
    }
    anyhow::ensure!(
        suitable(),
        if provider.is_some() {
            "setup requires a terminal for explicit funding confirmation; use dispatch resources for noninteractive status"
        } else {
            "setup needs a terminal to choose project checks, allow them, and install hooks; use dispatch setup --checks --verify '<command>' to save exact checks without one"
        }
    );
    options.plain |= std::env::var("TERM").is_ok_and(|v| v == "dumb");
    let mut ui = Ui::new(options)?;
    if project_checks {
        project_checks_menu(&mut ui, state, &std::env::current_dir()?, &verify).await
    } else if provider.is_some() {
        accounts(&mut ui, state, provider).await
    } else {
        project_setup(&mut ui, state, &std::env::current_dir()?, options).await
    }
}

/// One explicit pass for a project: save checks, authorize those exact
/// checks, offer observation hooks, then start watching.
async fn project_setup(
    ui: &mut Ui,
    state: &State,
    source: &std::path::Path,
    options: Options,
) -> Result<()> {
    let mut carry_on =
        choose_project_checks(ui, source).await? && allow_project_checks(ui, state, source).await?;
    for runtime in [Runtime::Claude, Runtime::Hermes] {
        if carry_on && which::which(runtime.program()).is_ok() {
            carry_on = hook_step(ui, state, runtime, false).await?;
        }
    }
    if !carry_on {
        return ui.commit(SETUP_STOPPED);
    }
    let rows = [
        Choice::new("Done"),
        Choice::new("Agents Dispatch launches…"),
    ];
    // Accounts are optional and come last; Enter alone finishes setup.
    let agents = ui
        .select(
            "Project setup is ready. Agents Dispatch launches can be configured separately.",
            &rows,
            0,
        )
        .await?;
    if agents.is_none() {
        return ui.commit(SETUP_STOPPED);
    }
    if agents == Some(1) {
        accounts(ui, state, None).await?;
    }
    // `start` prints the watching lines. Close the screen first, so a live
    // viewport has restored the terminal before they are printed.
    drop(ui.screen.take());
    crate::orchestrator::background::start_after_setup(state, Some(source.into()), 0, options).await
}

/// What setup says when the person backs out of a step.
const SETUP_STOPPED: &str = "Setup stopped; later steps were left unchanged.";

/// A selection is toggled in place; Save is the only choice that writes.
/// Existing commands stay selected until changed, including commands that
/// are no longer detected from filenames.
async fn choose_project_checks(ui: &mut Ui, source: &std::path::Path) -> Result<bool> {
    let expected = setup::project_config_bytes(source)?;
    let existing = crate::consent::effective_checks(source)?;
    let detected = setup::check_choices(source);
    let mut commands: Vec<String> = detected.iter().map(ToString::to_string).collect();
    for command in &existing {
        if !commands.contains(command) {
            commands.push(command.clone());
        }
    }
    let mut selected: Vec<bool> = commands.iter().map(|c| existing.contains(c)).collect();
    loop {
        let mut rows = commands
            .iter()
            .zip(&selected)
            .map(|(command, selected)| {
                Choice::new(format!("[{}] {command}", if *selected { "x" } else { " " }))
            })
            .collect::<Vec<_>>();
        let other = rows.len();
        rows.push(Choice::new("Other command…"));
        rows.push(Choice::new("Save selected checks"));
        rows.push(Choice::new(if existing.is_empty() {
            "Continue without checks"
        } else {
            "Keep current checks"
        }));
        rows.push(Choice::new("Cancel setup"));
        let current = if existing.is_empty() {
            "Current checks: none".to_owned()
        } else {
            format!("Current checks:\n  {}", existing.join("\n  "))
        };
        let body = format!(
            "Project checks\n{current}\nSelect each command, then Save selected checks. Checks run project code with your permissions."
        );
        match ui.select(&body, &rows, other + 2).await? {
            Some(i) if i < commands.len() => selected[i] = !selected[i],
            Some(i) if i == other => {
                if let Input::Submit(typed) = ui
                    .command_prompt("Other check command (one line, run on this host)")
                    .await?
                {
                    let typed = typed.trim();
                    anyhow::ensure!(
                        !typed.is_empty() && !typed.contains(['\n', '\r']),
                        "a check is one non-empty command line"
                    );
                    if let Some(i) = commands.iter().position(|command| command == typed) {
                        selected[i] = true;
                    } else {
                        commands.push(typed.to_owned());
                        selected.push(true);
                    }
                }
            }
            Some(i) if i == other + 1 => {
                let chosen = commands
                    .iter()
                    .zip(&selected)
                    .filter_map(|(command, selected)| selected.then_some(command.clone()))
                    .collect::<Vec<_>>();
                if chosen
                    .iter()
                    .all(|command| detected.contains(&command.as_str()))
                {
                    let chosen = chosen.iter().map(String::as_str).collect::<Vec<_>>();
                    setup::save_checks(source, &chosen, expected)?;
                } else {
                    // A typed or existing command is approved by its exact text.
                    setup::save_typed_checks(source, &chosen, expected)?;
                }
                ui.commit(&format!(
                    "Checks saved to dispatch.yml: {}",
                    if chosen.is_empty() {
                        "none".to_owned()
                    } else {
                        chosen.join(", ")
                    }
                ))?;
                return Ok(true);
            }
            Some(i) if i == other + 2 => return Ok(true),
            _ => return Ok(false),
        }
    }
}

async fn allow_project_checks(
    ui: &mut Ui,
    state: &State,
    source: &std::path::Path,
) -> Result<bool> {
    use crate::consent::{self, CheckConsent};
    let root = consent::project_root(source)?;
    let commands = consent::effective_checks(&root)?;
    if commands.is_empty() {
        ui.commit("No checks are configured; check consent was not granted.")?;
        return Ok(true);
    }
    let standing = match consent::consent(state, &root)? {
        CheckConsent::Valid(_) => {
            ui.commit("Check consent already holds for these exact commands.")?;
            return Ok(true);
        }
        CheckConsent::NotGranted => "No standing check consent has been given.",
        CheckConsent::Changed { .. } => "The checks changed; the earlier consent no longer holds.",
        CheckConsent::OutdatedScope => {
            "The earlier consent no longer holds: Dispatch now also runs these checks when you attach or finish Work yourself, and when a session ends."
        }
        CheckConsent::UnsupportedVersion(_) => {
            "This consent record uses a version Dispatch cannot read; please allow these checks again."
        }
    };
    let body = format!(
        "Allow checks for this project?\nExact commands:\n  {}\n{standing}\n{}",
        commands.join("\n  "),
        consent::SCOPE
    );
    let rows = [Choice::new("Allow"), Choice::new("Cancel")];
    if ui.select(&body, &rows, 1).await? != Some(0) {
        return Ok(false);
    }
    consent::grant_exact(state, &root, &commands)?;
    ui.commit("Check consent saved in Dispatch's state for these exact commands.")?;
    Ok(true)
}

/// `dispatch setup --checks --verify …` without a terminal: save exactly
/// these commands. Consent is never granted here; one given for other
/// commands no longer holds.
fn save_given_checks(state: &State, source: &std::path::Path, verify: &[String]) -> Result<()> {
    use crate::consent;
    let path = setup::save_typed_checks(source, verify, setup::project_config_bytes(source)?)?;
    let listed: Vec<String> = verify.iter().map(|c| format!("  {}", c.trim())).collect();
    let standing = match consent::consent(state, &consent::project_root(source)?)? {
        consent::CheckConsent::Valid(_) => {
            "Work here already runs exactly these checks by themselves."
        }
        consent::CheckConsent::OutdatedScope => {
            "Earlier consent no longer holds: it did not cover typed attach, finish, or session-end checks. Reapprove with dispatch setup --checks in a terminal. A single finish can run them with --allow-unsafe-local."
        }
        consent::CheckConsent::UnsupportedVersion(_) => {
            "The check consent version is unsupported. Reapprove with dispatch setup --checks in a terminal. A single finish can run them with --allow-unsafe-local."
        }
        _ => {
            "They do not run by themselves: allowing that needs dispatch setup --checks in a \
             terminal. A single finish can run them with --allow-unsafe-local."
        }
    };
    println!(
        "{}",
        sanitize(&format!(
            "✓ Checks saved to {}:\n{}\n{standing}",
            path.display(),
            listed.join("\n")
        ))
    );
    Ok(())
}

/// `dispatch start` on a terminal, in a project without checks: offer the
/// detected ones, listed exactly. `Some(saved)` goes on to watch; `None`
/// means the person backed out and nothing starts. Granting consent stays
/// `dispatch setup --checks`.
pub async fn start_checks(source: &std::path::Path, mut options: Options) -> Result<Option<bool>> {
    let commands = setup::check_choices(source);
    if commands.is_empty() {
        return Ok(Some(false));
    }
    let expected = setup::project_config_bytes(source)?;
    options.plain |= std::env::var("TERM").is_ok_and(|v| v == "dumb");
    let mut ui = Ui::new(options)?;
    ui.auto_apply_mode = false;
    let listed = commands
        .iter()
        .map(|command| format!("  {command}"))
        .collect::<Vec<_>>()
        .join("\n");
    let body = format!(
        "This project has no checks. Dispatch found:\n{listed}\nSaving them writes \
         checks.verify in dispatch.yml; it does not let Work run them by themselves."
    );
    let rows = [
        Choice::new("Save these checks"),
        Choice::new("Continue without checks"),
    ];
    // Focus starts on Continue: `start` watches, and Enter alone never
    // writes to the project.
    match ui.select(&body, &rows, 1).await? {
        Some(0) => {
            setup::save_checks(source, &commands, expected)?;
            ui.commit(&format!("Checks saved to dispatch.yml:\n{listed}"))?;
            Ok(Some(true))
        }
        Some(_) => Ok(Some(false)),
        None => Ok(None),
    }
}

pub(super) async fn accounts(
    ui: &mut Ui,
    state: &State,
    mut selected: Option<String>,
) -> Result<()> {
    const PROVIDERS: [(&str, &str); 2] = [("claude", "Claude Code"), ("codex", "Codex")];
    loop {
        if let Some(provider) = selected.take() {
            if let Err(e) = configure(ui, state, &provider, None).await {
                report(ui, &e)?;
            }
            continue;
        }
        let resources = crate::config::ResourceConfig::load(&state.root)?;
        let installed = |provider: &str| which::which(provider).is_ok();
        let mut choices = PROVIDERS
            .iter()
            .map(|(provider, name)| {
                if installed(provider) {
                    Choice::new(format!("Add {name}"))
                } else {
                    Choice::disabled(format!("Add {name}"), "not found on PATH")
                }
            })
            .collect::<Vec<_>>();
        choices.extend(resources.profiles.iter().map(profile_choice));
        let login_row = choices.len();
        choices.push(Choice::new("Provider login…"));
        choices.push(Choice::new("Runtime integrations…"));
        choices.push(Choice::new("Back"));
        let body = "Accounts / Resources\nLocal operation · no Dispatch login required\nA resource is needed only for Dispatch to launch an agent; attach, check, serve and review work without one.";
        let Some(choice) = ui.select(body, &choices, 0).await? else {
            return Ok(());
        };
        let result = if choice < PROVIDERS.len() {
            configure(ui, state, PROVIDERS[choice].0, None).await
        } else if choice < login_row {
            let index = choice - PROVIDERS.len();
            let provider = resources.profiles[index].harness.clone();
            configure(ui, state, &provider, Some(index)).await
        } else if choice == login_row {
            let providers = PROVIDERS
                .iter()
                .filter(|(provider, _)| installed(provider))
                .collect::<Vec<_>>();
            let mut rows = providers
                .iter()
                .map(|(_, name)| Choice::new(*name))
                .collect::<Vec<_>>();
            rows.push(Choice::new("Back"));
            match ui
                .select(
                    "Provider login changes the provider's saved account.",
                    &rows,
                    0,
                )
                .await?
            {
                Some(i) if i < providers.len() => login(ui, providers[i].0).await,
                _ => Ok(()),
            }
        } else if choice == login_row + 1 {
            // Hooks are not a resource, so no provider advice follows.
            if let Err(e) = integrations(ui, state).await {
                ui.commit(&format!(
                    "Nothing changed: {}",
                    clip(&format!("{e:#}"), 500)
                ))?;
            }
            Ok(())
        } else {
            return Ok(());
        };
        if let Err(e) = result {
            report(ui, &e)?;
        }
    }
}

/// Claude Code's session hooks and Hermes's session-end hook, which let Work
/// appear by itself in watched projects and keep a worktree's changes before
/// its runtime removes it. They are observation hooks, not a resource: no
/// account, model or funding is involved.
async fn integrations(ui: &mut Ui, state: &State) -> Result<()> {
    let found = |runtime: Runtime| -> Result<(std::path::PathBuf, Result<Change>)> {
        let path = runtime.path()?;
        let command = runtime.command(state)?;
        let change = runtime
            .found(&path)
            .map(|found| Change::of(&found, &command));
        Ok((path, change))
    };
    let (path, claude) = found(Runtime::Claude)?;
    let claude = claude?;
    let (hermes_path, hermes) = found(Runtime::Hermes)?;
    let installed = |change: &Change| match change {
        Change::Install => "not installed",
        _ => "installed",
    };
    let body = format!(
        "Runtime integrations\nClaude Code hooks: {} ({})\nHermes hook: {} ({})\nWith them, a Claude Code session in its own worktree of a project you watch \
         (dispatch start) appears as Work by itself; a session in the checkout itself is told Dispatch cannot follow it. \
         Hermes's hook keeps a hermes --worktree session's exact changes before Hermes removes its worktree.",
        installed(&claude),
        path.display(),
        match &hermes {
            Ok(change) => installed(change),
            Err(_) => "unknown, its config.yaml is unreadable",
        },
        hermes_path.display()
    );
    let rows = [
        Choice::new(format!("{} Claude Code hooks", claude.verb())),
        Choice::new(format!(
            "{} the Hermes hook",
            hermes.unwrap_or(Change::Install).verb()
        )),
        Choice::new("Back"),
    ];
    let runtime = match ui.select(&body, &rows, 0).await? {
        Some(0) => Runtime::Claude,
        Some(1) => Runtime::Hermes,
        _ => return Ok(()),
    };
    hook_step(ui, state, runtime, true).await.map(drop)
}

/// A runtime whose own configuration takes Dispatch's hook.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Runtime {
    Claude,
    Hermes,
}

impl Runtime {
    fn program(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Hermes => "hermes",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code hooks",
            Self::Hermes => "Hermes hook",
        }
    }

    fn path(self) -> Result<std::path::PathBuf> {
        match self {
            Self::Claude => crate::runtime::claude::settings_path(),
            Self::Hermes => crate::runtime::hermes::config_path(),
        }
    }

    fn command(self, state: &State) -> Result<String> {
        match self {
            Self::Claude => crate::runtime::claude::hook_command(state),
            Self::Hermes => crate::runtime::hermes::hook_command(state),
        }
    }

    fn found(self, path: &std::path::Path) -> Result<crate::runtime::Hook> {
        match self {
            Self::Claude => crate::runtime::claude::hook(path),
            Self::Hermes => crate::runtime::hermes::hook(path),
        }
    }
}

/// What a hook step changes in a runtime's configuration.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Change {
    Install,
    Update,
    Remove,
}

impl Change {
    /// Install when there is no hook of Dispatch's, Update when Dispatch's
    /// runs another command or is an earlier Dispatch's, and Remove otherwise.
    fn of(found: &crate::runtime::Hook, command: &str) -> Self {
        use crate::runtime::Hook;
        match found {
            Hook::Absent => Self::Install,
            Hook::Exact(other) if other != command => Self::Update,
            Hook::Earlier(_) => Self::Update,
            _ => Self::Remove,
        }
    }

    fn verb(self) -> &'static str {
        match self {
            Self::Install => "Install",
            Self::Update => "Update",
            Self::Remove => "Remove",
        }
    }
}

/// Hermes's consent stays Hermes's: Dispatch never writes its allowlist.
const HERMES_CONSENT: &str = "Hermes asks you to allow the hook the first time it runs it; \
     a run without a terminal needs hermes --accept-hooks.";

/// One runtime's hook, the step both setup's pass and Runtime integrations
/// take. It shows exactly what changes and where, with the existing and new
/// command on an Update, and needs a deliberate choice: focus starts on Skip,
/// or on Cancel in the menu. Only the menu (`menu`) removes; setup leaves a
/// hook that already runs this Dispatch, or that was changed by hand, as it
/// is. Returns false when the person backed out.
async fn hook_step(ui: &mut Ui, state: &State, runtime: Runtime, menu: bool) -> Result<bool> {
    use crate::runtime::{Hook, claude, hermes};
    let name = runtime.name();
    let path = runtime.path()?;
    let command = runtime.command(state)?;
    let found = runtime.found(&path)?;
    let change = Change::of(&found, &command);
    let shown = path.display();
    if change == Change::Remove && !menu {
        ui.commit(&if found == Hook::Changed {
            format!("{name}: changed by hand in {shown}; setup leaves that file as it is.")
        } else {
            format!("{name}: already running this Dispatch ({shown}).")
        })?;
        return Ok(true);
    }
    // An earlier Dispatch's hooks may already run this one, upgraded in place.
    let commands = match &found {
        Hook::Exact(previous) | Hook::Earlier(previous) if *previous != command => {
            format!("Existing hook command: {previous}\nNew hook command: {command}")
        }
        _ => format!("Hook command, unchanged: {command}"),
    };
    let what = match (runtime, change) {
        (Runtime::Claude, Change::Install) => format!(
            "Add these hooks to {shown}?\n{}\nOther settings and hooks are kept (key order may change); a backup is kept beside the file.",
            claude::hooks_preview(&command)
        ),
        (Runtime::Claude, Change::Update) => format!(
            "Update Dispatch's hooks in {shown}?\n{commands}\nDispatch's hooks there are replaced by exactly these:\n{}\nOther settings and hooks are kept; a backup is kept beside the file.",
            claude::hooks_preview(&command)
        ),
        (Runtime::Claude, Change::Remove) => format!(
            "Remove Dispatch's hooks from {shown}?\nOnly Dispatch's own hooks are removed, hook by hook; a backup is kept beside the file."
        ),
        (Runtime::Hermes, Change::Install) => match hermes::addition(&path, &command)? {
            hermes::Addition::Append(block) => format!(
                "Add this to the end of {shown}?\n{}\nEvery other byte of the file is kept; a backup is kept beside the file. {HERMES_CONSENT}",
                block.trim_start_matches('\n')
            ),
            hermes::Addition::ByHand(lines) => {
                ui.commit(&format!(
                    "{shown} has a hooks: key of its own, so Dispatch leaves the file alone. Add these lines under it by hand:\n{lines}{HERMES_CONSENT}"
                ))?;
                return Ok(true);
            }
        },
        (Runtime::Hermes, Change::Update) => format!(
            "Update Dispatch's hook in {shown}?\n{commands}\nOnly the command in Dispatch's block changes; a backup is kept beside the file. {HERMES_CONSENT}"
        ),
        (Runtime::Hermes, Change::Remove) => format!(
            "Remove Dispatch's hook from {shown}?\nOnly the block Dispatch added is removed, and only as Dispatch added it; a backup is kept beside the file."
        ),
    };
    let rows = [
        Choice::new(change.verb()),
        Choice::new(if menu { "Cancel" } else { "Skip" }),
    ];
    let body = format!("{name} · {}\n{what}", change.verb());
    match ui.select(&body, &rows, 1).await? {
        Some(0) => {}
        choice => return Ok(choice.is_some()),
    }
    let backup = match (runtime, change) {
        (Runtime::Claude, Change::Remove) => claude::uninstall_hooks(&path)?,
        (Runtime::Claude, _) => claude::install_hooks(&path, &command)?,
        (Runtime::Hermes, Change::Install) => hermes::install(&path, &command)?,
        (Runtime::Hermes, Change::Update) => hermes::update(&path, &command)?,
        (Runtime::Hermes, Change::Remove) => hermes::uninstall(&path)?,
    };
    let kept = backup.map_or_else(String::new, |backup| {
        format!(" Previous configuration: {}.", backup.display())
    });
    ui.commit(&format!(
        "{name} {} in {shown}.{kept}{}",
        match change {
            Change::Install => "installed",
            Change::Update => "updated",
            Change::Remove => "removed",
        },
        if (runtime, change) == (Runtime::Hermes, Change::Install) {
            format!(" {HERMES_CONSENT}")
        } else {
            String::new()
        }
    ))?;
    Ok(true)
}

/// A profile as a menu row: selecting it revalidates. A disabled profile
/// stays disabled until it is enabled in `resources.yml`.
fn profile_choice(profile: &crate::config::ResourceProfile) -> Choice {
    let name = format!(
        "Revalidate {} · {} · {}",
        profile.harness,
        profile.model,
        profile.effort.as_deref().unwrap_or("default effort")
    );
    if !profile.enabled {
        return Choice::disabled(name, "disabled in resources.yml");
    }
    let status = match profile.eligibility() {
        Ok(()) => match &profile.claude_subscription {
            Some(evidence) => format!("ready, expires in {}", remaining(evidence.valid_until)),
            None => "ready".to_owned(),
        },
        Err(e) => format!("needs revalidation: {e}"),
    };
    Choice::new(format!("{name} · {status}"))
}

fn remaining(until: chrono::DateTime<chrono::Utc>) -> String {
    let left = until - chrono::Utc::now();
    if left.num_hours() >= 1 {
        format!("{} h", left.num_hours())
    } else {
        format!("{} min", left.num_minutes().max(1))
    }
}

fn report(ui: &mut Ui, error: &anyhow::Error) -> Result<()> {
    ui.commit(&format!(
        "Resource unchanged: {}\nChoose Provider login for missing authentication, or revalidate after correcting provider settings.",
        clip(&format!("{error:#}"), 500)
    ))
}

async fn configure(ui: &mut Ui, state: &State, provider: &str, index: Option<usize>) -> Result<()> {
    let executable = setup::executable(provider)?;
    let discovery = ui.discover(provider, executable).await?;
    let Some(discovery) = discovery else {
        return Ok(());
    };
    ui.commit(&discovery.summary())?;
    // Revalidation keeps the profile's model and effort; only a new resource
    // chooses them.
    let (model, effort) = if index.is_some() {
        (String::new(), String::new())
    } else {
        let resources = crate::config::ResourceConfig::load(&state.root)?;
        let Some((model, efforts, default_effort)) =
            choose_model(ui, provider, &resources, &discovery.models).await?
        else {
            return Ok(());
        };
        let rows = efforts.iter().map(Choice::new).collect::<Vec<_>>();
        let initial = default_effort
            .and_then(|d| efforts.iter().position(|e| *e == d))
            .or_else(|| efforts.iter().position(|e| e == "medium"))
            .unwrap_or(0);
        let Some(effort) = ui
            .select(&format!("Effort for {model}"), &rows, initial)
            .await?
        else {
            return Ok(());
        };
        (model, efforts[effort].clone())
    };
    let proposal = Proposal::prepare(state, discovery, provider, index, model, effort)?;
    // The full assertion goes to scrollback so a short terminal never clips
    // the choice. Focus starts on Cancel: Enter alone never authorizes.
    ui.commit(&proposal.summary())?;
    let consent = [Choice::new("Authorize and save"), Choice::new("Cancel")];
    if ui.select("Authorize this resource?", &consent, 1).await? == Some(0) {
        proposal.confirm(state)?;
        ui.commit(
            "Resource saved. The current account and funding are checked again before launch.",
        )?;
    } else {
        ui.commit("Not authorized. Configuration unchanged.")?;
    }
    Ok(())
}

/// Choose a model: models already configured for this provider first, then
/// what the provider listed (or the adapter suggests), then typing another
/// ID. Returns the model, the efforts to offer and the one to focus.
async fn choose_model(
    ui: &mut Ui,
    provider: &str,
    resources: &crate::config::ResourceConfig,
    listed: &[crate::harness::ModelOption],
) -> Result<Option<(String, Vec<String>, Option<String>)>> {
    let fallback: Vec<String> = if provider == "claude" {
        crate::harness::claude::EFFORTS
            .iter()
            .map(|e| (*e).to_owned())
            .collect()
    } else {
        ["minimal", "low", "medium", "high", "xhigh"]
            .map(str::to_owned)
            .to_vec()
    };
    let mut options: Vec<crate::harness::ModelOption> = Vec::new();
    for profile in resources.profiles.iter().filter(|p| p.harness == provider) {
        if !options.iter().any(|o| o.id == profile.model) {
            options.push(
                listed
                    .iter()
                    .find(|o| o.id == profile.model)
                    .cloned()
                    .unwrap_or(crate::harness::ModelOption {
                        id: profile.model.clone(),
                        efforts: fallback.clone(),
                        default_effort: profile.effort.clone(),
                    }),
            );
        }
    }
    for option in listed {
        if !options.iter().any(|o| o.id == option.id) {
            options.push(option.clone());
        }
    }
    let mut rows = options
        .iter()
        .map(|o| Choice::new(&o.id))
        .collect::<Vec<_>>();
    rows.push(Choice::new("Other model ID…"));
    let body = "Model\nChoose a model included in your subscription; a listed model is not evidence of inclusion.";
    let Some(choice) = ui.select(body, &rows, 0).await? else {
        return Ok(None);
    };
    if let Some(option) = options.get(choice) {
        let efforts = if option.efforts.is_empty() {
            fallback
        } else {
            option.efforts.clone()
        };
        return Ok(Some((
            option.id.clone(),
            efforts,
            option.default_effort.clone(),
        )));
    }
    let Input::Submit(typed) = ui
        .command_prompt("Model ID (exact, as your provider names it)")
        .await?
    else {
        return Ok(None);
    };
    let model = typed.trim().to_owned();
    crate::config::validate_model(Some(&model))?;
    Ok(Some((model, fallback, None)))
}

impl Ui {
    async fn discover(
        &mut self,
        provider: &str,
        path: std::path::PathBuf,
    ) -> Result<Option<setup::Discovery>> {
        let future = setup::discover(provider, path);
        tokio::pin!(future);
        self.commit("Checking installed CLI and supported authentication · no model call")?;
        loop {
            self.draw(
                "Accounts / Resources\nChecking authentication…",
                None,
                "Ctrl+C cancel · no model call",
            )?;
            tokio::select! {
                result=&mut future=>return result.map(Some),
                event=self.next()=>match event? {
                    None=>{self.closed=true;return Ok(None)},
                    Some(Event::Key(k)) if k.code==KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL)=>return Ok(None),
                    _=>{}
                }
            }
        }
    }
}

async fn login(ui: &mut Ui, provider: &str) -> Result<()> {
    let executable = setup::executable(provider)?;
    let body = format!(
        "Open {provider}'s supported login? This may open a browser and change its saved account.\nNo funding is authorized by login."
    );
    let rows = [
        Choice::new(format!("Open {provider} login")),
        Choice::new("Cancel"),
    ];
    if ui.select(&body, &rows, 0).await? != Some(0) {
        return Ok(());
    }
    let temp = tempfile::tempdir()?;
    let mut command = std::process::Command::new(executable);
    command.current_dir(temp.path()).env_clear();
    for key in ["HOME", "USER", "PATH", "TERM", "TMPDIR"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    if provider == "codex" {
        command.args(["login", "--device-auth"]);
    } else {
        command.args(["auth", "login"]);
    }
    let status = handoff::run_named(
        ui,
        crate::reviewer::ReviewLaunch {
            command,
            wait: crate::reviewer::WaitMode::Process,
            terminal: true,
            warning: None,
        },
        "Provider login · return here to explicitly validate funding.",
    )
    .await?;
    anyhow::ensure!(status.success(), "provider login did not complete");
    ui.commit("Login returned. Choose a resource to revalidate its funding.")
}

/// `dispatch setup --checks`. With checks chosen, also where the person lets
/// Work in this project run them by themselves, or stops it: consent for
/// exactly these commands, kept in Dispatch's state, never in the project.
/// Commands given with `--verify` are offered first, exactly as given.
async fn project_checks_menu(
    ui: &mut Ui,
    state: &State,
    source: &std::path::Path,
    given: &[String],
) -> Result<()> {
    use crate::consent;
    if !given.is_empty() {
        let expected = setup::project_config_bytes(source)?;
        let listed: Vec<String> = given.iter().map(|c| format!("  {}", c.trim())).collect();
        let body = format!("Save these checks?\n{}", listed.join("\n"));
        let rows = [Choice::new("Save these checks"), Choice::new("Cancel")];
        if ui.select(&body, &rows, 0).await? != Some(0) {
            return ui.commit("Nothing changed.");
        }
        setup::save_typed_checks(source, given, expected)?;
        ui.commit(&format!("Approved checks saved:\n{}", listed.join("\n")))?;
    }
    let root = consent::project_root(source)?;
    let commands = consent::effective_checks(&root)?;
    if commands.is_empty() {
        return checks(ui, source).await;
    }
    let current = consent::consent(state, &root)?;
    let listed = commands
        .iter()
        .map(|command| format!("  {command}"))
        .collect::<Vec<_>>()
        .join("\n");
    let standing = match &current {
        consent::CheckConsent::Valid(_) => "Work here runs them by themselves.",
        consent::CheckConsent::Changed { .. } => {
            "Work here asks first: you allowed other commands, and the checks changed since."
        }
        consent::CheckConsent::OutdatedScope => {
            "Work here asks first: the earlier consent did not cover typed attach, finish, or session-end checks."
        }
        consent::CheckConsent::UnsupportedVersion(_) => {
            "Work here asks first: this consent record uses an unsupported version."
        }
        consent::CheckConsent::NotGranted => "Work here asks you before running them.",
    };
    let body = format!("Project checks\n{listed}\n{standing}");
    let toggle = if current.is_valid() {
        "Stop letting Work run these checks by themselves"
    } else {
        "Let Work run these checks by themselves…"
    };
    let rows = [
        Choice::new("Change checks…"),
        Choice::new(toggle),
        Choice::new("Back"),
    ];
    match ui.select(&body, &rows, 0).await? {
        Some(0) => checks(ui, source).await,
        Some(1) if current.is_valid() => {
            consent::revoke(state, &root)?;
            ui.commit("Work in this project now asks you before running its checks.")
        }
        Some(1) => {
            let body = format!(
                "Let Work in this project run these checks by themselves?\n{listed}\n{}",
                consent::SCOPE
            );
            let rows = [Choice::new("Allow"), Choice::new("Cancel")];
            if ui.select(&body, &rows, 1).await? != Some(0) {
                return ui.commit("Nothing changed.");
            }
            consent::grant_exact(state, &root, &commands)?;
            ui.commit("Work in this project will run these checks by themselves.")
        }
        _ => Ok(()),
    }
}

pub(super) async fn checks(ui: &mut Ui, source: &std::path::Path) -> Result<()> {
    let expected = setup::project_config_bytes(source)?;
    let commands = setup::check_choices(source);
    let mut choices = commands.iter().map(|c| Choice::new(*c)).collect::<Vec<_>>();
    choices.push(Choice::new("Other command…"));
    choices.push(Choice::new("Continue without checks"));
    choices.push(Choice::new("Back"));
    let body = "Choose checks\nA check runs project code with your permissions. Choosing a command approves it for this project; it does not prove task-specific correctness.";
    match ui.select(body, &choices, 0).await? {
        Some(i) if i < commands.len() => {
            setup::save_checks(source, &[commands[i]], expected)?;
            ui.commit(&format!("Approved check saved: {}", commands[i]))
        }
        Some(i) if i == commands.len() => {
            let Input::Submit(typed) = ui
                .command_prompt("Check command (runs in the project with your permissions)")
                .await?
            else {
                anyhow::bail!("check selection cancelled")
            };
            setup::save_typed_checks(source, std::slice::from_ref(&typed), expected)?;
            ui.commit(&format!("Approved check saved: {}", typed.trim()))
        }
        Some(i) if i == commands.len() + 1 => Ok(()),
        _ => anyhow::bail!("check selection cancelled"),
    }
}
