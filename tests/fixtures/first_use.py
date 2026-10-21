#!/usr/bin/env python3
"""First use through one real terminal session, with isolated runtime settings.

The executable called ``claude`` is a stand-in on PATH. Setup may discover it,
but no agent, hook, or project check is run by this fixture.
"""

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

sys.dont_write_bytecode = True
from review_refinement import Session

binary = str(Path(sys.argv[1]).resolve())


class Case:
    def __init__(self, parent, name):
        self.base = parent / name
        self.source = self.base / 'project'
        self.home = self.base / 'home'
        self.state = self.base / 'state'
        self.bin = self.base / 'bin'
        for path in (self.source, self.home, self.bin):
            path.mkdir(parents=True)
        self.config = self.source / 'dispatch.yml'
        self.settings = self.home / '.claude' / 'settings.json'
        self.ran = self.base / 'project-code-ran'
        self.agent_ran = self.base / 'agent-ran'
        (self.source / 'verify.sh').write_text(f'touch "{self.ran}"\n')
        (self.source / 'Makefile').write_text(f'test:\n\ttouch "{self.ran}"\n')
        # Discovery needs an executable on PATH, not a real provider call.
        stand_in = self.bin / 'claude'
        stand_in.write_text(f'#!/bin/sh\ntouch "{self.agent_ran}"\nexit 91\n')
        stand_in.chmod(0o755)
        self.env = {
            'HOME': str(self.home),
            'CLAUDE_CONFIG_DIR': str(self.home / '.claude'),
            'HERMES_HOME': str(self.home / '.hermes'),
            'PATH': str(self.bin) + os.pathsep + '/usr/bin:/bin',
            'NO_COLOR': '1',
        }
        self.captures = Path(os.environ.get('DISPATCH_FIRST_USE_CAPTURES', str(self.base / 'captures')))
        self.sessions = 0

    def session(self, name, args=('setup',)):
        self.sessions += 1
        return Session([binary, '--state-dir', str(self.state), '--plain'] + list(args),
                       self.source, self.captures, name, env=self.env, width=120, height=44)

    def consent_records(self):
        return list((self.state / 'projects').glob('*.json'))

    def watcher_record(self):
        records = [path for path in (self.state / 'watchers').glob('*.json')
                   if path.name.count('.') == 1]
        assert len(records) == 1, records
        record = json.loads(records[0].read_text())
        assert Path(record['root']) == self.source.resolve(), record
        assert record['background'] and record['discovery_ready'], record
        os.kill(record['identity']['pid'], 0)

    def __enter__(self):
        return self

    def __exit__(self, *_):
        # A failed assertion must still stop the background owner before the
        # temporary project is removed.
        stopped = subprocess.run([binary, '--state-dir', str(self.state), 'stop'],
                                 cwd=self.source, capture_output=True,
                                 env=dict(os.environ, **self.env))
        assert stopped.returncode == 0, stopped.stderr.decode()


def assert_only_config_added(case):
    """Setup wrote the project's configuration and no other file there."""
    names = sorted(path.name for path in case.source.iterdir())
    assert names == ['Makefile', 'dispatch.yml', 'verify.sh'], names


def assert_nothing_ran(case):
    assert not case.ran.exists(), 'setup ran project code'
    assert not case.agent_ran.exists(), 'setup launched the stand-in agent'


def select_two_checks(ui):
    ui.wait('sh ./verify.sh')
    ui.wait('make test')
    ui.wait('5) Continue without checks')
    ui.wait('[5] >')  # A bare Enter cannot write the project's config.
    ui.send('1\r')
    ui.wait('[5] >')
    ui.send('2\r')
    ui.wait('[5] >')
    ui.send('4\r')  # Save selected checks.
    ui.wait('Checks saved to dispatch.yml')


def allow_checks(ui):
    ui.wait('2) Cancel')
    assert 'sh ./verify.sh' in ui.clean and 'make test' in ui.clean, ui.clean[-5000:]
    assert 'code an agent wrote' in ui.clean and 'on this host' in ui.clean, ui.clean[-5000:]
    ui.wait('[2] >')  # A bare Enter cannot grant consent.
    ui.send('1\r')
    ui.wait('Check consent saved')


def hooks_prompt(ui, case, action):
    ui.wait('2) Skip')
    assert str(case.settings) in ui.clean, ui.clean[-5000:]
    assert 'hook claude' in ui.clean, ui.clean[-5000:]
    assert action in ui.clean, ui.clean[-5000:]
    ui.wait('[2] >')  # A bare Enter cannot edit Claude settings.


def finish_setup(ui, watching):
    ui.wait('Agents Dispatch launches')
    ui.send('\r')  # Done is focused; no account setup is required.
    ui.wait(watching)
    ui.finish()


def through_setup(case, name, done, watching='Already watching'):
    """Take the hook step in setup's pass, with no checks."""
    with case.session(name) as ui:
        ui.wait('5) Continue without checks')
        ui.send('\r')
        ui.wait('2) Skip')
        ui.wait('[2] >')
        ui.send('1\r')
        ui.wait(done)
        finish_setup(ui, watching)


def through_menu(case, name, row, done, skip=True):
    """Pass setup's hook step by, then take it from Runtime integrations."""
    with case.session(name) as ui:
        ui.wait('5) Continue without checks')
        ui.send('\r')
        if skip:
            ui.wait('2) Skip')
            ui.wait('[2] >')
            ui.send('\r')
        ui.wait('Agents Dispatch launches')
        ui.send('2\r')
        ui.wait('4) Runtime integrations')
        ui.send('4\r')
        ui.wait(row)
        ui.send(row.split(')')[0] + '\r')
        ui.wait('2) Cancel')
        ui.wait('[2] >')
        ui.send('1\r')
        ui.wait(done)
        ui.wait('5) Back')
        ui.send('5\r')
        ui.wait('Already watching')
        ui.finish()


with tempfile.TemporaryDirectory(prefix='dispatch-first-use-') as tmp:
    parent = Path(tmp)

    # One user command saves both detected checks, grants exactly those checks,
    # installs the previewed hooks, and starts watching. No check runs now.
    with Case(parent, 'one-pass') as case:
        with case.session('one-pass') as ui:
            select_two_checks(ui)
            allow_checks(ui)
            hooks_prompt(ui, case, 'Install')
            ui.send('1\r')
            ui.wait('Claude Code hooks installed')
            finish_setup(ui, 'Watching')
            assert 'Dispatch is running in the background' in ui.clean
        assert case.sessions == 1, 'first setup took more than one Dispatch command'
        saved = case.config.read_text()
        assert saved == 'checks:\n  verify:\n  - sh ./verify.sh\n  - make test\n', saved
        assert_only_config_added(case)
        records = case.consent_records()
        assert len(records) == 1, records
        consent = json.loads(records[0].read_text())
        assert consent['version'] == 2 and consent['commands'] == ['sh ./verify.sh', 'make test'], consent
        assert 'consent' not in saved, 'consent belongs in Dispatch state'
        settings = json.loads(case.settings.read_text())
        commands = [hook['command'] for event in settings['hooks'].values()
                    for entry in event for hook in entry['hooks']]
        assert commands and all(binary in command and str(case.state) in command
                                and command.endswith('hook claude') for command in commands), commands
        case.watcher_record()
        assert_nothing_ran(case)

        # A version 1 record has the older, narrower scope. Setup explains
        # the change and asks once for version 2 before continuing.
        consent_path = records[0]
        consent['version'] = 1
        consent_path.write_text(json.dumps(consent))
        with case.session('renew-check-consent') as ui:
            ui.wait('Keep current checks')
            ui.send('\r')
            ui.wait('earlier consent no longer holds')
            ui.wait('attach or finish Work yourself')
            ui.wait('2) Cancel')
            ui.wait('[2] >')
            ui.send('1\r')
            ui.wait('Check consent saved')
            finish_setup(ui, 'Already watching')
        assert json.loads(consent_path.read_text())['version'] == 2
        assert_nothing_ran(case)

        # The same project is already watched and consented. A hook naming an
        # older executable is offered as Update, with the exact old and new
        # commands shown before any settings change.
        old_executable = '/old/dispatch'
        for event in settings['hooks'].values():
            for entry in event:
                for hook in entry['hooks']:
                    hook['command'] = hook['command'].replace(binary, old_executable)
        settings['keep_this_setting'] = True
        case.settings.write_text(json.dumps(settings))
        stale = case.settings.read_bytes()
        with case.session('hook-update') as ui:
            ui.wait('Keep current checks')
            ui.send('\r')
            hooks_prompt(ui, case, 'Update')
            assert old_executable in ui.clean and binary in ui.clean, ui.clean[-5000:]
            assert case.settings.read_bytes() == stale, 'preview changed settings'
            ui.send('1\r')
            ui.wait('Claude Code hooks updated')
            finish_setup(ui, 'Already watching')
        updated = json.loads(case.settings.read_text())
        assert updated['keep_this_setting'] is True
        assert old_executable not in case.settings.read_text()
        assert binary in case.settings.read_text()
        assert_nothing_ran(case)

        # Hermes has the same explicit update decision. Its appended block is
        # changed in place while the user's preceding config stays byte exact.
        hermes = case.bin / 'hermes'
        hermes.write_text(f'#!/bin/sh\ntouch "{case.agent_ran}"\nexit 91\n')
        hermes.chmod(0o755)
        hermes_config = case.home / '.hermes' / 'config.yaml'
        hermes_config.parent.mkdir()
        prefix = '# my settings\nmodel:\n  default: keep\n'
        hermes_config.write_text(prefix)
        with case.session('hermes-install') as ui:
            ui.wait('Keep current checks')
            ui.send('\r')
            ui.wait('Hermes hook · Install')
            ui.wait('2) Skip')
            assert str(hermes_config) in ui.clean and 'hook hermes' in ui.clean
            ui.wait('[2] >')
            ui.send('1\r')
            ui.wait('Hermes hook installed')
            finish_setup(ui, 'Already watching')
        installed = hermes_config.read_text()
        assert installed.startswith(prefix) and binary in installed, installed
        stale = installed.replace(binary, old_executable)
        hermes_config.write_text(stale)
        with case.session('hermes-update') as ui:
            ui.wait('Keep current checks')
            ui.send('\r')
            ui.wait('Hermes hook · Update')
            ui.wait('2) Skip')
            assert old_executable in ui.clean and binary in ui.clean, ui.clean[-5000:]
            assert hermes_config.read_text() == stale
            ui.wait('[2] >')
            ui.send('1\r')
            ui.wait('Hermes hook updated')
            finish_setup(ui, 'Already watching')
        assert hermes_config.read_text().startswith(prefix)
        assert old_executable not in hermes_config.read_text()
        assert binary in hermes_config.read_text()
        assert_nothing_ran(case)

    # Canceling setup at checks writes nothing; canceling consent retains the
    # chosen checks and stops before hooks or watching.
    with Case(parent, 'cancel-checks') as case:
        with case.session('cancel-checks') as ui:
            ui.wait('6) Cancel setup')
            ui.send('6\r')
            ui.finish()
        assert not case.config.exists() and not case.settings.exists()
        assert not case.consent_records() and not list((case.state / 'watchers').glob('*.json'))
        assert_nothing_ran(case)

    with Case(parent, 'cancel-consent') as case:
        with case.session('cancel-consent') as ui:
            select_two_checks(ui)
            ui.wait('2) Cancel')
            ui.wait('[2] >')
            ui.send('\r')
            ui.finish()
        assert case.config.exists()
        assert not case.consent_records() and not case.settings.exists()
        assert not list((case.state / 'watchers').glob('*.json'))
        assert_nothing_ran(case)

    with Case(parent, 'continue-without-checks') as case:
        with case.session('continue-without-checks') as ui:
            ui.wait('5) Continue without checks')
            ui.wait('[5] >')
            ui.send('\r')
            ui.wait('No checks are configured; check consent was not granted.')
            hooks_prompt(ui, case, 'Install')
            ui.send('\r')
            finish_setup(ui, 'Watching')
            assert 'No checks are configured' in ui.clean
        assert not case.config.exists() and not case.consent_records()
        assert not case.settings.exists()
        case.watcher_record()
        assert_nothing_ran(case)

    with Case(parent, 'skip-hooks') as case:
        with case.session('skip-hooks') as ui:
            select_two_checks(ui)
            allow_checks(ui)
            hooks_prompt(ui, case, 'Install')
            ui.send('\r')
            finish_setup(ui, 'Watching')
        assert case.config.exists() and len(case.consent_records()) == 1
        assert not case.settings.exists(), 'Enter installed hooks despite Skip focus'
        case.watcher_record()
        assert_nothing_ran(case)

    # Setup's pass and Runtime integrations take one hook step per runtime:
    # from the same configuration, a change from either writes the same bytes.
    with Case(parent, 'claude-hook-steps') as case:
        theirs = json.dumps({'model': 'keep', 'hooks': {'SessionStart': [
            {'hooks': [{'type': 'command', 'command': 'direnv export'}]}]}})
        case.settings.parent.mkdir()
        case.settings.write_text(theirs)
        through_setup(case, 'claude-install-setup', 'Claude Code hooks installed', 'Watching')
        installed = case.settings.read_bytes()
        case.settings.write_text(theirs)
        through_menu(case, 'claude-install-menu', '1) Install Claude Code hooks',
                     'Claude Code hooks installed')
        assert case.settings.read_bytes() == installed
        stale = installed.replace(binary.encode(), b'/old/dispatch')
        case.settings.write_bytes(stale)
        through_setup(case, 'claude-update-setup', 'Claude Code hooks updated')
        updated = case.settings.read_bytes()
        assert b'/old/dispatch' not in updated and binary.encode() in updated, updated
        case.settings.write_bytes(stale)
        through_menu(case, 'claude-update-menu', '1) Update Claude Code hooks',
                     'Claude Code hooks updated')
        assert case.settings.read_bytes() == updated
        # Only the menu removes: setup leaves a hook that runs this Dispatch.
        through_menu(case, 'claude-remove-menu', '1) Remove Claude Code hooks',
                     'Claude Code hooks removed', skip=False)
        assert json.loads(case.settings.read_text()) == json.loads(theirs)
        assert_nothing_ran(case)

    # 0.4.16 installed four hooks; this Dispatch installs six. Such a file,
    # upgraded in place, holds Dispatch's own hooks: setup offers Update, shows
    # the change, and leaves exactly six beside the person's own Stop hook.
    # Runtime integrations writes the same bytes.
    with Case(parent, 'claude-upgrade') as case:
        command = f"'{binary}' --state-dir '{case.state}' hook claude"

        def ours(timeout, matcher=None):
            entry = {'hooks': [{'type': 'command', 'command': command, 'timeout': timeout}]}
            if matcher:
                entry['matcher'] = matcher
            return entry
        mine = {'hooks': [{'type': 'command', 'command': 'afplay done.aiff', 'timeout': 2}]}
        earlier = json.dumps({'model': 'keep', 'hooks': {
            'SessionStart': [ours(60)], 'SessionEnd': [ours(5)], 'WorktreeRemove': [ours(300)],
            'PreToolUse': [ours(300, 'ExitWorktree')], 'Stop': [mine]}}, indent=2) + '\n'
        case.settings.parent.mkdir()
        case.settings.write_text(earlier)
        with case.session('claude-upgrade-setup') as ui:
            ui.wait('5) Continue without checks')
            ui.send('\r')
            ui.wait('Claude Code hooks · Update')
            ui.wait('2) Skip')
            for shown in ('Hook command, unchanged', 'replaced by exactly these', '"Notification"',
                          '"Stop"', 'permission_prompt|idle_prompt|agent_needs_input', command):
                assert shown in ui.clean, (shown, ui.clean[-5000:])
            assert case.settings.read_text() == earlier, 'preview changed settings'
            ui.wait('[2] >')
            ui.send('1\r')
            ui.wait('Claude Code hooks updated')
            finish_setup(ui, 'Watching')
        upgraded = case.settings.read_bytes()
        settings = json.loads(upgraded)
        found = sorted((event, entry.get('matcher', ''), hook['timeout'])
                       for event, entries in settings['hooks'].items()
                       for entry in entries for hook in entry['hooks'] if hook['command'] == command)
        assert found == sorted([('SessionStart', '', 60), ('SessionEnd', '', 5),
                                ('WorktreeRemove', '', 300), ('PreToolUse', 'ExitWorktree', 300),
                                ('Notification', 'permission_prompt|idle_prompt|agent_needs_input', 5),
                                ('Stop', '', 5)]), found
        assert settings['model'] == 'keep' and settings['hooks']['Stop'][0] == mine, settings
        case.settings.write_text(earlier)
        through_menu(case, 'claude-upgrade-menu', '1) Update Claude Code hooks',
                     'Claude Code hooks updated')
        assert case.settings.read_bytes() == upgraded
        # A hook edited by hand is its owner's: setup says so and offers nothing.
        settings['hooks']['SessionEnd'][0]['hooks'][0]['timeout'] = 10
        edited = json.dumps(settings)
        case.settings.write_text(edited)
        with case.session('claude-edited') as ui:
            ui.wait('5) Continue without checks')
            ui.send('\r')
            ui.wait('changed by hand')
            finish_setup(ui, 'Already watching')
        assert case.settings.read_text() == edited
        assert_nothing_ran(case)

    with Case(parent, 'hermes-hook-steps') as case:
        (case.bin / 'claude').unlink()
        hermes = case.bin / 'hermes'
        hermes.write_text(f'#!/bin/sh\ntouch "{case.agent_ran}"\nexit 91\n')
        hermes.chmod(0o755)
        config = case.home / '.hermes' / 'config.yaml'
        config.parent.mkdir()
        theirs = b'# mine\nmodel:\n  default: keep  # theirs\n'
        config.write_bytes(theirs)
        through_setup(case, 'hermes-install-setup', 'Hermes hook installed', 'Watching')
        installed = config.read_bytes()
        config.write_bytes(theirs)
        through_menu(case, 'hermes-install-menu', '2) Install the Hermes hook',
                     'Hermes hook installed')
        assert config.read_bytes() == installed
        stale = installed.replace(binary.encode(), b'/old/dispatch')
        config.write_bytes(stale)
        through_setup(case, 'hermes-update-setup', 'Hermes hook updated')
        updated = config.read_bytes()
        assert updated.startswith(theirs) and b'/old/dispatch' not in updated, updated
        config.write_bytes(stale)
        through_menu(case, 'hermes-update-menu', '2) Update the Hermes hook',
                     'Hermes hook updated')
        assert config.read_bytes() == updated
        through_menu(case, 'hermes-remove-menu', '2) Remove the Hermes hook',
                     'Hermes hook removed', skip=False)
        assert config.read_bytes() == theirs
        # A hook added by hand stays its owner's: setup says so, and offers nothing.
        by_hand = theirs + f'hooks:\n  on_session_finalize:\n    - command: "{binary} hook hermes"\n'.encode()
        config.write_bytes(by_hand)
        with case.session('hermes-by-hand') as ui:
            ui.wait('5) Continue without checks')
            ui.send('\r')
            ui.wait('changed by hand')
            finish_setup(ui, 'Already watching')
        assert config.read_bytes() == by_hand
        assert_nothing_ran(case)

    # Explicit --checks --verify remains usable without entering the new pass.
    with Case(parent, 'explicit-checks') as case:
        with case.session('setup-verify', ('setup', '--checks', '--verify', 'sh ./verify.sh --strict')) as ui:
            ui.wait('Save these checks?')
            ui.wait('sh ./verify.sh --strict')
            ui.wait('[1] >')
            ui.send('\r')
            ui.wait('Approved checks saved')
            ui.wait('2) Let Work run these checks')
            ui.send('2\r')
            ui.wait('2) Cancel')
            ui.wait('[2] >')
            ui.send('\r')
            ui.wait('Nothing changed')
            ui.finish()
        assert case.config.read_text() == 'checks:\n  verify:\n  - sh ./verify.sh --strict\n'
        assert_only_config_added(case)
        assert not case.consent_records()
        assert_nothing_ran(case)

    print('one-command first setup, focused cancel/skip, hook update, one hook step from both menus, and explicit checks passed')
