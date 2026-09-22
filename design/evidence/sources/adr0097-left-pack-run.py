#!/usr/bin/env python3
"""One sequential, cumulative monotonic ledger for this bounded turn."""
import argparse, fcntl, hashlib, json, os, subprocess, sys, time
from pathlib import Path
root=Path(__file__).resolve().parent
p=argparse.ArgumentParser();p.add_argument('--name',required=True);p.add_argument('--cwd',default='/Users/yota/dev/gleaph-spill');p.add_argument('--repo',default='/Users/yota/dev/gleaph-spill');p.add_argument('--cap',type=float,default=120);p.add_argument('--final',action='store_true');p.add_argument('cmd',nargs=argparse.REMAINDER);a=p.parse_args();cmd=a.cmd[1:] if a.cmd[:1]==['--'] else a.cmd
with (root/'ledger.lock').open('w') as lock:
 fcntl.flock(lock,fcntl.LOCK_EX)
 ledger=root/'ledger.json';data=json.loads(ledger.read_text()) if ledger.exists() else {'budget_seconds':600,'reserved_final_seconds':120,'setup_charge_seconds':30,'setup_charge_note':'Conservative full tool cap for the pre-ledger non-Rust inspect.py import-shadowing failure; not a measured duration. No Rust command ran. Script renamed before validation.','runs':[]}
 assert all('elapsed_seconds' in r for r in data['runs']), 'incomplete prior command: stop and inspect'
 spent=data.get('setup_charge_seconds',0)+sum(r['elapsed_seconds'] for r in data['runs']);remaining=600-spent;available=remaining-(0 if a.final else 120)
 if available<5:print(f'NOT RUN: spent={spent:.3f}, remaining={remaining:.3f}, final reserve protected');sys.exit(125)
 assert not (root/(a.name+'.log')).exists(), 'unique run names required'
 cap=min(a.cap,available);record={'name':a.name,'command':cmd,'cwd':a.cwd,'repo':a.repo,'remaining_before':remaining,'timeout_seconds':cap,'final':a.final,'start_monotonic':time.monotonic()};data['runs'].append(record);ledger.write_text(json.dumps(data,indent=2)+'\n')
 print(f'RUN {a.name}: spent={spent:.3f}s remaining={remaining:.3f}s timeout={cap:.3f}s',flush=True)
 with (root/(a.name+'.log')).open('wb') as log:
  result=subprocess.run(['timeout',str(cap),*cmd],cwd=a.cwd,stdout=log,stderr=subprocess.STDOUT)
 record['end_monotonic']=time.monotonic();record['elapsed_seconds']=record['end_monotonic']-record['start_monotonic'];record['exit_code']=result.returncode
 cargo=Path(a.repo)/'Cargo.lock'
 if cargo.exists():
  content=cargo.read_bytes();(root/(a.name+'.Cargo.lock')).write_bytes(content);record['as_used_lock_sha256']=hashlib.sha256(content).hexdigest()
 record['log_sha256']=hashlib.sha256((root/(a.name+'.log')).read_bytes()).hexdigest();ledger.write_text(json.dumps(data,indent=2)+'\n')
 print(f'END {a.name}: exit={result.returncode} elapsed={record["elapsed_seconds"]:.3f}s cumulative={spent+record["elapsed_seconds"]:.3f}s remaining={600-spent-record["elapsed_seconds"]:.3f}s',flush=True)
 sys.exit(result.returncode)
