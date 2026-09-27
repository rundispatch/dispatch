#!/usr/bin/env python3
"""`dispatch watch` on a terminal: act on Work without typing its ID. Reject
asks first (Enter cancels); accept applies; Work that appears while watching
shows up; q leaves. Effects are checked in the recorded state, not the screen."""
import json, subprocess, sys, tempfile
from pathlib import Path
sys.dont_write_bytecode=True
from review_refinement import Session
binary=str(Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix='dispatch-watch-') as tmp:
    root=Path(tmp); source=root/'project'; source.mkdir(); state=root/'state'
    (source/'dispatch.yml').write_text('coherence:\n  poll_secs: 1\n')
    (source/'notes.txt').write_text('hello\n')
    def run():
        out=subprocess.run([binary,'--state-dir',str(state),'run',str(source),'--allow-unsafe-local',
            '--agent','fake-good','--task','Create the fake artifact.','--json'],capture_output=True,check=True)
        return json.loads(out.stdout)['run_id']
    def meta(run_id):return json.loads((state/'runs'/run_id/'metadata.json').read_text())
    def until(ui,check,what):
        # Redraws send only what changed, so the screen can split words; the
        # recorded state is what matters.
        for _ in range(300):
            if check():return
            ui.pump(.1)
        raise AssertionError('never '+what)
    first=run()
    captures=str(root/'captures')
    with Session([binary,'--state-dir',str(state),'watch','--root',str(source)],source,captures,'watch',width=160,height=30) as ui:
        ui.wait(first[:8]);ui.wait('leave')
        # Reject asks first; Enter alone cancels.
        ui.send('r');ui.wait('Cancel');ui.send('\r');ui.wait('Nothing');ui.pump(.3)
        assert meta(first)['outcome']['review']=='pending','Enter alone rejected'
        ui.send('r');ui.wait('Cancel');ui.send('\x1b[A');ui.pump(.3);ui.send('\r')
        until(ui,lambda:meta(first)['outcome']['review']=='rejected','rejected')
        # Work that appears while watching shows up; accept applies it.
        second=run();ui.wait(second[:8]);ui.send('j');ui.pump(.3);ui.send('a')
        until(ui,lambda:meta(second)['outcome']['application']=='applied','applied')
        assert (source/'dispatch-fake-good.txt').exists()
        ui.send('q');ui.finish()
    # Finishing work that carries no authority asks before running the
    # project's checks; Enter alone cancels.
    repo=root/'repo'; repo.mkdir()
    (repo/'dispatch.yml').write_text("coherence:\n  poll_secs: 1\nchecks:\n  verify: ['true']\n")
    (repo/'lib.txt').write_text('one\n')
    git=lambda *a:subprocess.run(['git','-C',str(repo),'-c','user.name=T','-c','user.email=t@e.invalid',*a],check=True,capture_output=True)
    git('init','-q');git('add','-A');git('commit','-qm','init');git('worktree','add','-q','-b','wt','.claude/worktrees/x')
    worktree=(repo/'.claude/worktrees/x').resolve()
    subprocess.run([binary,'--state-dir',str(state),'start','--root',str(repo)],check=True,capture_output=True)
    try:
        hook=json.dumps({'hook_event_name':'SessionStart','session_id':'s1','source':'startup','cwd':str(worktree)})
        subprocess.run([binary,'--state-dir',str(state),'hook','claude'],input=hook.encode(),check=True,capture_output=True)
        found=[p.parent.name for p in (state/'runs').glob('*/metadata.json') if json.loads(p.read_text()).get('attachment',{}) and json.loads(p.read_text())['attachment']['workspace']==str(worktree)]
        assert len(found)==1,found; work=found[0]
        (worktree/'lib.txt').write_text('two\n')
        with Session([binary,'--state-dir',str(state),'watch','--root',str(repo)],repo,captures,'watch-finish',width=160,height=30) as ui:
            ui.wait(work[:8]);ui.wait('leave')
            ui.send('f');ui.wait('Cancel');ui.send('\r');ui.pump(.5)
            assert meta(work)['outcome']['lifecycle']=='working','Enter alone ran the checks'
            ui.send('f');ui.wait('Cancel');ui.send('\x1b[A');ui.pump(.3);ui.send('\r')
            until(ui,lambda:meta(work)['outcome']['work_result']=='ready','finished')
            assert meta(work)['outcome']['verification']=='passed'
            ui.send('q');ui.finish()
    finally:
        subprocess.run([binary,'--state-dir',str(state),'stop','--root',str(repo)],capture_output=True)
    print('watch journeys passed')
