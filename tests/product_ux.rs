#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use assert_cmd::cargo_bin_cmd;
use predicates::prelude::PredicateBooleanExt;

fn repository(path: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(path.join("src"))?;
    fs::write(
        path.join("src/lib.rs"),
        "pub fn greeting() -> &'static str { \"helo\" }\n",
    )?;
    Ok(())
}

fn agent(path: &Path, id: &str, marker: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(
        path,
        format!(
            "#!/bin/sh\n\
             if [ \"$1\" = \"--version\" ]; then printf '{id} fixture 1.0\\n'; exit 0; fi\n\
             printf '{id}\\n' >> '{}'\n\
             printf '{id} changed this\\n' > agent-work.txt\n\
             printf '{{\"type\":\"result\",\"model\":\"fixture-{id}\"}}\\n'\n",
            marker.display()
        ),
    )?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[test]
fn no_configured_agent_is_refused_and_explicit_agent_runs() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("repo");
    let state = temp.path().join("state");
    let bin = temp.path().join("bin");
    let marker = temp.path().join("agents-ran");
    repository(&source)?;
    agent(&bin.join("cursor-agent"), "cursor", &marker)?;

    cargo_bin_cmd!("dispatch")
        .current_dir(&source)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .args(["--state-dir"])
        .arg(&state)
        .args(["run", "Fix the greeting typo"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "run `dispatch setup`, or pass --agent",
        ))
        .stderr(predicates::str::contains(
            "To protect work you run yourself, use `dispatch attach` (no setup needed)",
        ));
    assert!(
        !marker.exists(),
        "no agent may run without an explicit choice"
    );

    let second_state = temp.path().join("override-state");
    agent(&bin.join("codex"), "codex", &marker)?;
    let override_output = cargo_bin_cmd!("dispatch")
        .current_dir(&source)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .args(["--state-dir"])
        .arg(&second_state)
        .args(["run", "Fix the greeting typo", "--agent", "cursor"])
        .write_stdin("y\n")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let override_output = String::from_utf8(override_output)?;
    assert!(override_output.contains("Chosen with --agent (no profile)"));
    assert!(fs::read_to_string(&marker)?.ends_with("cursor\n"));
    Ok(())
}

/// Allowing a project needs a terminal: a piped `2` grants nothing and runs
/// nothing, and a piped `y` allows that one run and no later one.
#[test]
fn a_piped_answer_allows_one_run_and_never_the_project() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("repo");
    let state = temp.path().join("state");
    let bin = temp.path().join("bin");
    let marker = temp.path().join("agents-ran");
    repository(&source)?;
    agent(&bin.join("cursor-agent"), "cursor", &marker)?;
    let run = |answer: &str| {
        cargo_bin_cmd!("dispatch")
            .current_dir(&source)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .args(["--state-dir"])
            .arg(&state)
            .args(["run", "Fix the greeting typo", "--agent", "cursor"])
            .write_stdin(answer)
            .assert()
    };
    let granted = || {
        fs::read_dir(state.join("projects")).map_or(0, |entries| {
            entries
                .filter(|entry| {
                    entry.as_ref().is_ok_and(|entry| {
                        entry.file_name().to_string_lossy().starts_with("native-")
                    })
                })
                .count()
        })
    };

    run("2\n").failure().stderr(predicates::str::contains(
        "needs a terminal, so nothing was granted",
    ));
    assert!(!marker.exists(), "a refused launch ran the agent");
    assert_eq!(granted(), 0);

    run("y\n").success();
    assert_eq!(fs::read_to_string(&marker)?, "cursor\n");
    assert_eq!(granted(), 0);
    run("")
        .failure()
        .stderr(predicates::str::contains("--allow-unsafe-local"));
    assert_eq!(fs::read_to_string(&marker)?, "cursor\n");
    Ok(())
}

#[test]
fn latest_project_diff_accept_reject_and_explain_need_no_run_id() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("repo");
    let state = temp.path().join("state");
    let bin = temp.path().join("bin");
    let marker = temp.path().join("agents-ran");
    repository(&source)?;
    agent(&bin.join("codex"), "codex", &marker)?;
    let base = || {
        let mut command = cargo_bin_cmd!("dispatch");
        command
            .current_dir(&source)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .args(["--state-dir"])
            .arg(&state);
        command
    };

    base()
        .args(["run", "Fix the greeting typo", "--agent", "codex"])
        .write_stdin("y\n")
        .assert()
        .success();
    base()
        .arg("diff")
        .assert()
        .success()
        .stdout(predicates::str::contains("agent-work.txt"));
    let run_dir = fs::read_dir(state.join("runs"))?.next().unwrap()?.path();
    let id = run_dir.file_name().unwrap().to_str().unwrap().to_owned();
    // Without an ID, status says which Work it describes, and every command
    // it suggests names that Work.
    base()
        .arg("status")
        .assert()
        .success()
        .stdout(predicates::str::contains("Fix the greeting typo"))
        .stdout(predicates::str::contains(
            "Chosen with --agent (no profile)",
        ))
        .stdout(predicates::str::contains(format!(
            "Work {id}\n  native codex · began against directory snapshot"
        )))
        .stdout(predicates::str::contains(format!(
            "dispatch diff {id}\n\nThen\n  dispatch accept {id}\n  dispatch reject {id}"
        )))
        .stdout(predicates::str::contains("tier").not());
    // Reading a run never rewrites its projection when nothing changed.
    let metadata = run_dir.join("metadata.json");
    let before = fs::metadata(&metadata)?.modified()?;
    #[cfg(unix)]
    let inode = std::os::unix::fs::MetadataExt::ino(&fs::metadata(&metadata)?);
    for _ in 0..2 {
        base().arg("status").assert().success();
    }
    assert_eq!(fs::metadata(&metadata)?.modified()?, before);
    #[cfg(unix)]
    assert_eq!(
        std::os::unix::fs::MetadataExt::ino(&fs::metadata(&metadata)?),
        inode
    );
    base()
        .arg("history")
        .assert()
        .success()
        .stdout(predicates::str::contains("WORK"))
        .stdout(predicates::str::contains("native codex"))
        .stdout(predicates::str::contains("CANDIDATES").not())
        .stdout(predicates::str::contains("ready_for_evaluation").not());
    base()
        .arg("explain")
        .assert()
        .success()
        .stdout(predicates::str::contains("Chosen explicitly with --agent"));
    base()
        .arg("accept")
        .assert()
        .success()
        .stdout(predicates::str::contains("Applied Candidate"))
        .stdout(predicates::str::contains("routing-observation").not());
    assert!(source.join("agent-work.txt").is_file());

    fs::remove_file(source.join("agent-work.txt"))?;
    base()
        .args(["run", "Fix another typo", "--agent", "codex"])
        .write_stdin("y\n")
        .assert()
        .success();
    base()
        .arg("reject")
        .assert()
        .success()
        .stdout(predicates::str::contains("Result rejected"))
        .stdout(predicates::str::contains("routing-observation").not());
    assert!(!source.join("agent-work.txt").exists());
    base()
        .arg("status")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "Result\n  Rejected; source tree unchanged",
        ));
    Ok(())
}

#[test]
fn default_help_leads_with_task_completion_commands() -> anyhow::Result<()> {
    let output = cargo_bin_cmd!("dispatch")
        .arg("--help")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let help = String::from_utf8(output)?;
    assert!(help.contains("keep coding-agent work valid while the code moves"));
    for command in [
        "run", "status", "diff", "accept", "reject", "history", "explain",
    ] {
        assert!(help.contains(command), "help omits {command}: {help}");
    }
    assert!(!help.contains("routing observations"));
    Ok(())
}
