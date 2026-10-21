#!/usr/bin/env python3
"""Native host consent uses a terminal, a stand-in agent, and a private state."""
import json
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

sys.dont_write_bytecode = True
from review_refinement import Session

binary = str(Path(sys.argv[1]).resolve())


def executable(path, marker):
    path.write_text(
        "#!/bin/sh\n"
        "if [ \"$1\" = '--version' ]; then echo 'cursor fixture 1'; exit 0; fi\n"
        f"printf 'run\\n' >> '{marker}'\n"
        "printf '// changed by fixture\\n' > src/lib.rs\n"
        "printf 'done\\n' > agent-done\n"
        "printf '{\"type\":\"result\",\"model\":\"fixture\"}\\n'\n"
    )
    path.chmod(0o755)


def config(source, cursor, check="test -f agent-done", baseline="test -f src/lib.rs"):
    (source / "dispatch.yml").write_text(
        "execution:\n  timeout_secs: 120\n"
        f"checks:\n  baseline: [{json.dumps(baseline)}]\n"
        f"  verify: [{json.dumps(check)}]\n"
        "harnesses:\n"
        f"  cursor:\n    executable: {json.dumps(str(cursor))}\n"
        f"  claude:\n    executable: {json.dumps(str(cursor))}\n"
    )


def run_args(source, state, *tail):
    return [binary, "--state-dir", str(state), "run", str(source),
            "--task", "Change src/lib.rs", "--agent", "cursor", *tail]


def piped(args, source, env, answer=None):
    return subprocess.run(args, cwd=source, env=env, input=answer,
                          capture_output=True, text=True, timeout=120)


def terminal(args, source, captures, env, name, answer, succeeds=True):
    with Session(args, source, captures, name, env=env) as ui:
        ui.wait("This run only")
        ui.wait("Allow for this project")
        ui.wait("Cancel")
        ui.send(answer + "\r")
        if succeeds:
            ui.finish()
        else:
            ui.wait("local execution was not authorized")
            ui.process.wait(timeout=120)
            assert ui.process.returncode != 0, ui.clean
        return ui.clean


with tempfile.TemporaryDirectory(prefix="dispatch-native-consent-") as temp:
    base = Path(temp)
    source = base / "project"
    source.mkdir()
    (source / "src").mkdir()
    (source / "src/lib.rs").write_text("// original\n")
    home = base / "home"
    home.mkdir()
    state = base / "state"
    marker = base / "invocations"
    cursor = base / "cursor-v1"
    executable(cursor, marker)
    config(source, cursor)
    env = dict(os.environ, HOME=str(home), NO_COLOR="1")
    captures = base / "captures"
    native_records = lambda: list((state / "projects").glob("native-*.json"))
    count = lambda: len(marker.read_text().splitlines()) if marker.exists() else 0

    first = terminal(run_args(source, state), source, captures, env,
                     "grant", "2")
    old = re.search(r"\bRUN ([0-9A-Z]+)", first)
    assert old, first
    assert count() == 1
    records = native_records()
    assert len(records) == 1, records
    record = json.loads(records[0].read_text())
    commands = record["commands"]
    assert commands["root"] == str(source.resolve()), record
    assert commands["harness"] == "cursor", record
    assert commands["executable"] == str(cursor.resolve()), record
    assert commands["baseline"] == ["test -f src/lib.rs"], record
    assert commands["verify"] == ["test -f agent-done"], record
    assert not (source / "native-consent.json").exists()

    covered = piped(run_args(source, state, "--json"), source, env)
    assert covered.returncode == 0, covered.stderr
    assert json.loads(covered.stdout)["mode"] == "native"
    assert count() == 2
    refreshed = piped([binary, "--state-dir", str(state), "refresh", old.group(1),
                       "--json"], source, env)
    assert refreshed.returncode == 0, refreshed.stderr
    assert json.loads(refreshed.stdout)["run_id"] != old.group(1), refreshed.stdout
    assert count() == 3

    changed_exe = base / "cursor-v2"
    executable(changed_exe, marker)
    config(source, changed_exe)
    before = count()
    uncovered = piped(run_args(source, state, "--json"), source, env)
    assert uncovered.returncode != 0 and "--allow-unsafe-local" in uncovered.stderr
    assert count() == before
    assert json.loads(records[0].read_text())["commands"]["executable"] == str(cursor.resolve())
    terminal(run_args(source, state), source, captures, env, "one-run", "1")
    assert count() == before + 1
    assert len(native_records()) == 1
    assert json.loads(records[0].read_text())["commands"]["executable"] == str(cursor.resolve())

    config(source, cursor, "test -e agent-done")
    changed_checks = piped(run_args(source, state, "--json"), source, env)
    assert changed_checks.returncode != 0 and "--allow-unsafe-local" in changed_checks.stderr
    config(source, cursor)
    config_path = source / "dispatch.yml"
    config_path.write_text(config_path.read_text().replace(
        "test -f src/lib.rs", "test -e src/lib.rs"))
    changed_baseline = piped(run_args(source, state, "--json"), source, env)
    assert changed_baseline.returncode != 0 and "--allow-unsafe-local" in changed_baseline.stderr
    config(source, cursor)
    other_harness = piped([binary, "--state-dir", str(state), "run", str(source),
                           "--task", "Change src/lib.rs", "--agent", "claude", "--json"],
                          source, env)
    assert other_harness.returncode != 0 and "--allow-unsafe-local" in other_harness.stderr

    config(source, changed_exe)
    flag = piped(run_args(source, state, "--json", "--allow-unsafe-local"), source, env)
    assert flag.returncode == 0, flag.stderr
    assert len(native_records()) == 1, native_records()

    second_source = base / "second"
    (second_source / "src").mkdir(parents=True)
    (second_source / "src/lib.rs").write_text("// original\n")
    config(second_source, cursor)
    second_state = base / "second-state"
    # A piped answer may allow one run, never the project.
    piped_answer = piped(run_args(second_source, second_state), second_source, env, "2\n")
    assert piped_answer.returncode != 0 and "--allow-unsafe-local" in piped_answer.stderr
    assert not list((second_state / "projects").glob("native-*.json"))
    assert not (second_state / "runs").exists()
    terminal(run_args(second_source, second_state), second_source, captures,
             env, "cancel", "", succeeds=False)
    assert not list((second_state / "projects").glob("native-*.json"))
    assert not (second_state / "runs").exists()

    third_source = base / "third"
    (third_source / "src").mkdir(parents=True)
    (third_source / "src/lib.rs").write_text("// original\n")
    third_state = base / "third-state"
    before = base / "agent-before"
    after = base / "agent-after"
    executable(before, base / "ran-before")
    executable(after, base / "ran-after")
    link = base / "agent-link"
    link.symlink_to(before)
    config(third_source, link, baseline=f"ln -sf '{after}' '{link}'")
    terminal(run_args(third_source, third_state), third_source, captures,
             env, "pinned", "2")
    assert (base / "ran-before").exists(), "the authorized executable did not run"
    assert not (base / "ran-after").exists(), "a changed symlink target was launched"
    pinned_record = json.loads(next((third_state / "projects").glob("native-*.json")).read_text())
    assert pinned_record["commands"]["executable"] == str(before.resolve())
    changed_link = piped(run_args(third_source, third_state, "--json"), third_source, env)
    assert changed_link.returncode != 0 and "--allow-unsafe-local" in changed_link.stderr

print("native consent terminal, standing, exact-command, refresh and flag journeys passed")
