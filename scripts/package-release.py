#!/usr/bin/env python3
"""Create an offline Node Manager bundle, never include local database or credentials."""
import argparse
import hashlib
import io
import pathlib
import tarfile
p=argparse.ArgumentParser()
p.add_argument('--binary',required=True,type=pathlib.Path)
p.add_argument('--output',required=True,type=pathlib.Path)
a=p.parse_args()
r=pathlib.Path(__file__).resolve().parents[1]
files={'monitor-hub':a.binary,'install-hub.sh':r/'install-hub.sh'}
for name in ('scripts/hub-install.py','scripts/hub-lifecycle.py','scripts/hub_health.py','NODE_MANAGEMENT_DEPLOY.md','LICENSE'):
    files[name]=r/name
contents={name:path.read_bytes() for name,path in files.items()}
contents['sha256sums.txt']=''.join(hashlib.sha256(data).hexdigest()+'  '+name+'\n' for name,data in sorted(contents.items())).encode()
with tarfile.open(a.output,'w:gz') as archive:
    for name,data in contents.items():
        item=tarfile.TarInfo('node-manager/'+name);item.size=len(data)
        item.mode=0o755 if name in ('monitor-hub','install-hub.sh') or name.endswith('.py') else 0o644
        archive.addfile(item,io.BytesIO(data))
print(hashlib.sha256(a.output.read_bytes()).hexdigest()+'  '+a.output.name)
