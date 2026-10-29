#!/usr/bin/env python3
"""Fresh ordered ledger; foreground commands only, no old source swaps or thread overrides."""
import fcntl, hashlib, json, os, subprocess, sys, time
from pathlib import Path
R=Path(__file__).resolve().parent
REPO=Path('/Users/yota/dev/gleaph-spill')
FS='crates/ic-stable-lara/src/lara/edge/free_span.rs'
MAP='crates/ic-stable-paged-ordered-map/src/lib.rs'
BENCH=REPO/'crates/ic-stable-lara'
def test(pkg, filt): return ['cargo','test','-p',pkg,'--lib',filt,'--','--nocapture']
steps=[
 ('ic-before-split',130,['canbench','bench_r_fs_tkbfs_1024','--csv'],BENCH,False),
 ('ic-before-bin',20,['canbench','bench_r_fs_lbin_4096','--csv'],BENCH,False),
 ('before-free',40,test('ic-stable-lara','a0_admission_'),REPO,False),
 ('before-map',15,test('ic-stable-paged-ordered-map','a0_admission_'),REPO,False),
 ('format',5,['rustfmt','--edition','2024','--config','skip_children=true',FS,MAP],REPO,False),
 ('green-free',25,test('ic-stable-lara','a0_admission_'),REPO,False),
 ('green-map',10,test('ic-stable-paged-ordered-map','a0_admission_'),REPO,False),
 ('mutation-exact',15,test('ic-stable-lara','a0_admission_exact_span'),REPO,False),
 ('mutation-zero',10,test('ic-stable-paged-ordered-map','a0_admission_zero_reservation'),REPO,False),
 ('ic-after-split',130,['canbench','bench_r_fs_tkbfs_1024','--csv'],BENCH,False),
 ('ic-after-bin',20,['canbench','bench_r_fs_lbin_4096','--csv'],BENCH,False),
 ('final-format',5,['rustfmt','--edition','2024','--config','skip_children=true','--check',FS,MAP],REPO,True),
 ('final-clippy',25,['cargo','clippy','-p','ic-stable-lara','-p','ic-stable-paged-ordered-map','--lib','--','-D','warnings'],REPO,True),
 ('final-free',25,test('ic-stable-lara','lara::edge::free_span::tests::'),REPO,True),
 ('final-map',15,test('ic-stable-paged-ordered-map','tests::'),REPO,True),
 ('final-whitespace',5,['git','diff','--check'],REPO,True),
 ('final-plan',5,['python3','/Users/yota/.agents/skills/plan/scripts/validate_plan.py','plans/0339-prepared-edge-span-publication.md','--phase','final'],REPO,True),
]
sha=lambda b:hashlib.sha256(b).hexdigest()
name=sys.argv[1]
with (R/'ledger.lock').open('w') as lock:
 fcntl.flock(lock,fcntl.LOCK_EX)
 data=json.loads((R/'ledger.json').read_text())
 assert all('elapsed_seconds' in x for x in data['runs']), 'incomplete prior run'
 step=steps[len(data['runs'])]
 assert step[0]==name,(name,step[0])
 _,cap,cmd,cwd,final=step
 assert not os.environ.get('RUST_TEST_THREADS')
 assert not any('test-threads' in x for x in cmd)
 spent=sum(x['elapsed_seconds'] for x in data['runs'])+sum(x['charged_seconds'] for x in data['administrative_charges'])
 assert spent+cap <= (600 if final else 480), 'budget / final-reserve admission failed'
 if data['runs']: assert data['runs'][-1]['exit_code'] != 124, 'no unchanged timeout retry/continuation'
 original_lock=(R/'before/Cargo.lock').read_bytes()
 csv=BENCH/'canbench_results.csv'; original_csv=(R/'before/crates/ic-stable-lara/canbench_results.csv').read_bytes()
 assert csv.read_bytes()==original_csv
 rec={'name':name,'command':cmd,'cwd':str(cwd),'cap_seconds':cap,'final':final,'spent_before':spent,'source_hashes':{p:sha((REPO/p).read_bytes()) for p in [FS,MAP]},'start_monotonic':time.monotonic()}
 data['runs'].append(rec);(R/'ledger.json').write_text(json.dumps(data,indent=2)+'\n')
 print(f'RUN {name}: spent {spent:.3f}s; cap {cap}s; final reserve {0 if final else 120}s',flush=True)
 try:
  with (R/(name+'.log')).open('wb') as log:
   result=subprocess.run(['timeout',str(cap),*cmd],cwd=cwd,stdout=log,stderr=subprocess.STDOUT)
  rec['exit_code']=result.returncode
 finally:
  used=(REPO/'Cargo.lock').read_bytes();(R/(name+'.Cargo.lock')).write_bytes(used);rec['as_used_lock_sha256']=sha(used)
  if '--csv' in cmd:
   produced=csv.read_bytes() if csv.exists() else b''
   (R/(name+'.csv')).write_bytes(produced)
   rec['csv_sha256']=sha(produced);rec['csv_differs_from_foreign']=produced!=original_csv
  if not csv.exists() or csv.read_bytes()!=original_csv: csv.write_bytes(original_csv)
  if used!=original_lock:(REPO/'Cargo.lock').write_bytes(original_lock)
  rec['foreign_csv_restored']=csv.read_bytes()==original_csv
  rec['restored_lock_sha256']=sha((REPO/'Cargo.lock').read_bytes())
  rec['elapsed_seconds']=time.monotonic()-rec['start_monotonic']
  rec['log_sha256']=sha((R/(name+'.log')).read_bytes())
  (R/'ledger.json').write_text(json.dumps(data,indent=2)+'\n')
 print(f'END {name}: exit {rec["exit_code"]}; {rec["elapsed_seconds"]:.3f}s; cumulative {spent+rec["elapsed_seconds"]:.3f}s',flush=True)
 sys.exit(rec['exit_code'])
