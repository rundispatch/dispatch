//! CLI and in-session setup use this same prompt/controller and shared service.
use super::*;
use crate::setup::{self, Proposal};

pub async fn standalone(
    state: &State,
    provider: Option<String>,
    project_checks: bool,
    mut options: Options,
) -> Result<()> {
    anyhow::ensure!(
        suitable(),
        "setup requires a terminal for explicit funding confirmation; use dispatch resources for noninteractive status"
    );
    options.plain |= std::env::var("TERM").is_ok_and(|v| v == "dumb");
    let mut ui = Ui::new(options)?;
    if project_checks {
        project_checks_menu(&mut ui, state, &std::env::current_dir()?).await
    } else {
        accounts(&mut ui, state, provider).await
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
            integrations(ui, state).await
        } else {
            return Ok(());
        };
        if let Err(e) = result {
            report(ui, &e)?;
        }
    }
}

/// Claude Code's session hooks, which let Work appear by itself in watched
/// projects. They are observation hooks, not a resource: no account, model
/// or funding is involved. Installing shows exactly what is added and where,
/// and needs a deliberate choice; focus starts on Cancel.
async fn integrations(ui: &mut Ui, state: &State) -> Result<()> {
    use crate::runtime::claude;
    let path = claude::settings_path()?;
    let installed = claude::hooks_installed(&path)?;
    let action = if installed {
        "Remove Claude Code hooks"
    } else {
        "Install Claude Code hooks"
    };
    let body = format!(
        "Runtime integrations\nClaude Code hooks: {} ({})\nWith them, a Claude Code session in its own worktree of a project you watch \
         (dispatch start) appears as Work by itself; a session in the checkout itself is told Dispatch cannot follow it.",
        if installed {
            "installed"
        } else {
            "not installed"
        },
        path.display()
    );
    let rows = [Choice::new(action), Choice::new("Back")];
    if ui.select(&body, &rows, 0).await? != Some(0) {
        return Ok(());
    }
    let command = claude::hook_command(state)?;
    let consent = if installed {
        format!(
            "Remove Dispatch's hooks from {}?\nOnly the entries running `... hook claude` are removed; a backup is kept beside the file.",
            path.display()
        )
    } else {
        format!(
            "Add these hooks to {}?\n{}\nOther settings and hooks are kept (key order may change); a backup is kept beside the file.",
            path.display(),
            claude::hooks_preview(&command)
        )
    };
    let confirm = if installed { "Remove" } else { "Install" };
    let rows = [Choice::new(confirm), Choice::new("Cancel")];
    if ui.select(&consent, &rows, 1).await? != Some(0) {
        return Ok(());
    }
    let backup = if installed {
        claude::uninstall_hooks(&path)?
    } else {
        claude::install_hooks(&path, &command)?
    };
    let kept = backup.map_or_else(String::new, |backup| {
        format!(" Previous settings: {}.", backup.display())
    });
    ui.commit(&format!(
        "Claude Code hooks {}.{kept}",
        if installed { "removed" } else { "installed" }
    ))
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
async fn project_checks_menu(ui: &mut Ui, state: &State, source: &std::path::Path) -> Result<()> {
    use crate::consent;
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
                "Let Work in this project run these checks by themselves?\n{listed}\n\
                 They run with your permissions when a runtime registers Work here and when its \
                 worktree is removed. Nothing is applied by this. If the checks change, Dispatch \
                 asks again."
            );
            let rows = [Choice::new("Allow"), Choice::new("Cancel")];
            if ui.select(&body, &rows, 1).await? != Some(0) {
                return ui.commit("Nothing changed.");
            }
            consent::grant(state, &root)?;
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
            setup::save_checks(source, commands[i], expected)?;
            ui.commit(&format!("Approved check saved: {}", commands[i]))
        }
        Some(i) if i == commands.len() => {
            let Input::Submit(typed) = ui
                .command_prompt("Check command (runs in the project with your permissions)")
                .await?
            else {
                anyhow::bail!("check selection cancelled")
            };
            setup::save_typed_check(source, &typed, expected)?;
            ui.commit(&format!("Approved check saved: {}", typed.trim()))
        }
        Some(i) if i == commands.len() + 1 => Ok(()),
        _ => anyhow::bail!("check selection cancelled"),
    }
}
