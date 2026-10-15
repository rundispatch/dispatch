#![cfg(unix)]
//! Presenter/reviewer integration uses a deterministic executable, never a provider.
use anyhow::{Context, Result};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::Command};

struct Fixture {
    _temp: tempfile::TempDir,
    source: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new(shape: &str) -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        let source = root.join("source");
        let state = root.join("state");
        fs::create_dir_all(source.join("src"))?;
        fs::create_dir(&state)?;
        fs::write(root.join("shape"), shape)?;
        fs::write(source.join("src/lib.rs"), "const RADIUS: u32 = 20;\n")?;
        if shape == "large" {
            for index in 0..120 {
                fs::write(
                    source.join(format!("src/item{index:03}.rs")),
                    format!("// baseline {index}\n"),
                )?;
            }
            fs::write(source.join("deleted.txt"), "delete this file\n")?;
            fs::write(source.join("before rename.txt"), "same bytes\n")?;
            fs::write(source.join("image.bin"), [0, 1, 2, 3])?;
        }
        let agent = root.join("codex");
        fs::write(
            &agent,
            r#"#!/usr/bin/env python3
import pathlib, sys, time
root = pathlib.Path(__file__).parent
if '--version' in sys.argv:
    print('review fixture')
    raise SystemExit(0)
if 'app-server' in sys.argv:
    for line in sys.stdin:
        if '"id":0' in line:
            print('{"id":0,"result":{"userAgent":"fixture"}}', flush=True)
        elif '"id":1' in line:
            print('{"id":1,"result":{"account":{"type":"chatgpt","planType":"plus","email":"fixture@example.invalid"}}}', flush=True)
        elif '"id":2' in line:
            print('{"id":2,"result":{"rateLimitsByLimitId":{}}}', flush=True)
            break
    raise SystemExit(0)
with (root / 'invocations').open('a') as calls: calls.write('fixture\n')
time.sleep(0.25)
shape = (root / 'shape').read_text()
pathlib.Path('src/lib.rs').write_text('const RADIUS: u32 = 40; // delivered\n')
if shape == 'large':
    for index in range(120):
        pathlib.Path(f'src/item{index:03}.rs').write_text(f'// delivered {index}\n')
    pathlib.Path('deleted.txt').unlink()
    pathlib.Path('before rename.txt').rename('after rename.txt')
    pathlib.Path('image.bin').write_bytes(bytes([0, 4, 5, 6]))
    long_path = pathlib.Path('src') / ('long-directory-' * 7)
    long_path.mkdir()
    (long_path / '-- leading λ file.rs').write_text('// unusual path\n' + 'x' * 20000 + '\n')
elif shape == 'huge':
    pathlib.Path('src/lib.rs').write_text(''.join(f'// delivered row {i} ' + 'x' * 180 + '\n' for i in range(16000)))
print('{"type":"result","model":"light-model"}')
"#,
        )?;
        fs::set_permissions(&agent, fs::Permissions::from_mode(0o755))?;
        fs::write(
            source.join("dispatch.yml"),
            format!(
                "execution:\n  timeout_secs: 600\nharnesses:\n  codex:\n    executable: '{}'\n{}",
                agent.display(),
                if shape == "unverified" {
                    ""
                } else {
                    "checks:\n  verify: ['/usr/bin/true']\n"
                }
            ),
        )?;
        fs::write(
            state.join("resources.yml"),
            "version: 1\nallocation_enabled: true\nprofiles:\n  - provider: openai\n    funding_source: chatgpt-plus\n    harness: codex\n    model: light-model\n    effort: low\n    runtime: local\n    service_mode: standard\n    pool: fixture\n    tier: light\n    included: true\n    no_overage_verified: true\n    authorization_revision: 1\n    codex_account: {\"account_sha256\":\"cc6d96611cffa9f02c3626f0b9ee897dc171e2d540a5cae349d4ec316104997b\",\"checked_at\":\"2026-01-01T00:00:00Z\"}\n",
        )?;
        Ok(Self {
            _temp: temp,
            source,
            state,
        })
    }

    fn exercise(&self, scenario: &str) -> Result<()> {
        let output = Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/review_refinement.py"
            ))
            .arg(assert_cmd::cargo_bin!("dispatch"))
            .arg(&self.source)
            .arg(&self.state)
            .arg(scenario)
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "Review fixture {scenario}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let invocations = fs::read_to_string(self.state.parent().unwrap().join("invocations"))
            .context("fixture did not execute")?;
        assert_eq!(
            invocations.lines().count(),
            if matches!(scenario, "plain-transition" | "dumb-transition") {
                2
            } else {
                1
            },
            "unexpected fixture invocation count"
        );
        Ok(())
    }

    fn reviewer(&self, scenario: &str) -> Result<()> {
        let root = self.state.parent().unwrap();
        fs::write(root.join("reviewer-scenario"), scenario)?;
        let reviewer = root.join("reviewer fixture λ");
        fs::write(
            &reviewer,
            r#"#!/usr/bin/env python3
import json, os, pathlib, subprocess, sys, termios, time, tty
root = pathlib.Path(__file__).parent
scenario = (root / 'reviewer-scenario').read_text()
attributes = termios.tcgetattr(0)
assert attributes[3] & termios.ICANON, 'reviewer inherited raw input'
assert attributes[3] & termios.ECHO, 'reviewer inherited hidden input'
if scenario != 'gui-wait':
    assert os.tcgetpgrp(0) == os.getpgrp(), 'reviewer did not own the foreground terminal'
args = sys.argv[1:]
if scenario in ('editor-return', 'editor-cancel'):
    patch = pathlib.Path(args[-1])
    assert patch.is_absolute() and 'diff --git' in patch.read_text()
    (root / 'reviewer-observation.json').write_text(json.dumps({'patch': str(patch)}))
    print('fixture launcher exited', flush=True)
    raise SystemExit(0)
assert '--diff' in args or '-d' in args, args
before, after = map(pathlib.Path, args[-2:])
assert before.is_absolute() and after.is_absolute()
assert before.parent.parent == after.parent.parent
assert before.parent.name == 'before' and after.parent.name == 'after'
assert 'baseline' not in str(before) and 'workspace' not in str(after)
assert before.read_text() == 'const RADIUS: u32 = 20;\n'
assert after.read_text() == 'const RADIUS: u32 = 40; // delivered\n'
(root / 'reviewer-observation.json').write_text(json.dumps({'args': args, 'before': str(before), 'after': str(after)}))
print('fixture reviewer ready', flush=True)
if scenario == 'nested-signal':
    nested = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])
    (root / 'reviewer-processes.json').write_text(json.dumps([os.getpid(), nested.pid]))
    nested.wait()
    raise SystemExit(0)
if scenario == 'crash':
    tty.setraw(0)
    raise SystemExit(7)
if scenario == 'gui-wait':
    assert '--wait' in args
    while not (root / 'reviewer-release').exists(): time.sleep(.03)
else:
    assert sys.stdin.readline().strip() == 'reviewer-input'
if scenario == 'drift': (root / 'source/src/lib.rs').write_text('// founder edit during review\n')
if scenario == 'copy-edit':
    os.chmod(after, 0o600)
    after.write_text('// edit only to review copy\n')
print('fixture reviewer finished', flush=True)
"#,
        )?;
        fs::set_permissions(&reviewer, fs::Permissions::from_mode(0o755))?;
        fs::write(
            self.state.join("reviewer.yml"),
            format!(
                "kind: {}\nexecutable: '{}'\n",
                match scenario {
                    "gui-wait" => "code",
                    "editor-return" | "editor-cancel" => "editor",
                    _ => "nvim",
                },
                if scenario == "missing" {
                    root.join("absent-reviewer")
                } else {
                    reviewer
                }
                .display()
            ),
        )?;
        Ok(())
    }
}

#[test]
fn phase4_plain_review_transition_appears_once_per_delivery() -> Result<()> {
    for scenario in ["plain-transition", "dumb-transition"] {
        let fixture = Fixture::new("tiny")?;
        fs::remove_dir_all(fixture.source.join("src"))?;
        fs::create_dir(fixture.source.join("tests"))?;
        fs::write(
            fixture.source.join("main.c"),
            "int main(void) { return 0; }\n",
        )?;
        fs::write(
            fixture.source.join("tests/collision_test.c"),
            "// C fixture\n",
        )?;
        let agent = fixture.source.parent().unwrap().join("codex");
        let script = fs::read_to_string(&agent)?.replace("src/lib.rs", "tests/collision_test.c");
        fs::write(agent, script)?;
        fixture.exercise(scenario)?;
    }
    Ok(())
}

#[test]
fn phase4_review_depths_typeahead_and_large_change_index() -> Result<()> {
    for (shape, scenario) in [
        ("tiny", "tiny"),
        ("unverified", "unverified"),
        ("large", "large"),
        ("huge", "huge"),
        ("large", "large-color"),
    ] {
        Fixture::new(shape)?.exercise(scenario)?;
    }
    Ok(())
}

#[test]
fn phase4_external_review_owns_terminal_preserves_evidence_and_apply_guards() -> Result<()> {
    for scenario in [
        "terminal",
        "gui-wait",
        "crash",
        "missing",
        "cancel",
        "drift",
        "copy-edit",
        "concurrent-review",
        "editor-return",
        "editor-cancel",
        "nested-signal",
    ] {
        let fixture = Fixture::new("tiny")?;
        fixture.reviewer(scenario)?;
        fixture.exercise(scenario)?;
    }
    Ok(())
}

/// Drives `tests/fixtures/tui_session.py` directly (the same PTY fixture
/// `tests/native_runs.rs` uses), following the `exercise`/`reviewer`
/// pattern of sending keys and waiting for text. The fixture itself asserts
/// the run's committed outcome and events for each `auto_apply_*` scenario.
fn auto_apply_pty(fixture: &Fixture, scenario: &str) -> Result<()> {
    let output = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/tui_session.py"
        ))
        .arg(assert_cmd::cargo_bin!("dispatch"))
        .arg(&fixture.source)
        .arg(&fixture.state)
        .arg(scenario)
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "PTY {scenario}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[test]
fn auto_apply_toggle_flips_the_hint_row_on_shift_tab() -> Result<()> {
    // No goal is submitted; a "tiny" fixture only needs to exist on disk.
    let fixture = Fixture::new("tiny")?;
    auto_apply_pty(&fixture, "auto_apply_toggle")
}

#[test]
fn auto_apply_goal_applies_a_ready_result_without_recording_review() -> Result<()> {
    let fixture = Fixture::new("tiny")?;
    auto_apply_pty(&fixture, "auto_apply_goal")
}

#[test]
fn auto_apply_aa_records_human_review_then_turns_the_mode_on() -> Result<()> {
    let fixture = Fixture::new("tiny")?;
    auto_apply_pty(&fixture, "auto_apply_aa")
}

#[test]
fn auto_apply_skipped_without_checks_leaves_the_run_for_review() -> Result<()> {
    // "unverified" configures no checks, so stage 1 skips before any gate work.
    let fixture = Fixture::new("unverified")?;
    auto_apply_pty(&fixture, "auto_apply_skipped")
}

#[test]
fn phase4_available_native_pagers_return_to_the_same_review() -> Result<()> {
    for (program, scenario) in [("less", "native-pager"), ("delta", "native-delta")] {
        let Ok(executable) = which::which(program) else {
            // Native tools are optional; fake adapters cover the contract everywhere.
            continue;
        };
        let fixture = Fixture::new("tiny")?;
        fs::write(
            fixture.state.join("reviewer.yml"),
            format!(
                "kind: {}\nexecutable: '{}'\n",
                if program == "less" { "pager" } else { "delta" },
                executable.display()
            ),
        )?;
        fixture.exercise(scenario)?;
    }
    Ok(())
}
