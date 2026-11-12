"""Compare op aggregation policies that use per-window slot activity on realistic long questions."""
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
WINDOW, STRIDE = 20, 10
none_index = ORDER.index("none-of-these")
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
slot_head = torch.nn.Linear(model.encoder.config.hidden_size, 13).to("mps")
slot_head.load_state_dict({k.removeprefix("slot_head."): v for k, v in state.items()
                           if k.startswith("slot_head.")})
model.eval(); slot_head.eval()
mask_id, bos_id = tok.mask_token_id, tok.bos_token_id


def window(tokens):
    ids = [bos_id] + tokens + [mask_id] * 6
    input_ids = torch.tensor([ids], device="mps")
    attn = torch.ones_like(input_ids)
    markers = torch.tensor([[len(ids) - 6 + i for i in range(6)]], device="mps")
    with torch.no_grad():
        h = model.encoder(input_ids=input_ids, attention_mask=attn).last_hidden_state
        h = h + model.type_emb(torch.zeros(1, dtype=torch.long, device="mps"))[:, None, :]
        gathered = torch.gather(h, 1, markers[:, :, None].expand(-1, -1, h.size(-1)))
        probs = torch.softmax(model.scorer(gathered).squeeze(-1).float(), -1)[0]
        tags = slot_head(h)[0, 1:1 + len(tokens)].argmax(-1)
    return probs, int((tags > 0).sum())


def windows(tokens):
    out = []
    for start in range(0, max(len(tokens), 1), STRIDE):
        chunk = tokens[start:start + WINDOW]
        out.append(window(chunk))
        if start + WINDOW >= len(tokens):
            break
    return out


def aggregate(parts, policy):
    probs = torch.stack([part[0] for part in parts])
    activity = torch.tensor([part[1] for part in parts], dtype=torch.float32,
                            device=probs.device)
    if policy == "hybrid":
        merged = probs.max(0).values
        merged[none_index] = probs[:, none_index].mean()
        return merged / merged.sum()
    if policy == "activity":
        weights = 1.0 + activity
        merged = (probs * weights[:, None]).sum(0) / weights.sum()
        return merged / merged.sum()
    if policy == "active_only":
        keep = activity > 0
        if not bool(keep.any()):
            keep = torch.ones_like(keep)
        merged = probs[keep].max(0).values
        merged[none_index] = probs[keep][:, none_index].mean()
        return merged / merged.sum()
    if policy == "best_non_none":
        scores = probs.clone()
        scores[:, none_index] = -1
        best = int(scores.max(1).values.argmax())
        return probs[best] / probs[best].sum()
    raise ValueError(policy)


def decide(probs, tokens):
    if len(tokens) <= 32:
        temperature, threshold = 2.5, 0.23
    else:
        temperature, threshold = 1.0, 0.25
    scaled = probs ** (1.0 / temperature)
    scaled = scaled / scaled.sum()
    if scaled[none_index] >= threshold:
        return None
    scaled[none_index] = -1
    return ORDER[int(scaled.argmax())]


random.seed(9)
rows = [json.loads(l) for l in open(ROOT / "demo/knowledge/eval/routing/slots/dev.jsonl")]
for policy in ("hybrid", "activity", "active_only", "best_non_none"):
    for bucket, low, high in (("33-64", 33, 64), (">64", 65, 999)):
        random.seed(9)
        coverage = compared = routed_ok = oos = refused = 0
        for row in rows:
            filler = " ".join(random.sample(SENTENCES, random.randint(4, 8))) + " "
            text = filler + row["question"]
            tokens = [int(t) for t in encode_text(tok, text, add_special_tokens=False,
                                                  truncation=True, max_length=256)["input_ids"]]
            if not (low <= len(tokens) <= high):
                continue
            decision = decide(aggregate(windows(tokens), policy), tokens)
            if row.get("expected_op"):
                compared += 1
                if decision is not None:
                    coverage += 1
                    routed_ok += int(decision == row["expected_op"])
            else:
                oos += 1
                refused += int(decision is None)
        print(f"{policy:14s} {bucket:5s}: coverage {coverage/max(compared,1):.3f} "
              f"routed-acc {routed_ok/max(coverage,1):.3f} OOS {refused}/{oos}")
