from pathlib import Path
import subprocess, json, shutil, hashlib, sys
r=Path(__file__).resolve().parent
repo=Path('/Users/yota/dev/gleaph-spill')
paths=[Path('crates/ic-stable-lara/src/labeled/graph')/n for n in ['traverse.rs','remove.rs','tree_read.rs','tree_write.rs']]
for p in paths: shutil.copyfile(repo/p,r/('final-'+p.name))
foreign=repo/'crates/ic-stable-lara/canbench_results.csv'
original=foreign.read_bytes(); (r/'foreign-canbench-before.csv').write_bytes(original)
result={}
def run(stage):
 source={str(p):hashlib.sha256((repo/p).read_bytes()).hexdigest() for p in paths}
 (r/(stage+'-source.json')).write_text(json.dumps(source,indent=2)+'\n')
 code=subprocess.run([sys.executable,str(r/'run.py'),'--name',stage,'--cwd',str(repo/'crates/ic-stable-lara'),'--cap','150','--','canbench','bench_t_vip_tail','--csv']).returncode
 if foreign.exists(): shutil.copyfile(foreign,r/(stage+'.csv'))
 foreign.write_bytes(original)
 result[stage]={'exit':code,'source_sha256':source}
 (r/'bench-pair.json').write_text(json.dumps(result,indent=2)+'\n')
 return code
try:
 for p in paths: shutil.copyfile(r/('base-'+p.name),repo/p)
 baseline=run('point-bench-before')
finally:
 for p in paths: shutil.copyfile(r/('final-'+p.name),repo/p)
 foreign.write_bytes(original)
if baseline: sys.exit(baseline)
try:
 after=run('point-bench-after')
finally:
 foreign.write_bytes(original)
assert foreign.read_bytes()==original
sys.exit(after)
