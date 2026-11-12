"""Mid-length policy boundary and long-question op+slot end-to-end evaluation."""
import json, os, random, sys
from pathlib import Path
import torch
from safetensors.torch import load_file
import laya
from laya.common import encode_text

ROOT = Path(__file__).resolve().parents[4]
MODEL_DIR = Path(os.environ.get("GLEV_MODEL_DIR", ROOT / "target" / "glev-model"))
MODEL = Path(sys.argv[1])
ORDER = ["citation-reach", "document-text-search", "shortest-path",
         "team-readable-documents", "variable-length-reach", "none-of-these"]
SLOT_TYPES = ["anchor", "hops", "concept", "depth", "team", "topic"]
NUM_TAGS = 1 + 2 * len(SLOT_TYPES)
OP_SLOTS = {
    "citation-reach": ["anchor", "depth"],
    "document-text-search": ["topic"],
    "shortest-path": ["from", "to"],
    "team-readable-documents": ["team", "topic"],
    "variable-length-reach": ["anchor", "hops"],
}
WINDOW, STRIDE = 20, 10
SENTENCES = [
    "Thanks for asking.", "Here is some background.", "We discussed several topics earlier.",
    "Please focus on the question below.", "The team reviewed the material.",
    "Consider the following details.", "This is an internal note.", "Now for the actual request.",
]
agent = laya.load(str(MODEL_DIR), device="mps")
model, tok = agent.model, agent.tok
state = load_file(str(MODEL / "model.safetensors"))
model.load_state_dict({k: v for k, v in state.items() if not k.startswith("slot_head.")},
                      strict=False)
slot_head = torch.nn.Linear(model.encoder.config.hidden_size, NUM_TAGS).to("mps")
slot_head.load_state_dict({k.removeprefix("slot_head."): v for k, v in state.items()
                           if k.startswith("slot_head.")})
model.eval(); slot_head.eval()
mask_id, bos_id = tok.mask_token_id, tok.bos_token_id
none_index = ORDER.index("none-of-these")


def forward_window(tokens):
    ids = [bos_id] + tokens + [mask_id] * 6
    input_ids = torch.tensor([ids], device="mps")
    attn = torch.ones_like(input_ids)
    markers = torch.tensor([[len(ids) - 6 + i for i in range(6)]], device="mps")
    with torch.no_grad():
        h = model.encoder(input_ids=input_ids, attention_mask=attn).last_hidden_state
        h = h + model.type_emb(torch.zeros(1, dtype=torch.long, device="mps"))[:, None, :]
        gathered = torch.gather(h, 1, markers[:, :, None].expand(-1, -1, h.size(-1)))
        logits = model.scorer(gathered).squeeze(-1).float()
        probs = torch.softmax(logits, -1)[0]
        tag_probs = torch.softmax(slot_head(h)[0, 1:1 + len(tokens)].float(), -1).cpu()
    return probs, tag_probs


def analyse(tokens):
    probs = None
    none_sum, count = 0.0, 0
    tags, conf, best_edge = [0] * len(tokens), [0.0] * len(tokens), [-1] * len(tokens)
    second_tag, second_prob = [0] * len(tokens), [0.0] * len(tokens)
    for start in range(0, max(len(tokens), 1), STRIDE):
        chunk = tokens[start:start + WINDOW]
        window_probs, window_tags = forward_window(chunk)
        none_sum += float(window_probs[none_index]); count += 1
        probs = window_probs if probs is None else torch.maximum(probs, window_probs)
        for offset in range(len(chunk)):
            position = start + offset
            distance = min(offset, len(chunk) - 1 - offset) if len(chunk) > 1 else 0
            if distance > best_edge[position]:
                best_edge[position] = distance
                top = window_tags[offset].topk(2)
                tags[position] = int(top.indices[0])
                conf[position] = float(top.values[0])
                second_tag[position] = int(top.indices[1])
                second_prob[position] = float(top.values[1])
        if start + WINDOW >= len(tokens):
            break
    threshold2 = float(os.environ.get("SLOT_TOP2", "0.2"))
    for index in range(len(tokens)):
        if threshold2 > 0 and tags[index] == 0 and second_prob[index] >= threshold2:
            tags[index] = second_tag[index]
            conf[index] = second_prob[index]
    probs = probs.clone()
    probs[none_index] = none_sum / count
    probs = probs / probs.sum()
    return probs, tags, conf


def decide(probs, temperature, threshold):
    scaled = probs ** (1.0 / temperature)
    scaled = scaled / scaled.sum()
    if scaled[none_index] >= threshold:
        return None
    scaled[none_index] = -1.0
    return ORDER[int(scaled.argmax())]


def rule_bind(text, op):
    """Approximate slots::extract for the demo vocabulary (fallback measurement)."""
    lowered = " ".join(text.lower().split())

    def longest(names):
        hits = [name for name in names if name.lower() in lowered]
        return max(hits, key=len) if hits else None

    def concepts_in_order():
        found = []
        for name in sorted(CONCEPTS, key=len, reverse=True):
            position = lowered.find(name.lower())
            if position >= 0 and all(not (start <= position < start + len(value))
                                     for start, value in found):
                found.append((position, name))
        found.sort()
        return [name for _, name in found]

    def number():
        digits = [int(ch) for ch in text if ch.isdigit()]
        return digits[0] if digits else None

    if op == "citation-reach":
        return {"anchor": longest(DOCUMENTS), "depth": number()}
    if op == "variable-length-reach":
        return {"anchor": longest(CONCEPTS), "hops": number()}
    if op == "shortest-path":
        found = concepts_in_order()
        return {"from": found[0] if found else None,
                "to": found[1] if len(found) > 1 else None}
    if op == "document-text-search":
        return {"topic": longest(CONCEPTS + DOCUMENTS)}
    if op == "team-readable-documents":
        return {"topic": longest(CONCEPTS + DOCUMENTS),
                "team": longest(TEAMS) or "Platform"}
    return {}


def normalise_slot(slot, value):
    text = " ".join(str(value).split()).lower()
    if slot in ("depth", "hops"):
        digits = "".join(ch for ch in text if ch.isdigit())
        return digits or text
    return text


def spans_from(tags, offsets, text):
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
        if merged and merged[-1][0] == slot and merged[-1][2] >= start:
            merged[-1] = (slot, merged[-1][1], max(merged[-1][2], end))
        else:
            merged.append((slot, start, end))
    return [(slot, text[offsets[start][0]:offsets[end - 1][1]], start, end)
            for slot, start, end in merged]


BOUNDARY = int(os.environ.get("POLICY_BOUNDARY", "32"))
E2E_MODE = os.environ.get("E2E_MODE", "fallback")
total_stats = dict(positives=0, coverage=0, routed_ok=0, abstained=0, oos=0,
                   refused=0, exact=0, compared=0)
by_op = {}
entities = json.load(open(ROOT / "demo/knowledge/eval/routing/entities.json"))
CONCEPTS, DOCUMENTS = entities.get("concepts", []), entities.get("documents", [])
TEAMS = entities.get("teams", [])


def snap(text, names):
    needle = " ".join(text.split()).lower()
    exact = [name for name in names if " ".join(name.split()).lower() == needle]
    if exact:
        return exact[0]
    candidates = [name for name in names
                  if needle in " ".join(name.split()).lower()
                  or " ".join(name.split()).lower() in needle]
    return max(candidates, key=len) if candidates else text


def select_value(values, select):
    if not values:
        return None
    if select == "conf":
        return max(values, key=lambda item: item[1])[0]
    if select == "last":
        return values[-1][0]
    if select == "longest":
        return max(values, key=lambda item: len(str(item[0])))[0]
    return values[0][0]


random.seed(9)
rows = [json.loads(l) for l in open(ROOT / "demo/knowledge/eval/routing/slots/dev.jsonl")]
for bucket, low, high in (("33-48", 33, 48), ("49-64", 49, 64), (">64", 65, 999)):
    coverage = positives = abstained = routed_ok = 0
    oos = refused = 0
    exact = compared = 0
    for row in rows:
        filler = " ".join(random.sample(SENTENCES, random.randint(4, 8))) + " "
        placement = os.environ.get("E2E_POSITION", "end")
        if placement == "start":
            text = row["question"] + " " + filler
        elif placement == "split":
            text = filler[: len(filler) // 2] + row["question"] + " " + filler[len(filler) // 2:]
        else:
            text = filler + row["question"]
        tokens = [int(t) for t in encode_text(tok, text, add_special_tokens=False,
                                              truncation=True, max_length=256)["input_ids"]]
        if not (low <= len(tokens) <= high):
            continue
        offsets = [(int(a), int(b)) for a, b in encode_text(
            tok, text, add_special_tokens=False, truncation=True, max_length=256,
            return_offsets_mapping=True)["offset_mapping"]]
        probs, tags, conf = analyse(tokens)
        if len(tokens) <= BOUNDARY:
            decision = decide(probs, 2.5, 0.23)
        else:
            decision = decide(probs, 1.0, 0.25)
        if row.get("expected_op"):
            positives += 1
            compared += 1
            total_stats["positives"] += 1
            total_stats["compared"] += 1
            if decision is None:
                abstained += 1
                total_stats["abstained"] += 1
            else:
                coverage += 1
                routed_ok += int(decision == row["expected_op"])
                total_stats["coverage"] += 1
                total_stats["routed_ok"] += int(decision == row["expected_op"])
                predicted = {}
                for slot, value, start, end in spans_from(tags, offsets, text):
                    pool = {"anchor": CONCEPTS + DOCUMENTS, "concept": CONCEPTS,
                            "team": TEAMS, "topic": CONCEPTS + DOCUMENTS}.get(slot)
                    predicted.setdefault(slot, []).append((
                        " ".join((snap(value, pool) if pool else value).split()).lower(),
                        sum(conf[start:end]) / max(end - start, 1)))
                model_ok = decision == row["expected_op"]
                if decision in ("citation-reach", "variable-length-reach"):
                    for source, target in (("depth", "hops"), ("hops", "depth")):
                        if not predicted.get(target) and predicted.get(source):
                            predicted[target] = predicted[source]
                    documents = [name.lower() for name in DOCUMENTS]
                    if not predicted.get("anchor") and predicted.get("topic"):
                        values = [item for item in predicted["topic"]
                                  if item[0] in documents]
                        if values:
                            predicted["anchor"] = values
                if decision == "shortest-path":
                    concepts = predicted.pop("concept", [])
                    if concepts:
                        predicted["from"] = [concepts[0]]
                    if len(concepts) > 1:
                        predicted["to"] = [concepts[1]]
                for slot in OP_SLOTS[decision]:
                    expected = row["slots"].get(slot)
                    if expected is None:
                        continue
                    values = predicted.get(slot, [])
                    expected_value = normalise_slot(slot, expected)
                    select = os.environ.get("E2E_SELECT", "first")
                    if select == "any":
                        model_ok = model_ok and any(
                            normalise_slot(slot, value) == expected_value for value, _ in values)
                    else:
                        chosen = select_value(values, select)
                        model_ok = model_ok and chosen is not None and normalise_slot(
                            slot, chosen) == expected_value
                rule = rule_bind(text, decision)
                rule_ok = decision == row["expected_op"] and all(
                    rule.get(slot) is not None
                    and " ".join(str(rule[slot]).split()).lower()
                    == normalise_slot(slot, expected)
                    for slot, expected in row["slots"].items()
                    if slot in OP_SLOTS[decision]
                )
                ok = {"model": model_ok, "rule": rule_ok,
                      "fallback": model_ok or rule_ok}[E2E_MODE]
                if os.environ.get("E2E_VERBOSE") == "1" and not ok and \
                        by_op.get(row["expected_op"], [0, 0])[1] < 4:
                    print(f"FAIL {row['expected_op']}: gold={row['slots']} pred={predicted} "
                          f"model={model_ok} rule={rule_ok}")
                entry = by_op.setdefault(row["expected_op"], [0, 0])
                entry[0] += int(ok)
                entry[1] += 1
                exact += int(ok)
                total_stats["exact"] += int(ok)
        else:
            oos += 1
            refused += int(decision is None)
            total_stats["oos"] += 1
            total_stats["refused"] += int(decision is None)
    if os.environ.get("E2E_VERBOSE") == "1":
        print("   by op:", {op: f"{hit}/{total}" for op, (hit, total) in by_op.items()})
    print(f"{bucket:6s}: positives {positives} coverage {coverage/max(compared,1):.3f} "
          f"routed-acc {routed_ok/max(coverage,1):.3f} abstain {abstained/max(compared,1):.3f} "
          f"e2e-exact {exact/max(coverage,1):.3f} | OOS refusal {refused}/{oos}")
print(f"boundary {BOUNDARY}: positives {total_stats['positives']} "
      f"coverage {total_stats['coverage']/max(total_stats['compared'],1):.3f} "
      f"routed-acc {total_stats['routed_ok']/max(total_stats['coverage'],1):.3f} "
      f"e2e {total_stats['exact']/max(total_stats['coverage'],1):.3f} "
      f"abstain {total_stats['abstained']/max(total_stats['compared'],1):.3f} "
      f"OOS {total_stats['refused']}/{total_stats['oos']}")
