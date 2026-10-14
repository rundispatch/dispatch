#!/usr/bin/env python3
"""First use on a terminal: `dispatch start` offers the detected checks, and
`setup --checks --verify` pre-fills the checks menu. No agent, no model call,
and no project code runs."""
import os, subprocess, sys, tempfile
from pathlib import Path
sys.dont_write_bytecode = True
from review_refinement import Session
binary = str(Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix='dispatch-first-use-') as tmp:
    root = Path(tmp); source = root/'project'; source.mkdir(); home = root/'home'; home.mkdir(); state = root/'state'
    ran = root/'project-code-ran'
    (source/'verify.sh').write_text(f'touch {ran}\n')
    (source/'Makefile').write_text(f'test:\n\ttouch {ran}\n')
    config = source/'dispatch.yml'
    env = {'HOME': str(home), 'CLAUDE_CONFIG_DIR': str(home/'.claude'), 'NO_COLOR': '1'}
    captures = os.environ.get('DISPATCH_FIRST_USE_CAPTURES', str(root/'captures'))
    def session(name, args): return Session([binary, '--state-dir', str(state)] + args, source, captures, name, env=env, width=100, height=36)
    def stop(): subprocess.run([binary, '--state-dir', str(state), 'stop'], cwd=source, check=True, capture_output=True, env=dict(os.environ, **env))
    consents = lambda: list((state/'projects').glob('*.json')) if (state/'projects').exists() else []
    try:
        # Rows: 1 Save these checks, 2 Continue without checks. Focus starts on
        # Continue: Enter alone watches and never writes to the project.
        with session('start-continue', ['start']) as ui:
            ui.wait('Dispatch found'); ui.wait('sh ./verify.sh'); ui.wait('make test'); ui.wait('Continue without checks'); ui.mark('offer')
            ui.send('\r'); ui.wait('Watching'); ui.wait('No checks are configured'); ui.finish()
        assert not config.exists(), 'Enter alone wrote dispatch.yml'
        stop()
        # Backing out starts nothing and writes nothing.
        with session('start-back', ['start']) as ui:
            ui.wait('Continue without checks'); ui.send('\x1b'); ui.wait('Not watching'); ui.finish()
        assert not config.exists() and 'Watching' not in ui.clean
        # Saving is one arrow key away.
        with session('start-save', ['start']) as ui:
            ui.wait('Continue without checks'); ui.send('\x1b[A'); ui.pump(.2); ui.send('\r'); ui.wait('Checks saved'); ui.wait('Watching'); ui.finish()
        assert 'No checks are configured' not in ui.clean
        assert config.read_text() == 'checks:\n  verify:\n  - sh ./verify.sh\n  - make test\n', config.read_text()
        stop()
        # With checks, start asks nothing.
        with session('start-checked', ['start']) as ui:
            ui.wait('Watching'); ui.finish()
        assert 'Dispatch found' not in ui.clean
        stop()
        # --verify pre-fills the menu; then the existing flow goes on, where
        # consent is its own choice and Enter cancels it.
        with session('setup-verify', ['--plain', 'setup', '--checks', '--verify', 'sh ./verify.sh --strict']) as ui:
            ui.wait('Save these checks?'); ui.wait('sh ./verify.sh --strict'); ui.wait('[1] >'); ui.send('\r')
            ui.wait('Approved checks saved'); ui.wait('2) Let Work run these checks'); ui.send('2\r')
            ui.wait('code an agent wrote'); ui.wait('on this host'); ui.wait('merged scratch tree at accept'); ui.wait('2) Cancel'); ui.mark('consent')
            ui.send('\r'); ui.wait('Nothing changed'); ui.finish()
        assert config.read_text() == 'checks:\n  verify:\n  - sh ./verify.sh --strict\n', config.read_text()
        assert not consents(), 'consent granted without the explicit choice'
        assert not ran.exists(), 'project code ran during first use'
    finally:
        try: stop()
        except subprocess.CalledProcessError: pass
    print('first-use terminal journeys passed; nothing ran and no consent was granted')
