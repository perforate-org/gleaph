#!/usr/bin/env python3
"""Admission-only evidence; reads source/logs, never executes Rust or swaps source."""
import csv, difflib, hashlib, importlib.metadata, json, os, re, shutil, subprocess, time
from pathlib import Path
import lizard
R = Path(__file__).resolve().parent
REPO = Path('/Users/yota/dev/gleaph-spill')
FS = 'crates/ic-stable-lara/src/lara/edge/free_span.rs'
MAP = 'crates/ic-stable-paged-ordered-map/src/lib.rs'
OUT = REPO / 'design/evidence/sources'
PREFIX = 'adr0097-admission-'
sha = lambda p: hashlib.sha256(p.read_bytes()).hexdigest()
start = time.monotonic()
s = json.loads((R/'start.json').read_text())
ledger = json.loads((R/'ledger.json').read_text())
candidate = json.loads((R/'candidate-hashes.json').read_text())
def git(*args):
    return subprocess.check_output(['git', *args], cwd=REPO, text=True)
assert git('rev-parse','HEAD').strip() == s['base']
assert git('rev-parse','main').strip() == s['main']
assert git('branch','--show-current').strip() == 'spill-swap'
assert git('diff','--cached','--name-only') == ''
assert set(git('diff','--name-only','HEAD','--','crates/').splitlines()) == {FS, MAP}
assert not os.environ.get('RUST_TEST_THREADS')
for p,h in s['protected'].items(): assert sha(REPO/p) == h, p
for p,h in candidate.items(): assert sha(REPO/p) == h, p
for p,h in s['before_hashes'].items(): assert sha(R/'before'/p) == h, p
for p in ['design/evidence/sources/adr0097-demotion-plan-r2.txt', 'design/evidence/sources/adr0097-demotion-plan-r2-draft.txt']:
    assert sha(REPO/p) == s['before_hashes'][p]
prior=Path(s['prior_root']);assert sha(prior/'manifest.sha256') == s['prior_manifest_sha256']
for line in (prior/'manifest.sha256').read_text().splitlines():
    h,p=line.split('  ',1);assert sha(prior/p)==h,p
# Unchanged original/r1/r2 work outside the two currently authorized prose files.
for root,sub in [(prior/'r1',''),(Path('/tmp/spill-demotion-revise-f297-32aisjkk')/'reviewed','')]:
    for p in root.rglob('*'):
        if not p.is_file(): continue
        rel=p.relative_to(root)
        if str(rel) in ['plans/0339-prepared-edge-span-publication.md','design/evidence/adr0097-demotion-design.md']: continue
        # The older reviewed copies differ intentionally from r1; compare only r1's unchanged files.
        if root==prior/'r1' and (REPO/rel).exists(): assert sha(REPO/rel)==sha(p),str(rel)
# Exact source reconstruction, tests-only baseline, unchanged legacy tests and unchanged fast path.
metric={};function_evidence={};test_inventory={};format_diff=[]
for p in [FS, MAP]:
    b=(R/'before'/p).read_text();w=(R/'witness'/p).read_text();c=(R/'candidate'/p).read_text()
    marker='#[cfg(test)]\nmod tests'
    assert b.split(marker)[0] == w.split(marker)[0]
    functions={}
    for phase,text in [('before',b),('witness',w),('candidate',c)]:
        a=lizard.analyze_file.analyze_source_code(p,text)
        functions[phase]={f.name:f for f in a.function_list}
        metric.setdefault(phase,{})[p]=[{'name':f.name,'long_name':f.long_name,'start':f.start_line,'end':f.end_line,'nloc':f.nloc,'ccn':f.cyclomatic_complexity,'tokens':f.token_count,'parameters':f.parameter_count,'length':f.length} for f in a.function_list]
    def body(phase,name):
        text={'before':b,'witness':w,'candidate':c}[phase];f=functions[phase][name]
        return '\n'.join(text.splitlines()[f.start_line-1:f.end_line])
    tests=re.findall(r'#\[test\]\s*fn (\w+)', b)
    added=re.findall(r'#\[test\]\s*fn (a0_admission_\w+)', c)
    for name in tests: assert body('before',name)==body('candidate',name),(p,name)
    # witness-format.diff was inspected: only whitespace and optional trailing commas
    # in multi-element tuples, struct literals and calls changed. This is not a Rust parser.
    def formatted_body(text):
        return re.sub(r',(?=[)}])','',re.sub(r'\s','',text))
    for name in added:
        assert formatted_body(body('witness',name))==formatted_body(body('candidate',name)),name
        format_diff.extend(difflib.unified_diff((body('witness',name)+'\n').splitlines(True),(body('candidate',name)+'\n').splitlines(True),fromfile='witness/'+name,tofile='candidate/'+name,n=0))
    mutation_name, mutated_function = ('mutation-exact','remove_span_exact') if p==FS else ('mutation-zero','reserve_for_inserts')
    reconstructed = c.replace(body('candidate',mutated_function),body('before',mutated_function),1)
    mutation_run = next(run for run in ledger['runs'] if run['name']==mutation_name)
    assert hashlib.sha256(reconstructed.encode()).hexdigest()==mutation_run['source_hashes'][p],mutation_name
    (R/(mutation_name+'-zero.patch')).write_text(''.join(difflib.unified_diff(c.splitlines(True),reconstructed.splitlines(True),fromfile='a/'+p,tofile='b/'+p,n=0)))
    other=MAP if p==FS else FS
    assert mutation_run['source_hashes'][other]==candidate[other],mutation_name
    if p==FS:
        assert body('before','restore_allocated_prefix')==body('candidate','restore_allocated_prefix')
        assert 'fn ensure_span_exact' not in c
        assert c.count('rec.len != span.len') == 1
        names=['replace_exact_pair_with','ensure_span_exact','exact_span_record','replace_existing_span','remove_span_exact','admission_snapshot',*added]
    else: names=['reserve_for_inserts',*added]
    function_evidence[p]={phase:[m for m in metric[phase][p] if m['name'] in names] for phase in ['before','candidate']}
    test_inventory[p]={'legacy':len(tests),'added':added,'current':len(tests)+len(added),'legacy_bodies_unchanged':True,'added_oracles_unchanged_except_format':True}
for phase,patch_name in [('witness','witness.patch'),('candidate','candidate.patch')]:
    patch=[]
    for p in [FS,MAP]:
        patch.extend(difflib.unified_diff((R/'before'/p).read_text().splitlines(True),(R/phase/p).read_text().splitlines(True),fromfile='a/'+p,tofile='b/'+p))
    assert ''.join(patch)==(R/patch_name).read_text(),patch_name
    zero=[]
    for p in [FS,MAP]:
        zero.extend(difflib.unified_diff((R/'before'/p).read_text().splitlines(True),(R/phase/p).read_text().splitlines(True),fromfile='a/'+p,tofile='b/'+p,n=0))
    (R/(phase+'-zero.patch')).write_text(''.join(zero))
summary={'mode':'working tree, exact two-file scope','baseline':s['base'],'lizard_version':importlib.metadata.version('lizard'),'functions':function_evidence,'inventory':test_inventory}
(R/'lizard.json').write_text(json.dumps(summary,indent=2)+'\n')
(R/'lizard-all.json').write_text(json.dumps(metric,indent=2)+'\n')
(R/'witness-format-zero.diff').write_text(''.join(format_diff))
results=[]
for run in ledger['runs']:
    n=run['name'];text=(R/(n+'.log')).read_text()
    assert sha(R/(n+'.log'))==run['log_sha256'],n
    assert sha(R/(n+'.Cargo.lock'))==run['as_used_lock_sha256'],n
    assert run['foreign_csv_restored'] and run['restored_lock_sha256']==s['protected']['Cargo.lock']
    assert run['elapsed_seconds'] < run['cap_seconds'],n
    assert not any('test-threads' in arg for arg in run['command'])
    result=re.findall(r'test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed;.*?finished in ([\d.]+)s',text)
    timing=re.findall(r'Finished [^\n]+? in ([^\n]+)',text)
    results.append({'name':n,'exit':run['exit_code'],'seconds':run['elapsed_seconds'],'compile':timing,'tests':result})
    if n in ['before-free','before-map']:
        assert run['source_hashes']=={p:sha(R/'witness'/p) for p in [FS,MAP]}
    if n in ['green-free','green-map','ic-after-split','ic-after-bin'] or n.startswith('final-'):
        assert run['source_hashes']==candidate,n
    if n.startswith('ic-before-'):
        assert run['source_hashes']=={p:s['before_hashes'][p] for p in [FS,MAP]}
    if '--csv' in run['command']: assert sha(R/(n+'.csv'))==run['csv_sha256']
expected={'before-free':('0','2'),'before-map':('1','1'),'green-free':('2','0'),'green-map':('2','0'),'mutation-exact':('0','1'),'mutation-zero':('0','1'),'final-free':('27','0'),'final-map':('12','0')}
for n,counts in expected.items():
    row=next(row for row in results if row['name']==n);assert tuple(row['tests'][0][:2])==counts,n
    assert row['exit']==(101 if counts[1]!='0' else 0),n
assert 'replacement-key' not in (R/'before-free.log').read_text()
for n in ['green-free','final-free']:
    assert 'ADMISSION_PAIR replacement-key: unchanged ownership verified' in (R/(n+'.log')).read_text()
assert 'ADMISSION_ZERO zero and already-backed positive reservation passed' in (R/'final-map.log').read_text()
assert 'ADMISSION_DIRECTORY exact removal/retry reused page 2' in (R/'before-map.log').read_text()
bench=[]
for short in ['split','bin']:
    rows=[]
    for phase in ['before','after']:
        rows.append({x['name']:x for x in csv.DictReader((R/f'ic-{phase}-{short}.csv').open())})
    assert rows[0].keys()==rows[1].keys() and len(rows[0])==2
    for name,b in rows[0].items():
        a=rows[1][name];bi=int(b['instructions']);ai=int(a['instructions'])
        for x in [a,b]:
            assert int(x['heap_increase'])==0 and int(x['stable_memory_increase'])==0
        bench.append({'name':name,'before':bi,'after':ai,'delta':ai-bi,'percent':(ai-bi)*100/bi,'heap_pages_before_after':[0,0],'stable_pages_before_after':[0,0]})
(R/'measurements.json').write_text(json.dumps({'commands':results,'benchmarks':bench},indent=2)+'\n')
# Check exact allowed working-tree additions, not only counts.
initial={line[3:]:line[:2] for line in s['status'].splitlines()}
actual={line[3:]:line[:2] for line in git('status','--short','--untracked-files=all').splitlines()}
for p,status in initial.items(): assert actual.get(p)==status,p
new=set(actual)-set(initial)
assert all(p in [FS,MAP] or p.startswith('design/evidence/sources/'+PREFIX) for p in new),new
bench_source='crates/ic-stable-lara/src/lara/edge/free_span/bench.rs'
assert (REPO/bench_source).read_text()==git('show',s['base']+':'+bench_source)
verification={'result':'pass','head':s['base'],'main':s['main'],'source':candidate,'protected':len(s['protected']),'prior_manifest_entries':len((prior/'manifest.sha256').read_text().splitlines()),'scope':actual,'test_inventory':test_inventory,'benchmark_source_sha256':sha(REPO/bench_source),'elapsed_seconds':time.monotonic()-start}
(R/'source-verification.json').write_text(json.dumps(verification,indent=2)+'\n')
# Raw logs are immutable; normalize only their durable whitespace, recording both hashes.
# Patch derivatives use explicit zero-context generation; CSV/JSON/scripts are byte copies.
artifacts=[]
def copy(src,dst):
    src=R/src;dst=OUT/(PREFIX+dst);data=src.read_bytes()
    if src.suffix=='.log' and data:
        data=('\n'.join(line.rstrip() for line in data.decode().splitlines()).rstrip()+'\n').encode()
    dst.write_bytes(data)
    artifacts.append({'raw':str(src),'durable':str(dst.relative_to(REPO)),'raw_sha256':sha(src),'sha256':sha(dst),'log_whitespace_normalized':src.suffix=='.log' and sha(src)!=sha(dst)})
for run in ledger['runs']:
    n=run['name'];copy(n+'.log',n+'.txt')
    if '--csv' in run['command']:copy(n+'.csv',n+'.csv')
for n in ['witness','candidate','mutation-exact','mutation-zero']:
    copy(n+'-zero.patch',n+'.patch')
for n in ['lizard.json','measurements.json','versions.json','ledger.json','start.json','restoration.json']:
    copy(n,n)
copy('run.py','runner.py')
copy('collect.py','collector.py')
copy('witness-format-zero.diff','witness-format.diff')
if (R/'plan-final.md').exists():copy('plan-final.md','plan-final.txt')
run={'base':s['base'],'main':s['main'],'raw_root':str(R),'code_source_sha256':candidate,'as_used_lock_sha256':sorted(set(x['as_used_lock_sha256'] for x in ledger['runs'])),'restored_lock_sha256':s['protected']['Cargo.lock'],'shared_as_used_lock_patch':'design/evidence/sources/adr0097-crossover-as-used-lock.patch','prior_r2_manifest_sha256':s['prior_manifest_sha256'],'commands':len(ledger['runs']),'command_seconds':sum(x['elapsed_seconds'] for x in ledger['runs']),'administrative_seconds':sum(x['charged_seconds'] for x in ledger['administrative_charges']),'budget_seconds':600,'artifacts':artifacts,'coverage_limits':['malformed admission on valid backing only','no valid-pair late-failure or full A0/A1 atomicity','no native/Graph/PocketIC/lifecycle/landing approval','no global heap-allocation or transient-write instrumentation','baseline pair collision case preempted by identical-input failure','benchmark instruction deltas not isolated per read; no before-Wasm archive']}
run['charged_seconds']=run['command_seconds']+run['administrative_seconds'];run['unused_seconds']=600-run['charged_seconds']
(OUT/(PREFIX+'run.json')).write_text(json.dumps(run,indent=2)+'\n')
print(json.dumps({'result':'pass','commands':len(results),'benchmarks':bench,'source':candidate,'charged_seconds':run['charged_seconds'],'durable_artifacts':len(artifacts)+1,'shell_seconds':time.monotonic()-start},indent=2))
