from pathlib import Path
import csv,difflib,hashlib,json,re,subprocess
r=Path(__file__).parent;repo=Path('/Users/yota/dev/gleaph-spill');sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
mp='crates/ic-stable-paged-ordered-map/src/lib.rs';bp='crates/ic-stable-paged-ordered-map/src/bench.rs'
assert sha(repo/mp)=='be81cd30103fda176a1d6517f12fc2f2b28f90960c11d7243bc8d0a40f209460'
# Parse interleaved normal-scheduling output without modifying the raw transcripts.
def native(name):
 raw=(r/name).read_text();clean=re.sub(r'test tests::\w+ \.\.\. ','',raw);clean=clean.replace('ok\n','')
 result={}
 for op,traffic,work in re.findall(r'SCALAR_WORK (\w+) traffic=Traffic \{([^}]+)\} scratch=WorkObservation \{([^}]+)\}',clean):
  t={k:int(v) for k,v in re.findall(r'(\w+): (\d+)',traffic)}
  assert set(t)=={'reads','read_bytes','writes','write_bytes','max_read','max_write','sizes','grows','grown_pages'},(op,t)
  observed='observed: true' in work
  result[op]={'traffic':t,'scratch':({k:int(v) for k,v in re.findall(r'(\w+): (\d+)',work)} if observed else 'unobserved'),'observed':observed}
 assert set(result)=={'replace','missing','tail_insert','tail_remove','split'}
 return result
before=native('baseline-native.log');after=native('final-map.log')
for op,w in [('replace',(1,8)),('missing',(0,0)),('tail_insert',(3,26)),('tail_remove',(2,10))]:
 a=after[op];b=before[op]
 assert (a['traffic']['writes'],a['traffic']['write_bytes'])==w
 for field in ['reads','read_bytes']:assert a['traffic'][field]<=b['traffic'][field]
 assert a['traffic']['max_read']<=32 and a['traffic']['grows']==0
 for field in ['byte_capacity','range_capacity','result_capacity','reserves']:assert a['scratch'][field]==0
pairs=[]
for kind in ['scalar','allocator']:
 def rows(stage):return {row['name']:row for row in csv.DictReader((r/f'{stage}-{kind}.csv').open())}
 old,new=rows('baseline'),rows('candidate');assert old.keys()==new.keys()
 for name in sorted(old):
  a=int(old[name]['instructions']);b=int(new[name]['instructions'])
  assert all(int(row[f])==0 for row in [old[name],new[name]] for f in ['heap_increase','stable_memory_increase'])
  pairs.append({'name':name,'before':a,'after':b,'delta':b-a,'percent':100*(b-a)/a,'gate':'regressed; no waiver'})
layout=re.search(r'PREPARED_LAYOUT image=(\d+) range=(\d+) result=(\d+) inline_bytes=(\d+) inline_ranges=(\d+) inline_results=(\d+)',(r/'final-map.log').read_text());assert layout
measurement={'native_before':before,'native_after':after,'ic_pairs':pairs,'native_object_sizes':dict(zip(['image','range','result','inline_bytes','inline_ranges','inline_results'],map(int,layout.groups()))),'limitations':['Native object sizes are not compiler frame sizes or Wasm object/frame sizes.','Owner reserve counters do not observe all process/backend allocations.','Zero canbench page growth is not zero allocation.','Traffic and instruction deltas are separate gates; no component-level instruction attribution.'],'status':'native traffic/scratch gates pass; every IC phase and retained control regressed'}
(r/'measurements.json').write_text(json.dumps(measurement,indent=2)+'\n')
# Preserve exact current source plus owner-only projections used for Lizard.
def end_brace(s,a):
 depth=1;i=a+1
 while depth:
  depth+=(s[i]=='{')-(s[i]=='}');i+=1
 return i
def mask(s,a,b):return s[:a]+''.join('\n' if c=='\n' else ' ' for c in s[a:b])+s[b:]
def production(s):
 excluded=[]
 a=s.index('#[cfg(test)]\nmod tests {');excluded.append((a,len(s),'test module'));s=mask(s,a,len(s))
 # All these cfg nodes are within this one inspected file, not a general Rust preprocessor.
 for match in reversed(list(re.finditer(r'(?m)^[ \t]*#\[cfg\(test\)\]\n',s))):
  a=match.start();j=match.end();tail=s[j:].lstrip();label=tail.splitlines()[0]
  if tail.startswith(('fn ','pub fn ','struct ','#[derive','if ','{')):
   opening=s.index('{',j);b=end_brace(s,opening)
  else:
   stack=[];b=j
   while b<len(s):
    c=s[b]
    if c in '([{':stack.append(c)
    elif c in ')]}':stack.pop()
    elif c in ',;' and not stack:b+=1;break
    b+=1
  excluded.append((a,b,label));s=mask(s,a,b)
 m=re.search(r'#\[cfg\(any\(test, feature = "canbench"\)\)\]\nfn seed_scalar_work',s)
 if m:
  a=m.start();b=end_brace(s,s.index('{',m.end()));excluded.append((a,b,'shared fixture seeder'));s=mask(s,a,b)
 s=re.sub(r'#\[cfg\(not\(test\)\)\]',lambda m:' '*len(m.group()),s)
 return s,excluded
def owner(s,line):
 pos=sum(len(x) for x in s.splitlines(True)[:line-1]);candidates=[]
 for m in re.finditer(r'(?m)^impl[^\{]+\{',s):
  if m.start()<pos<end_brace(s,m.end()-1):
   name=m.group()[4:-1].strip()
   while re.search(r'<[^<>]*>',name):name=re.sub(r'<[^<>]*>','',name)
   candidates.append(name.strip())
 return candidates[-1] if candidates else '<free>'
originals={'base':(r/'before'/mp).read_text(),'current':(repo/mp).read_text()}
projections={};exclusions={}
for stage,text in originals.items():
 proj,exclusions[stage]=production(text);projections[stage]=proj
 p=r/'lizard'/stage/'lib.rs';p.parent.mkdir(parents=True,exist_ok=True);p.write_text(proj)
 b=r/'lizard'/stage/'bench.rs';b.write_bytes((r/'before'/bp if stage=='base' else repo/bp).read_bytes())
 current=r/'after'/mp;current.parent.mkdir(parents=True,exist_ok=True)
for path in [mp,bp]:
 p=r/'after'/path;p.parent.mkdir(parents=True,exist_ok=True);p.write_bytes((repo/path).read_bytes())
diff=''.join(difflib.unified_diff(originals['base'].splitlines(True),originals['current'].splitlines(True),n=0,fromfile='approved-admission/lib.rs',tofile='candidate/lib.rs'))
(r/'production-and-tests.patch').write_text(diff)
hunks={'base':[],'current':[]}
for a,n,b,m in re.findall(r'@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@',diff):
 for stage,x,count in [('base',a,n),('current',b,m)]:
  count=int(count or 1)
  if count:hunks[stage].append((int(x),int(x)+count-1))
metrics={}
for stage in ['base','current']:
 with (r/'lizard'/f'{stage}.csv').open('w') as out:
  subprocess.run(['lizard','--csv','-i','-1',str(r/'lizard'/stage/'lib.rs'),str(r/'lizard'/stage/'bench.rs')],stdout=out,check=True,timeout=4)
 rows=[]
 for row in csv.reader((r/'lizard'/f'{stage}.csv').open()):
  file=Path(row[6]).name;line=int(row[9]);end=int(row[10]);name=row[7]
  qualified=(owner(projections[stage],line)+'::'+name) if file=='lib.rs' else name
  touched=(file=='lib.rs' and any(line<=b and a<=end for a,b in hunks[stage])) or (file=='bench.rs' and name=='bench_paged_scalar_edits_97')
  rows.append(dict(file=file,name=name,qualified=qualified,long_name=row[8],nloc=int(row[0]),ccn=int(row[1]),tokens=int(row[2]),parameters=int(row[3]),length=int(row[4]),start=line,end=end,touched=touched))
 metrics[stage]=rows
summary={}
for stage,rows in metrics.items():
 summary[stage]={}
 for group,subset in [('owner_all',[x for x in rows if x['file']=='lib.rs']),('owner_touched',[x for x in rows if x['file']=='lib.rs' and x['touched']]),('new_benchmark',[x for x in rows if x['file']=='bench.rs' and x['touched']])]:
  summary[stage][group]={'functions':len(subset),'ccn':sum(x['ccn'] for x in subset),'nloc':sum(x['nloc'] for x in subset),'tokens':sum(x['tokens'] for x in subset),'max_ccn':max([x['ccn'] for x in subset],default=0)}
lizard={'mode':'explicit: two allowed Rust files; approved admission snapshot, not HEAD before that repair','version':'1.24.0','baseline_sha256':sha(r/'before'/mp),'current_sha256':sha(repo/mp),'summary':summary,'functions':metrics,'projection_exclusions':exclusions,'limitations':'Line-preserving source projection removes test nodes and shared fixture seeder; not compiled-code complexity. Raw owner snapshots/CSV and exclusions are preserved. Keep inline cfg removal separate from public API/owner logic. No Lizard threshold or suppression introduced.'}
(r/'lizard.json').write_text(json.dumps(lizard,indent=2)+'\n')
# Build provenance: Wasm hashes are per invocation. Capture the remaining final native executable.
log=(r/'final-map.log').read_text();m=re.search(r'Running unittests src/lib.rs \(([^)]+)\)',log);assert m
binary=repo/m.group(1);assert binary.exists()
(r/'native-build.json').write_text(json.dumps({'final_test_binary':str(binary),'sha256':sha(binary),'source_sha256':sha(repo/mp),'baseline_native_binary_hash':'not captured; source/lock/compile/runtime transcript retained, no retrospective rerun','wasm_hashes':{name:json.loads((r/(name+'.json')).read_text())['wasm_sha256'] for name in ['baseline-scalar','baseline-allocator','candidate-scalar','candidate-allocator']}},indent=2)+'\n')
print(json.dumps({'measurements':measurement,'lizard_summary':summary},indent=2))
