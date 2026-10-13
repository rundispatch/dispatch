"""Deterministic PTY exercise; no provider calls. Invoked by Rust fixtures."""
import os, sys, pty, select, time, fcntl, termios, struct, signal, sqlite3, json, re, subprocess
binary, source, state, scenario = sys.argv[1:]
master, slave = pty.openpty()
before = termios.tcgetattr(slave)
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 28, 100, 0, 0))
pid = os.fork()
if pid == 0:
    os.setsid()
    fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
    for fd in (0,1,2): os.dup2(slave,fd)
    os.close(master)
    os.chdir(source)
    os.environ['TERM'] = 'xterm-256color'
    args = [binary,'--state-dir',state,'--no-color']
    if scenario == 'plain': args += ['--plain','--ascii']
    if scenario in ('panic','inspection-panic'): args = [binary,'--exact','presenter::tests::terminal_panic_fixture','--ignored','--nocapture']
    if scenario=='inspection-panic': os.environ['DISPATCH_TEST_INSPECTION_PANIC']='1'
    child=subprocess.Popen(args)
    open(os.path.join(state,'pty-'+str(os.getpid())+'-child'),'w').write(str(child.pid))
    code=child.wait()
    with open(os.path.join(state,'pty-'+str(os.getpid())+'-restored.json'),'w') as result:
        attrs=termios.tcgetattr(0)
        json.dump(attrs[:6]+[[v if isinstance(v,int) else list(v) for v in attrs[6]]],result)
    os._exit(code if code>=0 else 128-code)
os.set_blocking(master,False)
transcript=b''
clean=''
position=0

def pump(seconds=.1):
    global transcript,clean
    end=time.time()+seconds
    while time.time()<end:
        if select.select([master],[],[],max(0,end-time.time()))[0]:
            try: chunk=os.read(master,65536)
            except BlockingIOError: continue
            transcript+=chunk
            # Answer cursor queries from the single terminal owner.
            for _ in range(chunk.count(b'\x1b[6n')): os.write(master,b'\x1b[1;1R')
            clean=re.sub(r'\x1b\[[0-?]*[ -/]*[@-~]', ' ',transcript.decode('utf8','replace'))

# Longest wait for any expected screen or state. Waits return as soon as the
# condition holds, so a generous limit only matters on a slow machine; set
# DISPATCH_TEST_WAIT_SECS to change it.
WAIT=float(os.environ.get('DISPATCH_TEST_WAIT_SECS','60'))
def wait(word,timeout=WAIT):
    global position
    end=time.time()+timeout
    while time.time()<end:
        pump()
        index=clean.find(word,position)
        if index>=0:
            position=index+len(word)
            return
    raise AssertionError('waiting for '+word+'\n'+clean[-6000:])

def send(value): os.write(master,value)
def resize(w,h):
    fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',h,w,0,0));os.kill(pid,signal.SIGWINCH);pump(.2)
def runs():
    path=os.path.join(state,'runs')
    return [json.load(open(os.path.join(path,d,'metadata.json'))) for d in os.listdir(path) if os.path.exists(os.path.join(path,d,'metadata.json'))]
try:
    if scenario in ('panic','inspection-panic'): wait('panic-fixture-ready')
    else: wait('accomplish?')
    if scenario=='inspection-panic':
        wait('panic-restored-read');send(b'\n')
    elif scenario=='panic': pass
    elif scenario=='eof': send(b'\x04')
    elif scenario=='signal': os.kill(int(open(os.path.join(state,'pty-'+str(pid)+'-child')).read()),signal.SIGTERM)
    elif scenario=='auto_apply_toggle':
        send(b'\x1b[Z');wait('auto-apply on');send(b'\x1b[Z');wait('review before apply');send(b'\x04')
    elif scenario=='auto_apply_goal':
        send(b'\x1b[Z');wait('auto-apply on')
        send(b'Add tests in src/lib.rs\r');wait('[y/N]');send(b'y\r')
        wait('Auto-applied');wait('accomplish?');send(b'\x04')
    elif scenario=='auto_apply_aa':
        send(b'Add tests in src/lib.rs\r');wait('[y/N]');send(b'y\r')
        wait('Review changes');send(b'aa\r')
        wait('Merged checks: not run');wait('Auto-apply on');wait('accomplish?');send(b'\x04')
    elif scenario=='auto_apply_skipped':
        send(b'\x1b[Z');wait('auto-apply on')
        send(b'Add tests in src/lib.rs\r');wait('[y/N]');send(b'y\r')
        wait('Auto-apply skipped: no checks configured');wait('Leave pending')
        send(b'n\r');wait('accomplish?');send(b'\x04')
    else:
        if scenario=='natural': send(b"Let's adjust the size of the bouncing ball to make it twice as big.\r")
        elif scenario=='plain': send(b'Add tests in src/lib.rs\n')
        else:
            send('Add tests in src/lib.rs'.encode());resize(38,16)
            send(b'\x1b[200~\nKeep the public API unchanged.\x1b[201~');pump(.2)
            if scenario!='concurrent': assert not os.path.exists(os.path.join(os.path.dirname(state),'invocations'))
            resize(100,28);send(b'\r')
        wait('[y/N]');send(b'y\r')
        if scenario=='activity':
            wait('elapsed');activity_start=len(transcript)
            # The indicator must move; a loaded machine may draw frames slowly.
            frames=lambda: sum(c.encode() in transcript[activity_start:] for c in '◐◓◑◒')
            end=time.time()+WAIT
            while frames()<2 and time.time()<end: pump(.2)
            assert frames()>=2, 'activity indicator did not update'
            resize(38,16);resize(100,28)
        if scenario in ('cancel','active-eof','hangup'):
            end=time.time()+WAIT
            while not os.path.exists(os.path.join(os.path.dirname(state),'invocations')) and time.time()<end:pump()
            if scenario=='active-eof':send(b'\x04')
            elif scenario=='hangup':os.kill(int(open(os.path.join(state,'pty-'+str(pid)+'-child')).read()),signal.SIGHUP)
            else:
                send(b'\x03');wait('Work stopped.');send(b'n\r');wait('accomplish?');send(b'\x04')
        else:
            if scenario=='clarify':
                wait('Which');resize(35,16)
                with sqlite3.connect(os.path.join(state,'dispatch.db')) as db:
                    assert db.execute("SELECT COUNT(*) FROM attempt_launches WHERE state IN ('intent','spawned','uncertain')").fetchone()[0]==0
                send(b'\x1b[200~blue\x1b[201~');resize(100,28);send(b'\r')
            wait('Review changes');diff_start=len(clean);send(b'd\r');wait('delivered');pump(.2)
            if scenario!='plain': assert 'index ' not in clean[diff_start:], 'visual diff leaked index hashes'
            send(b'q\n' if scenario=='plain' else b'q');wait('Review changes')
            if scenario=='drift': open(os.path.join(source,'src','lib.rs'),'w').write('// user edit\n')  # conflicts with the delivered change; unrelated edits no longer block
            send(b'A\n' if scenario=='plain' else b'r\r' if scenario in ('reject','recovery','concurrent','natural') else b'a\r')
            wait('accomplish?');send(b'\x04')
    end=time.time()+WAIT
    while time.time()<end:
        pump()
        done,status=os.waitpid(pid,os.WNOHANG)
        if done:break
    else:raise AssertionError('session did not exit')
    assert os.waitstatus_to_exitcode(status)==0,(status,clean[-3000:])
    after=json.load(open(os.path.join(state,'pty-'+str(pid)+'-restored.json')))
    normalized=before[:6]+[[v if isinstance(v,int) else list(v) for v in before[6]]]
    assert after==normalized, ('terminal attributes not restored', after, normalized)
    if scenario in ('cancel','active-eof','hangup','eof','signal','panic','plain'):
        assert b'\x1b[?1049h' not in transcript, 'ordinary session entered alternate screen'
    else:
        assert transcript.count(b'\x1b[?1049h')==transcript.count(b'\x1b[?1049l'), 'inspection did not restore inline screen'
    if scenario=='inspection-panic':
        entered=transcript.index(b'\x1b[?1049h')
        left=transcript.index(b'\x1b[?1049l',entered)
        assert b'primary-terminal-sentinel' in transcript[:entered], 'primary sentinel was not displayed'
        assert not re.search(rb'\x1b\[[0-9;]*[JK]',transcript[left:]), 'inspection panic erased primary terminal display'
    sgr_codes=[int(value) for parameters in re.findall(rb'\x1b\[([0-9;]*)m',transcript) for value in parameters.split(b';') if value]
    assert not any(30<=code<=38 or 40<=code<=48 or 90<=code<=97 or 100<=code<=107 for code in sgr_codes), 'color emitted with no-color'
    if scenario=='plain':assert b'\x1b' not in transcript
    assert not any(word in clean for word in ('NotConfigured','NotRequested','NotRun')), 'internal state labels leaked'
    if scenario not in ('eof','signal','panic','inspection-panic','auto_apply_toggle'):
        records=[r for r in runs() if r['source_path']==os.path.realpath(source)];assert len(records)==1, 'paste submitted more than one goal'
        run=records[0]
        if scenario=='natural':
            assert run['task']=="Let's adjust the size of the bouncing ball to make it twice as big."
            # One selection rule: the first available configured profile.
            assert run['allocation']['selected']['resolved_model']=='light-model'
            assert len(run['attempts'])==1
        if scenario=='clarify':assert len(run['attempts'])==2 and len(run['execution']['questions'])==1
        if scenario in ('cancel','active-eof','hangup'):assert run['outcome']['work_result'] in ('cancelled','interrupted')
        elif scenario=='drift':assert run['outcome']['application']=='blocked_by_source_drift'
        elif scenario in ('reject','recovery','concurrent','natural'):assert run['outcome']['review']=='rejected'
        elif scenario=='auto_apply_skipped':assert run['outcome']['application']=='not_applied'
        elif scenario=='auto_apply_goal':
            assert run['outcome']['application']=='applied'
            assert run['outcome']['applied_by']=='auto_apply'
            assert run['outcome']['review']=='pending'
            with sqlite3.connect(os.path.join(state,'dispatch.db')) as db:
                assert db.execute("select count(*) from events where run_id=? and event_type='review.accepted'",(run['id'],)).fetchone()[0]==0
        elif scenario=='auto_apply_aa':
            assert run['outcome']['application']=='applied'
            assert run['outcome']['applied_by']=='human'
            assert run['outcome']['review']=='accepted'
        else:assert run['outcome']['application']=='applied'
        with sqlite3.connect(os.path.join(state,'dispatch.db')) as db:
            if scenario!='concurrent': assert db.execute("SELECT COUNT(*) FROM attempt_launches WHERE state IN ('intent','spawned','uncertain')").fetchone()[0]==0
    print('PTY passed:',scenario)
finally:
    try:os.killpg(pid,signal.SIGKILL)
    except ProcessLookupError:pass
    open(os.path.join(os.path.dirname(state),'pty-'+scenario+'-'+str(pid)+'.log'),'wb').write(transcript)
    os.close(master);os.close(slave)
