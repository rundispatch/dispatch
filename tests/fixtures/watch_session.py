#!/usr/bin/env python3
"""`dispatch watch` on a terminal: act on Work without typing its ID. Work is
named, not numbered; reject and finish ask first, naming the Work (Enter
cancels); accept applies, and a stale accept is refused with what to do next;
Work that appears while watching shows up without taking the selection; q
leaves. Effects are checked in the recorded state, and what the person sees is
checked on the screen as a terminal would show it."""
import codecs, json, os, re, subprocess, sys, tempfile, time
from pathlib import Path
sys.dont_write_bytecode=True
from review_refinement import Session, WAIT, has_color
binary=str(Path(sys.argv[1]).resolve())

CSI=re.compile(r'\x1b\[([0-?]*)[ -/]*([@-~])')
class Screen:
    """Just enough of a terminal to read what it shows: Dispatch redraws only
    the cells that changed, so the byte stream alone can split words."""
    def __init__(self,ui):
        self.ui=ui; self.width,self.height=ui.width,ui.height
        self.cells=[[' ']*self.width for _ in range(self.height)]
        self.x=self.y=0; self.fed=0; self.tail=''
        self.decoder=codecs.getincrementaldecoder('utf-8')('replace')
    def resize(self,width,height):
        self.update()
        self.cells=[(row+[' ']*width)[:width] for row in (self.cells+[[' ']*width]*height)[:height]]
        self.width,self.height=width,height; self.x=min(self.x,width-1); self.y=min(self.y,height-1)
        self.ui.resize(width,height)
    def scroll(self,n):
        for _ in range(n): self.cells.pop(0); self.cells.append([' ']*self.width)
    def erase(self,y,start,end):
        self.cells[y][start:end]=[' ']*(end-start)
    def csi(self,params,final):
        if params.startswith('?'): return
        p=[int(v) if v.isdigit() else 0 for v in params.split(';')]
        n=max(p[0],1)
        if final in 'Hf': self.y=n-1; self.x=max(p[1],1)-1 if len(p)>1 else 0
        elif final=='A': self.y-=n
        elif final=='B': self.y+=n
        elif final=='C': self.x+=n
        elif final=='D': self.x-=n
        elif final=='G': self.x=n-1
        elif final=='d': self.y=n-1
        elif final=='E': self.y+=n; self.x=0
        elif final=='F': self.y-=n; self.x=0
        elif final=='J':
            rows={0:range(self.y+1,self.height),1:range(0,self.y)}.get(p[0],range(self.height))
            for y in rows: self.erase(y,0,self.width)
            if p[0]==0: self.erase(self.y,self.x,self.width)
            if p[0]==1: self.erase(self.y,0,self.x+1)
        elif final=='K': self.erase(self.y,*{0:(self.x,self.width),1:(0,self.x+1)}.get(p[0],(0,self.width)))
        elif final=='S': self.scroll(n)
        elif final=='T':
            for _ in range(n): self.cells.pop(); self.cells.insert(0,[' ']*self.width)
        self.x=min(max(self.x,0),self.width-1); self.y=min(max(self.y,0),self.height-1)
    def update(self):
        text=self.tail+self.decoder.decode(bytes(self.ui.output[self.fed:])); self.fed=len(self.ui.output); self.tail=''
        i=0
        while i<len(text):
            c=text[i]
            if c=='\x1b':
                kind=text[i+1:i+2]
                if kind=='[':
                    found=CSI.match(text,i)
                    if not found: self.tail=text[i:]; return
                    self.csi(found.group(1),found.group(2)); i=found.end(); continue
                if kind in (']','P','_'):
                    ends=[e for e in (text.find('\x07',i),text.find('\x1b\\',i)) if e>=0]
                    if not ends: self.tail=text[i:]; return
                    i=min(ends)+1+(text[min(ends)]=='\x1b'); continue
                if not kind: self.tail=text[i:]; return
                i+=2; continue
            if c=='\r': self.x=0
            elif c=='\n':
                if self.y==self.height-1: self.scroll(1)
                else: self.y+=1
            elif c=='\b': self.x=max(self.x-1,0)
            elif c>=' ' and self.x<self.width: self.cells[self.y][self.x]=c; self.x+=1
            i+=1
    def rows(self):
        self.update(); return [''.join(row) for row in self.cells]
    def see(self,*texts,timeout=WAIT):
        """Wait until every text is on screen, each on one row; the rows."""
        deadline=time.monotonic()+timeout
        while True:
            self.ui.pump(); rows=self.rows()
            if all(any(t in row for row in rows) for t in texts): return rows
            if time.monotonic()>deadline or self.ui.process.poll() is not None:
                raise AssertionError('not on screen: %r\n%s'%(texts,'\n'.join(rows)))
    def row(self,text):
        return next(row for row in self.rows() if text in row)

def short(this,*others):
    """The Work's ID as the view shortens it among `others`."""
    shared=max(len(os.path.commonprefix([this,other])) for other in others)
    return this[:max(shared+1,8)]

with tempfile.TemporaryDirectory(prefix='dispatch-watch-') as tmp:
    root=Path(tmp); source=root/'project'; source.mkdir(); state=root/'state'
    (source/'dispatch.yml').write_text('coherence:\n  poll_secs: 1\n')
    (source/'notes.txt').write_text('hello\n')
    def run(task,where=source):
        out=subprocess.run([binary,'--state-dir',str(state),'run',str(where),'--allow-unsafe-local',
            '--agent','fake-good','--task',task,'--json'],capture_output=True,check=True)
        return json.loads(out.stdout)['run_id']
    def meta(run_id):return json.loads((state/'runs'/run_id/'metadata.json').read_text())
    def until(ui,check,what):
        for _ in range(300):
            if check():return
            ui.pump(.1)
        raise AssertionError('never '+what)
    first=run('Create the fake artifact')
    captures=str(root/'captures')
    with Session([binary,'--state-dir',str(state),'watch','--root',str(source)],source,captures,'watch',width=160,height=30) as ui:
        screen=Screen(ui)
        # Work is named by its task; its ID is in the details, its S0 nowhere.
        rows=screen.see('› Create the fake artifact','Work '+first[:8],'leave')
        assert not any('S0' in row or 'Shift+Tab' in row for row in rows),rows
        # Reject asks first, naming the Work; Enter alone cancels.
        ui.send('r');screen.see('Reject Create the fake artifact (Work %s)?'%first[:8],'Cancel')
        ui.send('\r');screen.see('Create the fake artifact: nothing changed.');ui.pump(.3)
        assert meta(first)['outcome']['review']=='pending','Enter alone rejected'
        ui.send('r');screen.see('Cancel');ui.send('\x1b[A');ui.pump(.3);ui.send('\r')
        until(ui,lambda:meta(first)['outcome']['review']=='rejected','rejected')
        screen.see('Create the fake artifact: rejected; nothing was applied.')
        # Work that appears while watching shows up first, since it needs a
        # decision; the selection stays on the Work it was on.
        second=run('Write a second artifact')
        rows=screen.see('Write a second artifact','› Create the fake artifact')
        assert screen.row('Write a second artifact')!=screen.row('Create the fake artifact')
        assert rows.index(screen.row('Write a second artifact'))<rows.index(screen.row('Create the fake artifact')),rows
        # Its review has no auto-apply mode to turn on: `aa` changes nothing,
        # and neither does Shift+Tab.
        ui.send('k');screen.see('› Write a second artifact');ui.send('d');screen.see('[aa]')
        ui.send('\x1b[Z');ui.send('aa\r');screen.see('watch has no auto-apply mode; nothing changed.')
        assert meta(second)['outcome']['review']=='pending' and meta(second)['outcome']['application']=='not_applied'
        assert 'Auto-apply on' not in ui.clean and 'auto-apply on' not in ui.clean
        ui.send('\x1b');screen.see('› Write a second artifact','a accept')
        # Accept applies it.
        ui.send('a')
        until(ui,lambda:meta(second)['outcome']['application']=='applied','applied')
        screen.see('Write a second artifact: accepted and applied.')
        assert (source/'dispatch-fake-good.txt').exists()
        ui.send('q');ui.finish()
    # Finishing work that carries no authority asks before running the
    # project's checks, naming the Work by its worktree's folder; Enter alone
    # cancels.
    repo=root/'repo'; repo.mkdir()
    (repo/'dispatch.yml').write_text("coherence:\n  poll_secs: 1\nchecks:\n  verify: ['true']\n")
    (repo/'lib.txt').write_text('one\n')
    git=lambda *a:subprocess.run(['git','-C',str(repo),'-c','user.name=T','-c','user.email=t@e.invalid',*a],check=True,capture_output=True)
    git('init','-q');git('add','-A');git('commit','-qm','init');git('worktree','add','-q','-b','wt','.claude/worktrees/auth-ctx')
    worktree=(repo/'.claude/worktrees/auth-ctx').resolve()
    subprocess.run([binary,'--state-dir',str(state),'start','--root',str(repo)],check=True,capture_output=True)
    try:
        hook=json.dumps({'hook_event_name':'SessionStart','session_id':'s1','source':'startup','cwd':str(worktree)})
        subprocess.run([binary,'--state-dir',str(state),'hook','claude'],input=hook.encode(),check=True,capture_output=True)
        found=[p.parent.name for p in (state/'runs').glob('*/metadata.json') if json.loads(p.read_text()).get('attachment',{}) and json.loads(p.read_text())['attachment']['workspace']==str(worktree)]
        assert len(found)==1,found; work=found[0]
        (worktree/'lib.txt').write_text('two\n')
        with Session([binary,'--state-dir',str(state),'watch','--root',str(repo)],repo,captures,'watch-finish',width=160,height=30) as ui:
            screen=Screen(ui)
            screen.see('› auth-ctx','auth-ctx · claude session in its own worktree · .claude/worktrees/auth-ctx · Work '+work[:8],'f finish')
            ui.send('f');screen.see('Finish auth-ctx (Work %s)?'%work[:8],'Cancel');ui.send('\r')
            screen.see('auth-ctx: nothing changed.');ui.pump(.5)
            assert meta(work)['outcome']['lifecycle']=='working','Enter alone ran the checks'
            ui.send('f');screen.see('Cancel');ui.send('\x1b[A');ui.pump(.3);ui.send('\r')
            until(ui,lambda:meta(work)['outcome']['work_result']=='ready','finished')
            assert meta(work)['outcome']['verification']=='passed'
            screen.see('auth-ctx: finished; checks passed. It now waits for your review.')
            ui.send('q');ui.finish()
    finally:
        subprocess.run([binary,'--state-dir',str(state),'stop','--root',str(repo)],capture_output=True)
    # dispatch clean asks before removing a workspace Dispatch made; Enter alone
    # removes nothing.
    subprocess.run([binary,'--state-dir',str(state),'attach','--allow-unsafe-local','--','sh','-c','echo made > lib.txt'],
        cwd=repo,stdin=subprocess.DEVNULL,check=True,capture_output=True)
    made=[p.parent.name for p in (state/'runs').glob('*/metadata.json')
          if (json.loads(p.read_text()).get('attachment') or {}).get('workspace_owner')=='dispatch']
    assert len(made)==1,made; made=made[0]
    subprocess.run([binary,'--state-dir',str(state),'reject',made],check=True,capture_output=True)
    kept=Path(meta(made)['attachment']['workspace']); assert kept.exists()
    with Session([binary,'--state-dir',str(state),'clean'],repo,captures,'clean-cancel',width=160,height=30) as ui:
        ui.wait('Cancel');ui.send('\r');ui.wait('Nothing was removed');ui.finish()
    assert kept.exists(),'Enter alone removed a workspace'
    with Session([binary,'--state-dir',str(state),'clean'],repo,captures,'clean-remove',width=160,height=30) as ui:
        ui.wait('Cancel');ui.send('\x1b[A');ui.pump(.3);ui.send('\r');ui.wait('Removed 1');ui.finish()
    assert not kept.exists()
    # Work that touches other Work names it in its row, and the selected
    # Work's details say how, naming both, apart from its verdict.
    proj=root/'interacting'; proj.mkdir(); (proj/'src').mkdir()
    (proj/'dispatch.yml').write_text('coherence:\n  poll_secs: 1\n')
    (proj/'src/auth.rs').write_text('pub fn validate(token: &str) -> bool {\n    !token.is_empty()\n}\n')
    (proj/'src/api.rs').write_text('pub fn serve() {}\n')
    pgit=lambda *a:subprocess.run(['git','-C',str(proj),'-c','user.name=T','-c','user.email=t@e.invalid',*a],check=True,capture_output=True)
    pgit('init','-q');pgit('add','-A');pgit('commit','-qm','init')
    ids=[]
    for name in ('wt-a','wt-b'):
        pgit('worktree','add','-q','-b',name,str(root/name))
        out=subprocess.run([binary,'--state-dir',str(state),'attach','--workspace',str(root/name),'--agent',name],cwd=proj,check=True,capture_output=True).stdout.decode()
        ids.append(next(l.split()[1] for l in out.splitlines() if l.startswith('ATTACHED ')))
    (root/'wt-a/src/auth.rs').write_text('pub fn validate(token: &str, strict: bool) -> bool {\n    !token.is_empty()\n}\n')
    (root/'wt-b/src/api.rs').write_text('pub fn serve() {}\n\npub fn login(t: &str) -> bool {\n    validate(t)\n}\n')
    subprocess.run([binary,'--state-dir',str(state),'start','--root',str(proj)],check=True,capture_output=True)
    try:
        with Session([binary,'--state-dir',str(state),'watch','--root',str(proj)],proj,captures,'watch-interactions',width=200,height=30) as ui:
            screen=Screen(ui)
            said='wt-a changes the signature of validate (src/auth.rs), which wt-b uses'
            screen.see('› wt-a',said,'Advisory: nothing is held back.')
            assert 'wt-b' in screen.row('› wt-a').split('wt-a',1)[1],screen.rows()
            ui.send('j');screen.see('› wt-b',said)
            assert 'wt-a' in screen.row('› wt-b').split('wt-b',1)[1],screen.rows()
            ui.send('q');ui.finish()
    finally:
        subprocess.run([binary,'--state-dir',str(state),'stop','--root',str(proj)],capture_output=True)
    # Stale Work at any width: its name and verdict are on screen; resizing
    # keeps the selection on the same Work; accepting it is refused, naming
    # it; and NO_COLOR means no color at all.
    tiny=root/'tinyauth'; tiny.mkdir()
    (tiny/'dispatch.yml').write_text('coherence:\n  poll_secs: 1\n')
    (tiny/'notes.txt').write_text('hello\n')
    stale=run('Make the first artifact',tiny)
    subprocess.run([binary,'--state-dir',str(state),'start','--root',str(tiny)],check=True,capture_output=True)
    try:
        (tiny/'dispatch-fake-good.txt').write_text("someone else's\n")
        deadline=time.monotonic()+WAIT
        while ((meta(stale).get('coherence') or {}).get('validity') or {}).get('decision')!='refresh':
            assert time.monotonic()<deadline,'the owner never recorded REFRESH'
            time.sleep(.1)
        other=run('Make the second artifact',tiny)
        with Session([binary,'--state-dir',str(state),'watch','--root',str(tiny)],tiny,captures,'watch-stale',env={'NO_COLOR':'1'},width=200,height=30) as ui:
            screen=Screen(ui)
            screen.see('› Make the first artifact','Make the second artifact')
            for width in (60,80,120,200):
                screen.resize(width,30)
                screen.see('Make the first artifact','Make the second artifact','q leave')
                assert 'REFRESH' in screen.row('Make the first artifact'),(width,screen.rows())
            ui.send('j');screen.see('› Make the second artifact')
            screen.resize(80,30);screen.see('› Make the second artifact')
            ui.send('r');screen.see('Reject Make the second artifact (Work %s)?'%short(other,stale))
            ui.send('\x1b');screen.see('Make the second artifact: nothing changed.')
            screen.resize(200,30);ui.send('k');screen.see('› Make the first artifact','r reject · d review')
            ui.send('a');screen.see('Make the first artifact: not applied: stale (REFRESH). The source is unchanged. Next: r reject it.')
            # Refused: nothing applied, and the refusal is recorded as such.
            assert meta(stale)['outcome']['application']=='blocked_by_source_drift',meta(stale)['outcome']
            assert (tiny/'dispatch-fake-good.txt').read_text()=="someone else's\n"
            ui.send('q');ui.finish()
            assert not has_color(bytes(ui.output)),'NO_COLOR drew color'
    finally:
        subprocess.run([binary,'--state-dir',str(state),'stop','--root',str(tiny)],capture_output=True)
    print('watch journeys passed')
