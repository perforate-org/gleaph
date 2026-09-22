from pathlib import Path
import subprocess, json, hashlib, sys
r=Path(__file__).resolve().parent
repo=Path('/Users/yota/dev/gleaph-spill')
p=Path('crates/ic-stable-lara/src/labeled/graph/traverse.rs')
candidate=(repo/p).read_bytes();(r/'bench-traverse.rs').write_bytes(candidate)
foreign=repo/'crates/ic-stable-lara/canbench_results.csv'
original=foreign.read_bytes();(r/'foreign-canbench-before.csv').write_bytes(original)
result={}
def run(stage):
 source={str(p):hashlib.sha256((repo/p).read_bytes()).hexdigest()}
 (r/(stage+'-source.json')).write_text(json.dumps(source,indent=2)+'\n')
 code=subprocess.run([sys.executable,str(r/'run.py'),'--name',stage,'--cwd',str(repo/'crates/ic-stable-lara'),'--cap','150','--','canbench','bench_t_vip_tail','--csv']).returncode
 if foreign.exists():(r/(stage+'.csv')).write_bytes(foreign.read_bytes())
 foreign.write_bytes(original)
 result[stage]={'exit':code,'source_sha256':source}
 (r/'bench-pair.json').write_text(json.dumps(result,indent=2)+'\n')
 return code
try:
 (repo/p).write_bytes((r/'base-traverse.rs').read_bytes())
 baseline=run('scalar-before')
finally:
 (repo/p).write_bytes(candidate);foreign.write_bytes(original)
if baseline:sys.exit(baseline)
try:after=run('scalar-after')
finally:foreign.write_bytes(original)
assert (repo/p).read_bytes()==candidate and foreign.read_bytes()==original
sys.exit(after)
