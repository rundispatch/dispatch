#!/usr/bin/env python3
"""Package an already validated local executable; no installation or publication.
Reproducible archive metadata. Rebuild reproducibility is not implied.
"""
import argparse, gzip, hashlib, io, json, platform, re, subprocess, tarfile
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
TARGETS=('macos-arm64','macos-x86_64','linux-x86_64','linux-aarch64')
parser=argparse.ArgumentParser();parser.add_argument('binary',type=Path);parser.add_argument('output',type=Path)
# A cross build names its target: the host's own machine is then not the binary's.
parser.add_argument('--target',choices=TARGETS,help='the archive is dispatch-<target>; default: this machine, as uname -m says')
args=parser.parse_args();binary=args.binary.resolve();out=args.output.resolve();out.mkdir(parents=True,exist_ok=True)
hashof=lambda data:hashlib.sha256(data).hexdigest()
version=subprocess.check_output([str(binary),'version'],text=True).strip()
expected=re.search(r'^version = "([^"]+)"', (ROOT/'Cargo.toml').read_text(), re.M).group(1)
if version != 'dispatch '+expected: raise SystemExit('Binary version does not match this source; build the candidate first.')
if args.target: platform_name,arch=args.target.split('-',1)
else: platform_name,arch={'Darwin':'macos','Linux':'linux'}.get(platform.system(),platform.system().lower()),platform.machine()
tracked=subprocess.check_output(['git','ls-files','-z','--cached','--others','--exclude-standard'],cwd=ROOT).split(b'\0')
paths=sorted(set(p.decode() for p in tracked if p))
# Freeze runtime source, fixtures and build contract, excluding generated evidence/reports.
source={p:hashof((ROOT/p).read_bytes()) for p in paths if (ROOT/p).is_file() and
        (p.startswith(('src/','tests/','assets/','scripts/','.github/workflows/')) or p in ('Cargo.toml','Cargo.lock','AGENTS.md','LICENSE','docs/release-install.md','docs/captures/phase4-refinement/render-capture.py'))}
source_digest=hashof(json.dumps(source,sort_keys=True,separators=(',',':')).encode())
manifest={'version':version,'platform':platform_name,'architecture':arch,'binary_sha256':hashof(binary.read_bytes()),'source_sha256':source_digest,'source_files':source,'head':subprocess.check_output(['git','rev-parse','HEAD'],cwd=ROOT,text=True).strip(),'publication':False,'fixture_validation_does_not_certify_live_providers':True}
files={'dispatch':binary.read_bytes(),'LICENSE':(ROOT/'LICENSE').read_bytes(),'INSTALL.md':(ROOT/'docs/release-install.md').read_bytes(),'BUILD.json':(json.dumps(manifest,indent=2,sort_keys=True)+'\n').encode()}
archive=out/f'dispatch-{platform_name}-{arch}.tar.gz'
with archive.open('wb') as raw:
    with gzip.GzipFile(fileobj=raw,mode='wb',mtime=0,filename='') as gz:
        with tarfile.open(fileobj=gz,mode='w',format=tarfile.USTAR_FORMAT) as tar:
            for name,data in sorted(files.items()):
                entry=tarfile.TarInfo(name);entry.size=len(data);entry.mode=0o755 if name=='dispatch' else 0o644
                entry.mtime=0;entry.uid=entry.gid=0;entry.uname=entry.gname='';tar.addfile(entry,io.BytesIO(data))
(out/'BUILD.json').write_bytes(files['BUILD.json'])
(out/'SHA256SUMS').write_text(hashof(archive.read_bytes())+'  '+archive.name+'\n'+hashof(files['dispatch'])+'  dispatch\n')
(out/'dispatch').write_bytes(files['dispatch']);(out/'dispatch').chmod(0o755)
print(json.dumps({key:manifest[key] for key in ('version','source_sha256','binary_sha256')},indent=2));print(archive)
