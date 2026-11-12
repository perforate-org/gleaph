"""Long-question benchmark using real repository prose as context (vs synthetic filler)."""
import hashlib, json, os, random, re, sys
from pathlib import Path
import torch
from safetensors.torch import load_file
import laya
from laya.common import encode_text

ROOT = Path(__file__).resolve().parents[4]
MODEL_DIR = Path(os.environ.get("GLEV_MODEL_DIR", ROOT / "target" / "glev-model"))
DATA_DIR = Path(os.environ.get("GLEV_DATA_DIR", ROOT / "target" / "glev-data"))
MODEL = Path(sys.argv[1])
ORDER = ["citation-reach", "document-text-search", "shortest-path",
         "team-readable-documents", "variable-length-reach", "none-of-these"]
SLOT_TYPES = ["anchor", "hops", "concept", "depth", "team", "topic"]
OP_SLOTS = {
    "citation-reach": ["anchor", "depth"],
    "document-text-search": ["topic"],
    "shortest-path": ["from", "to"],
    "team-readable-documents": ["team", "topic"],
    "variable-length-reach": ["anchor", "hops"],
}
WINDOW, STRIDE = 20, 10
FILLER_MODE = os.environ.get("BENCH_FILLER", "doc")
CORPUS = os.environ.get("BENCH_CORPUS", "repo")
SOURCES = {value for value in os.environ.get("BENCH_SOURCES", "").split(",") if value}
SPLIT = os.environ.get("BENCH_SPLIT", "all")
POSITIONS = os.environ.get("BENCH_POSITIONS", "end,start,split").split(",")
NONE_AGG = os.environ.get("BENCH_NONE_AGG", "mean")
CJK_POLICY = os.environ.get("BENCH_CJK_POLICY") == "1"
VERBOSE = os.environ.get("BENCH_VERBOSE") == "1"
BUCKETS = [int(value) for value in os.environ.get("BENCH_BUCKETS", "64,96,128").split(",")]

agent = laya.load(str(MODEL_DIR), device="mps")
model, tok = agent.model, agent.tok
state = load_file(str(MODEL / "model.safetensors"))
model.load_state_dict({k: v for k, v in state.items() if not k.startswith("slot_head.")},
                      strict=False)
slot_head = torch.nn.Linear(model.encoder.config.hidden_size, 13).to("mps")
slot_head.load_state_dict({k.removeprefix("slot_head."): v for k, v in state.items()
                           if k.startswith("slot_head.")})
model.eval(); slot_head.eval()
mask_id, bos_id = tok.mask_token_id, tok.bos_token_id
none_index = ORDER.index("none-of-these")

entities = json.load(open(ROOT / "demo/knowledge/eval/routing/entities.json"))
CONCEPTS, DOCUMENTS = entities.get("concepts", []), entities.get("documents", [])
TEAMS = entities.get("teams", [])
SENTENCES = [
    "Thanks for asking.", "Here is some background.", "We discussed several topics earlier.",
    "Please focus on the question below.", "The team reviewed the material.",
    "Consider the following details.", "This is an internal note.", "Now for the actual request.",
]


def numbered_tokens(text):
    return [int(t) for t in encode_text(tok, text, add_special_tokens=False,
                                        truncation=True, max_length=256)["input_ids"]]


def load_external():
    pool = []
    path = ROOT / "target" / "laya-routing" / "corpus" / "external.jsonl"
    if not path.exists():
        return pool
    with open(path) as handle:
        for line in handle:
            row = json.loads(line)
            if SOURCES and row["source"] not in SOURCES:
                continue
            if SPLIT != "all":
                bucket = int(hashlib.md5(row["doc_id"].encode()).hexdigest(), 16) % 2
                if (SPLIT == "dev" and bucket != 0) or (SPLIT == "holdout" and bucket != 1):
                    continue
            pool.append(row["text"])
    return pool


def load_paragraphs():
    pool = load_external() if CORPUS in ("external", "mixed") else []
    if CORPUS == "external":
        return pool
    if CORPUS == "repo" and SPLIT == "holdout":
        return []
    paths = [ROOT / "README.md", ROOT / "AGENTS.md", ROOT / "design" / "README.md",
             ROOT / "design" / "glossary.md"]
    paths += sorted((ROOT / "design").glob("architecture/*.md"))
    paths += sorted((ROOT / "design").glob("storage/*.md"))
    for path in paths:
        if not path.exists():
            continue
        text = re.sub(r"```.*?```", "", path.read_text(errors="ignore"), flags=re.S)
        for block in text.split("\n\n"):
            block = " ".join(line.strip() for line in block.splitlines()
                             if line.strip() and not line.lstrip().startswith(("#", "|", "-", "*", ">", "!")))
            if len(block.split()) >= 20 and sum(ch.isascii() for ch in block) / max(len(block), 1) > 0.95:
                pool.append(block.strip())
    return pool


def snap(text, names):
    needle = " ".join(text.split()).lower()
    exact = [name for name in names if " ".join(name.split()).lower() == needle]
    if exact:
        return exact[0]
    candidates = [name for name in names
                  if needle in " ".join(name.split()).lower()
                  or " ".join(name.split()).lower() in needle]
    return max(candidates, key=len) if candidates else text


def rule_bind(text, op):
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

    digits = [int(ch) for ch in text if ch.isdigit()]
    number = digits[0] if digits else None
    if op == "citation-reach":
        return {"anchor": longest(DOCUMENTS), "depth": number}
    if op == "variable-length-reach":
        return {"anchor": longest(CONCEPTS), "hops": number}
    if op == "shortest-path":
        found = concepts_in_order()
        return {"from": found[0] if found else None,
                "to": found[1] if len(found) > 1 else None}
    if op == "document-text-search":
        return {"topic": longest(CONCEPTS + DOCUMENTS)}
    if op == "team-readable-documents":
        return {"topic": longest(CONCEPTS + DOCUMENTS), "team": longest(TEAMS) or "Platform"}
    return {}


def forward_window(tokens):
    ids = [bos_id] + tokens + [mask_id] * 6
    input_ids = torch.tensor([ids], device="mps")
    attn = torch.ones_like(input_ids)
    markers = torch.tensor([[len(ids) - 6 + i for i in range(6)]], device="mps")
    with torch.no_grad():
        h = model.encoder(input_ids=input_ids, attention_mask=attn).last_hidden_state
        h = h + model.type_emb(torch.zeros(1, dtype=torch.long, device="mps"))[:, None, :]
        gathered = torch.gather(h, 1, markers[:, :, None].expand(-1, -1, h.size(-1)))
        probs = torch.softmax(model.scorer(gathered).squeeze(-1).float(), -1)[0]
        tag_probs = torch.softmax(slot_head(h)[0, 1:1 + len(tokens)].float(), -1).cpu()
    return probs, tag_probs


def analyse(tokens):
    probs = None
    none_sum, count = 0.0, 0
    nones = []
    tags, conf, best_edge = [0] * len(tokens), [0.0] * len(tokens), [-1] * len(tokens)
    second_tag, second_prob = [0] * len(tokens), [0.0] * len(tokens)
    for start in range(0, max(len(tokens), 1), STRIDE):
        chunk = tokens[start:start + WINDOW]
        window_probs, window_tags = forward_window(chunk)
        none_sum += float(window_probs[none_index]); count += 1
        nones.append(float(window_probs[none_index]))
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
    for index in range(len(tokens)):
        if tags[index] == 0 and second_prob[index] >= 0.2:
            tags[index] = second_tag[index]
            conf[index] = second_prob[index]
    probs = probs.clone()
    if NONE_AGG == "min":
        none = min(nones)
    elif NONE_AGG == "max":
        none = max(nones)
    elif NONE_AGG == "trim":
        ordered = sorted(nones)
        drop = max(len(ordered) // 4, 1)
        kept = ordered[: max(len(ordered) - drop, 1)]
        none = sum(kept) / len(kept)
    elif NONE_AGG == "median":
        ordered = sorted(nones)
        none = ordered[len(ordered) // 2]
    else:
        none = none_sum / count
    probs[none_index] = none
    probs = probs / probs.sum()
    return probs, tags, conf


def cjk_fraction(text):
    total = cjk = 0
    for character in text:
        if character.isspace():
            continue
        total += 1
        code = ord(character)
        if (0x3040 <= code <= 0x30FF or 0x3400 <= code <= 0x4DBF
                or 0x4E00 <= code <= 0x9FFF or 0xF900 <= code <= 0xFAFF
                or 0xFF66 <= code <= 0xFF9F):
            cjk += 1
    return cjk / total if total else 0.0


def decide(probs, token_count, text):
    threshold = float(os.environ.get("BENCH_THRESHOLD", "0.25"))
    cutoff = float(os.environ.get("BENCH_CJK_CUTOFF", "0.10"))
    if token_count > 32 and CJK_POLICY and cjk_fraction(text) >= cutoff:
        threshold = float(os.environ.get("BENCH_CJK_THRESHOLD", "0.40"))
    temperature = 2.5 if token_count <= 32 else 1.0
    scaled = probs ** (1.0 / temperature)
    scaled = scaled / scaled.sum()
    if scaled[none_index] >= threshold:
        return None
    scaled[none_index] = -1
    return ORDER[int(scaled.argmax())]


def spans_from(tags, offsets, text, conf):
    raw, index = [], 0
    while index < len(tags):
        tag = tags[index]
        if tag > 0:
            raw.append((SLOT_TYPES[(tag - 1) // 2], index, index + 1))
        index += 1
    merged = []
    for slot, start, end in raw:
        if merged and merged[-1][0] == slot and merged[-1][2] >= start:
            merged[-1] = (slot, merged[-1][1], max(merged[-1][2], end))
        else:
            merged.append((slot, start, end))
    return [(slot, text[offsets[start][0]:offsets[end - 1][1]], start, end,
             sum(conf[start:end]) / (end - start)) for slot, start, end in merged]


def select(values):
    return max(values, key=lambda item: item[1])[0] if values else None


def normalise(slot, value):
    text = " ".join(str(value).split()).lower()
    if slot in ("depth", "hops"):
        digits = "".join(ch for ch in text if ch.isdigit())
        return digits or text
    return text


def context_for(target, question_tokens, paragraphs):
    budget = max(target - len(question_tokens), 20)
    pieces, tokens = [], []
    random.shuffle(paragraphs)
    for paragraph in paragraphs:
        candidate = numbered_tokens(paragraph)
        if len(tokens) + len(candidate) > budget:
            continue
        pieces.append(paragraph); tokens.extend(candidate)
        if len(tokens) >= budget - 10:
            break
    return " ".join(pieces)


random.seed(7)
paragraphs = load_paragraphs()
print(f"corpus paragraphs {len(paragraphs)} filler {FILLER_MODE}", flush=True)
rows = [json.loads(l) for l in open(ROOT / "demo/knowledge/eval/routing/slots/dev.jsonl")]
if os.environ.get("BENCH_OOS") == "real":
    oos = [json.loads(l) for l in open(DATA_DIR / "corpus" / "oos_questions.jsonl")]
    rows = [{"question": row["question"]} for row in oos]
for position in POSITIONS:
    for target in BUCKETS:
        stats = dict(pos=0, compared=0, covered=0, routed_ok=0, exact=0, oos=0, refused=0)
        per_op = {}
        for row in rows:
            question = row["question"]
            question_tokens = numbered_tokens(question)
            if FILLER_MODE == "doc":
                context = context_for(target, question_tokens, paragraphs)
            else:
                sentences = []
                while len(numbered_tokens(" ".join(sentences))) < max(target - len(question_tokens), 20):
                    sentences.append(random.choice(SENTENCES))
                context = " ".join(sentences)
            if position == "end":
                text = context + " " + question
            elif position == "start":
                text = question + " " + context
            else:
                half = len(context) // 2
                text = context[:half] + " " + question + " " + context[half:]
            encoded = encode_text(tok, text, add_special_tokens=False, truncation=True,
                                  max_length=256, return_offsets_mapping=True)
            tokens = [int(t) for t in encoded["input_ids"]]
            offsets = [(int(a), int(b)) for a, b in encoded["offset_mapping"]]
            probs, tags, conf = analyse(tokens)
            decision = decide(probs, len(tokens), text)
            predicted = {}
            for slot, value, start, end, score in spans_from(tags, offsets, text, conf):
                pool = {"anchor": CONCEPTS + DOCUMENTS, "concept": CONCEPTS,
                        "team": TEAMS, "topic": CONCEPTS + DOCUMENTS}.get(slot)
                matched = snap(value, pool) if pool else value
                predicted.setdefault(slot, []).append((matched, score))
            if row.get("expected_op"):
                stats["pos"] += 1; stats["compared"] += 1
                per_op.setdefault(row["expected_op"], [0, 0])
                per_op[row["expected_op"]][1] += 1
                if decision is not None:
                    stats["covered"] += 1
                    per_op[row["expected_op"]][0] += 1
                    stats["routed_ok"] += int(decision == row["expected_op"])
                    if decision in ("citation-reach", "variable-length-reach"):
                        for source, dest in (("depth", "hops"), ("hops", "depth")):
                            if not predicted.get(dest) and predicted.get(source):
                                predicted[dest] = predicted[source]
                    if decision == "shortest-path":
                        concepts = [value for value, _ in predicted.pop("concept", [])]
                        if concepts:
                            predicted["from"] = [(concepts[0], 1.0)]
                        if len(concepts) > 1:
                            predicted["to"] = [(concepts[1], 1.0)]
                    ok = decision == row["expected_op"]
                    for slot in OP_SLOTS[decision]:
                        expected = row["slots"].get(slot)
                        if expected is None:
                            continue
                        chosen = select(predicted.get(slot, []))
                        ok = ok and chosen is not None and normalise(slot, chosen) == normalise(slot, expected)
                    if not ok and os.environ.get("BENCH_FALLBACK", "1") == "1":
                        rule = rule_bind(text, decision)
                        ok = all(
                            rule.get(slot) is not None
                            and normalise(slot, rule[slot]) == normalise(slot, expected)
                            for slot, expected in row["slots"].items()
                            if slot in OP_SLOTS[decision]
                        )
                    stats["exact"] += int(ok)
            else:
                stats["oos"] += 1
                refused = decision is None
                if not refused:
                    rule = rule_bind(text, decision)
                    def has(slot):
                        return bool(predicted.get(slot)) or bool(rule.get(slot))
                    if decision == "citation-reach":
                        bound = has("anchor") and (has("depth") or has("hops"))
                    elif decision == "variable-length-reach":
                        bound = has("anchor") and (has("hops") or has("depth"))
                    elif decision == "shortest-path":
                        bound = len(predicted.get("concept", [])) >= 2 or (
                            bool(rule.get("from")) and bool(rule.get("to")))
                    elif decision == "document-text-search":
                        bound = has("topic")
                    else:
                        bound = has("topic") and (has("team") or True)
                    refused = not bound
                stats["refused"] += int(refused)
                if VERBOSE:
                    per_op.setdefault(decision or "ABSTAIN", [0, 0])
                    per_op[decision or "ABSTAIN"][0] += 1
        compared = max(stats["compared"], 1); covered = max(stats["covered"], 1)
        if VERBOSE:
            print("   coverage by op:",
                  {op: f"{hit}/{total}" for op, (hit, total) in sorted(per_op.items())}, flush=True)
        print(f"pos={position:5s} ctx={target:3d}: positives {stats['pos']} "
              f"coverage {stats['covered']/compared:.3f} routed-acc {stats['routed_ok']/covered:.3f} "
              f"e2e {stats['exact']/covered:.3f} OOS {stats['refused']}/{stats['oos']}", flush=True)
