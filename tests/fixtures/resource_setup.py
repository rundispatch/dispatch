#!/usr/bin/env python3
"""Isolated CLI/TUI setup journeys. Only synthetic provider auth is invoked."""
import json, os, sys, tempfile, subprocess, hashlib, re
from pathlib import Path
sys.dont_write_bytecode=True
from review_refinement import Session
binary=str(Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix='dispatch-setup-') as tmp:
    root=Path(tmp); source=root/'project'; source.mkdir(); home=root/'home';home.mkdir();bins=root/'bin';bins.mkdir();state=root/'state'
    (source/'main.c').write_text('int main(void) { return 0; }\n')
    (root/'auth.json').write_text(json.dumps({'loggedIn':True,'authMethod':'claude.ai','apiProvider':'firstParty','email':'fixture@example.invalid','orgId':'fixture'}))
    for provider in ('codex','claude'):
        script='''#!/usr/bin/env python3
import sys,json,pathlib,os
root=pathlib.Path(__file__).resolve().parent.parent
with (root/'probe-args').open('a') as log:log.write(json.dumps(sys.argv[1:])+'\\n')
if '--version' in sys.argv: print('setup fixture 1');sys.exit()
if 'login' in sys.argv:
    assert not any(k in os.environ for k in ('OPENAI_API_KEY','ANTHROPIC_API_KEY','DISPATCH_CONTROL_GRANT_FD'))
    print('Fixture device login - press Enter',flush=True);input();sys.exit()
if 'auth' in sys.argv and 'status' in sys.argv: print((root/'auth.json').read_text());sys.exit()
assert 'app-server' in sys.argv, 'MODEL INVOCATION FORBIDDEN'
for line in sys.stdin:
    q=json.loads(line)
    if 'id' not in q:continue
    if q['method']=='initialize':r={'userAgent':'setup fixture'}
    elif q['method']=='account/read':r={'account':{'type':'chatgpt','planType':'plus','email':'fixture@example.invalid'}}
    elif q['method']=='model/list':r={'data':[{'id':'fixture-codex-model','hidden':False,'isDefault':True,'defaultReasoningEffort':'medium','supportedReasoningEfforts':[{'reasoningEffort':'low'},{'reasoningEffort':'medium'},{'reasoningEffort':'ultra'}]},{'id':'hidden-fixture-model','hidden':True,'isDefault':False,'supportedReasoningEfforts':[]}]}
    else:r={}
    print(json.dumps({'id':q['id'],'result':r}),flush=True)
'''
        (bins/provider).write_text(script);(bins/provider).chmod(0o755)
    env={'HOME':str(home),'PATH':str(bins)+os.pathsep+os.environ['PATH'],'USER':'dispatch-fixture','DISPATCH_COLOR':'truecolor','COLORTERM':'truecolor','NO_COLOR':None,'OPENAI_API_KEY':'not-a-real-key','ANTHROPIC_API_KEY':'not-a-real-key'}
    captures=os.environ.get('DISPATCH_SETUP_CAPTURES',str(root/'captures'))
    def session(name,args=None):return Session([binary,'--state-dir',str(state)]+(args or []),source,captures,name,env=env,width=100,height=36)
    # Setup is chosen, not typed. Rows with no profiles: 1 Add Claude Code,
    # 2 Add Codex, 3 Provider login…, 4 Runtime integrations…, 5 Back. A digit
    # moves focus; Enter chooses.
    with session('fresh') as ui:
        ui.wait('accomplish?');ui.send('Update main.c safely\r');ui.wait('Protect work I run myself');ui.mark('first-goal')
        ui.send('\r');ui.wait('Add Codex');ui.mark('missing')
        ui.send('2\r');ui.wait('Other model ID');ui.mark('models');ui.send('\r')
        ui.wait('Effort for fixture-codex-model');ui.send('\r')
        # Focus starts on Cancel: Enter alone never authorizes.
        ui.wait('Authorize this resource?');ui.mark('funding');ui.send('\r');ui.wait('Not authorized');assert not state.exists()
        for absent in ('hidden-fixture-model','ultra','Resource tier','Type confirm'):
            assert absent not in ui.clean, absent
        ui.wait('Add Codex');ui.send('5\r');ui.wait('accomplish?');ui.mark('preserved');ui.pump(.1)
        assert 'Update main.c safely' in ui.clean
        ui.send(b'\x03');ui.finish()
    # Observing needs no setup: the second choice explains attach and serve,
    # changes nothing and returns to the preserved goal.
    with session('observe') as ui:
        ui.wait('accomplish?');ui.send('Update main.c safely\r');ui.wait('Protect work I run myself')
        ui.send('2\r');ui.wait('no setup needed');ui.wait('dispatch attach --workspace');ui.wait('accomplish?');ui.mark('observe')
        ui.send(b'\x03');ui.finish()
    assert not state.exists()
    # A new resource: the listed (or suggested) model, its default effort, Authorize.
    with session('cli-codex',['setup','codex']) as ui:
        ui.wait('Other model ID');ui.send('\r');ui.wait('Effort for');ui.send('\r')
        ui.wait('Authorize this resource?');ui.mark('confirm');ui.send('1\r');ui.wait('Resource saved')
        ui.wait('Revalidate codex');ui.send('6\r');ui.finish()
    with session('cli-claude',['setup','claude']) as ui:
        ui.wait('claude-sonnet-5');ui.send('\r');ui.wait('Effort for claude-sonnet-5');ui.send('\r')
        ui.wait('Authorize this resource?');ui.send('1\r');ui.wait('Resource saved')
        ui.wait('Revalidate claude');ui.send('7\r');ui.finish()
    original=(state/'resources.yml').read_bytes();config=original.decode()
    assert config.count('provider: ')==2
    assert 'model: fixture-codex-model' in config and 'model: claude-sonnet-5' in config
    assert config.count('effort: medium')==2 and config.count('tier: standard')==2
    assert 'print_mode_included: true' in config and re.search(r'account_sha256: [a-f0-9]{64}',config)
    # Rows now: 1 Add Claude Code, 2 Add Codex, 3 codex profile, 4 claude profile,
    # 5 Provider login…, 6 Runtime integrations…, 7 Back. Expired evidence is shown and can be refreshed;
    # cancellation never renews it.
    config=re.sub(r'valid_until: [^\n]+','valid_until: 2020-01-01T00:00:00Z',config)
    (state/'resources.yml').write_text(config);expired=(state/'resources.yml').read_bytes()
    with session('revalidate',['setup']) as ui:
        ui.wait('needs revalidation');ui.mark('expired');ui.send('4\r');ui.wait('Authorize this resource?');ui.send('\r');ui.wait('Not authorized')
        assert (state/'resources.yml').read_bytes()==expired
        ui.wait('Add Codex');ui.send('4\r');ui.wait('Authorize this resource?');ui.mark('refresh');ui.send('1\r');ui.wait('Resource saved')
        ui.wait('expires in');ui.send('7\r');ui.finish()
    refreshed=(state/'resources.yml').read_bytes();assert max(map(int,re.findall(rb'authorization_revision: (\d+)',refreshed)))>2
    auth=json.loads((root/'auth.json').read_text());auth['email']='different@example.invalid';(root/'auth.json').write_text(json.dumps(auth))
    with session('account-change',['setup']) as ui:
        ui.wait('Add Codex');ui.send('4\r');ui.wait('account changed');ui.mark('blocked');ui.wait('Add Codex');ui.send('7\r');ui.finish()
    assert (state/'resources.yml').read_bytes()==refreshed
    auth['authMethod']='api';(root/'auth.json').write_text(json.dumps(auth))
    with session('unsupported',['setup','claude']) as ui:
        ui.wait('Resource unchanged');ui.wait('Add Codex');ui.send('7\r');ui.finish()
    # Login: choose the provider, then Open (focused) or Cancel; never typed.
    with session('login',['setup']) as ui:
        ui.wait('Add Codex');ui.send('5\r');ui.wait('Provider login changes');ui.send('2\r')
        ui.wait('Open codex login');ui.send('\r');ui.wait('Fixture device login');ui.send('\r');ui.wait('Add Codex');ui.mark('return');ui.send('7\r');ui.finish()
    assert (state/'resources.yml').read_bytes()==refreshed
    (source/'verify.sh').write_text('exit 0\n')
    # Checks are chosen, never typed: arrows move, Enter chooses the focused row.
    # Rows: 1 sh ./verify.sh, 2 Other command…, 3 Continue without checks, 4 Back.
    with session('checks-skip',['setup','--checks']) as ui:
        ui.wait('Continue without checks');ui.mark('choices');ui.send('\x1b[B');ui.pump(.2);ui.send('\x1b[B');ui.pump(.3);ui.send('\r');ui.finish()
    assert not (source/'dispatch.yml').exists()
    # A command the person types is saved as typed.
    with session('checks-other',['setup','--checks']) as ui:
        ui.wait('Other command');ui.send('2\r');ui.wait('runs in the project');ui.send('sh ./verify.sh --strict\r')
        ui.wait('Approved check saved');ui.finish()
    assert '- sh ./verify.sh --strict' in (source/'dispatch.yml').read_text()
    (source/'dispatch.yml').unlink()
    # Plain mode numbers the same rows; an empty line chooses the focused row.
    with session('checks-plain',['--plain','setup','--checks']) as ui:
        ui.wait('1) sh ./verify.sh');ui.wait('[1] >');ui.send('\r');ui.wait('Approved check saved');ui.finish()
    assert '- sh ./verify.sh' in (source/'dispatch.yml').read_text()
    # With checks chosen, setup --checks is also where Work may be allowed to
    # run them by themselves: consent for exactly these commands, kept in the
    # state, never the project. Enter on the consent screen cancels.
    consents=lambda: list((state/'projects').glob('*.json')) if (state/'projects').exists() else []
    with session('checks-consent',['--plain','setup','--checks']) as ui:
        ui.wait('asks you before running them');ui.wait('2) Let Work run these checks');ui.send('2\r')
        ui.wait('2) Cancel');ui.mark('consent');ui.send('\r');ui.wait('Nothing changed');ui.finish()
    assert not consents(), 'Enter alone gave consent'
    with session('checks-consent-allow',['--plain','setup','--checks']) as ui:
        ui.wait('2) Let Work run these checks');ui.send('2\r');ui.wait('2) Cancel');ui.send('1\r')
        ui.wait('will run these checks by themselves');ui.finish()
    granted=consents();assert len(granted)==1 and 'sh ./verify.sh' in granted[0].read_text(), granted
    assert 'consent' not in (source/'dispatch.yml').read_text(), 'consent must not be written into the project'
    with session('checks-consent-revoke',['--plain','setup','--checks']) as ui:
        ui.wait('runs them by themselves');ui.wait('2) Stop letting Work');ui.send('2\r');ui.wait('now asks you');ui.finish()
    assert not consents(), 'revoking left consent behind'
    calls=[json.loads(l) for l in (root/'probe-args').read_text().splitlines()]
    assert all('--version' in c or 'app-server' in c or 'status' in c or 'login' in c for c in calls)
    assert not (state/'dispatch.db').exists(), 'setup acquired execution authority'
    # A state that has already been used has a database; setup must still save.
    used=root/'used-state'
    subprocess.run([binary,'--state-dir',str(used),'history'],check=True,capture_output=True,env={k:v for k,v in env.items() if v is not None})
    before=(used/'dispatch.db').read_bytes()
    def used_session(name,args):return Session([binary,'--state-dir',str(used)]+args,source,captures,name,env=env,width=100,height=36)
    with used_session('with-database',['setup','codex']) as ui:
        ui.wait('Other model ID');ui.send('\r');ui.wait('Effort for');ui.send('\r')
        ui.wait('Authorize this resource?');ui.send('1\r');ui.wait('Resource saved');ui.wait('Revalidate codex');ui.send('6\r');ui.finish()
    assert 'provider: openai' in (used/'resources.yml').read_text()
    assert (used/'dispatch.db').read_bytes()==before, 'setup wrote to the database'
    saved=(used/'resources.yml').read_bytes()
    # Other model ID: typed text is validated, then shown verbatim before consent.
    # Rows: 1 fixture-codex-model (configured), 2 Other model ID…
    with used_session('other-model',['setup','codex']) as ui:
        ui.wait('Other model ID');ui.send('2\r');ui.wait('as your provider names it');ui.send('-rf\r');ui.wait('non-option identifier')
        ui.wait('Add Codex');ui.send('2\r');ui.wait('Other model ID');ui.send('2\r');ui.wait('as your provider names it');ui.send('codex-custom-9\r')
        ui.wait('Effort for');ui.send('\r');ui.wait('codex / codex-custom-9 / medium');ui.wait('Authorize this resource?');ui.send('\r')
        ui.wait('Not authorized');ui.wait('Add Codex');ui.send('6\r');ui.finish()
    # Plain mode numbers the same menus; an empty line chooses the focused row,
    # which for consent is Cancel.
    with used_session('plain',['--plain','setup','codex']) as ui:
        ui.wait('1) fixture-codex-model');ui.send('\r');ui.wait('Effort for');ui.wait('[2] >');ui.send('\r')
        ui.wait('2) Cancel');ui.wait('[2] >');ui.send('\r');ui.wait('Not authorized');ui.wait('6) Back');ui.send('6\r');ui.finish()
    assert (used/'resources.yml').read_bytes()==saved
    # Runtime integrations: Claude Code's observation hooks, installed only on
    # a deliberate choice. Rows with one profile: 4 Provider login…,
    # 5 Runtime integrations…, 6 Back. Enter on the consent screen cancels.
    claude_settings=home/'.claude'/'settings.json'
    hooks_env=dict(env,CLAUDE_CONFIG_DIR=str(home/'.claude'))
    # Plain mode, so each menu arrives as whole numbered lines.
    with Session([binary,'--state-dir',str(used),'--plain','setup'],source,captures,'integrations',env=hooks_env,width=100,height=36) as ui:
        ui.wait('5) Runtime integrations');ui.send('5\r');ui.wait('1) Install Claude Code hooks');ui.send('1\r')
        ui.wait('hook claude');ui.wait('2) Cancel');ui.mark('hooks-consent');ui.send('\r')
        ui.wait('6) Back');assert not claude_settings.exists(), 'Enter alone installed hooks'
        ui.send('5\r');ui.wait('1) Install Claude Code hooks');ui.send('1\r');ui.wait('2) Cancel');ui.send('1\r')
        ui.wait('hooks installed');ui.wait('6) Back');ui.send('6\r');ui.finish()
    installed=claude_settings.read_text()
    assert 'hook claude' in installed and str(used) in installed and 'SessionStart' in installed, installed
    print('CLI/TUI setup journeys passed; zero model calls; no database or grants created')
