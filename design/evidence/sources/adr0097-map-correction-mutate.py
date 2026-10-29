"""Apply each authorized mutation once; unchanged tests, exact source restoration."""
import difflib
import hashlib
import json
from pathlib import Path
import sys
import time

from run import ROOT, REPO, MAP, admit, run

name = sys.argv[1]
admit(name)  # Mutation preparation is also admitted before touching source.
began = time.monotonic()
path = REPO / MAP
original = path.read_bytes()
sha = lambda b: hashlib.sha256(b).hexdigest()
candidate = ROOT / 'candidate' / 'lib.rs'
if not candidate.exists():
    candidate.parent.mkdir()
    candidate.write_bytes(original)
assert candidate.read_bytes() == original
text = original.decode()
if name == 'mutation-early':
    old = '            plan.image.previous.push_reserved(previous);'
    new = '''            if at == 0 {
                for range in plan.image.ranges.as_slice() {
                    self.memory.write(range.address,
                        &plan.image.bytes.as_slice()[range.offset..range.offset + range.len]);
                }
            }
''' + old
    assert text.count(old) == 1
    mutated = text.replace(old, new)
elif name == 'mutation-resource':
    old = '        self.admit_image(plan.image)?;'
    assert text.count(old) == 1
    mutated = text.replace(old, '')
    old = '    fn publish_image(&self, image: &EditImage) {\n'
    assert mutated.count(old) == 1
    mutated = mutated.replace(old, old + '        self.admit_image(image).expect("deferred backing reservation");\n')
else:
    raise ValueError(name)
(ROOT / (name + '.patch')).write_text(''.join(difflib.unified_diff(text.splitlines(True), mutated.splitlines(True), fromfile='correction/lib.rs', tofile='mutation/lib.rs')))
preparation = time.monotonic() - began
try:
    path.write_text(mutated)
    run(name)
finally:
    restore_began = time.monotonic()
    path.write_bytes(original)
    assert path.read_bytes() == original
    restoration = {'candidate_sha256': sha(original), 'mutation_sha256': sha(mutated.encode()), 'restored_sha256': sha(path.read_bytes()), 'preparation_seconds': preparation, 'restoration_seconds': time.monotonic()-restore_began}
    ledger = json.loads((ROOT / 'ledger.json').read_text())
    entry = ledger['entries'][-1]
    assert entry['name'] == name
    entry['mutation_extra_charge'] = preparation + restoration['restoration_seconds']
    entry['charged'] += entry['mutation_extra_charge']
    assert entry['charged'] <= entry['cap']
    (ROOT / 'ledger.json').write_text(json.dumps(ledger, indent=2) + '\n')
    (ROOT / (name + '-restoration.json')).write_text(json.dumps(restoration, indent=2) + '\n')
    print(json.dumps(restoration, indent=2))
