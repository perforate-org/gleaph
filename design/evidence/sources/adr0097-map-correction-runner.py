"""Fresh, fixed-order correction validation; no source-baseline swapping or retry path."""
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time

ROOT = Path(__file__).parent
REPO = Path('/Users/yota/dev/gleaph-spill')
MAP = 'crates/ic-stable-paged-ordered-map/src/lib.rs'
START = json.loads((ROOT / 'start.json').read_text())
MAP_CWD = REPO / 'crates/ic-stable-paged-ordered-map'
LARA_CWD = REPO / 'crates/ic-stable-lara'
MAP_TEST = ['cargo', 'test', '-p', 'ic-stable-paged-ordered-map', '--lib', 'tests::', '--', '--nocapture']
DOC = ['cargo', 'test', '-p', 'ic-stable-paged-ordered-map', '--doc', '--', '--show-output']
SCHEDULE = [
    ('setup', 20, None, 'pre', REPO),
    ('format', 5, ['rustfmt', '--edition', '2024', '--config', 'skip_children=true', MAP], 'pre', REPO),
    ('map', 30, MAP_TEST, 'pre', REPO),
    ('doc', 10, DOC, 'pre', REPO),
    ('mutation-early', 10, ['cargo', 'test', '-p', 'ic-stable-paged-ordered-map', '--lib', 'prepared_edits_directory_rejection', '--', '--nocapture'], 'pre', REPO),
    ('mutation-resource', 10, ['cargo', 'test', '-p', 'ic-stable-paged-ordered-map', '--lib', 'prepared_edits_resources_and_retry', '--', '--nocapture'], 'pre', REPO),
    ('source-audit', 20, None, 'pre', REPO),
    ('scalar', 75, ['canbench', 'bench_paged_scalar_edits_97', '--csv'], 'pre', MAP_CWD),
    ('allocator', 100, ['canbench', 'bench_r_fs_tkbfs_1024', '--csv'], 'pre', LARA_CWD),
    ('collect', 20, None, 'pre', REPO),
    ('final-format', 5, ['rustfmt', '--check', '--edition', '2024', '--config', 'skip_children=true', MAP, 'crates/ic-stable-paged-ordered-map/src/bench.rs'], 'final', REPO),
    ('final-clippy', 25, ['cargo', 'clippy', '-p', 'ic-stable-paged-ordered-map', '-p', 'ic-stable-lara', '--lib', '--', '-D', 'warnings'], 'final', REPO),
    ('final-map', 20, MAP_TEST, 'final', REPO),
    ('final-free', 25, ['cargo', 'test', '-p', 'ic-stable-lara', '--lib', 'lara::edge::free_span::tests::', '--', '--nocapture'], 'final', REPO),
    ('final-doc', 15, DOC, 'final', REPO),
    ('closure', 15, None, 'final', REPO),
    ('final-evidence-contingency', 15, None, 'final', REPO),
]


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest() if path.is_file() else None


def admit(name):
    ledger = json.loads((ROOT / 'ledger.json').read_text())
    assert ledger['status'] == 'active', ledger['status']
    next_entry = SCHEDULE[len(ledger['entries'])]
    assert name == next_entry[0], (name, next_entry)
    _, cap, _, phase, _ = next_entry
    total = sum(e['charged'] for e in ledger['entries'])
    phase_used = sum(e['charged'] for e in ledger['entries'] if e['phase'] == phase)
    assert total + cap + (120 if phase == 'pre' else 0) <= 600
    assert phase_used + cap <= (300 if phase == 'pre' else 120)
    assert 'RUST_TEST_THREADS' not in os.environ
    budget = {'before_total': total, 'before_phase': phase_used, 'cap': cap, 'remaining_maximum': 600-total, 'protected_final': 120 if phase == 'pre' else 120-phase_used}
    (ROOT / f'{name}-admission.json').write_text(json.dumps(budget, indent=2) + '\n')
    return ledger, next_entry


def record(ledger, entry):
    ledger['entries'].append(entry)
    if entry['status'] != 'passed':
        ledger['status'] = 'stopped-incomplete'
    (ROOT / 'ledger.json').write_text(json.dumps(ledger, indent=2) + '\n')


def admin(name, work):
    ledger, (_, cap, _, phase, _) = admit(name)
    began = time.monotonic()
    entry = {'name': name, 'phase': phase, 'cap': cap, 'charged': cap, 'status': 'failed'}
    try:
        result = work()
        entry['actual'] = time.monotonic() - began
        assert entry['actual'] <= cap, entry
        entry['status'] = 'passed'
        entry['result'] = result
        return result
    finally:
        entry.setdefault('actual', time.monotonic() - began)
        record(ledger, entry)


def run(name):
    ledger, (_, cap, argv, phase, cwd) = admit(name)
    assert argv is not None
    began = time.monotonic()
    log = ROOT / f'{name}.log'
    assert not log.exists(), 'no repeated invocation'
    lock = REPO / 'Cargo.lock'
    csvs = [MAP_CWD / 'canbench_results.csv', LARA_CWD / 'canbench_results.csv']
    snapshots = {p: p.read_bytes() if p.exists() else None for p in [lock, *csvs]}
    entry = {'name': name, 'phase': phase, 'cap': cap, 'argv': argv, 'cwd': str(cwd), 'status': 'failed', 'source_hashes': {}, 'artifacts': [], 'exit': None}
    try:
        for p, h in START['initial_hashes'].items():
            if p == MAP or p in ('plans/0340-prepared-paged-map-edits.md', 'design/adr/0088-tree-csr-mode-for-high-degree-label-buckets.md', 'design/evidence/adr0097-demotion-design.md'):
                continue
            assert sha(REPO / p) == h, ('protected input', p)
        entry['source_hashes'] = {p: sha(REPO / p) for p in (MAP, 'crates/ic-stable-paged-ordered-map/src/bench.rs', 'crates/ic-stable-lara/src/lara/edge/free_span.rs')}
        source = ROOT / 'sources' / f'{name}.rs'
        source.parent.mkdir(exist_ok=True)
        shutil.copyfile(REPO / MAP, source)
        with log.open('wb') as stream:
            r = subprocess.run(argv, cwd=cwd, stdout=stream, stderr=subprocess.STDOUT, timeout=max(0.1, cap - (time.monotonic()-began) - 0.5))
        entry['exit'] = r.returncode
        text = log.read_text(errors='replace')
        artifacts = ROOT / 'artifacts' / name
        artifacts.mkdir(parents=True)
        # Archive every durable native test executable named by the actual Cargo invocation.
        for number, match in enumerate(re.finditer(r'Running unittests [^\n]*\(([^\n]+)\)', text)):
            path = Path(match.group(1))
            if not path.is_absolute():
                path = cwd / path
            assert path.is_file(), ('missing emitted native executable', path)
            target = artifacts / f'native-{number}'
            shutil.copyfile(path, target)
            entry['artifacts'].append({'kind': 'native-test-executable', 'emitted_path': str(path), 'archive': str(target.relative_to(ROOT)), 'sha256': sha(target), 'bytes': target.stat().st_size})
        if argv[0] == 'canbench':
            wasm_name = 'ic_stable_paged_ordered_map' if name == 'scalar' else 'ic_stable_lara'
            wasm = REPO / f'target/canbench/wasm32-unknown-unknown/release/{wasm_name}.wasm'
            assert wasm.is_file()
            target = artifacts / (wasm_name + '.wasm')
            shutil.copyfile(wasm, target)
            entry['artifacts'].append({'kind': 'wasm', 'emitted_path': str(wasm), 'archive': str(target.relative_to(ROOT)), 'sha256': sha(target), 'bytes': target.stat().st_size})
            csv = cwd / 'canbench_results.csv'
            assert csv.exists()
            shutil.copyfile(csv, ROOT / f'{name}.csv')
            entry['csv_sha256'] = sha(csv)
        if '--lib' in argv and 'test' in argv:
            assert entry['artifacts'], 'native build provenance was not captured'
        entry['used_lock_sha256'] = sha(lock)
        shutil.copyfile(lock, artifacts / 'as-used.Cargo.lock')
        if argv[0] in ('cargo', 'canbench'):
            assert entry['used_lock_sha256'] == 'f5d323d0c66d9afe939a6c7e13db316e201c39fce4c4bde44e515a8c855496bf', 'as-used lock mismatch blocks comparability'
        expected = 101 if name.startswith('mutation-') else 0
        assert r.returncode == expected, ('unexpected command exit', r.returncode, expected)
        if name in ('map', 'final-map'):
            assert 'test result: ok. 17 passed; 0 failed;' in text
        if name == 'final-free':
            assert 'test result: ok. 27 passed; 0 failed;' in text
        if name in ('doc', 'final-doc'):
            assert 'test result: ok. 5 passed; 0 failed;' in text
            assert all(code in text for code in ('E0502', 'E0499', 'E0061', 'E0382'))
        if name == 'mutation-early':
            assert 'late DirectoryFull changed backing' in text and 'test result: FAILED. 0 passed; 1 failed;' in text
        if name == 'mutation-resource':
            assert 'expected actual restricted grow3/+2 before ready' in text and 'test result: FAILED. 0 passed; 1 failed;' in text
        entry['status'] = 'passed'
    except subprocess.TimeoutExpired:
        entry['exit'] = 124
        entry['failure'] = 'command cap reached; no retry authorized'
        raise
    except BaseException as error:
        entry['failure'] = repr(error)
        raise
    finally:
        # Preserve observed lock even after a failed/timeout invocation; always restore before next.
        after_lock = lock.read_bytes()
        lock_dir = ROOT / 'locks'
        lock_dir.mkdir(exist_ok=True)
        (lock_dir / f'{name}.lock').write_bytes(after_lock)
        entry['observed_lock_sha256'] = hashlib.sha256(after_lock).hexdigest()
        for p, data in snapshots.items():
            if data is None:
                if p.exists():
                    if p.name.endswith('.csv'):
                        shutil.copyfile(p, ROOT / f'{name}-{p.parent.name}-generated.csv')
                    p.unlink()
            elif not p.exists() or p.read_bytes() != data:
                p.write_bytes(data)
        entry['restored'] = {str(p.relative_to(REPO)): sha(p) for p in snapshots}
        assert all((p.read_bytes() if p.exists() else None) == data for p, data in snapshots.items())
        entry['charged'] = time.monotonic() - began
        if entry['charged'] > cap:
            entry['status'] = 'failed'
            entry['failure'] = 'invocation including restoration exceeded cap'
        record(ledger, entry)
        (ROOT / f'{name}.json').write_text(json.dumps(entry, indent=2) + '\n')
        print(json.dumps({'name': name, 'status': entry['status'], 'exit': entry['exit'], 'charged': entry['charged'], 'total': sum(e['charged'] for e in ledger['entries']), 'final_used': sum(e['charged'] for e in ledger['entries'] if e['phase']=='final'), 'artifacts': entry['artifacts']}, indent=2))
    assert entry['status'] == 'passed'


if __name__ == '__main__':
    run(sys.argv[1])
