#!/usr/bin/env python3
"""A source, a state directory and one deterministic fake provider executable
(Claude or Codex, from DISPATCH_FIXTURE_PROVIDER) configured as an eligible
resource profile. Never invokes a model provider."""
import json
import hashlib
from datetime import datetime, timedelta, timezone
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile

sys.dont_write_bytecode = True


class Fixture:
    def __init__(self, binary, mode='success'):
        self.provider = os.environ.get('DISPATCH_FIXTURE_PROVIDER', 'codex')
        self.binary = str(Path(binary).resolve())
        self.temp = tempfile.TemporaryDirectory(prefix='dispatch-provider-')
        self.root = Path(self.temp.name)
        self.source = self.root / 'source'
        self.source.mkdir()
        self.state = self.root / 'state'
        self.state.mkdir(mode=0o700)
        (self.source / 'result.txt').write_text('ok\n')
        (self.source / 'src').mkdir()
        (self.source / 'src/lib.rs').write_text('// baseline\n')
        self.mode = self.root / 'mode'
        self.mode.write_text(mode)
        self.invocations = self.root / 'invocations'
        self.barrier = self.root / 'barrier'
        os.mkfifo(self.barrier)
        self.ready = self.root / "ready"
        os.mkfifo(self.ready)
        agent = self.root / self.provider
        agent.write_text('''#!/usr/bin/env python3
import json, pathlib, sys, os
root=pathlib.Path(''' + repr(str(self.root)) + ''')
if '--version' in sys.argv:
    print('codex phase5 fixture');sys.exit()
if 'app-server' in sys.argv:
    # The account probe. Tests steer it through files beside the fixture:
    # codex-account.json / codex-limits.json replace the responses,
    # codex-probe=exit makes the probe fail, codex-switch-at=N reports
    # another account from the N-th probe on, and codex-mutate-resources-at=N
    # edits the bound profile during the N-th probe.
    probes=root/'codex-probes'
    n=int(probes.read_text())+1 if probes.exists() else 1
    probes.write_text(str(n))
    if (root/'codex-probe').exists() and (root/'codex-probe').read_text()=='exit': sys.exit(1)
    account=json.loads((root/'codex-account.json').read_text()) if (root/'codex-account.json').exists() else {'type':'chatgpt','planType':'plus','email':'fixture@example.invalid'}
    switch=root/'codex-switch-at'
    if switch.exists() and n>=int(switch.read_text()): account=dict(account,email='other@example.invalid')
    mutate=root/'codex-mutate-resources-at'
    if mutate.exists() and n==int(mutate.read_text()):
        resources=root/'state'/'resources.yml'
        resources.write_text(resources.read_text().replace('effort: low','effort: medium'))
    limits=json.loads((root/'codex-limits.json').read_text()) if (root/'codex-limits.json').exists() else {'rateLimitsByLimitId':{}}
    for line in sys.stdin:
        if '"id":0' in line:
            print(json.dumps({'id':0,'result':{'userAgent':'fixture'}}), flush=True)
        elif '"id":1' in line:
            print(json.dumps({'id':1,'result':{'account':account}}), flush=True)
        elif '"id":2' in line:
            print(json.dumps({'id':2,'result':limits}), flush=True)
            break
    raise SystemExit(0)
prompt=sys.stdin.read()
with (root/'invocations').open('a') as f: f.write('invocation\\n')
count=len((root/'invocations').read_text().splitlines())
mode=(root/'mode').read_text()
if mode=='wait':
    try:
        fd=os.open(root/'ready',os.O_WRONLY|os.O_NONBLOCK);os.write(fd,b'R');os.close(fd)
    except OSError: pass
    with (root/'barrier').open('rb',buffering=0) as gate: gate.read(1)
if mode=='noisy':
    for i in range(30000): print('x'*1024)
    sys.stdout.flush()
    try:
        fd=os.open(root/'ready',os.O_WRONLY|os.O_NONBLOCK);os.write(fd,b'R');os.close(fd)
    except OSError: pass
    with (root/'barrier').open('rb',buffering=0) as gate: gate.read(1)
if mode in ('clarify','unclassified') and count==1:
    checkpoint={'version':1,'question':'Which label?','choices':['blue','green']}
    if mode=='clarify': checkpoint['category']='factual'
    print(json.dumps({'type':'item.completed','item':{'type':'agent_message','text':json.dumps({'dispatch_checkpoint':checkpoint})}}))
else:
    pathlib.Path('src/lib.rs').write_text('// delivered\\n')
    pathlib.Path('result.txt').write_text('ok\\n')
    print(json.dumps({'type':'item.completed','item':{'type':'agent_message','text':'Done'}}))
''')
        if self.provider == 'claude':
            os.environ.setdefault('USER', 'dispatch-fixture-user')
            # version-sleep delays the version answer by that many seconds and
            # version-text replaces it; auth-sleep delays auth status.
            script = agent.read_text().replace("print('codex phase5 fixture');sys.exit()", """import time
    if (root/'version-sleep').exists(): time.sleep(float((root/'version-sleep').read_text()))
    print((root/'version-text').read_text() if (root/'version-text').exists() else 'claude phase6 fixture');sys.exit()""")
            script = script.replace('prompt=sys.stdin.read()', """if 'auth' in sys.argv and 'status' in sys.argv:
    assert sys.argv[-4:]==['--no-chrome','auth','status','--json']
    assert os.environ.get('USER'), 'Keychain lookup needs username metadata'
    count_path=root/'auth-count'
    n=int(count_path.read_text())+1 if count_path.exists() else 1
    count_path.write_text(str(n))
    if (root/'auth-sleep').exists():
        import time; time.sleep(float((root/'auth-sleep').read_text()))
    boundary=root/'auth-boundary'
    if boundary.exists() and n==int(boundary.read_text()):
        auth=json.loads((root/'auth.json').read_text());auth['email']='changed@example.invalid'
        (root/'auth.json').write_text(json.dumps(auth))
    print((root/'auth.json').read_text());sys.exit()
assert '--dangerously-skip-permissions' not in sys.argv and '--bare' not in sys.argv
assert os.environ.get('USER'), 'model invocation must use the same Keychain environment'
assert sys.argv[sys.argv.index('--setting-sources')+1]==''
assert sys.argv[sys.argv.index('--permission-mode')+1]=='dontAsk'
assert sys.argv[sys.argv.index('--tools')+1]=='Bash,Read,Edit,Write,Glob,Grep'
assert not any(k in os.environ for k in ['ANTHROPIC_API_KEY','ANTHROPIC_AUTH_TOKEN','CLAUDE_CODE_USE_BEDROCK','DISPATCH_CONTROL_GRANT_FD'])
prompt=sys.argv[-1]
def emit(text):
    model=sys.argv[sys.argv.index('--model')+1]
    model_usage={model:{'inputTokens':10,'outputTokens':4}}
    settings=json.loads(sys.argv[sys.argv.index('--settings')+1])
    # Reproduce the real CLI's extra title-generation model unless disabled.
    if settings.get('env',{}).get('CLAUDE_CODE_DISABLE_TERMINAL_TITLE')!='1':
        model_usage['claude-haiku-4-5-20251001']={'inputTokens':1095,'outputTokens':13}
    print(json.dumps({'type':'system','subtype':'init','model':model}))
    print(json.dumps({'type':'result','subtype':'success','is_error':False,'result':text,
        'usage':{'input_tokens':10,'output_tokens':4,'cache_read_input_tokens':6,'cache_creation_input_tokens':2},
        'total_cost_usd':0.42,'modelUsage':model_usage}))
""")
            script = script.replace("print(json.dumps({'type':'item.completed','item':{'type':'agent_message','text':json.dumps({'dispatch_checkpoint':checkpoint})}}))", "emit(json.dumps({'dispatch_checkpoint':checkpoint}))")
            script = script.replace("print(json.dumps({'type':'item.completed','item':{'type':'agent_message','text':'Done'}}))", "emit('Done')")
            agent.write_text(script)
            (self.root/'auth.json').write_text(json.dumps({'loggedIn':True,'authMethod':'claude.ai','apiProvider':'firstParty','email':'fixture@example.invalid','orgId':'fixture-org'}))
            home = self.root/'home'
            home.mkdir()
            os.environ['HOME'] = str(home)
        agent.chmod(0o755)
        (self.source / 'dispatch.yml').write_text(
            f"execution:\n  timeout_secs: 30\nchecks:\n  verify: ['test -f result.txt']\nharnesses:\n  {self.provider}:\n    executable: '{agent}'\n")
        (self.state / 'resources.yml').write_text('''version: 1
allocation_enabled: true
capacity:
  codex_probe: false
profiles:
  - provider: openai
    funding_source: chatgpt-plus
    harness: codex
    model: fixture-model
    effort: low
    runtime: local
    service_mode: standard
    pool: shared
    provider_buckets: [codex]
    tier: light
    included: true
    no_overage_verified: true
    authorization_revision: 1
''')
        if self.provider == 'codex':
            with (self.state / 'resources.yml').open('a') as resources:
                resources.write('    codex_account: {"account_sha256":"cc6d96611cffa9f02c3626f0b9ee897dc171e2d540a5cae349d4ec316104997b","checked_at":"2026-01-01T00:00:00Z"}\n')
        if self.provider == 'claude':
            path = self.state/'resources.yml'
            text = path.read_text().replace('provider: openai','provider: anthropic').replace('funding_source: chatgpt-plus','funding_source: claude-fixture').replace('harness: codex','harness: claude')
            now = datetime.now(timezone.utc)
            evidence = {'contract_version':1,'cli_version':'claude phase6 fixture',
                'executable_sha256':hashlib.sha256(agent.read_bytes()).hexdigest(),
                'account_sha256':hashlib.sha256(json.dumps(['fixture@example.invalid','fixture-org'],separators=(',',':')).encode()).hexdigest(),
                'checked_at':now.isoformat(),'valid_until':(now+timedelta(hours=1)).isoformat(),
                'print_mode_included':True,'usage_credits_disabled':True,'unmanaged_account':True}
            text += '    claude_subscription: '+json.dumps(evidence)+'\n'
            path.write_text(text)

    def command(self, *args, check=True):
        return subprocess.run([self.binary, '--state-dir', str(self.state), *map(str,args)],
                              capture_output=True, check=check)

    def count(self):
        return len(self.invocations.read_text().splitlines()) if self.invocations.exists() else 0

    def stored(self, run_id):
        with sqlite3.connect(self.state / 'dispatch.db') as db:
            return json.loads(db.execute('SELECT run_projection_json FROM runs WHERE id=?', (run_id,)).fetchone()[0])

    def cleanup(self):
        if sys.exc_info()[0] is not None:
            for metadata in (self.state/'runs').glob('*/metadata.json'):
                run=json.loads(metadata.read_text())
                print('fixture failure:', run.get('outcome'),
                      [c.get('error') for c in run.get('candidates',[])], file=sys.stderr)
        self.temp.cleanup()
