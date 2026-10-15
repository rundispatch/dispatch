"""Real PTY interaction/capture support for deterministic review fixtures.

The .cast files record actual terminal bytes (asciinema v2), not screenshots or
proof of graphical contrast. No provider or desktop application is launched.
"""
import codecs
import fcntl
import json
import os
import pty
import re
import select
import signal
import struct
import subprocess
import sqlite3
import sys
import termios
import time
from pathlib import Path



# Longest wait for any expected screen or exit. Waits return as soon as the
# condition holds, so a generous limit only matters on a slow machine; set
# DISPATCH_TEST_WAIT_SECS to change it.
WAIT = float(os.environ.get("DISPATCH_TEST_WAIT_SECS", "300"))

def has_color(output):
    for parameters in re.findall(rb"\x1b\[([0-9;]*)m", output):
        codes = [int(value) for value in parameters.split(b";") if value]
        if any(30 <= code <= 38 or 40 <= code <= 48 or 90 <= code <= 97 or 100 <= code <= 107 for code in codes):
            return True
    return False


class Session:
    def __init__(self, args, source, captures, name, env=None, width=100, height=30):
        self.master, self.slave = pty.openpty()
        self.before = termios.tcgetattr(self.slave)
        self.width, self.height = width, height
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
        environment = dict(os.environ, TERM="xterm-256color")
        if env:
            for key, value in env.items():
                if value is None: environment.pop(key, None)
                else: environment[key] = value
        self.started = time.monotonic()
        self.output = bytearray()
        self.clean = ""
        self.position = 0
        self.markers = {}
        self.cleanup_pids = []
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")
        captures = Path(captures)
        captures.mkdir(parents=True, exist_ok=True)
        self.capture_path = captures / name
        self.cast = self.capture_path.with_suffix(".cast").open("w")
        self.cast.write(json.dumps({"version": 2, "width": width, "height": height,
                                   "timestamp": int(time.time()), "title": name,
                                   "env": {key: environment[key] for key in
                                           ("TERM", "COLORTERM", "DISPATCH_COLOR", "DISPATCH_THEME", "NO_COLOR")
                                           if key in environment}}) + "\n")

        def terminal_owner():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        # macOS revokes the slave after its session leader exits. Keep a tiny
        # terminal owner alive until it records attributes after Dispatch exits.
        self.restored = self.capture_path.with_suffix(".termios.json")
        self.child_pid = self.capture_path.with_suffix(".child.pid")
        wrapper = """
import json, signal, subprocess, sys, termios
signal.signal(signal.SIGINT, signal.SIG_IGN)
child = subprocess.Popen(json.loads(sys.argv[1]))
with open(sys.argv[3], 'w') as out: out.write(str(child.pid))
code = child.wait()
attrs = termios.tcgetattr(0)
with open(sys.argv[2], 'w') as out:
    json.dump(attrs[:6] + [[v if isinstance(v, int) else list(v) for v in attrs[6]]], out)
raise SystemExit(code if code >= 0 else 128-code)
"""
        self.process = subprocess.Popen([sys.executable, "-c", wrapper, json.dumps(args), str(self.restored), str(self.child_pid)], cwd=source, env=environment,
                                        stdin=self.slave, stdout=self.slave,
                                        stderr=self.slave, preexec_fn=terminal_owner)
        os.set_blocking(self.master, False)

    def pump(self, duration=0.05):
        deadline = time.monotonic() + duration
        while time.monotonic() < deadline:
            if not select.select([self.master], [], [], max(0, deadline-time.monotonic()))[0]:
                continue
            try:
                chunk = os.read(self.master, 65536)
            except BlockingIOError:
                continue
            if not chunk:
                break
            self.output.extend(chunk)
            for _ in range(chunk.count(b"\x1b[6n")):
                os.write(self.master, b"\x1b[1;1R")
            decoded = self.decoder.decode(chunk)
            self.cast.write(json.dumps([round(time.monotonic()-self.started, 6), "o", decoded]) + "\n")
            self.cast.flush()
            self.clean = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", " ", self.output.decode("utf8", "replace"))

    def wait(self, text, timeout=WAIT):
        deadline = time.monotonic()+timeout
        pattern = re.compile(re.escape(text).replace(r"\ ", r"\s+"))
        while time.monotonic() < deadline:
            self.pump()
            found = pattern.search(self.clean, self.position)
            if found:
                self.position = found.end()
                return
            if self.process.poll() is not None:
                break
        raise AssertionError("waiting for " + repr(text) + "\n" + self.clean[-7000:])

    def send(self, value):
        os.write(self.master, value.encode() if isinstance(value, str) else value)

    def resize(self, width, height):
        self.width, self.height = width, height
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
        os.killpg(self.process.pid, signal.SIGWINCH)
        self.pump(0.1)

    def mark(self, label):
        self.pump(.08)
        self.markers[label] = {"seconds": round(time.monotonic()-self.started, 6),
                               "bytes": len(self.output), "width": self.width, "height": self.height}
        Path(str(self.capture_path) + "." + label + ".ansi").write_bytes(self.output)
        self.capture_path.with_suffix(".markers.json").write_text(json.dumps(self.markers, indent=2))

    def finish(self, timeout=WAIT):
        deadline = time.monotonic()+timeout
        while self.process.poll() is None and time.monotonic() < deadline:
            self.pump()
        self.pump()
        assert self.process.poll() == 0, (self.process.poll(), self.clean[-7000:])
        normalized = self.before[:6] + [[v if isinstance(v, int) else list(v) for v in self.before[6]]]
        assert json.loads(self.restored.read_text()) == normalized, "terminal attributes not restored"

    def close(self):
        if self.process.poll() is None:
            os.killpg(self.process.pid, signal.SIGKILL)
            self.process.wait(timeout=WAIT)
        for pid in self.cleanup_pids:
            try: os.kill(pid, signal.SIGKILL)
            except ProcessLookupError: pass
        self.capture_path.with_suffix(".ansi").write_bytes(self.output)
        self.capture_path.with_suffix(".transcript.txt").write_text(self.clean)
        self.cast.close()
        os.close(self.master)
        os.close(self.slave)

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()


def run_fixture(binary, source, state, scenario):
    source, state = Path(source), Path(state)
    captures = os.environ.get("DISPATCH_REVIEW_CAPTURES", str(state.parent / "captures"))
    args = [binary, "--state-dir", str(state)]
    if scenario in ("plain-transition", "dumb-transition"):
        flags = ['--plain', '--no-color'] if scenario == 'plain-transition' else ['--no-color']
        environment = {'TERM': 'dumb'} if scenario == 'dumb-transition' else None
        with Session(args + flags, source, captures, scenario, env=environment) as session:
            session.wait('accomplish?')
            for number in (1, 2):
                start = len(session.clean)
                session.send(f'Add tests in tests/collision_test.c for case {number}\r')
                session.wait('[y/N]')
                session.send('y\r')
                session.wait('Review changes')
                transition = session.clean[start:session.position]
                assert transition.count('Ready for review') == 1, transition
                assert 'Verification passed' in transition, transition
                with sqlite3.connect(state / 'dispatch.db') as db:
                    runs = [json.loads(row[0]) for row in db.execute('SELECT run_projection_json FROM runs')]
                    assert len(runs) == number
                    for run in runs:
                        assert Path(run['source_path']).resolve() == source.resolve()
                        assert run['outcome']['review'] == 'pending'
                        assert run['outcome']['verification'] == 'passed'
                        assert db.execute("SELECT count(*) FROM events WHERE run_id=? AND event_type='run.finished'", (run['id'],)).fetchone()[0] == 1
                session.send('i\r')
                session.wait('[Enter] Back to review')
                session.send('\r')
                session.wait('Review changes')
                session.send('d\r')
                session.wait('[q] back')
                session.send('q\r')
                session.wait('Review changes')
                session.send('n\r')
                session.wait('accomplish?')
            session.send(b'\x04')
            session.finish()
            assert not has_color(bytes(session.output))
        print('PASS one plain review transition per distinct delivery; committed completion events preserved')
        return
    if scenario == "large": args += ["--ascii"]
    if scenario not in ("native-delta", "large-color"): args += ["--no-color"]
    environment = {"NO_COLOR": None, "DISPATCH_COLOR": "truecolor", "COLORTERM": "truecolor"} if scenario in ("native-delta", "large-color") else None
    with Session(args, source,
                 captures, scenario, env=environment) as session:
        session.wait("accomplish?")
        session.mark("startup")
        assert b"\x1b[?1049h" not in session.output, "startup took over the terminal"
        session.send("Add tests in src/lib.rs\r")
        session.wait("[y/N]")
        session.send("y\r")
        session.wait("Working")
        session.mark("working")
        session.wait("Review changes")
        session.mark("review-summary")
        metadata = list((state / "runs").glob("*/metadata.json"))
        assert len(metadata) == 1

        def current():
            return json.loads(metadata[0].read_text())

        run = current()
        candidate = run["candidates"][0]
        immutable = [Path(run["baseline_path"]) / "src/lib.rs",
                     Path(candidate["workspace_path"]) / "src/lib.rs",
                     Path(candidate["diff_path"])]
        fingerprints = [path.read_bytes() for path in immutable]
        assert run["outcome"]["review"] == "pending"
        with sqlite3.connect(state / "dispatch.db") as connection:
            assert connection.execute("SELECT COUNT(*) FROM attempt_launches WHERE state IN ('intent','spawned','uncertain')").fetchone()[0] == 0
        if scenario == "unverified":
            assert "Unverified" in session.clean and "no checks configured" in session.clean
        if scenario in ("editor-return", "editor-cancel"):
            session.send("e\r")
            session.wait("fixture launcher exited")
            session.wait("Close the external review document")
            observation = json.loads((state.parent / "reviewer-observation.json").read_text())
            assert Path(observation["patch"]).exists(), "launcher exit deleted active review material"
            assert current()["outcome"]["review"] == "pending"
            if scenario == "editor-cancel":
                session.send(b"\x04")
                session.finish()
                assert current()["outcome"]["review"] == "pending"
                assert [path.read_bytes() for path in immutable] == fingerprints
                return
            session.send(b"\ra\r")
            session.wait("Review changes")
            assert current()["outcome"]["review"] == "pending", "explicit return accepted work"
            assert [path.read_bytes() for path in immutable] == fingerprints
            session.send("n\r")
            session.wait("accomplish?")
            session.send(b"\x04")
            session.finish()
            return
        if scenario == "nested-signal":
            session.send("e\r")
            session.wait("fixture reviewer ready")
            pids_path = state.parent / "reviewer-processes.json"
            deadline = time.monotonic() + WAIT
            while not pids_path.exists() and time.monotonic() < deadline: session.pump()
            pids = json.loads(pids_path.read_text())
            session.cleanup_pids = pids
            os.kill(int(session.child_pid.read_text()), signal.SIGTERM)
            session.finish()
            assert current()["outcome"]["review"] == "pending"
            assert [path.read_bytes() for path in immutable] == fingerprints
            for pid in pids:
                deadline = time.monotonic() + WAIT
                while time.monotonic() < deadline:
                    try: os.kill(pid, 0)
                    except ProcessLookupError: break
                    time.sleep(.03)
                else: raise AssertionError("reviewer process survived Dispatch: " + str(pid))
            session.cleanup_pids = []
            return
        if scenario in ("native-pager", "native-delta"):
            session.send("e\r")
            session.wait("delivered")
            session.pump(.2)
            session.send("qa\r")
            session.wait("Review changes")
            session.pump(.3)
            assert current()["outcome"]["review"] == "pending"
            assert [path.read_bytes() for path in immutable] == fingerprints
            session.send("n\r")
            session.wait("accomplish?")
            session.send(b"\x04")
            session.finish()
            return
        external = scenario in ("terminal", "gui-wait", "crash", "missing", "cancel", "drift", "copy-edit", "concurrent-review")
        if external:
            session.send("e\r")
            if scenario == "missing":
                session.wait("unavailable")
            else:
                session.wait("fixture reviewer ready")
                if scenario == "gui-wait":
                    session.pump(.4)
                    assert "Review changes" not in session.clean[session.position:], "GUI launcher wait did not hold review session"
                    session.send("a\r")
                    (state.parent / "reviewer-release").touch()
                    session.wait("fixture reviewer finished")
                elif scenario == "cancel":
                    session.send(b"\x03")
                elif scenario != "crash":
                    if scenario == "concurrent-review":
                        peer = subprocess.run([binary, "--state-dir", str(state), "reject", run["id"]],
                                              cwd=source, capture_output=True, timeout=WAIT)
                        assert peer.returncode == 0, peer.stderr.decode()
                    session.send("reviewer-input\na\r")
                    session.wait("fixture reviewer finished")
            if scenario == "concurrent-review":
                session.wait("accomplish?")
                assert current()["outcome"]["review"] == "rejected"
                assert current()["outcome"]["application"] != "applied"
                assert [path.read_bytes() for path in immutable] == fingerprints
                session.send(b"\x04")
                session.finish()
                return
            if scenario != "missing": session.wait("Review changes")
            session.pump(.3)
            assert current()["outcome"]["review"] == "pending", "reviewer keystrokes accepted a candidate"
            assert [path.read_bytes() for path in immutable] == fingerprints, "external review changed immutable evidence"
            if scenario != "missing":
                observation = json.loads((state.parent / "reviewer-observation.json").read_text())
                assert "--wait" in observation["args"] if scenario == "gui-wait" else "-R" in observation["args"]
            if scenario == "drift":
                session.send("a\r")
                session.wait("accomplish?")
                assert current()["outcome"]["application"] == "blocked_by_source_drift"
                assert (source / "src/lib.rs").read_text() == "// founder edit during review\n"
            else:
                session.send("n\r")
                session.wait("accomplish?")
                assert current()["outcome"]["review"] == "pending"
            session.send(b"\x04")
            session.finish()
            return
        review_start = len(session.clean)
        # Enter inspects. Pasted/queued acceptance on the transition must not act.
        session.send(b"\ra\r")
        if scenario in ("large", "large-color"):
            session.wait("item000.rs")
            session.mark("changed-file-index")
            session.position = len(session.clean)
            session.resize(38, 16)
            session.wait("modified binary")
            session.resize(110, 32)
            session.send("/item119")
            session.send("\r")
            session.pump(0.2)
            session.send("\r")
            session.wait("delivered 119")
        else:
            session.wait("delivered")
        assert current()["outcome"]["review"] == "pending", "typeahead accepted a candidate"
        assert "Alt+Enter" not in session.clean[review_start:], "composer hint leaked into review"
        assert "index " not in session.clean[review_start:], "patch hashes leaked into compact preview"
        session.send("q")
        session.wait("Review changes")
        for _ in range(2):
            session.send("i\r")
            session.wait("Run:")
            session.send("q")
            session.wait("Review changes")
        session.send("d\r")
        if scenario in ("large", "large-color"):
            session.wait("item119.rs")
        else:
            session.wait("delivered")
        session.send("q")
        session.wait("Review changes")
        assert [path.read_bytes() for path in immutable] == fingerprints, "inspection changed immutable evidence"
        assert current()["outcome"]["review"] == "pending"
        session.send("n\r")
        session.wait("accomplish?")
        session.send(b"\x04")
        session.finish()
        assert current()["outcome"]["review"] == "pending", "leave pending recorded a review"
        assert session.output.count(b"\x1b[?1049h") == session.output.count(b"\x1b[?1049l")
        if scenario != "large-color":
            assert not has_color(session.output)
        if scenario == "large":
            assert not any(mark in session.clean for mark in "·↑↓←→"), "Unicode controls leaked into ASCII review"
        assert len(session.output) < 500_000, "bounded review poured a large patch into the terminal"
    print("Review PTY passed:", scenario)


if __name__ == "__main__":
    run_fixture(*sys.argv[1:])
