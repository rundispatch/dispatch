#!/usr/bin/env python3
"""Proof suite for the funding-identity invariant (0.4.1 S5a): an included-only
profile never launches when its adapter observes funding other than what the
user authorized, and a refusal stays until setup re-authorizes the profile.

Codex scenarios run with `codex_probe: false`, so the capacity path never
probes the account and every refusal here comes from the adapter preflight.
Never invokes a model provider; the provider executables are fixtures."""
import json
import sqlite3
import sys

sys.dont_write_bytecode = True
from provider_fixture import Fixture


def run(f):
    return f.command('run', f.source, '--task', 'Add tests in src/lib.rs',
                     '--allow-unsafe-local', '--json', check=False)


def refused(f, output, reason):
    stderr = output.stderr.decode()
    assert output.returncode != 0, (output.stdout, stderr)
    assert f.count() == 0, 'a refused launch must not spawn the agent'
    assert reason in stderr, (reason, stderr)


def launched(f, output, count=1):
    assert output.returncode == 0, (output.stdout, output.stderr)
    assert f.count() == count, f.count()


def refusals(f):
    with sqlite3.connect(f.state / 'dispatch.db') as db:
        return db.execute('SELECT authorization_revision, reason FROM funding_refusals').fetchall()


def reauthorize(f):
    path = f.state / 'resources.yml'
    path.write_text(path.read_text().replace('authorization_revision: 1', 'authorization_revision: 2'))


CODEX = {
    # C1-C5 and the unknown-identity decision, one fixture each.
    'auth': ({'type': 'apiKey', 'planType': 'plus', 'email': 'fixture@example.invalid'}, None,
             'authentication changed to apiKey'),
    'credits': (None, {'rateLimitsByLimitId': {'codex': {'credits': {'hasCredits': True}}}},
                'paid credits are now available'),
    'tier': (None, {'serviceTier': 'priority', 'rateLimitsByLimitId': {}},
             'service tier changed to priority'),
    'plan': ({'type': 'chatgpt', 'planType': 'pro', 'email': 'fixture@example.invalid'}, None,
             'funding plan changed to pro'),
    'account': ({'type': 'chatgpt', 'planType': 'plus', 'email': 'other@example.invalid'}, None,
                'the Codex account changed'),
    'unknown': ({'type': 'chatgpt', 'planType': 'plus'}, None,
                'could not be observed'),
}


def scenario(binary, name):
    f = Fixture(binary)
    try:
        if name == 'codex_authorized':
            launched(f, run(f))
            assert refusals(f) == []
        elif name in CODEX:
            account, limits, reason = CODEX[name]
            if account:
                (f.root / 'codex-account.json').write_text(json.dumps(account))
            if limits:
                (f.root / 'codex-limits.json').write_text(json.dumps(limits))
            refused(f, run(f), reason)
            assert [r for (r, _) in refusals(f)] == [1], refusals(f)
        elif name == 'codex_unreadable':
            (f.root / 'codex-probe').write_text('exit')
            refused(f, run(f), 'could not be read')
        elif name == 'codex_missing_evidence':
            path = f.state / 'resources.yml'
            path.write_text(''.join(l for l in path.read_text().splitlines(True) if 'codex_account' not in l))
            refused(f, run(f), 'Codex account evidence missing')
        elif name == 'codex_spawn_boundary':
            # Selection's preflight sees the authorized account; the preflight
            # at the spawn boundary sees another. Nothing is spawned and the
            # refusal is recorded, so the next run is refused at selection.
            (f.root / 'codex-switch-at').write_text('2')
            refused(f, run(f), 'the Codex account changed')
            assert [r for (r, _) in refusals(f)] == [1], refusals(f)
            (f.root / 'codex-switch-at').unlink()
            refused(f, run(f), 'was refused')
        elif name == 'codex_profile_changed_after_selection':
            # C9: the bound profile changes in resources.yml after selection.
            (f.root / 'codex-mutate-resources-at').write_text('1')
            refused(f, run(f), 'resource configuration changed after selection')
        elif name in ('codex_sticky', 'claude_sticky'):
            if name == 'codex_sticky':
                switch = f.root / 'codex-account.json'
                switch.write_text(json.dumps({'type': 'chatgpt', 'planType': 'plus',
                                              'email': 'other@example.invalid'}))
                refused(f, run(f), 'the Codex account changed')
                switch.unlink()
            else:
                auth = f.root / 'auth.json'
                original = auth.read_text()
                value = json.loads(original)
                value['email'] = 'other@example.invalid'
                auth.write_text(json.dumps(value))
                refused(f, run(f), 'account changed')
                auth.write_text(original)
            # The account is back, but the refusal holds until re-authorization.
            refused(f, run(f), 'was refused')
            # A new authorization revision is not refused by the preflight's
            # record. (Launching after re-authorization is asserted once the
            # capacity path, which keeps its own stale rejection, is removed.)
            reauthorize(f)
            output = run(f)
            assert b'was refused' not in output.stderr, output.stderr
            assert [r for (r, _) in refusals(f)] == [1], refusals(f)
        elif name == 'claude_slow_version':
            # A version probe that times out observes nothing: the executable's
            # bytes and the account are unchanged, so the run launches and no
            # refusal is recorded.
            (f.root / 'version-sleep').write_text('7')
            launched(f, run(f))
            assert refusals(f) == [], refusals(f)
        elif name == 'claude_version_changed':
            # An observed different version is a change: refused, and sticky.
            (f.root / 'version-text').write_text('claude 9.9.9 (Claude Code)')
            refused(f, run(f), 'Claude CLI version changed')
            assert [r for (r, _) in refusals(f)] == [1], refusals(f)
            (f.root / 'version-text').unlink()
            refused(f, run(f), 'was refused')
        elif name == 'claude_slow_account':
            # An account probe that does not answer refuses that launch only.
            (f.root / 'auth-sleep').write_text('7')
            refused(f, run(f), 'did not confirm its account')
            assert refusals(f) == [], refusals(f)
            (f.root / 'auth-sleep').unlink()
            launched(f, run(f))
        else:
            raise SystemExit(f'unknown scenario {name}')
    finally:
        f.cleanup()


if __name__ == '__main__':
    scenario(sys.argv[1], sys.argv[2])
