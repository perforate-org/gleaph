"""Compare chunked-window routing against truncation on synthetic long questions."""
import json, os, random, sys
from pathlib import Path
import torch
from safetensors.torch import load_file
import laya
from laya.common import encode_text

ROOT = Path(__file__).resolve().parents[4]
MODEL = Path(sys.argv[1])
CAP = 128
WINDOW = int(os.environ.get("CHUNK_WINDOW", "32"))
ORDER = ["citation-reach", "document-text-search", "shortest-path",
         "team-readable-documents", "variable-length-reach", "none-of-these"]
agent = laya.load(str(ROOT / "target/laya-routing/model20-qonly-h640-xl-ft"), device="mps")
model, tok = agent.model, agent.tok
state = load_file(str(MODEL / "model.safetensors"))
missing = model.load_state_dict({k: v for k, v in state.items()
                                 if not k.startswith("slot_head.")}, strict=False)
assert not missing.unexpected_keys
model.eval()
mask_id, bos_id = tok.mask_token_id, tok.bos_token_id


def window_prob_list(tokens):
    probs_list = []
    for start in range(0, len(tokens), WINDOW):
        window = tokens[start:start + WINDOW]
        ids = [bos_id] + window + [mask_id] * 6
        input_ids = torch.tensor([ids], device="mps")
        attn = torch.ones_like(input_ids)
        markers = torch.tensor([[len(ids) - 6 + i for i in range(6)]], device="mps")
        with torch.no_grad():
            h = model.encoder(input_ids=input_ids, attention_mask=attn).last_hidden_state
            h = h + model.type_emb(torch.zeros(1, dtype=torch.long, device="mps"))[:, None, :]
            m = torch.gather(h, 1, markers[:, :, None].expand(-1, -1, h.size(-1)))
            logits = model.scorer(m).squeeze(-1).float()
            probs = torch.softmax(logits, -1)
        temperature = float(os.environ.get("CHUNK_TEMP", "1"))
        if temperature != 1.0:
            probs = probs ** (1.0 / temperature)
            probs = probs / probs.sum(-1, keepdim=True)
        probs_list.append(probs)
    return probs_list


def aggregate(probs_list, mode):
    if mode == "mean":
        return torch.stack(probs_list).mean(0)
    if mode == "max":
        return torch.stack(probs_list).max(0).values
    if mode == "lse":
        stacked = torch.stack(probs_list).squeeze(1)
        return torch.logsumexp(stacked * 4.0, dim=0).unsqueeze(0)
    if mode == "hybrid":
        stacked = torch.stack(probs_list).squeeze(1)
        merged = stacked.max(0).values
        none = 5
        merged[none] = stacked[:, none].mean()
        return (merged / merged.sum()).unsqueeze(0)
    if mode == "weighted":
        stacked = torch.stack(probs_list).squeeze(1)
        weights = stacked.max(-1).values
        return (stacked * weights[:, None]).sum(0, keepdim=True) / weights.sum()
    if mode == "top2":
        stacked = torch.stack(probs_list).squeeze(1)
        if stacked.size(0) == 1:
            return stacked[0].unsqueeze(0)
        return stacked.topk(2, dim=0).values.mean(0).unsqueeze(0)
    raise ValueError(mode)


def tokenize(text):
    ids = encode_text(tok, text, add_special_tokens=False, truncation=True,
                      max_length=CAP)["input_ids"]
    return [int(t) for t in ids]


import os
random.seed(int(os.environ.get("CHUNK_SEED", "11")))
for split in ("dev", "test"):
    rows = [json.loads(l) for l in open(ROOT / f"demo/knowledge/eval/routing/slots/{split}.jsonl")]
    by_op = {}
    for row in rows:
        if row.get("expected_op"):
            by_op.setdefault(row["expected_op"], []).append(row)
    short_correct = truncate_correct = total = 0
    correct = {"mean": 0, "max": 0, "hybrid": 0, "lse": 0, "weighted": 0, "top2": 0}
    for op, pool in by_op.items():
        for _ in range(40):
            count = random.randint(2, 4)
            question = " ".join(random.choice(pool)["question"] for _ in range(count))
            tokens = tokenize(question)
            gold = ORDER.index(op)
            probs_list = window_prob_list(tokens)
            for mode in correct:
                correct[mode] += int(int(aggregate(probs_list, mode).argmax()) == gold)
            truncate_correct += int(int(window_prob_list(tokens[:WINDOW])[0].argmax()) == gold)
            short = random.choice(pool)["question"]
            short_correct += int(int(window_prob_list(tokenize(short))[0].argmax()) == ORDER.index(
                random.choice(pool).get("expected_op") or op))
            total += 1
    print(f"{split}: short {short_correct/total:.3f}  truncate32 {truncate_correct/total:.3f}  "
          + "  ".join(f"{mode} {value/total:.3f}" for mode, value in correct.items())
          + f"  (n={total})")
