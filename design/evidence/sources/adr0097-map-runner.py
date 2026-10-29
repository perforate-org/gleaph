import hashlib, json, os, signal, subprocess, sys, time
from pathlib import Path
ROOT=Path(__file__).parent
REPO=Path('/Users/yota/dev/gleaph-spill')
START=json.loads((ROOT/'start.json').read_text())
MAP='crates/ic-stable-paged-ordered-map'
LARA='crates/ic-stable-lara'
COMMANDS={
 'baseline-native':(20,'.',['cargo','test','-p','ic-stable-paged-ordered-map','--lib','tests::','--','--nocapture']),
 'baseline-scalar':(75,MAP,['canbench','bench_paged_scalar_edits_97','--csv']),
 'baseline-allocator':(100,LARA,['canbench','bench_r_fs_tkbfs_1024','--csv']),
 'format':(5,'.',['rustfmt','--edition','2024','--config','skip_children=true',MAP+'/src/lib.rs',MAP+'/src/bench.rs']),
 'candidate-native':(30,'.',['cargo','test','-p','ic-stable-paged-ordered-map','--lib','tests::','--','--nocapture']),
 'candidate-doc':(10,'.',['cargo','test','-p','ic-stable-paged-ordered-map','--doc','--','--show-output']),
 'mutation-early':(10,'.',['cargo','test','-p','ic-stable-paged-ordered-map','--lib','prepared_edits_directory_rejection','--','--nocapture']),
 'mutation-resource':(10,'.',['cargo','test','-p','ic-stable-paged-ordered-map','--lib','prepared_edits_resources_and_retry','--','--nocapture']),
 'candidate-scalar':(75,MAP,['canbench','bench_paged_scalar_edits_97','--csv']),
 'candidate-allocator':(100,LARA,['canbench','bench_r_fs_tkbfs_1024','--csv']),
 'final-format':(5,'.',['rustfmt','--check','--edition','2024','--config','skip_children=true',MAP+'/src/lib.rs',MAP+'/src/bench.rs']),
 'final-clippy':(25,'.',['cargo','clippy','-p','ic-stable-paged-ordered-map','-p','ic-stable-lara','--lib','--','-D','warnings']),
 'final-map':(20,'.',['cargo','test','-p','ic-stable-paged-ordered-map','--lib','tests::','--','--nocapture']),
 'final-free':(25,'.',['cargo','test','-p','ic-stable-lara','--lib','lara::edge::free_span::tests::','--','--nocapture']),
 'final-doc':(15,'.',['cargo','test','-p','ic-stable-paged-ordered-map','--doc','--','--show-output']),
}
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
name=sys.argv[1];cap,cwd,cmd=COMMANDS[name]
ledger=json.loads((ROOT/'ledger.json').read_text());assert not ledger['closed']
assert not any(e['step']==name for e in ledger['entries']), 'one ordered invocation per gate'
final=name.startswith('final-');spent=sum(e['charged'] for e in ledger['entries'])
assert spent+cap <= (600 if final else 480)
if final:
 assert sum(e['charged'] for e in ledger['entries'] if e['step'].startswith('final-'))+cap<=120
assert 'RUST_TEST_THREADS' not in os.environ
assert sha(REPO/LARA/'src/lara/edge/free_span.rs')==START['initial_hashes'][LARA+'/src/lara/edge/free_span.rs']
source={p:sha(REPO/p) for p in START['source_scope']+[LARA+'/src/lara/edge/free_span.rs']}
lock=REPO/'Cargo.lock'; original=(ROOT/'before/Cargo.lock').read_bytes()
assert lock.read_bytes()==original
is_bench=cmd[0]=='canbench'; csv=REPO/cwd/'canbench_results.csv'
if is_bench:
 pin=START['csvs'][str(csv.relative_to(REPO))]
 assert csv.exists()==pin['exists']
 if csv.exists(): assert sha(csv)==pin['sha256']
 entry_csv=csv.read_bytes() if csv.exists() else None
start=time.monotonic();code=None;timed_out=False;restored=False
try:
 with (ROOT/(name+'.log')).open('wb') as log:
  proc=subprocess.Popen(cmd,cwd=REPO/cwd,stdout=log,stderr=subprocess.STDOUT)
  try:code=proc.wait(timeout=max(.1,cap-2))
  except subprocess.TimeoutExpired:
   timed_out=True;proc.send_signal(signal.SIGINT)
   try:proc.wait(timeout=1)
   except subprocess.TimeoutExpired:proc.kill();proc.wait()
   code=124
finally:
 (ROOT/(name+'-as-used.lock')).write_bytes(lock.read_bytes())
 used_lock=sha(lock)
 if is_bench:
  if csv.exists():(ROOT/(name+'.csv')).write_bytes(csv.read_bytes())
  if entry_csv is None:
   if csv.exists():csv.unlink()
  else:csv.write_bytes(entry_csv)
  assert csv.exists()==(entry_csv is not None)
  if entry_csv is not None:assert csv.read_bytes()==entry_csv
 lock.write_bytes(original);assert lock.read_bytes()==original;restored=True
 elapsed=time.monotonic()-start
 result={'step':name,'command':cmd,'cwd':str(REPO/cwd),'cap':cap,'charged':elapsed,'exit':code,
         'timed_out':timed_out,'restored':restored,'source_sha256':source,'as_used_lock':used_lock,
         'local_lock':sha(lock),'log_sha256':sha(ROOT/(name+'.log'))}
 if is_bench:
  actual=ROOT/(name+'.csv');result['csv_sha256']=sha(actual) if actual.exists() else None
  wasm=REPO/'target/canbench/wasm32-unknown-unknown/release'/('ic_stable_paged_ordered_map.wasm' if cwd==MAP else 'ic_stable_lara.wasm')
  result['wasm_sha256']=sha(wasm) if wasm.exists() else None
 ledger['entries'].append(result);(ROOT/'ledger.json').write_text(json.dumps(ledger,indent=2)+'\n')
 (ROOT/(name+'.json')).write_text(json.dumps(result,indent=2)+'\n')
print(json.dumps(result,indent=2));print('cumulative_seconds',spent+elapsed)
if elapsed>cap or timed_out:sys.exit(124)
sys.exit(code or 0)
