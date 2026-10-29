"""Entry10: retained-reference comparisons and ONE static disassembly of corrected Wasm."""
import csv
import hashlib
import json
from pathlib import Path
import re
import subprocess
import time

from run import ROOT, REPO, MAP, admin, sha

OLD = Path('/tmp/spill-map-f297-cecc5rw_')


def load_csv(path):
    return {row['name']: row for row in csv.DictReader(path.read_text().splitlines())}


def native(path):
    text = path.read_text()
    # Normal scheduler completions may occur inside an output field; remove whole completion text.
    text = re.sub(r'test tests::\w+ \.\.\. ok\n', '', text)
    result = {}
    for name, traffic, work in re.findall(r'SCALAR_WORK (\w+) traffic=Traffic \{([^}]+)\} scratch=WorkObservation \{([^}]+)\}', text):
        fields = lambda value: {key: int(number) for key,number in re.findall(r'(\w+): (\d+)', value)}
        result[name] = {'traffic': fields(traffic), 'scratch': fields(work)}
    assert set(result) == {'replace','missing','tail_insert','tail_remove','split'}, result
    expected = {'replace':(9,112,1,8),'missing':(12,128,0,0),'tail_insert':(12,128,3,26),'tail_remove':(13,136,2,10),'split':(94,1368,8,1146)}
    for name, numbers in expected.items():
        traffic = result[name]['traffic']; work = result[name]['scratch']
        assert tuple(traffic[k] for k in ('reads','read_bytes','writes','write_bytes')) == numbers
        assert traffic['grows'] == traffic['grown_pages'] == 0
        if name != 'split':
            assert traffic['max_read'] <= 32
            assert all(work[k] == 0 for k in ('byte_capacity','range_capacity','result_capacity','reserves','grows','grown_pages'))
    assert result['missing']['scratch']['dirty_bytes'] == result['missing']['scratch']['ranges'] == 0
    return result


def collect():
    assert (REPO / MAP).read_bytes() == (ROOT / 'pre-ic/lib.rs').read_bytes()
    pairs = {}
    for kind, old_kind in [('scalar','scalar'), ('allocator','allocator')]:
        base = load_csv(OLD / f'baseline-{old_kind}.csv')
        blocked = load_csv(OLD / f'candidate-{old_kind}.csv')
        current = load_csv(ROOT / f'{kind}.csv')
        assert current.keys() == base.keys() == blocked.keys()
        for name,row in current.items():
            n = int(row['instructions']); a = int(base[name]['instructions']); b = int(blocked[name]['instructions'])
            assert row['scope_calls'] == base[name]['scope_calls'] == blocked[name]['scope_calls']
            assert int(row['heap_increase']) == int(row['stable_memory_increase']) == 0
            pairs[name] = {'admission':a, 'blocked':b, 'correction':n, 'delta_vs_admission':n-a, 'percent_vs_admission':100*(n-a)/a, 'delta_vs_blocked':n-b, 'percent_vs_blocked':100*(n-b)/b, 'original_gate_passed': n<=a}
    measurements = {'source_sha256': sha(REPO / MAP), 'references': 'retained immutable measurements; no new baseline campaign', 'ic':pairs, 'native':native(ROOT/'map.log'), 'baseline_scratch':'unobserved, not zero', 'ic_page_growth':0, 'not_claimed':'per-component IC pricing, noise waiver, global allocations or peak stack'}
    (ROOT/'measurements.json').write_text(json.dumps(measurements,indent=2)+'\n')
    meta = json.loads((ROOT/'scalar.json').read_text())
    artifact = next(a for a in meta['artifacts'] if a['kind']=='wasm')
    wasm = ROOT/artifact['archive']
    assert sha(wasm) == artifact['sha256']
    assert meta['source_hashes'][MAP] == sha(REPO / MAP)
    tool = Path('/opt/homebrew/Cellar/binaryen/132/bin/wasm-dis')
    assert sha(tool) == '644465f704a4d27e5bfae0d8914e83df73dc83e0516d1827086e57113f2f7bb5'
    wat = ROOT/'corrected-scalar.wat'
    assert not wat.exists(), 'only one disassembly authorized'
    argv = [str(tool),str(wasm),'-o',str(wat)]
    began = time.monotonic()
    r = subprocess.run(argv,capture_output=True,text=True,timeout=8)
    (ROOT/'wasm-dis.stdout').write_text(r.stdout); (ROOT/'wasm-dis.stderr').write_text(r.stderr)
    dis = {'argv':argv,'exit':r.returncode,'elapsed':time.monotonic()-began,'tool_sha256':sha(tool),'tool_version':'132, identical verified existing binary','wasm_sha256':sha(wasm),'source_sha256':sha(REPO / MAP),'wat_sha256':sha(wat),'mode':'static inspection only; no Wasm execution probe'}
    (ROOT/'disassembly.json').write_text(json.dumps(dis,indent=2)+'\n')
    assert r.returncode==0,(r.stdout,r.stderr)
    lines=wat.read_text().splitlines(True)
    starts=[i for i,line in enumerate(lines) if line.startswith(' (func ')]
    index=[]
    out=ROOT/'wasm-functions'; out.mkdir()
    for n,a in enumerate(starts):
        symbol=lines[a].strip()
        if not ('ic_stable_paged_ordered_map' in symbol and any(token in symbol for token in ('scalar_edit','prepare_image','apply_edit','9EditImage','8EditPlan','10PageHeader'))):
            continue
        b=starts[n+1] if n+1<len(starts) else len(lines)
        text=''.join(lines[a:b]); target=out/f'{a+1}.wat'; target.write_text(text)
        calls=sorted(set(re.findall(r'\(call (\$\S+)',text)))
        operations=[{'line':i+1,'operator':lines[i].strip()} for i in range(a,b) if any(op in lines[i] for op in ('(memory.copy','(memory.fill','(global.set $__stack_pointer'))]
        index.append({'symbol':symbol,'start':a+1,'end':b,'path':str(target.relative_to(ROOT)),'sha256':sha(target),'calls':calls,'structural_operations':operations})
    (ROOT/'wasm-index.json').write_text(json.dumps(index,indent=2)+'\n')
    return {'ic':pairs,'function_index':[{k:r[k] for k in ('symbol','start','end','path')} for r in index], 'disassembly':dis}


if __name__ == '__main__':
    print(json.dumps(admin('collect',collect),indent=2))
