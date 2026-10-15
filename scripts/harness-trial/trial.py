#!/usr/bin/env python3
"""Helpers for scripts/harness-trial.sh, and the conformance table.

  trial.py work STATE RUN          workspace, baseline path, S0 commit, Δ path
  trial.py ended STATE RUN         exit 0 once every session in the Work has an end
  trial.py interacts STATUS B      exit 0 if the status JSON shows B's validate interaction
  trial.py record OUT              write OUT/record.json, print the summary line, exit 0-3
  trial.py table [--write DOC] OUT...   the table, from records only

The record judges Dispatch against its own contract (plan-0.4.14 §3 A). It never
compares the agents' code: an agent that could not do the task makes the cell
`skipped` with the harness's own words, not `fail`.
"""

import json
import os
import re
import subprocess
import sys

EXIT = {"pass": 0, "fail": 1, "not offered": 2, "skipped": 3}
TASKS = ["b", "a", "c"]
# What each task's change must be for the scenario to say anything about
# Dispatch: the files it may touch, text its result must contain, and text it
# must not. C must not name B's files: Dispatch rightly makes a File fact of a
# path the added lines mention, and C would then be REFRESH, not CONTINUE.
FITS = {
    "b": ({"auth.py", "test_app.py"}, [("auth.py", r"def validate\(\s*ctx\b[^)]*\btoken\b")], []),
    "a": ({"admin.py"}, [("admin.py", r"validate\(token\)"), ("admin.py", r"audit\.record\(")], []),
    "c": ({"README.md"}, [("README.md", r"(?m)^#+ .*Auth")], [("README.md", r"auth\.py|test_app\.py")]),
}
OLD_SIGNATURE = "def validate(token):"


def read(path, default=None):
    try:
        with open(path) as f:
            return f.read().strip()
    except OSError:
        return default


def load(path):
    try:
        with open(path) as f:
            return json.load(f)
    except (OSError, ValueError):
        return None


def metadata(state, run):
    return load(os.path.join(state, "runs", run, "metadata.json")) or {}


def cmd_work(state, run):
    m = metadata(state, run)
    c = (m.get("candidates") or [{}])[0]
    for v in (c.get("workspace_path"), m.get("baseline_path"), m.get("baseline_commit"), c.get("diff_path")):
        print(v or "")


def ended(state, run):
    """True when Dispatch recorded the end of every session it saw in the Work."""
    sessions = (metadata(state, run).get("attachment") or {}).get("sessions") or []
    return all(s.get("ended_at") for s in sessions)


def end_not_observable(out, run):
    """Dispatch's own word, in its owner log, that it cannot see the end of
    the session in Work `run` (a runtime that names no process)."""
    watchers = os.path.join(out, "state", "watchers")
    if not os.path.isdir(watchers):
        return False
    for name in sorted(os.listdir(watchers)):
        if name.endswith(".log"):
            with open(os.path.join(watchers, name), errors="replace") as log:
                if any(run in line and "its end is not observable" in line for line in log):
                    return True
    return False


def interacts(status, b):
    rows = (status or {}).get("interactions") or []
    return [r for r in rows if r.get("with") == b and r.get("symbol") == "validate"]


def harness_message(stdout, stderr, usage=None):
    """The harness's own words for why it stopped, as it reported them."""
    for line in reversed((stdout or "").splitlines()):
        try:
            e = json.loads(line)
        except ValueError:
            continue
        if not isinstance(e, dict):
            continue
        msg = None
        if e.get("type") == "result" and e.get("is_error"):
            msg = e.get("result") or e.get("error")
        elif e.get("type") in ("error", "turn.failed"):
            msg = e.get("message") or e.get("error")
        if isinstance(msg, dict):
            msg = msg.get("message") or json.dumps(msg)
        if msg:
            # Codex wraps the provider's JSON error in a string.
            try:
                inner = json.loads(msg)
                msg = inner.get("error", {}).get("message") or msg
            except (ValueError, AttributeError):
                pass
            return clip(msg)
    if usage and usage.get("failed") and usage.get("failure"):
        return clip(usage["failure"])
    # The last two lines: a CLI often ends with a generic line after its reason.
    lines = [l for l in (stderr or "").splitlines() if l.strip()]
    if lines:
        return clip(" / ".join(lines[-2:]))
    lines = [l for l in (stdout or "").splitlines() if l.strip()]
    return clip(lines[-1]) if lines else None


def clip(s, n=300):
    s = " ".join(str(s).split())
    return s if len(s) <= n else s[: n - 1] + "…"


def git_show(repo, tree, path):
    r = subprocess.run(["git", "-C", repo, "show", f"{tree}:{path}"], capture_output=True, text=True)
    return r.stdout if r.returncode == 0 else ""


def session_of(m, attempt):
    att = m.get("attachment")
    if att and att.get("sessions"):
        s = att["sessions"]
        return {"source": "attachment.sessions", "started_at": s[0].get("started_at"),
                "ended_at": s[-1].get("ended_at")}
    if att:
        return {"source": "attachment", "started_at": att.get("attached_at"),
                "ended_at": att.get("finished_at")}
    return {"source": "attempt", "started_at": attempt.get("started_at"),
            "ended_at": attempt.get("completed_at")}


def usage_of(attempt):
    result = (attempt.get("detail") or {}).get("result") or {}
    usage = {
        "tokens": result.get("tokens"),
        "token_semantics": result.get("token_semantics"),
        "categories": (attempt.get("detail") or {}).get("usage_categories") or None,
        "cost_usd": result.get("cost_usd"),
        "model": attempt.get("observed_model"),
    }
    if all(usage[k] is None for k in ("tokens", "categories", "cost_usd")):
        return "unknown"
    return {k: v for k, v in usage.items() if v is not None}


def task_record(out, path, t, repo, fixture_tree):
    """Facts for one task, and what they say: (record, skip reason, defects)."""
    td = os.path.join(out, "tasks", t)
    state = os.path.join(out, "state")
    run = read(os.path.join(td, "run-id.txt"), "")
    m = metadata(state, run) if run else {}
    status = load(os.path.join(out, "steps", f"{t}.status.out")) or {}
    attempt = (status.get("attempts") or [{}])[-1]
    result = (attempt.get("detail") or {}).get("result") or {}
    rec = {"run_id": run or None, "work_created": bool(run)}
    defects = []

    # How the agent ended, in its own terms.
    if path == "native":
        out_txt = read(result.get("stdout_path") or "", "")
        err_txt = read(result.get("stderr_path") or "", "")
        exit_ = "timeout" if result.get("timed_out") else result.get("exit_code")
        failed = exit_ not in (None, 0)
    else:
        out_txt = read(os.path.join(td, "agent.stdout"), "")
        err_txt = read(os.path.join(td, "agent.stderr"), "")
        exit_ = read(os.path.join(td, "agent.status"))
        failed = exit_ is not None and exit_ != "0"
    usage_file = load(os.path.join(td, "usage.json"))
    rec["agent"] = {"exit": exit_}
    if failed:
        rec["agent"]["message"] = harness_message(out_txt, err_txt, usage_file) or f"exited {exit_}"
        return rec, f"{t.upper()}: the harness stopped ({exit_}): {rec['agent']['message']}", defects

    if not run:
        if path == "native":
            msg = read(os.path.join(out, "steps", f"{t}.run.err"), "")
            observed = clip(msg.splitlines()[-1]) if msg.strip() else "no run recorded"
        elif path == "attach":
            msg = read(os.path.join(out, "steps", f"{t}.attach.err"), "")
            observed = clip(msg.splitlines()[-1]) if msg.strip() else "no run recorded"
        else:
            observed = "no Work within the wait; the harness's worktrees: " + ", ".join(
                new_worktrees(td) or ["none"])
        defects.append({"step": f"{t.upper()}: Work", "expected": "Work created", "observed": observed})
        return rec, None, defects
    if path == "native" and result.get("status") != "completed":
        # The harness exited 0, or never ran: what went wrong is Dispatch's.
        defects.append({"step": f"{t.upper()}: run", "expected": "a completed attempt",
                        "observed": f"{result.get('status') or 'no attempt'}: "
                        + clip(result.get("error") or read(os.path.join(out, "steps", f"{t}.run.err"), ""))})
        return rec, None, defects

    # The agent's own change, read from its workspace.
    agent_tree = read(os.path.join(td, "agent-tree.txt"))
    files = (read(os.path.join(td, "agent-files.txt"), "") or "").split()
    check = read(os.path.join(out, "steps", f"{t}.agent-check.exit"))
    rec["agent"].update({"tree": agent_tree, "changed_files": files,
                         "check": None if check is None else ("passed" if check == "0" else "failed")})
    if path == "worktree":
        rec["harness_worktrees"] = new_worktrees(td)
    allowed, needs, avoids = FITS[t]
    misfit = []
    if not files:
        misfit.append("changed nothing")
    if set(files) - allowed:
        misfit.append("also changed " + ", ".join(sorted(set(files) - allowed)))
    for f, pattern in needs:
        if files and not re.search(pattern, git_show(repo, agent_tree, f)):
            misfit.append(f"{f} lacks /{pattern}/")
    for f, pattern in avoids:
        if files and re.search(pattern, git_show(repo, agent_tree, f)):
            misfit.append(f"{f} has /{pattern}/")
    if check not in (None, "0"):
        misfit.append("the project's check fails on it")
    skip = None
    if misfit:
        msg = harness_message(out_txt, err_txt, usage_file) if not files else None
        skip = f"{t.upper()}: the agent's change does not fit the task: " + "; ".join(misfit)
        if msg:
            skip += f" (harness: {msg})"

    # What Dispatch recorded: S0, Δ, the session, usage.
    s0 = read(os.path.join(td, "s0-tree.txt"))
    att = m.get("attachment") or {}
    rec["s0"] = {"commit": m.get("baseline_commit"), "tree": s0, "matches_fixture": s0 == fixture_tree,
                 "provenance": att.get("provenance") or "native: snapshot of the source at run start",
                 "confidence": att.get("confidence")}
    if s0 != fixture_tree:
        defects.append({"step": f"{t.upper()}: S0", "expected": f"the fixture's tree {fixture_tree}",
                        "observed": s0})
    delta = read(os.path.join(td, "delta-tree.txt"))
    rec["delta"] = {"s0_plus_delta_tree": delta, "agent_tree": agent_tree,
                    "equal": None if delta is None or agent_tree is None else delta == agent_tree}
    if rec["delta"]["equal"] is False:
        defects.append({"step": f"{t.upper()}: Δ", "expected": f"S0 + Δ = the agent's tree {agent_tree}",
                        "observed": delta})
    rec["session"] = session_of(m, attempt)
    started, ended = rec["session"]["started_at"], rec["session"]["ended_at"]
    # Codex and Cursor name no process, so Dispatch cannot see their sessions
    # end and says so (docs/attach.md). That is the contract, not a defect, as
    # long as Dispatch declared it and recorded no end it did not observe.
    unobservable = path == "worktree" and not ended and end_not_observable(out, run)
    rec["session"]["end_observable"] = not unobservable
    rec["session"]["recorded"] = bool(started and (ended or unobservable))
    if not rec["session"]["recorded"]:
        defects.append({"step": f"{t.upper()}: session",
                        "expected": "start and end recorded, or the end declared not observable",
                        "observed": f"start {started}, end {ended}"})
    rec["usage"] = usage_of(attempt)
    rec["verification"] = (status.get("outcome") or {}).get("verification")
    rec["harness_version_recorded"] = attempt.get("harness_version")
    if path == "worktree":
        rec["finish_exit"] = read(os.path.join(out, "steps", f"{t}.finish.exit"))
        if rec["finish_exit"] != "0":
            defects.append({"step": f"{t.upper()}: dispatch finish", "expected": "exit 0",
                            "observed": f"exit {rec['finish_exit']}: " + clip(
                                read(os.path.join(out, "steps", f"{t}.finish.err"), ""))})
    return rec, skip, defects


def new_worktrees(td):
    before = set((read(os.path.join(td, "worktrees-before.txt"), "") or "").splitlines())
    after = (read(os.path.join(td, "worktrees-after.txt"), "") or "").splitlines()
    return [w for w in after if w not in before]


def first_line(path):
    text = read(path, "") or ""
    return text.splitlines()[0] if text else None


def cmd_record(out):
    h, path, timeout, model = (read(os.path.join(out, "trial.txt"), "") + "\n\n\n\n").splitlines()[:4]
    steps = os.path.join(out, "steps")
    record = {
        "schema": 1,
        "harness": h,
        "path": path,
        "started_at": read(os.path.join(out, "started-at.txt")),
        "ended_at": read(os.path.join(out, "ended-at.txt")),
        "dispatch_version": first_line(os.path.join(steps, "dispatch-version.out")),
        "harness_version": first_line(os.path.join(steps, "harness-version.out")),
        "model_override": model or None,
        "timeout_secs": int(timeout) if timeout.isdigit() else None,
        "evidence": out,
    }
    skip, defects = None, []
    early = read(os.path.join(out, "early.txt"))
    if early:
        status, reason = (early + "\n").split("\n", 1)
        return write(out, record, status, reason.strip())

    # Relative to the trial directory, so a moved trial can be judged again.
    repo = os.path.join(out, os.path.basename(read(os.path.join(out, "repo.txt"), "")))
    fixture_tree = ((read(os.path.join(out, "fixture-s0.txt"), "") or "") + "\n\n").splitlines()[1]
    record["fixture_s0"] = fixture_tree
    record["work"] = {}
    for t in TASKS:
        if not os.path.isdir(os.path.join(out, "tasks", t)):
            break
        rec, s, d = task_record(out, path, t, repo, fixture_tree)
        record["work"][t] = rec
        skip = skip or s
        defects += d

    record["verdicts"] = []
    if len(record["work"]) == 3 and all(w.get("work_created") for w in record["work"].values()):
        ids = {t: record["work"][t]["run_id"] for t in TASKS}
        a_status = load(os.path.join(steps, "a.interactions.out"))
        seen = interacts(a_status, ids["b"])
        record["interaction"] = {
            "expected": f"A ({ids['a']}) uses validate, whose signature B ({ids['b']}) changes",
            "seen": bool(seen),
            "observed": {t: (load(os.path.join(steps, f"{t}.interactions.out")) or {}).get("interactions")
                         for t in TASKS},
        }
        if not seen and not skip:
            defects.append({"step": "interaction before accept", "expected": record["interaction"]["expected"],
                            "observed": json.dumps((a_status or {}).get("interactions"))})

        new_sig = next((l.strip() for l in git_show(repo, record["work"]["b"]["agent"]["tree"], "auth.py")
                        .splitlines() if l.startswith("def validate(")), None)
        accept_exit = read(os.path.join(steps, "accept-b.exit"))
        landed = bool(new_sig) and new_sig in (read(os.path.join(steps, "source-after-accept.out"), "") or "")
        said = read(os.path.join(steps, "accept-b.err"), "") or read(os.path.join(steps, "accept-b.out"), "")
        verdicts = [{"step": "accept B", "expected": "applied: the source gets B's change",
                     "observed": f"exit {accept_exit}; "
                     + ("applied" if landed else "the source lacks B's change; accept said: " + clip(said)),
                     "ok": accept_exit == "0" and landed}]
        for t, decision, fact in (("a", "refresh", f"{OLD_SIGNATURE} => {new_sig}"), ("c", "continue", None)):
            v = (load(os.path.join(steps, f"check-{t}.out")) or {}).get("validity") or {}
            reasons = [f"{r.get('code')}: {r.get('detail')}" for r in v.get("reasons") or []]
            expected = decision.upper() + (f" with fact_broken: {fact}" if fact else "")
            ok = v.get("decision") == decision and (not fact or f"fact_broken: {fact}" in reasons)
            verdicts.append({"step": f"check {t.upper()}", "expected": expected,
                             "observed": (v.get("decision") or "none").upper()
                             + (" with " + "; ".join(reasons) if reasons else ""), "ok": ok})
        record["verdicts"] = verdicts
        # A task that does not fit leaves the verdicts meaningless, not wrong.
        if not skip:
            defects += [{k: v[k] for k in ("step", "expected", "observed")} for v in verdicts if not v["ok"]]

    record["defects"] = defects
    if defects:
        d = defects[0]
        reason = f"{d['step']}: expected {d['expected']}; observed {d['observed']}"
        if len(defects) > 1:
            reason += f" (+{len(defects) - 1} more)"
        return write(out, record, "fail", reason)
    if skip:
        return write(out, record, "skipped", skip)
    return write(out, record, "pass", None)


def write(out, record, status, reason):
    record["status"] = status
    record["reason"] = reason
    with open(os.path.join(out, "record.json"), "w") as f:
        json.dump(record, f, indent=2, ensure_ascii=False)
        f.write("\n")
    print(summary(record))
    return EXIT.get(status, 1)


def summary(r):
    line = f"{r['harness']} {r['path']}: {r['status']}"
    if r.get("reason"):
        line += f": {clip(r['reason'], 200)}"
    versions = ", ".join(v for v in (r.get("dispatch_version"), r.get("harness_version")) if v)
    return line + (f" ({versions})" if versions else "") + f" · {r['evidence']}"


HARNESSES = ["claude", "codex", "cursor", "hermes"]
PATHS = ["attach", "native", "worktree"]
BEGIN, END = "<!-- conformance-table:begin -->", "<!-- conformance-table:end -->"


def cmd_table(args):
    doc = None
    if args[:1] == ["--write"]:
        doc, args = args[1], args[2:]
    records = {}
    for root in args:
        for name in sorted(os.listdir(root)):
            r = load(os.path.join(root, name, "record.json"))
            if r:
                records[(r["harness"], r["path"])] = r
    roots = sorted({os.path.dirname(r["evidence"]) for r in records.values()})

    def shown(path):
        return path.replace(os.path.expanduser("~"), "~", 1)

    lines = [f"Evidence under `{shown(roots[0])}/`, on the machine that ran the trials."
             if len(roots) == 1 else "Evidence on the machine that ran the trials.", "",
             "| Harness | Path | Result | Dispatch | Harness | Date | Evidence |",
             "|---|---|---|---|---|---|---|"]
    for h in HARNESSES:
        for p in PATHS:
            r = records.get((h, p))
            if not r:
                lines.append(f"| {h} | {p} | not run | | | | |")
                continue
            result = r["status"] + (f": {clip(r['reason'], 160)}" if r.get("reason") else "")
            harness = (r.get("harness_version") or "unknown") + (
                f", model {r['model_override']}" if r.get("model_override") else "")
            evidence = os.path.basename(r["evidence"]) + "/" if len(roots) == 1 else shown(r["evidence"])
            lines.append("| " + " | ".join(cell.replace("|", "\\|") for cell in [
                h, p, result, r.get("dispatch_version") or "unknown", harness,
                (r.get("started_at") or "")[:10], f"`{evidence}`"]) + " |")
    table = "\n".join(lines)
    if not doc:
        print(table)
        return 0
    with open(doc) as f:
        text = f.read()
    start, end = text.index(BEGIN) + len(BEGIN), text.index(END)
    with open(doc, "w") as f:
        f.write(text[:start] + "\n" + table + "\n" + text[end:])
    return 0


def main(argv):
    if argv[:1] == ["work"] and len(argv) == 3:
        return cmd_work(argv[1], argv[2]) or 0
    if argv[:1] == ["ended"] and len(argv) == 3:
        return 0 if ended(argv[1], argv[2]) else 1
    if argv[:1] == ["interacts"] and len(argv) == 3:
        return 0 if interacts(load(argv[1]), argv[2]) else 1
    if argv[:1] == ["record"] and len(argv) == 2:
        return cmd_record(argv[1])
    if argv[:1] == ["table"]:
        return cmd_table(argv[1:])
    print(__doc__, file=sys.stderr)
    return 64


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
