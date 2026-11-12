"""Sweep window stride, seeds and maskless slot extraction on the long slot benchmark."""
import json, os, random, sys
from pathlib import Path
import torch
from safetensors.torch import load_file
import laya
from laya.common import encode_text

ROOT = Path(__file__).resolve().parents[4]
MODEL_DIR = Path(os.environ.get("GLEV_MODEL_DIR", ROOT / "target" / "glev-model"))
MODEL = Path(sys.argv[1])
WINDOW = int(os.environ.get("SWEEP_WINDOW", "32"))
SLOT_TYPES = ["anchor", "hops", "concept", "depth", "team", "topic"]
NUM_TAGS = 1 + 2 * len(SLOT_TYPES)
OP_SLOTS = {
    "citation-reach": ["anchor", "depth"],
    "document-text-search": ["topic"],
    "shortest-path": ["from", "to"],
    "team-readable-documents": ["team", "topic"],
    "variable-length-reach": ["anchor", "hops"],
}
entities = json.load(open(ROOT / "demo/knowledge/eval/routing/entities.json"))
CONCEPTS, DOCUMENTS = entities.get("concepts", []), entities.get("documents", [])
TEAMS = entities.get("teams", [])
agent = laya.load(str(MODEL_DIR), device="mps")
model, tok = agent.model, agent.tok
slot_head = torch.nn.Linear(model.encoder.config.hidden_size, NUM_TAGS).to("mps")
state = load_file(str(MODEL / "model.safetensors"))
assert not model.load_state_dict(
    {k: v for k, v in state.items() if not k.startswith("slot_head.")}, strict=False
).unexpected_keys
slot_head.load_state_dict({k.removeprefix("slot_head."): v for k, v in state.items()
                           if k.startswith("slot_head.")})
model.eval(); slot_head.eval()
mask_id, bos_id = tok.mask_token_id, tok.bos_token_id


def window_probs(tokens, maskless):
    ids = ([bos_id] + tokens) if maskless else ([bos_id] + tokens + [mask_id] * 6)
    input_ids = torch.tensor([ids], device="mps")
    attn = torch.ones_like(input_ids)
    with torch.no_grad():
        h = model.encoder(input_ids=input_ids, attention_mask=attn).last_hidden_state
        h = h + model.type_emb(torch.zeros(1, dtype=torch.long, device="mps"))[:, None, :]
        return torch.softmax(slot_head(h)[0, 1:1 + len(tokens)].float(), -1).cpu()


BOUNDARY_IDS = set()
for mark in (".", "?", "!", ";", "。", "？", "！", "、"):
    value = tok.convert_tokens_to_ids(mark)
    if value is not None and value != tok.unk_token_id:
        BOUNDARY_IDS.add(int(value))
BOUNDARY = os.environ.get("SWEEP_BOUNDARY") == "1"


def window_starts(tokens, stride):
    if not BOUNDARY:
        return list(range(0, max(len(tokens), 1), stride))
    starts = [0]
    while starts[-1] < len(tokens) - 1:
        start = starts[-1]
        end = min(start + WINDOW, len(tokens))
        for position in range(end, start + WINDOW // 2, -1):
            if tokens[position - 1] in BOUNDARY_IDS:
                end = position
                break
        next_start = max(end - 6, start + 1)
        starts.append(next_start)
    return starts


def fuse(tokens, stride, maskless=False, mode="edge"):
    tags, conf = [0] * len(tokens), [0.0] * len(tokens)
    edge = [-1] * len(tokens)
    total = torch.zeros((len(tokens), NUM_TAGS))
    for start in window_starts(tokens, stride):
        window = tokens[start:start + WINDOW]
        probs = window_probs(window, maskless)
        for offset in range(len(window)):
            position = start + offset
            total[position] += probs[offset]
            distance = min(offset, len(window) - 1 - offset) if len(window) > 1 else 0
            if distance > edge[position]:
                edge[position] = distance
                tags[position] = int(probs[offset].argmax())
                conf[position] = float(probs[offset].max())
    if mode == "sum":
        tags = total.argmax(-1).tolist()
        conf = total.max(-1).values.tolist()
    return tags, conf


def spans_from(tags, offsets, text, conf, threshold):
    raw = []
    index = 0
    while index < len(tags):
        tag = tags[index]
        if tag > 0:
            slot = SLOT_TYPES[(tag - 1) // 2]
            first = index
            while index + 1 < len(tags) and tags[index + 1] == 2 * (SLOT_TYPES.index(slot) + 1):
                index += 1
            raw.append((slot, first, index + 1))
        index += 1
    merged = []
    for slot, start, end in raw:
        average = sum(conf[start:end]) / max(end - start, 1)
        if average < threshold:
            continue
        if merged and merged[-1][0] == slot and merged[-1][2] >= start:
            merged[-1] = (slot, merged[-1][1], max(merged[-1][2], end))
        else:
            merged.append((slot, start, end))
    return [(slot, text[offsets[start][0]:offsets[end - 1][1]], start, end)
            for slot, start, end in merged]


def snap(text, names):
    needle = " ".join(text.split()).lower()
    exact = [name for name in names if " ".join(name.split()).lower() == needle]
    if exact:
        return exact[0]
    candidates = [name for name in names
                  if needle in " ".join(name.split()).lower()
                  or " ".join(name.split()).lower() in needle]
    return max(candidates, key=len) if candidates else text


POLICY = os.environ.get("SWEEP_POLICY", "")


def policy_windows(length):
    if length <= 32:
        return [(0, length)]
    if POLICY == "adaptive":
        window, stride = (24, 12) if length <= 64 else (WINDOW, max(1, WINDOW // 2))
    else:
        window, stride = WINDOW, max(1, WINDOW // 2)
    return [(start, min(start + window, length)) for start in range(0, length, stride)]


def evaluate(rows, stride, seed, maskless=False, threshold=0.0, mode="edge"):
    random.seed(seed)
    totals = {slot: [0, 0, 0] for slot in SLOT_TYPES + ["from", "to"]}
    buckets = {"first": [0, 0, 0], "middle": [0, 0, 0], "last": [0, 0, 0]}
    exact = count = 0
    for row in rows:
        question = row["question"]
        if os.environ.get("SWEEP_FILLER") == "none":
            filler = ""
        elif os.environ.get("SWEEP_FILLER", "plain") == "sentences":
            sentences = [
                "Thanks for asking.",
                "Here is some background.",
                "We discussed several topics earlier.",
                "Please focus on the question below.",
                "The team reviewed the material.",
                "Consider the following details.",
                "This is an internal note.",
                "Now for the actual request.",
            ]
            filler = " ".join(random.sample(sentences, random.randint(3, 6))) + " "
        else:
            filler = "note " * random.randint(8, 40)
        position = os.environ.get("SWEEP_POSITION", "before")
        if filler == "":
            text = question
        elif position == "before":
            text = filler + question
        elif position == "after":
            text = question + " " + filler
        else:
            text = filler[: len(filler) // 2] + question + " " + filler[len(filler) // 2:]
        encoded = encode_text(tok, text, add_special_tokens=False, truncation=True,
                              max_length=256, return_offsets_mapping=True)
        tokens = [int(t) for t in encoded["input_ids"]]
        offsets = [(int(a), int(b)) for a, b in encoded["offset_mapping"]]
        if POLICY:
            tags, conf = [0] * len(tokens), [0.0] * len(tokens)
            edge = [-1] * len(tokens)
            second_tag = [0] * len(tokens)
            second_prob = [0.0] * len(tokens)
            accum = torch.zeros((len(tokens), NUM_TAGS))
            for start, end in policy_windows(len(tokens)):
                window = tokens[start:end]
                probs = window_probs(window, maskless)
                for offset in range(len(window)):
                    position = start + offset
                    accum[position] += probs[offset]
                    distance = min(offset, len(window) - 1 - offset) if len(window) > 1 else 0
                    if distance > edge[position]:
                        edge[position] = distance
                        top = probs[offset].topk(2)
                        tags[position] = int(top.indices[0])
                        conf[position] = float(top.values[0])
                        second_tag[position] = int(top.indices[1])
                        second_prob[position] = float(top.values[1])
            threshold2 = float(os.environ.get("SWEEP_TOP2", "0"))
            if threshold2 > 0:
                for index in range(len(tokens)):
                    if tags[index] == 0 and second_prob[index] >= threshold2:
                        tags[index] = second_tag[index]
                        conf[index] = second_prob[index]
        else:
            tags, conf = fuse(tokens, stride, maskless, mode)
        predicted = {}
        expand = os.environ.get("SWEEP_EXPAND") == "1"
        for slot, value, start, end in spans_from(tags, offsets, text, conf, threshold):
            pool = {"anchor": CONCEPTS + DOCUMENTS, "concept": CONCEPTS,
                    "team": TEAMS, "topic": CONCEPTS + DOCUMENTS}.get(slot)
            matched = snap(value, pool) if pool else value
            if expand and pool and matched == value:
                for radius in (1, 2, 3):
                    first = offsets[max(start - radius, 0)][0]
                    last = offsets[min(end - 1 + radius, len(offsets) - 1)][1]
                    context = text[first:last].lower()
                    candidates = [name for name in pool if " ".join(name.split()).lower() in context]
                    if candidates:
                        matched = max(candidates, key=len)
                        break
            predicted.setdefault(slot, []).append(matched)
        if row["expected_op"] == "shortest-path":
            concepts = predicted.pop("concept", [])
            if concepts:
                predicted["from"] = [concepts[0]]
            if len(concepts) > 1:
                predicted["to"] = [concepts[1]]
        ok = True
        for slot in OP_SLOTS[row["expected_op"]]:
            expected = row["slots"].get(slot)
            if expected is None:
                continue
            values = predicted.get(slot, [])
            match = bool(values) and " ".join(values[0].split()).lower() == " ".join(
                str(expected).split()).lower()
            totals[slot][0] += int(match); totals[slot][1] += int(not match); totals[slot][2] += int(not match)
            pos = text.find(str(expected), text.find(question))
            if pos >= 0:
                first = next((i for i, (a, b) in enumerate(offsets) if a <= pos < b), 0)
                ratio = first / max(len(tokens) - 1, 1)
                bucket = "first" if ratio < 1 / 3 else ("middle" if ratio < 2 / 3 else "last")
                buckets[bucket][0] += int(match)
                buckets[bucket][1] += int(not match)
                buckets[bucket][2] += int(not match)
            ok = ok and match
        exact += int(ok); count += 1
    tp = sum(v[0] for v in totals.values())
    fp = sum(v[1] for v in totals.values())
    fn = sum(v[2] for v in totals.values())
    precision, recall = tp / max(tp + fp, 1), tp / max(tp + fn, 1)
    f1 = 2 * precision * recall / max(precision + recall, 1e-9)
    for name, (b_tp, b_fp, b_fn) in buckets.items():
        b_precision, b_recall = b_tp / max(b_tp + b_fp, 1), b_tp / max(b_tp + b_fn, 1)
        b_f1 = 2 * b_precision * b_recall / max(b_precision + b_recall, 1e-9)
        print(f"   position {name:6s}: F1 {b_f1:.3f} (tp {b_tp} fp {b_fp} fn {b_fn})")
    if os.environ.get("SWEEP_VERBOSE") == "1":
        for slot, (s_tp, s_fp, s_fn) in totals.items():
            if s_tp + s_fp + s_fn:
                precision = s_tp / max(s_tp + s_fp, 1)
                recall = s_tp / max(s_tp + s_fn, 1)
                slot_f1 = 2 * precision * recall / max(precision + recall, 1e-9)
                print(f"      {slot:8s} F1 {slot_f1:.3f} tp {s_tp} fp {s_fp} fn {s_fn}")
    return f1, exact / count


rows = [json.loads(l) for l in open(ROOT / "demo/knowledge/eval/routing/slots/dev.jsonl")]
rows = [row for row in rows if row.get("expected_op") and row.get("slots")]
scores = [evaluate(rows, 10, seed) for seed in (5, 17, 29)]
print(f"policy {POLICY or 'off'} (3 seeds): F1 {sum(s[0] for s in scores)/3:.3f} "
      f"exact {sum(s[1] for s in scores)/3:.3f}")
