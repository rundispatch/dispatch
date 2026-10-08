"""Acceptance tests for the interactive `dispatch watch` (docs/plan-0.4.11.md, §3).

Written from the plan's contract, independently of the implementation's own
tests. Each scenario builds a real project in a temporary directory: linked Git
worktrees attached with `attach --workspace`, and Claude Code worktrees
registered through `dispatch hook claude`. The test plays every agent by
editing files itself; no agent is ever launched. The real binary runs in a
PTY (review_refinement.Session), and a small screen model below turns its
bytes into the screen a person would see.

Usage: watch_acceptance.py <dispatch binary> <scenario>
"""
import codecs
import json
import os
import re
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import unicodedata
from datetime import datetime, timedelta, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from review_refinement import Session, has_color, WAIT  # noqa: E402

AUTH = "def validate(token):\n    return bool(token)\n\n\ndef refresh(token):\n    return token\n"
AUTH_MOVED = "def validate(ctx, token):\n    return bool(token)\n\n\ndef refresh(token):\n    return token\n"
API = "def serve():\n    return None\n"
CALLER = "def serve():\n    return None\n\n\ndef login(t):\n    return validate(t)\n"
UTIL = "def slug(text):\n    return text.lower()\n"
UTIL_EDIT = "def slug(text):\n    return text.strip().lower()\n"
ROOT = "tinyauth"
VIEWPORT = 20  # the inline viewport is at most 20 lines tall
SETTLED = 0.15  # a frame unchanged this long is drawn whole


# ---------------------------------------------------------------- the screen

def cells(char):
    if unicodedata.combining(char):
        return 0
    return 2 if unicodedata.east_asian_width(char) in "WF" else 1


class Term:
    """Just enough of a VT100 to know what is on screen. `wraps` counts the
    times a printed character ran past the right margin: a row that wrapped."""

    CSI = re.compile(r"\x1b\[([0-?]*)[ -/]*([@-~])")
    OSC = re.compile(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)")
    SHORT = re.compile(r"\x1b(?:[()][0-9A-Za-z]|[0-9A-Za-z=><])")

    def __init__(self, cols, rows):
        self.cols, self.rows = cols, rows
        self.grid = self.blank(rows)
        self.main = None
        self.x = self.y = 0
        self.saved = (0, 0)
        self.pending = False
        self.top, self.bottom = 0, rows - 1
        self.buffer = ""
        self.wraps = 0

    def blank(self, rows):
        return [[" "] * self.cols for _ in range(rows)]

    def resize(self, cols, rows):
        old = self.grid
        self.cols, self.rows = cols, rows
        self.grid = self.blank(rows)
        for y, row in enumerate(old[:rows]):
            self.grid[y][:min(cols, len(row))] = row[:cols]
        self.x, self.y = min(self.x, cols - 1), min(self.y, rows - 1)
        self.top, self.bottom = 0, rows - 1
        self.pending = False

    def scroll_up(self, n=1):
        for _ in range(n):
            del self.grid[self.top]
            self.grid.insert(self.bottom, [" "] * self.cols)

    def scroll_down(self, n=1):
        for _ in range(n):
            del self.grid[self.bottom]
            self.grid.insert(self.top, [" "] * self.cols)

    def linefeed(self):
        if self.y == self.bottom:
            self.scroll_up()
        elif self.y < self.rows - 1:
            self.y += 1

    def put(self, char):
        width = cells(char)
        if width == 0:
            return
        if self.pending or self.x + width > self.cols:
            self.wraps += 1
            self.x, self.pending = 0, False
            self.linefeed()
        self.grid[self.y][self.x] = char
        if width == 2 and self.x + 1 < self.cols:
            self.grid[self.y][self.x + 1] = "\0"
        self.x += width
        if self.x >= self.cols:
            self.x, self.pending = self.cols - 1, True

    def erase(self, y, start, end):
        for x in range(max(0, start), min(self.cols, end)):
            self.grid[y][x] = " "

    def csi(self, params, final):
        private = params.startswith("?")
        values = [int(v) if v.isdigit() else 0 for v in params.lstrip("?").split(";")] if params.lstrip("?") else []
        n = values[0] if values and values[0] else 1
        if final in "hl" and private and 1049 in values:
            if final == "h" and self.main is None:
                self.main, self.grid = self.grid, self.blank(self.rows)
            elif final == "l" and self.main is not None:
                self.grid, self.main = self.main, None
            return
        if final in "hlmnqtsu" or private:
            return
        self.pending = False
        if final in "Hf":
            self.y = min(max((values[0] if values else 1) - 1, 0), self.rows - 1)
            self.x = min(max((values[1] if len(values) > 1 else 1) - 1, 0), self.cols - 1)
        elif final == "A": self.y = max(self.y - n, 0)
        elif final == "B": self.y = min(self.y + n, self.rows - 1)
        elif final == "C": self.x = min(self.x + n, self.cols - 1)
        elif final == "D": self.x = max(self.x - n, 0)
        elif final == "E": self.y, self.x = min(self.y + n, self.rows - 1), 0
        elif final == "F": self.y, self.x = max(self.y - n, 0), 0
        elif final == "G": self.x = min(n - 1, self.cols - 1)
        elif final == "d": self.y = min(n - 1, self.rows - 1)
        elif final == "J":
            mode = values[0] if values else 0
            if mode == 0:
                self.erase(self.y, self.x, self.cols)
                for y in range(self.y + 1, self.rows): self.erase(y, 0, self.cols)
            elif mode == 1:
                self.erase(self.y, 0, self.x + 1)
                for y in range(0, self.y): self.erase(y, 0, self.cols)
            else:
                self.grid = self.blank(self.rows)
        elif final == "K":
            mode = values[0] if values else 0
            if mode == 0: self.erase(self.y, self.x, self.cols)
            elif mode == 1: self.erase(self.y, 0, self.x + 1)
            else: self.erase(self.y, 0, self.cols)
        elif final == "X": self.erase(self.y, self.x, self.x + n)
        elif final == "S": self.scroll_up(n)
        elif final == "T": self.scroll_down(n)
        elif final == "L":
            for _ in range(n):
                del self.grid[self.bottom]
                self.grid.insert(self.y, [" "] * self.cols)
        elif final == "M":
            for _ in range(n):
                del self.grid[self.y]
                self.grid.insert(self.bottom, [" "] * self.cols)
        elif final == "r":
            self.top = (values[0] - 1) if values and values[0] else 0
            self.bottom = (values[1] - 1) if len(values) > 1 and values[1] else self.rows - 1
            self.x = self.y = 0

    def feed(self, text):
        data = self.buffer + text
        self.buffer = ""
        i = 0
        while i < len(data):
            char = data[i]
            if char == "\x1b":
                for pattern in (self.CSI, self.OSC, self.SHORT):
                    found = pattern.match(data, i)
                    if found:
                        break
                if not found:
                    if len(data) - i < 64:
                        self.buffer = data[i:]
                        return
                    i += 1
                    continue
                sequence = found.group(0)
                if pattern is self.CSI:
                    self.csi(found.group(1), found.group(2))
                elif sequence == "\x1b7":
                    self.saved = (self.x, self.y)
                elif sequence == "\x1b8":
                    self.x, self.y = self.saved
                elif sequence == "\x1bM":
                    if self.y == self.top: self.scroll_down()
                    else: self.y = max(self.y - 1, 0)
                i = found.end()
                continue
            if char == "\r": self.x, self.pending = 0, False
            elif char == "\n": self.pending = False; self.linefeed()
            elif char == "\b": self.x, self.pending = max(self.x - 1, 0), False
            elif char == "\t": self.x = min((self.x // 8 + 1) * 8, self.cols - 1)
            elif ord(char) >= 32: self.put(char)
            i += 1

    def cell_lines(self):
        """Each line with one character per terminal cell: a wide character
        is followed by "\\0", so an index is a column."""
        return ["".join(row) for row in self.grid]

    def lines(self):
        return [line.replace("\0", "").rstrip() for line in self.cell_lines()]


class Watch(Session):
    """`dispatch watch` in a PTY, with the screen it draws."""

    def __init__(self, project, width, height=30, flags=(), env=None, label=None):
        project.sessions += 1
        label = label or f"watch-{project.sessions}-{width}"
        args = [project.binary, "--state-dir", str(project.state), "watch", *flags]
        environment = {"NO_COLOR": None, "DISPATCH_COLOR": None, "DISPATCH_THEME": None}
        environment.update(env or {})
        super().__init__(args, project.root, project.base / "captures", label,
                         env=environment, width=width, height=height)
        self.term = Term(width, height)
        self.fed = 0
        self.ascii = "--ascii" in flags
        self.terminal_decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self.settled_at = None  # `fed` when the screen was last seen settled

    def pump(self, duration=0.05):
        super().pump(duration)
        if len(self.output) > self.fed:
            self.term.feed(self.terminal_decoder.decode(bytes(self.output[self.fed:])))
            self.fed = len(self.output)

    def resize(self, width, height):
        self.term.resize(width, height)
        self.settled_at = None
        super().resize(width, height)

    def settle_frame(self, timeout=WAIT):
        """Wait for a whole frame before any read: its last line, the hint, is
        drawn (`q leave`, or a confirmation's `Esc back`) and the screen stays
        the same for SETTLED seconds. A frame is drawn top to bottom, so a
        read in between sees half of one. Nothing new since the last settled
        frame means it is still that frame."""
        self.pump(0.01)
        if self.settled_at == self.fed:
            return
        deadline = time.monotonic() + timeout
        seen, since = None, time.monotonic()
        while self.process.poll() is None:
            now = self.term.lines()[:VIEWPORT]
            if now != seen:
                seen, since = now, time.monotonic()
            else:
                drawn = [line.strip() for line in now if line.strip()]
                if (drawn and drawn[-1].endswith(("q leave", "Esc back"))
                        and time.monotonic() - since >= SETTLED):
                    self.settled_at = self.fed
                    return
            if time.monotonic() > deadline:
                raise AssertionError(f"no settled frame in {timeout} s\n--- screen ---\n" + "\n".join(now))
            self.pump()

    # What is on screen: always a whole frame.
    def lines(self):
        self.settle_frame()
        return self.term.lines()[:VIEWPORT]

    def cell_lines(self):
        self.settle_frame()
        return self.term.cell_lines()[:VIEWPORT]

    def screen(self):
        return "\n".join(self.lines())

    def marker(self):
        return ">" if self.ascii else "›"

    def rule_char(self):
        return "-" if self.ascii else "─"

    def is_rule(self, line):
        stripped = line.strip()
        return len(stripped) >= 10 and set(stripped) == {self.rule_char()}

    def rules(self):
        return [i for i, line in enumerate(self.lines()) if self.is_rule(line)]

    def selected(self):
        found = [line for line in self.lines() if line.lstrip().startswith(self.marker() + " ")]
        return found[0] if found else None

    def hint(self):
        lines = [line for line in self.lines() if line.strip()]
        return lines[-1].strip() if lines else ""

    def column_titles(self):
        for i, line in enumerate(self.cell_lines()):
            if re.search(r"\bWORK\s+AGENT\s+STATE\s+VERDICT\s+CHECKS\s+TOUCHES\b", line):
                return i, {title: line.index(title) for title in
                           ("WORK", "AGENT", "STATE", "VERDICT", "CHECKS", "TOUCHES")}
        return None, None

    def table(self):
        """Wide layout: the table rows as (text, cell line)."""
        at, _ = self.column_titles()
        if at is None:
            return []
        rows = []
        for cell_line in self.cell_lines()[at + 1:]:
            text = cell_line.replace("\0", "").rstrip()
            if self.is_rule(text) or not text.strip():
                break
            rows.append((text, cell_line))
        return rows

    def cards(self):
        """Narrow layout: (line 1, line 2) for each card between the first two rules."""
        rules = self.rules()
        if len(rules) < 2:
            return []
        body = self.lines()[rules[0] + 1:rules[1]]
        return [(body[i], body[i + 1] if i + 1 < len(body) else "") for i in range(0, len(body), 2)]

    def details(self):
        """Everything below the list, joined, whitespace collapsed (values wrap)."""
        rules = self.rules()
        if len(rules) < 2:
            return ""
        return " ".join(" ".join(self.lines()[rules[1] + 1:]).split())

    def joined(self):
        return " ".join(self.screen().split())

    def until(self, what, check, timeout=WAIT):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.pump()
            try:
                if check():
                    return
            except (IndexError, ValueError, TypeError):
                pass
            if self.process.poll() is not None:
                break
        raise AssertionError(f"waiting for {what}\n--- screen ({self.term.cols}x{self.term.rows}) ---\n"
                             + self.screen())

    def shows(self, text, timeout=WAIT):
        wanted = " ".join(text.split())
        self.until(repr(text), lambda: wanted in self.joined(), timeout)

    def settle(self, seconds=0.6):
        self.pump(seconds)

    def leave(self):
        self.send("q")
        self.finish()


def is_row_of(text, name):
    """Whether a row or card line 1 is `name`'s: its WORK cell starts with it
    (never a mention in TOUCHES)."""
    work = re.sub(r"^\s*(?:›|>)?\s*", "", text)
    return re.match(re.escape(name) + r"(\s|$)", work) is not None


def row_of(watch, name):
    """The table row (wide) or card (narrow) that names `name`."""
    for text, cell_line in watch.table():
        if is_row_of(text, name):
            return text, cell_line
    for first, second in watch.cards():
        if is_row_of(first, name):
            return first, second
    raise ValueError(name)


def visit(watch, count):
    """Every row (wide: text, cell line) or card (narrow: line 1, line 2),
    moving the selection down so that a scrolled list shows each in turn. The
    selection marker is blanked so that an entry reads the same either way."""
    seen = {}
    blank = lambda text: text.replace(watch.marker() + " ", "  ", 1)

    def collect():
        for first, second in watch.table() or watch.cards():
            seen.setdefault(blank(first), (blank(first), blank(second)))

    def move(key):
        # One key at a time: wait until the move is drawn.
        before = watch.selected()
        watch.send(key)
        try:
            watch.until(f"the selection to move ({key})", lambda: watch.selected() != before, timeout=10)
        except AssertionError:
            return False
        watch.settle(0.1)
        return True

    watch.settle(0.25)
    collect()
    moves = 0
    while len(seen) < count and moves < count and move("j"):
        moves += 1
        collect()
    for _ in range(moves):
        move("k")
    return list(seen.values())


def selected_name(watch, candidates):
    line = watch.selected()
    assert line is not None, watch.screen()
    hits = [name for name in candidates if is_row_of(line, name)]
    assert len(hits) == 1, (hits, line)
    return hits[0]


def select(watch, name, candidates):
    """Move the selection to `name` with j/k, as a person would."""
    for _ in range(2 * len(candidates) + 2):
        watch.settle(0.15)
        current = selected_name(watch, candidates)
        if current == name:
            return
        listed = listed_order(watch, candidates)
        watch.send("j" if listed.index(name) > listed.index(current) else "k")
        # One key at a time: wait until the move is drawn before the next.
        watch.until(f"selection to move from {current}",
                    lambda: selected_name(watch, candidates) != current, timeout=10)
    raise AssertionError(f"could not select {name}\n" + watch.screen())


def listed_order(watch, candidates):
    """Names in display order (only those visible)."""
    rows = [text for text, _ in watch.table()] or [first for first, _ in watch.cards()]
    return [name for text in rows for name in candidates if is_row_of(text, name)]


def confirm(watch, question):
    """Answer a confirmation with its non-Cancel choice."""
    watch.shows(question)
    for digit in "123":
        watch.send(digit)
        watch.settle(0.3)
        chosen = watch.selected() or ""
        if "Cancel" not in chosen:
            watch.send("\r")
            return
    raise AssertionError("no choice besides Cancel\n" + watch.screen())


# --------------------------------------------------------------- the project

class Project:
    def __init__(self, binary, base, checks=None):
        self.binary = binary
        self.base = Path(os.path.realpath(base))
        self.root = self.base / ROOT
        self.state = self.base / "state"
        self.checks = checks
        self.ids = {}
        self.workspaces = {}
        self.sessions = 0
        self.watching = False
        self.root.mkdir(parents=True)
        (self.root / "auth.py").write_text(AUTH)
        (self.root / "api.py").write_text(API)
        (self.root / "util.py").write_text(UTIL)
        config = "coherence:\n  poll_secs: 1\n"
        if checks:
            config += "checks:\n  verify: " + json.dumps(checks) + "\n"
        (self.root / "dispatch.yml").write_text(config)
        self.git("init", "--quiet")
        self.git("add", "-A")
        self.git("commit", "--quiet", "-m", "initial")

    def git(self, *args, cwd=None):
        environment = {k: v for k, v in os.environ.items() if k not in ("GIT_DIR", "GIT_WORK_TREE")}
        subprocess.run(["git", "-C", str(cwd or self.root), "-c", "user.name=Test",
                        "-c", "user.email=test@example.invalid", *args],
                       check=True, env=environment, capture_output=True)

    def dispatch(self, *args, ok=True):
        result = subprocess.run([self.binary, "--state-dir", str(self.state), *args],
                                cwd=self.root, capture_output=True, text=True, timeout=120,
                                stdin=subprocess.DEVNULL)
        if ok:
            assert result.returncode == 0, (args, result.stdout, result.stderr)
        return result

    def unsafe(self):
        return ["--allow-unsafe-local"] if self.checks else []

    def attach(self, name, parent="wt", task=None, extra=()):
        workspace = self.base / parent / name
        workspace.parent.mkdir(parents=True, exist_ok=True)
        self.git("worktree", "add", "--quiet", "-b", f"work-{len(self.ids)}", str(workspace))
        args = ["attach", "--workspace", str(workspace), "--agent", "fake", *self.unsafe(), *extra]
        if task:
            args += ["--task", task]
        out = self.dispatch(*args).stdout
        run_id = next(line.split()[1] for line in out.splitlines() if line.startswith("ATTACHED "))
        key = name if name not in self.ids else f"{parent}/{name}"
        self.ids[key] = run_id
        self.workspaces[key] = Path(os.path.realpath(workspace))
        return run_id

    def hook(self, event):
        result = subprocess.run([self.binary, "--state-dir", str(self.state), "hook", "claude"],
                                input=json.dumps(event), capture_output=True, text=True, timeout=60)
        assert result.returncode == 0, result.stderr
        return result.stdout

    def claude(self, name):
        """A Claude Code session starting in its own worktree, as `claude --worktree <name>`."""
        assert self.watching, "the hook registers Work only in a watched project"
        relative = f".claude/worktrees/{name}"
        self.git("worktree", "add", "--quiet", "-b", f"worktree-{name}", relative)
        workspace = Path(os.path.realpath(self.root / relative))
        reply = self.hook({"hook_event_name": "SessionStart", "session_id": name, "source": "startup",
                           "cwd": str(workspace), "transcript_path": "/dev/null", "model": "claude-sonnet-5"})
        assert "tracking this worktree" in reply, reply
        for run in self.runs():
            if (run.get("attachment") or {}).get("workspace") == str(workspace):
                self.ids[name] = run["id"]
                self.workspaces[name] = workspace
                return run["id"]
        raise AssertionError(f"no Work for {workspace}")

    def end_session(self, name):
        self.hook({"hook_event_name": "SessionEnd", "session_id": name, "reason": "clear",
                   "cwd": str(self.workspaces[name])})

    def remove_worktree(self, name):
        reply = self.hook({"hook_event_name": "WorktreeRemove", "session_id": name, "cwd": str(self.root),
                           "worktree_path": str(self.workspaces[name]), "name": name})
        assert "kept this worktree's changes" in reply, reply
        self.git("worktree", "remove", "--force", str(self.workspaces[name]))

    def edit(self, name, path, text):
        (self.workspaces[name] / path).write_text(text)

    def finish(self, name):
        return self.dispatch("finish", self.ids[name], *self.unsafe())

    def commit(self, files, message="moves on"):
        for path, text in files.items():
            (self.root / path).write_text(text)
        self.git("add", "-A")  # with whatever accept applied, as a person commits
        self.git("commit", "--quiet", "-m", message)

    def start(self):
        self.dispatch("start")
        self.watching = True

    def stop(self):
        self.dispatch("stop", ok=False)
        self.watching = False

    def run(self, name):
        return json.loads((self.state / "runs" / self.ids.get(name, name) / "metadata.json").read_text())

    def runs(self):
        runs = []
        for path in sorted((self.state / "runs").glob("*/metadata.json")):
            try:
                runs.append(json.loads(path.read_text()))
            except (OSError, ValueError):
                pass
        return runs

    def short(self, name):
        """The shortest unambiguous ID over the runs (at least 8, as state::short_ids)."""
        ids = [run["id"] for run in self.runs()]
        run_id = self.ids[name]
        size = 8
        while any(other != run_id and other[:size] == run_id[:size] for other in ids):
            size += 1
        return run_id[:size]

    def wait_run(self, name, what, check, timeout=WAIT):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                if check(self.run(name)):
                    return self.run(name)
            except (OSError, ValueError, KeyError, TypeError):
                pass
            time.sleep(0.2)
        raise AssertionError(f"{name}: {what}: " + json.dumps(self.run(name).get("outcome")) + " "
                             + json.dumps((self.run(name).get("coherence") or {}).get("validity")))

    def decision(self, name):
        validity = (self.run(name).get("coherence") or {}).get("validity") or {}
        return validity.get("decision"), validity.get("world_changed")

    def age_out(self, name):
        """Finished more than an hour ago: the view's filter drops it, as it
        would an hour from now. The committed projection and its file agree."""
        run = self.run(name)
        assert run["outcome"]["lifecycle"] == "finished", run["outcome"]
        run["completed_at"] = (datetime.now(timezone.utc) - timedelta(hours=2)).isoformat().replace("+00:00", "Z")
        text = json.dumps(run)
        with sqlite3.connect(self.state / "dispatch.db") as db:
            db.execute("UPDATE runs SET run_projection_json=? WHERE id=?", (text, run["id"]))
        (self.state / "runs" / run["id"] / "metadata.json").write_text(text)

    def watch(self, width, height=30, flags=(), env=None):
        return Watch(self, width, height, flags, env)


def applied(project, name):
    return project.run(name)["outcome"]["application"] == "applied"


def review(project, name):
    return project.run(name)["outcome"]["review"]


# ------------------------------------------------------------------ fixtures

def board(project):
    """The herdr demo's shape: a stale result, a landed one, live work, and a
    fresh ready one. Returns the names in their expected display order."""
    project.attach("me-endpoint")
    project.edit("me-endpoint", "api.py", CALLER)
    project.finish("me-endpoint")
    project.attach("fresh")
    project.attach("auth-ctx")
    project.edit("auth-ctx", "auth.py", AUTH_MOVED)
    project.finish("auth-ctx")
    project.dispatch("accept", project.ids["auth-ctx"])
    project.attach("slugify")
    project.edit("slugify", "util.py", UTIL_EDIT)
    project.edit("fresh", "fresh.py", "VALUE = 1\n")
    project.finish("fresh")
    project.start()
    project.wait_run("me-endpoint", "REFRESH after auth-ctx landed",
                     lambda run: run["coherence"]["validity"]["decision"] == "refresh")
    project.wait_run("fresh", "checked", lambda run: run["coherence"]["validity"]["decision"] == "continue")
    return {
        "me-endpoint": ("blocked", "REFRESH"),
        "fresh": ("ready", "CONTINUE"),
        "slugify": ("working", "CONTINUE"),
        "auth-ctx": ("applied", "–"),
    }


def check_row(watch, name, state, verdict, wide):
    first, second = row_of(watch, name)
    if wide:
        at, titles = watch.column_titles()
        cell_line = second
        name_at = cell_line.index(name)
        assert name_at == titles["WORK"], (name, cell_line, titles)
        assert cell_line[titles["STATE"]:].startswith(state + " "), (name, state, cell_line)
        assert cell_line[titles["VERDICT"]:].startswith(verdict), (name, verdict, cell_line)
        assert cell_line[titles["VERDICT"]:titles["CHECKS"]].strip() == verdict, (name, verdict, cell_line)
    else:
        assert first.endswith(" " + verdict), ("verdict right-aligned, never truncated", name, verdict, first)
        # Line 2: 4 spaces (after the view's one-column margin), state, checks.
        dot = "-" if watch.ascii else "·"
        assert re.match(r"^ ?    " + re.escape(f"{state} {dot} checks "), second), (name, state, second)


# ----------------------------------------------------------------- scenarios

def widths(binary, base):
    """60/80/120/200 columns: every Work's name, state and verdict, one line each."""
    project = Project(binary, base)
    expected = board(project)
    order = list(expected)
    for width in (60, 80, 120, 200):
        with project.watch(width) as watch:
            watch.until("every Work listed", lambda: all(row_of(watch, name) for name in order))
            watch.settle()
            wide = width >= 90
            assert (watch.column_titles()[0] is not None) == wide, ("layout A switches at 90", width, watch.screen())
            for name, (state, verdict) in expected.items():
                check_row(watch, name, state, verdict, wide)
            # Order (§3.4): needs you, in progress, done; no group headings.
            assert listed_order(watch, order) == order, (listed_order(watch, order), watch.screen())
            assert watch.term.wraps == 0, ("a row wrapped", width, watch.screen())
            assert all(len(line) <= width for line in watch.lines())
            # The first draw selects the first item: the stale one.
            assert selected_name(watch, order) == "me-endpoint", watch.screen()
            details = watch.details()
            assert "Verdict REFRESH · " in details or "Verdict REFRESH" in details, details
            assert "def validate(token)" in details, ("the reason is visible", width, watch.screen())
            assert "Next It can't be accepted as is. r reject it, then run the agent again from the current source." \
                in details, (width, watch.screen())
            hint = watch.hint()
            assert hint.startswith("r reject · d review"), (width, hint)
            assert hint.endswith("↑↓ select · q leave" if wide else "↑↓ · q leave"), (width, hint)
            assert "Shift+Tab" not in watch.screen() and "auto-apply" not in watch.screen().lower(), watch.screen()
            header = watch.lines()[0]
            assert header.lstrip().startswith(ROOT + " · watched"), header
            if wide:
                assert "watched in the background since" in header and "(pid" not in header, header
                assert header.rstrip().endswith("2 need you · 1 in progress · 1 done"), header
            watch.leave()
    print("PASS widths")


def reorder(binary, base):
    """Inserted, removed and regrouped Work while watching: the selection stays on its Work."""
    project = Project(binary, base)
    for name in ("alpha", "bravo", "charlie"):
        project.attach(name)
        project.edit(name, f"{name}.py", f"NAME = {name!r}\n")
    project.finish("alpha")
    project.dispatch("reject", project.ids["alpha"])
    project.start()
    names = ["alpha", "bravo", "charlie", "delta", "echo"]
    with project.watch(120) as watch:
        watch.until("listed", lambda: listed_order(watch, names) == ["bravo", "charlie", "alpha"])
        select(watch, "charlie", names)
        # Inserted: delta is attached (in progress, after charlie), echo finishes
        # into "needs you", above the selection.
        project.attach("delta")
        watch.until("delta inserted", lambda: "delta" in listed_order(watch, names))
        assert selected_name(watch, names) == "charlie", watch.screen()
        project.attach("echo")
        project.edit("echo", "echo.py", "E = 1\n")
        project.finish("echo")
        watch.until("echo inserted first", lambda: listed_order(watch, names)[0] == "echo")
        assert selected_name(watch, names) == "charlie", ("insertion above", watch.screen())
        # Regrouped: charlie itself finishes and moves up to "needs you".
        project.finish("charlie")
        watch.until("charlie regrouped", lambda: listed_order(watch, names)[:2] == ["charlie", "echo"])
        assert selected_name(watch, names) == "charlie", ("state change moved it", watch.screen())
        # Removed above and below: alpha ages out (below), echo is rejected and
        # moves to done (a group change of other Work).
        project.dispatch("reject", project.ids["echo"])
        watch.until("echo regrouped", lambda: listed_order(watch, names)[-1] in ("echo", "alpha"))
        assert selected_name(watch, names) == "charlie", watch.screen()
        project.age_out("alpha")
        watch.until("alpha gone", lambda: "alpha" not in listed_order(watch, names))
        assert selected_name(watch, names) == "charlie", ("removal of other Work", watch.screen())
        assert "left the list" not in watch.joined(), ("only the selection leaving notices", watch.screen())
        watch.leave()
    print("PASS reorder")


def actions(binary, base):
    """Right before accept and reject, the action acts on the Work shown
    selected; when the selected Work leaves, the next action key only notices."""
    project = Project(binary, base)
    names = ["xray", "yankee", "zulu", "papa", "quebec"]
    for name in names:
        project.attach(name)
        project.edit(name, f"{name}.py", f"NAME = {name!r}\n")
        project.finish(name)
    project.start()
    with project.watch(120) as watch:
        watch.until("listed", lambda: listed_order(watch, names) == names)
        select(watch, "yankee", names)
        # Work above the selection leaves "needs you" just before the key.
        project.dispatch("reject", project.ids["xray"])
        watch.until("xray moved down", lambda: listed_order(watch, names)[0] == "yankee")
        assert selected_name(watch, names) == "yankee", watch.screen()
        watch.send("r")
        confirm_text = f"Reject yankee (Work {project.short('yankee')})? Nothing is applied, and a workspace Dispatch made is kept."
        watch.shows(confirm_text)
        watch.send("\r")  # Cancel is the default
        watch.shows("yankee: nothing changed.")
        assert review(project, "yankee") == "pending"
        watch.send("a")
        watch.shows("yankee: accepted and applied.")
        assert applied(project, "yankee"), project.run("yankee")["outcome"]
        assert not any(applied(project, other) for other in ("zulu", "papa", "quebec"))
        # Done group is now [xray, yankee] (by creation). Select xray, then it
        # leaves: the selection goes to the item now at its position.
        watch.until("regrouped", lambda: listed_order(watch, names) == ["zulu", "papa", "quebec", "xray", "yankee"])
        select(watch, "quebec", names)
        project.dispatch("reject", project.ids["quebec"])
        watch.until("quebec done", lambda: listed_order(watch, names) == ["zulu", "papa", "xray", "yankee", "quebec"])
        assert selected_name(watch, names) == "quebec", watch.screen()
        project.age_out("quebec")
        watch.shows("quebec left the list; now on yankee.")
        assert selected_name(watch, names) == "yankee", watch.screen()
        # Select papa (ready): age-out cannot remove ready Work, so select xray
        # (rejected, position 3) and make it leave: the item now at position 3
        # is yankee (applied).
        select(watch, "xray", names)
        project.age_out("xray")
        watch.shows("xray left the list; now on yankee.")
        # The next action key only says where the selection went.
        watch.send("r")
        watch.shows("Selection moved to yankee. Press r again to act on it.")
        assert "Reject yankee" not in watch.joined()
        # Now a ready Work at the end leaves its position to the previous one.
        select(watch, "yankee", names)
        project.age_out("yankee")
        watch.shows("yankee left the list; now on papa.")
        watch.send("a")
        watch.shows("Selection moved to papa. Press a again to act on it.")
        watch.settle(1)
        assert not applied(project, "papa"), "the first key after the move acted"
        watch.send("a")
        watch.shows("papa: accepted and applied.")
        assert applied(project, "papa") and not applied(project, "zulu")
        watch.leave()
    print("PASS actions")


def resize(binary, base):
    """Resizing while a Work is selected keeps that Work selected."""
    project = Project(binary, base)
    names = ["one", "two", "three"]
    for name in names:
        project.attach(name)
        project.edit(name, f"{name}.py", "X = 1\n")
        project.finish(name)
    project.start()
    with project.watch(120) as watch:
        watch.until("listed", lambda: listed_order(watch, names) == names)
        select(watch, "two", names)
        # A frame is drawn top to bottom, ending with the hint: wait for the
        # whole frame before the next resize, so none of it lands at the new
        # size.
        two = f"Work {project.short('two')}"
        watch.until("two's details", lambda: two in watch.details()
                    and watch.hint() == "a accept · d review · r reject · ↑↓ select · q leave")

        def drawn(width, selected):
            hint = "a accept · d review · r reject · " + ("↑↓ select" if width >= 90 else "↑↓") + " · q leave"
            watch.until(f"redrawn at {width} on {selected}", lambda: listed_order(watch, names) == names
                        and (watch.column_titles()[0] is not None) == (width >= 90)
                        and selected_name(watch, names) == selected and watch.hint() == hint)

        for width in (60, 200, 80, 90, 89, 120):
            watch.resize(width, 30)
            drawn(width, "two")
            assert watch.term.wraps == 0, (width, watch.screen())
        # Below 40 columns: only a request to widen, and q; keys still work.
        watch.resize(39, 30)
        watch.until("only a request to widen", lambda: "Widen the terminal to at least 40 columns."
                    in watch.joined() and watch.hint() == "q leave")
        watch.send("j")
        watch.resize(60, 30)
        # j below 40 columns moved the selection.
        drawn(60, "three")
        watch.send("k")
        watch.until("back on two", lambda: selected_name(watch, names) == "two")
        watch.send("r")
        watch.shows(f"Reject two (Work {project.short('two')})?")
        watch.send("\x1b")
        watch.shows("two: nothing changed.")
        assert review(project, "two") == "pending"
        watch.leave()
    print("PASS resize")


def names(binary, base):
    """Long workspace names, duplicate folder names in different parents, and
    names that only collide once truncated; widths in cells, not bytes."""
    project = Project(binary, base)
    project.attach("feature", parent="one")
    project.attach("feature", parent="two")
    long_a = "a-really-long-workspace-name-for-the-alpha-variant"
    long_b = "a-really-long-workspace-name-for-the-bravo-variant"
    project.attach(long_a)
    project.attach(long_b)
    project.attach("認証コンテキスト")  # 8 characters, 16 cells
    project.attach("plain", task=None)
    project.start()
    for width in (200, 120, 80, 60):
        with project.watch(width) as watch:
            watch.until("drawn", lambda: watch.table() or watch.cards())
            watch.settle()
            entries = visit(watch, 6)
            assert len(entries) == 6, (width, entries)
            rows = [first for first, _ in entries]
            if width >= 90:
                _, titles = watch.column_titles()
                works = [line[titles["WORK"]:titles["AGENT"]].replace("\0", "").strip() for _, line in entries]
            else:
                works = [re.sub(r"\s+(CONTINUE|not checked)$", "", first.strip()) for first, _ in entries]
            # Duplicates: each holder gets " <short id>", at least 4 characters,
            # enough to tell the group apart; the ID part is never truncated.
            ids = list(project.ids.values())
            for key in ("feature", "two/feature"):
                run_id = project.ids[key]
                hits = []
                for work in works:
                    shown = re.fullmatch(r"feature(?:…)? ([0-9A-Z]+)", work)
                    if shown and run_id.startswith(shown.group(1)):
                        assert len(shown.group(1)) >= 4, (width, work)
                        hits.append(work)
                assert len(hits) == 1, (width, key, run_id, works)
            # The two long names are told apart however they are truncated.
            work_cells = [work for work in works if work.startswith("a-really")]
            assert len(work_cells) == 2, (width, works)
            assert work_cells[0] != work_cells[1], ("truncation made a duplicate", width, work_cells)
            for cell in work_cells:
                if cell.startswith(long_a) or cell.startswith(long_b):
                    continue
                assert "…" in cell, ("truncated with a single …", width, cell)
                assert cell.count("…") == 1, (width, cell)
            assert any("認証コンテキスト" in row for row in rows), (width, rows)
            assert watch.term.wraps == 0, (width, watch.screen())
            if width >= 90:
                _, titles = watch.column_titles()
                for _, cell_line in entries:
                    assert cell_line[titles["STATE"]:].startswith("working "), (width, "aligned in cells", cell_line)
            else:
                for first, second in entries:
                    assert first.endswith((" CONTINUE", " not checked")), (width, first)
                    assert re.match(r"^ ?    working · checks ", second), (width, second)
            watch.leave()
    print("PASS names")


def states_ready(binary, base):
    """CONTINUE (unmoved and moved), REFRESH and STOP (blocked), failed checks,
    applied and rejected: §3.7's wording, §3.8's Next and keys."""
    project = Project(binary, base, checks=["test ! -e FAIL"])
    project.attach("refresh-me")
    project.edit("refresh-me", "api.py", CALLER)
    project.attach("stop-me")
    project.edit("stop-me", "util.py", UTIL_EDIT)
    project.attach("fails")
    project.edit("fails", "notes.py", "NOTE = 1\n")
    project.edit("fails", "FAIL", "\n")
    project.attach("moved")
    project.edit("moved", "moved.py", "MOVED = 1\n")
    project.attach("landed")
    project.edit("landed", "landed.py", "LANDED = 1\n")
    project.attach("rejected")
    project.edit("rejected", "rejected.py", "REJECTED = 1\n")
    project.attach("auto", extra=["--auto-apply"])
    project.edit("auto", "auto.py", "AUTO = 1\n")
    for name in ("refresh-me", "stop-me", "fails", "moved", "landed", "rejected", "auto"):
        project.finish(name)
    project.dispatch("accept", project.ids["landed"])
    project.dispatch("reject", project.ids["rejected"])
    project.start()
    project.wait_run("auto", "auto-applied", lambda r: r["outcome"]["applied_by"] == "auto_apply"
                     and r["outcome"]["application"] == "applied")
    project.commit({"auth.py": AUTH_MOVED, "util.py": UTIL_EDIT})
    project.attach("unmoved")
    project.edit("unmoved", "unmoved.py", "UNMOVED = 1\n")
    project.finish("unmoved")
    project.wait_run("refresh-me", "REFRESH", lambda r: r["coherence"]["validity"]["decision"] == "refresh")
    project.wait_run("stop-me", "STOP", lambda r: r["coherence"]["validity"]["decision"] == "stop")
    project.wait_run("moved", "moved", lambda r: r["coherence"]["validity"]["world_changed"])
    project.wait_run("unmoved", "checked", lambda r: r["coherence"]["validity"]["decision"] == "continue")
    assert project.decision("unmoved") == ("continue", False), project.decision("unmoved")
    assert project.run("fails")["outcome"]["verification"] == "failed"
    order =["refresh-me", "stop-me", "fails", "moved", "unmoved", "landed", "rejected", "auto"]
    expect = {
        "auto": ("applied", "–", "passed", ["applied by auto-apply · it was CONTINUE"],
                 "Nothing to do: it is in tinyauth.", "↑↓ select · q leave"),
        "refresh-me": ("blocked", "REFRESH", "passed", ["Verdict REFRESH · "],
                       "It can't be accepted as is. r reject it, then run the agent again from the current source.",
                       "r reject · d review · ↑↓ select · q leave"),
        "stop-me": ("blocked", "STOP", "passed", ["Verdict STOP · its changes are already in the source"],
                    "Its changes are already in the source. r reject it.",
                    "r reject · ↑↓ select · q leave"),
        "fails": ("ready", "CONTINUE", "failed", ["Checks failed · d review the output"],
                  "Its checks failed. d review the output, then a accept or r reject.",
                  "d review · a accept · r reject · ↑↓ select · q leave"),
        "moved": ("ready", "CONTINUE", "passed",
                  ["Verdict CONTINUE · the source moved; nothing this Work relies on changed", "Checks passed"],
                  "a accept: runs your checks on the merged result, then applies it to tinyauth.",
                  "a accept · d review · r reject · ↑↓ select · q leave"),
        "unmoved": ("ready", "CONTINUE", "passed", ["Verdict CONTINUE · the source has not moved since it began"],
                    "a accept: applies it to tinyauth.",
                    "a accept · d review · r reject · ↑↓ select · q leave"),
        "landed": ("applied", "–", "passed", ["applied by you · it was CONTINUE (the source had not moved)"],
                   "Nothing to do: it is in tinyauth.", "↑↓ select · q leave"),
        "rejected": ("rejected", "–", "passed", ["Rejected · nothing was applied"],
                     "Nothing to do.", "↑↓ select · q leave"),
    }
    with project.watch(130, 30) as watch:
        watch.until("listed", lambda: listed_order(watch, order) == order)
        watch.settle()
        header = watch.lines()[0]
        assert header.rstrip().endswith("5 need you · 3 done"), header
        for name in order:
            select(watch, name, order)
            state, verdict, checks, detail_lines, next_text, hint = expect[name]
            watch.until(f"{name}'s details", lambda: watch.details().split(" ")[0] == name)
            check_row(watch, name, state, verdict, True)
            text, cell_line = row_of(watch, name)
            _, titles = watch.column_titles()
            assert cell_line[titles["CHECKS"]:titles["TOUCHES"]].strip() == checks, (name, cell_line)
            details = watch.details()
            for line in detail_lines:
                assert line in details, (name, line, watch.screen())
            assert "Next " + next_text in details, (name, next_text, watch.screen())
            assert f"Work {project.short(name)}" in details, (name, watch.screen())
            assert "attached workspace" in details, (name, watch.screen())
            # The word S0, as 0.4.10's rows said `S0 merge-base …`; a Work ID
            # such as 01M4EES0 may contain the letters.
            assert not re.search(r"\bS0\b", watch.screen()), ("S0 leaves the rows", watch.screen())
            assert watch.hint() == hint, (name, watch.hint(), hint)
            if name == "refresh-me":
                assert "Or: dispatch refresh" not in details, details
        watch.leave()
    print("PASS states-ready")


def states_live(binary, base):
    """Work still in progress or gone: working (attached and a Claude session),
    idle, removed and lost."""
    project = Project(binary, base, checks=["true"])
    project.start()
    project.attach("slugify")
    # Still working, and already stale: the mid-run verdict is advisory.
    project.attach("me-live")
    project.edit("me-live", "api.py", CALLER)
    time.sleep(2.5)  # its footprint is seen before the source moves
    project.commit({"auth.py": AUTH_MOVED})
    project.wait_run("me-live", "advisory REFRESH", lambda r: r["coherence"]["validity"]["decision"] == "refresh")
    project.claude("auth-ctx")
    project.claude("idle-one")
    project.end_session("idle-one")
    project.claude("gone-exact")
    project.edit("gone-exact", "util.py", UTIL_EDIT)
    project.remove_worktree("gone-exact")
    project.attach("gone-lost")
    project.edit("gone-lost", "lost.py", "LOST = 1\n")
    time.sleep(2.5)  # seen by the watcher before it vanishes
    shutil.rmtree(project.workspaces["gone-lost"])
    project.wait_run("gone-lost", "lost",
                     lambda r: (r["attachment"].get("workspace_removed") or {}).get("exact") is False)
    order = ["gone-exact", "gone-lost", "slugify", "me-live", "auth-ctx", "idle-one"]
    finish_next = "When the agent is done, f finish: freezes its changes and runs true."
    expect = {
        "me-live": ("working", finish_next, "f finish · ↑↓ select · q leave", "attached workspace"),
        "gone-exact": ("removed", "Its worktree was removed and its exact changes were kept. f finish to check them.",
                       "f finish · ↑↓ select · q leave", "claude session in its own worktree"),
        "gone-lost": ("lost", "Its worktree is gone; only its last-seen changes were kept. f finish to freeze them, then review.",
                      "f finish · ↑↓ select · q leave", "attached workspace"),
        "slugify": ("working", finish_next, "f finish · ↑↓ select · q leave", "attached workspace"),
        "auth-ctx": ("working", finish_next, "f finish · ↑↓ select · q leave", "claude session in its own worktree"),
        "idle-one": ("idle", "No session is open. f finish to freeze its changes, or resume the session in its worktree.",
                     "f finish · ↑↓ select · q leave", "claude session in its own worktree"),
    }
    with project.watch(130, 30) as watch:
        watch.until("listed", lambda: listed_order(watch, order) == order)
        watch.settle()
        for name in order:
            select(watch, name, order)
            state, next_text, hint, origin = expect[name]
            watch.until(f"{name}'s details", lambda: watch.details().split(" ")[0] == name)
            text, cell_line = row_of(watch, name)
            _, titles = watch.column_titles()
            assert cell_line[titles["STATE"]:].startswith(state + " "), (name, cell_line)
            details = watch.details()
            assert origin in details, (name, origin, details)
            assert "Next " + next_text in details, (name, next_text, watch.screen())
            assert watch.hint() == hint, (name, watch.hint())
            if name == "auth-ctx":
                assert ".claude/worktrees/auth-ctx" in details, details
                began = re.search(r"Began a Dispatch snapshot \(([0-9a-f]{8})\) of its worktree when its first "
                                  r"session started, not a commit on any branch", details)
                assert began, ("the snapshot is called one", details)
            if name in ("slugify", "auth-ctx", "idle-one"):
                assert "discovered" not in details
            if name == "me-live":
                assert cell_line[titles["VERDICT"]:titles["CHECKS"]].strip() == "REFRESH", cell_line
                assert "Verdict REFRESH · " in details, details
                assert "Advisory while the agent works: nothing is stopped." in details, details
        watch.leave()
    print("PASS states-live")


def interactions(binary, base):
    """Interactions name both pieces of Work, in both directions."""
    project = Project(binary, base)
    project.attach("auth-ctx")
    project.attach("me-endpoint")
    project.attach("slugify")
    project.edit("auth-ctx", "auth.py", AUTH_MOVED)
    project.edit("me-endpoint", "api.py", CALLER)
    project.edit("slugify", "util.py", UTIL_EDIT)
    project.start()
    deadline = time.monotonic() + WAIT
    while True:
        files = list((project.state / "watchers").glob("*.interactions.json"))
        edges = json.loads(files[0].read_text()).get("edges", []) if files else []
        if edges:
            break
        assert time.monotonic() < deadline, "no interaction"
        time.sleep(0.2)
    order = ["auth-ctx", "me-endpoint", "slugify"]
    sentence = "auth-ctx changes the signature of validate (auth.py), which me-endpoint uses"
    with project.watch(160, 30) as watch:
        watch.until("touches", lambda: row_of(watch, "auth-ctx")[1].rstrip().endswith("me-endpoint"))
        _, titles = watch.column_titles()
        assert row_of(watch, "me-endpoint")[1][titles["TOUCHES"]:].strip() == "auth-ctx", watch.screen()
        assert row_of(watch, "slugify")[1][titles["TOUCHES"]:].strip() == "–", watch.screen()
        for name in ("auth-ctx", "me-endpoint"):
            select(watch, name, order)
            watch.until(f"{name}'s touches", lambda: sentence in watch.details())
            details = watch.details()
            assert "Advisory: nothing is held back." in details, details
            touches = details[details.index("Touches"):details.index("Advisory")]
            assert not re.search(r"\b(it|this Work|its)\b", touches), ("no pronoun for Work", touches)
        select(watch, "slugify", order)
        watch.until("slugify's details", lambda: watch.details().split(" ")[0] == "slugify")
        assert "Touches" not in watch.details(), watch.details()
        watch.leave()
    with project.watch(70, 30) as watch:
        watch.until("cards", lambda: watch.cards())
        _, second = row_of(watch, "auth-ctx")
        assert re.search(r" · touches (me-endpoint|1)$", second), second
        watch.leave()
    print("PASS interactions")


def feedback(binary, base):
    """Success and refusal notices name the Work; refusals come from the
    stored verdict; notices age out."""
    project = Project(binary, base, checks=["true"])
    project.attach("stale")
    project.edit("stale", "api.py", CALLER)
    project.attach("dup")
    project.edit("dup", "util.py", UTIL_EDIT)
    project.attach("good")
    project.edit("good", "good.py", "GOOD = 1\n")
    for name in ("stale", "dup", "good"):
        project.finish(name)
    project.commit({"auth.py": AUTH_MOVED, "util.py": UTIL_EDIT})
    project.start()
    # A Claude session's Work has no --allow-unsafe-local (attach --workspace
    # requires it once checks exist): finishing it must ask, as in 0.4.10.
    project.claude("live")
    project.edit("live", "live.py", "LIVE = 1\n")
    project.wait_run("stale", "REFRESH", lambda r: r["coherence"]["validity"]["decision"] == "refresh")
    project.wait_run("dup", "STOP", lambda r: r["coherence"]["validity"]["decision"] == "stop")
    order = ["stale", "dup", "good", "live"]
    source_before = (project.root / "api.py").read_text()
    with project.watch(120) as watch:
        watch.until("listed", lambda: listed_order(watch, order) == order)
        select(watch, "stale", order)
        watch.send("a")
        watch.shows("stale: not applied: stale (REFRESH). The source is unchanged. Next: r reject it.")
        assert not applied(project, "stale") and (project.root / "api.py").read_text() == source_before
        select(watch, "dup", order)
        watch.send("a")
        watch.shows("dup: not applied: STOP, its changes are already in the source. Next: r reject it.")
        assert not applied(project, "dup")
        select(watch, "live", order)
        watch.send("d")
        watch.shows("live: only a result waiting for review can be reviewed.")
        select(watch, "good", order)
        watch.send("f")
        watch.shows("good: only attached Work still in progress can be finished.")
        # The notice ages out after 10 seconds.
        watch.until("notice aged out", lambda: "only attached Work" not in watch.joined(), timeout=14)
        select(watch, "live", order)
        watch.send("f")
        watch.shows(f"Finish live (Work {project.short('live')})? Finishing runs this project's checks on your "
                    "machine, with your permissions:")
        watch.shows("true")
        watch.send("\r")  # Cancel is the default
        watch.shows("live: nothing changed.")
        assert project.run("live")["outcome"]["lifecycle"] != "finished"
        watch.send("f")
        confirm(watch, f"Finish live (Work {project.short('live')})?")
        watch.shows("live: finished; checks passed. It now waits for your review.")
        assert project.run("live")["outcome"]["verification"] == "passed"
        select(watch, "good", order)
        watch.send("a")
        watch.shows("good: accepted and applied.")
        assert applied(project, "good")
        select(watch, "dup", order)
        watch.send("r")
        confirm(watch, f"Reject dup (Work {project.short('dup')})?")
        watch.shows("dup: rejected; nothing was applied.")
        assert review(project, "dup") == "rejected" and not applied(project, "dup")
        watch.leave()
    print("PASS feedback")


def no_color(binary, base):
    """NO_COLOR, --no-color, DISPATCH_COLOR=none and TERM=dumb draw no color; bold stays."""
    project = Project(binary, base)
    board(project)
    cases = [({"NO_COLOR": "1"}, ()), ({}, ("--no-color",)), ({"DISPATCH_COLOR": "none"}, ()),
             ({"TERM": "dumb"}, ())]
    for env, flags in cases:
        with project.watch(120, flags=flags, env=env) as watch:
            watch.until("drawn", lambda: listed_order(watch, ["me-endpoint"]) == ["me-endpoint"])
            watch.settle()
            output = bytes(watch.output)
            assert not has_color(output), (env, flags, "color codes drawn")
            assert re.search(rb"\x1b\[(?:[0-9;]*;)?1(?:;[0-9;]*)?m", output), (env, flags, "bold stays")
            watch.leave()
    with project.watch(120, env={"DISPATCH_COLOR": "truecolor", "COLORTERM": "truecolor"}) as watch:
        watch.until("drawn", lambda: listed_order(watch, ["me-endpoint"]) == ["me-endpoint"])
        watch.settle()
        assert has_color(bytes(watch.output)), "the control: a color terminal gets color"
        watch.leave()
    print("PASS no-color")


def ascii_mode(binary, base):
    """--ascii draws nothing outside ASCII, at both layouts."""
    project = Project(binary, base)
    project.attach("feature", parent="one")
    project.attach("feature", parent="two")
    board(project)
    project.attach("a-really-long-workspace-name-for-the-alpha-variant")
    for width in (120, 60):
        with project.watch(width, flags=("--ascii",)) as watch:
            watch.until("drawn", lambda: listed_order(watch, ["me-endpoint"]) == ["me-endpoint"])
            watch.settle()
            output = bytes(watch.output)
            bad = sorted({chr(b) for b in output if b >= 0x80} | {c for c in output.decode("utf8", "replace") if ord(c) >= 0x80})
            assert not bad, (width, "non-ASCII drawn", bad, watch.screen())
            assert watch.selected().lstrip().startswith("> "), watch.screen()
            assert watch.rules(), ("rules drawn with -", watch.screen())
            assert watch.hint().endswith("up/down select - q leave" if width >= 90 else "up/down - q leave"), watch.hint()
            longs = [first for first, _ in visit(watch, 7) if "a-really-long" in first]
            assert len(longs) == 1 and "..." in longs[0] and "…" not in longs[0], (width, longs)
            assert all(b < 0x80 for b in bytes(watch.output)), (width, "non-ASCII drawn while scrolling")
            watch.leave()
    print("PASS ascii")


def refusal_other(binary, base):
    """§3.9: accept refused for a reason other than REFRESH or STOP gives the
    error's first sentence, naming the Work."""
    # A failing check on the merged result is a REFRESH reason, so the
    # "other" refusal is accepting Work that has no result yet.
    project = Project(binary, base)
    project.attach("unfinished")
    project.edit("unfinished", "feature.py", "FEATURE = 1\n")
    cli = project.dispatch("accept", project.ids["unfinished"], ok=False)
    assert cli.returncode != 0, cli.stdout
    error = " ".join(cli.stderr.strip().removeprefix("error: ").split())
    first = error.split(". ", 1)[0].rstrip(".")
    with project.watch(200) as watch:
        watch.until("drawn", lambda: watch.selected())
        watch.send("a")
        watch.shows(f"unfinished: not applied: {first}.")
        watch.settle(1)
        assert not applied(project, "unfinished") and project.run("unfinished")["outcome"]["lifecycle"] != "finished"
        watch.leave()
    print("PASS refusal-other")


def long_name_detail(binary, base):
    """§3.7: details line 1 ends with `Work <short id>`, however long the name."""
    project = Project(binary, base)
    name = "feature-" + "x" * 72  # an 80-character worktree folder
    project.attach(name)
    project.attach("other")
    with project.watch(90) as watch:
        watch.until("details", lambda: watch.details().startswith("feature-"))
        line_one = watch.lines()[watch.rules()[1] + 1]
        assert line_one.rstrip().endswith(f"Work {project.short(name)}"), (line_one, watch.screen())
        watch.leave()
    print("PASS long-name-detail")


def short_terminal(binary, base):
    """§3.4: the list never scrolls the selection off screen, even on a short
    terminal with a notice showing."""
    project = Project(binary, base)
    project.attach("me-endpoint")
    project.edit("me-endpoint", "api.py", CALLER)
    project.finish("me-endpoint")
    project.commit({"auth.py": AUTH_MOVED})
    for name in ("two", "three", "four"):
        project.attach(name)
    project.start()
    project.wait_run("me-endpoint", "REFRESH", lambda r: r["coherence"]["validity"]["decision"] == "refresh")
    order = ["me-endpoint", "two", "three", "four"]
    for width, height in ((60, 12), (100, 10)):
        with project.watch(width, height) as watch:
            watch.until("the selected Work on screen", lambda: watch.selected(), timeout=15)
            watch.send("a")  # refused: a notice of up to two lines, shown whole
            watch.shows("me-endpoint: not applied: stale (REFRESH). The source is unchanged. Next: r reject it.")
            watch.settle()
            assert watch.selected() and selected_name(watch, order) == "me-endpoint", \
                (width, height, "the selection is off screen", watch.screen())
            watch.leave()
    print("PASS short-terminal")


def narrowest_hint(binary, base):
    """§3.8: the hint is the offered keys, then `↑↓ · q leave`, at 40 columns too.
    The whole hint is 45 cells; what does not fit in 40 columns goes from the
    end of the offered keys, and `q leave` always stays (0.4.11 A2, fix 3)."""
    project = Project(binary, base)
    project.attach("ready-one")
    project.edit("ready-one", "ready.py", "READY = 1\n")
    project.finish("ready-one")
    with project.watch(60) as watch:
        watch.until("drawn", lambda: watch.cards())
        watch.settle()
        assert watch.hint() == "a accept · d review · r reject · ↑↓ · q leave", (watch.hint(), watch.screen())
        watch.leave()
    with project.watch(40) as watch:
        watch.until("drawn", lambda: watch.cards())
        watch.settle()
        assert watch.hint() == "a accept · d review · ↑↓ · q leave", (watch.hint(), watch.screen())
        watch.leave()
    print("PASS narrowest-hint")


def six_cards(binary, base):
    """Every Work is listed before the optional details: six Work at 60x30
    (a view at most 20 lines tall) show all six cards (0.4.11 A2, fix 4)."""
    project = Project(binary, base)
    names = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"]
    for name in names:
        project.attach(name)
    with project.watch(60, 30) as watch:
        watch.until("six cards", lambda: listed_order(watch, names) == names
                    and watch.hint().endswith("q leave"))
        assert len(watch.cards()) == 6, watch.screen()
        assert selected_name(watch, names) == "alpha", watch.screen()
        assert "Verdict not checked yet" in watch.details(), watch.screen()
        assert "Next When the agent is done, f finish: freezes its changes." in watch.details(), watch.screen()
        watch.leave()
    print("PASS six-cards")


def unwatched(binary, base):
    """Nothing watches the project: the header says so, ready Work is not
    checked yet, Shift+Tab does nothing, and Esc and Ctrl+C leave."""
    project = Project(binary, base)
    project.attach("pending")
    project.edit("pending", "pending.py", "PENDING = 1\n")
    project.finish("pending")
    project.attach("busy")
    assert project.decision("pending") == (None, None), project.run("pending").get("coherence")
    order = ["pending", "busy"]
    with project.watch(120) as watch:
        watch.until("listed", lambda: listed_order(watch, order) == order)
        header = watch.lines()[0]
        assert re.match(r"^ ?tinyauth · not watched · start watching: dispatch start(\s|$)", header), header
        check_row(watch, "pending", "ready", "not checked", True)
        # Unknown stays unknown: nothing computes interactions (0.4.11 A2, fix 5).
        _, titles = watch.column_titles()
        for name in order:
            assert row_of(watch, name)[1][titles["TOUCHES"]:].strip() == "?", (name, watch.screen())
        details = watch.details()
        assert "Touches not known: the project is not watched" in details, details
        assert "Verdict not checked yet" in details, details
        assert "Next a accept: Dispatch checks it against the source first." in details, details
        hint = watch.hint()
        assert hint == "a accept · d review · r reject · ↑↓ select · q leave", hint
        watch.send("\x1b[Z")  # Shift+Tab
        watch.settle(1)
        assert watch.hint() == hint and "auto-apply" not in watch.joined().lower(), watch.screen()
        watch.send("a")
        watch.shows("pending: accepted and applied.")
        assert applied(project, "pending")
        watch.send("\x1b")  # Esc leaves
        watch.finish()
    with project.watch(60) as watch:
        watch.until("cards", lambda: watch.cards())
        assert re.match(r"^ ?tinyauth · not watched(\s|$)", watch.lines()[0]), watch.lines()[0]
        assert "start watching" not in watch.lines()[0], watch.lines()[0]
        watch.send("\x03")  # Ctrl+C leaves
        watch.finish()
    print("PASS unwatched")


def empty(binary, base):
    """§3.11: the empty view, watched and not."""
    project = Project(binary, base)
    with project.watch(120) as watch:
        watch.shows("No Work in the last hour.")
        watch.shows("Nothing is watching this project: dispatch start.")
        assert "not watched" in watch.lines()[0], watch.lines()[0]
        watch.leave()
    project.start()
    with project.watch(120) as watch:
        watch.shows("No Work in the last hour.")
        watch.shows("A Claude Code session in its own worktree of this project, or dispatch attach -- <agent>, appears here.")
        watch.leave()
    print("PASS empty")


# ------------------------------------------------- plain, piped and JSON: 0.4.10

def expected_line(project, name, shorts):
    """One row exactly as 0.4.10's `serve::row` prints it for these fixtures:
    `<short id> · <WorkLine>`, the WorkLine as its Display writes it."""
    run = project.run(name)
    attachment = run["attachment"]
    provenance = attachment["provenance"]
    if "git_merge_base" in provenance:
        s0 = f"merge-base {provenance['git_merge_base']['commit'][:8]} (full)"
    else:
        raise AssertionError(provenance)
    validity = (run.get("coherence") or {}).get("validity")
    outcome = run["outcome"]
    if outcome["application"] == "applied" and not validity:
        verdict, reason = "unmoved", None
    elif not validity:
        verdict, reason = "not checked", None
    elif validity["decision"] == "continue" and not validity["world_changed"]:
        verdict, reason = "unmoved", None
    else:
        verdict = validity["decision"].upper()
        reasons = validity.get("reasons") or []
        reason = " ".join(reasons[0]["detail"].split()) if reasons else None
    if outcome["lifecycle"] != "finished":
        state = "working"
    elif outcome["application"] == "applied":
        state = "applied"
    elif outcome["review"] == "rejected":
        state = "rejected"
    elif validity and validity["decision"] != "continue":
        state = "blocked"
    else:
        state = "ready"
    verification = {"not_configured": "no checks", "not_run": "checks not run", "passed": "checks passed",
                    "failed": "checks failed"}[outcome["verification"]]
    review_state = {"accepted": "accepted", "rejected": "rejected"}.get(outcome["review"])
    if review_state is None and outcome.get("work_result") == "ready":
        review_state = "pending"
    line = f"{shorts[name]} · attached fake · S0 {s0} · {verdict}"
    if reason:
        line += f": {reason}"
    line += f" · {state} · {verification}"
    if review_state:
        line += f" · review {review_state}"
    return line, {
        "type": "work", "run_id": run["id"], "agent": "fake",
        "verdict": validity["decision"] if validity else None,
        "state": state, "reason": reason or "—", "origin": "attached", "s0": s0,
        "verification": verification, "review": review_state,
        "applied_by": "human" if state == "applied" else None, "overridden": False,
    }


HEADER = re.compile(r"^(?P<root>/.+) · watched in the background since \d\d:\d\d \(pid \d+\)$")


def plain(binary, base):
    """Piped `watch`, `watch --plain` on a terminal and `watch --json` print
    what 0.4.10 printed."""
    project = Project(binary, base)
    project.attach("me-endpoint")
    project.edit("me-endpoint", "api.py", CALLER)
    project.finish("me-endpoint")
    project.attach("auth-ctx")
    project.edit("auth-ctx", "auth.py", AUTH_MOVED)
    project.finish("auth-ctx")
    project.dispatch("accept", project.ids["auth-ctx"])
    project.attach("slugify")
    project.start()
    project.wait_run("me-endpoint", "REFRESH", lambda r: r["coherence"]["validity"]["decision"] == "refresh")
    time.sleep(2)  # let the owner settle so every record is stable
    names = sorted(project.ids, key=lambda name: project.ids[name])  # view_rows sorts by ID
    ids = [project.ids[name] for name in names]
    shorts = {}
    for name in names:
        size = 8
        while any(other != project.ids[name] and other[:size] == project.ids[name][:size] for other in ids):
            size += 1
        shorts[name] = project.ids[name][:size]
    expected = [expected_line(project, name, shorts) for name in names]

    def piped(json_mode):
        args = [binary, "--state-dir", str(project.state), "watch"] + (["--json"] if json_mode else [])
        child = subprocess.Popen(args, cwd=project.root, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                 stdin=subprocess.DEVNULL, text=True)
        lines = []
        try:
            deadline = time.monotonic() + 20
            while len(lines) < 1 + len(names) and time.monotonic() < deadline:
                line = child.stdout.readline()
                if not line:
                    break
                lines.append(line.rstrip("\n"))
        finally:
            child.kill()
            child.wait()
        return lines

    lines = piped(False)
    header = HEADER.match(lines[0])
    assert header and header.group("root") == str(project.root), lines[0]
    assert lines[1:] == [line for line, _ in expected], "\n".join(["got:"] + lines[1:] + ["want:"] + [l for l, _ in expected])
    records = piped(True)
    watcher = json.loads(records[0])
    assert list(watcher) == ["type", "watcher"] and watcher["type"] == "watcher", records[0]
    assert HEADER.match(watcher["watcher"]), watcher
    for raw, (_, want) in zip(records[1:], expected):
        record = json.loads(raw)
        keys = sorted(want) + ["interactions"]
        assert list(record) == sorted(keys), ("0.4.10 key order (sorted)", list(record))
        assert record["interactions"] in (None, []) or isinstance(record["interactions"], list), record
        record.pop("interactions")
        assert record == want, (record, want)
    # `--plain` on a terminal is the same report, not the interactive view.
    session = Session([binary, "--state-dir", str(project.state), "--plain", "watch"], project.root,
                      project.base / "captures", "plain-tty", env={"NO_COLOR": None}, width=200)
    with session:
        for line, _ in expected:
            session.wait(line)
        assert "WORK" not in session.clean and "↑↓" not in session.clean
        session.send(b"\x03")
        session.finish()
    print("PASS plain")


SCENARIOS = {
    "widths": widths, "reorder": reorder, "actions": actions, "resize": resize, "names": names,
    "states-ready": states_ready, "states-live": states_live, "interactions": interactions,
    "feedback": feedback, "no-color": no_color, "ascii": ascii_mode, "empty": empty, "plain": plain,
    "unwatched": unwatched, "long-name-detail": long_name_detail, "short-terminal": short_terminal,
    "narrowest-hint": narrowest_hint, "refusal-other": refusal_other, "six-cards": six_cards,
}


def main():
    binary, scenario = sys.argv[1], sys.argv[2]
    base = Path(tempfile.mkdtemp(prefix=f"watch-acceptance-{scenario}-"))
    project_holder = []
    original = Project.__init__

    def tracked(self, *args, **kwargs):
        project_holder.append(self)
        original(self, *args, **kwargs)

    Project.__init__ = tracked
    try:
        SCENARIOS[scenario](binary, base)
    except BaseException:
        print("captures kept in", base / "captures", file=sys.stderr)
        raise
    finally:
        for project in project_holder:
            project.stop()
    shutil.rmtree(base, ignore_errors=True)


if __name__ == "__main__":
    main()
