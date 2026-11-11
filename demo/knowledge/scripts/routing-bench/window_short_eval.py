"""Short-question op accuracy when the runtime splits into windows of size W (stride W/2)."""
import json, sys
from pathlib import Path
import torch
from safetensors.torch import load_file
import laya

ROOT = Path(__file__).resolve().parents[4]
MODEL = Path(sys.argv[1])
ORDER = ["citation-reach", "document-text-search", "shortest-path",
         "team-readable-documents", "variable-length-reach", "none-of-these"]
agent = laya.load(str(ROOT / "target/laya-routing/model20-qonly-h640-xl-ft"), device="mps")
model, tok = agent.model, agent.tok
state = load_file(str(MODEL / "model.safetensors"))
model.load_state_dict({k: v for k, v in state.items() if not k.startswith("slot_head.")},
                      strict=False)
model.eval()
mask_id, bos_id = tok.mask_token_id, tok.bos_token_id
import os
TOKENIZED = os.environ.get("TOKENIZED", "target/canister-infer/tokenized-eval-qonly.jsonl")
LONG_WINDOW = int(os.environ.get("LONG_WINDOW", "0"))
MIN_LEN = int(os.environ.get("MIN_LEN", "0"))
MAX_LEN = int(os.environ.get("MAX_LEN", "999"))
rows = [json.loads(l) for l in open(ROOT / TOKENIZED)]


def probabilities(tokens):
    ids = [bos_id] + tokens + [mask_id] * 6
    input_ids = torch.tensor([ids], device="mps")
    attn = torch.ones_like(input_ids)
    markers = torch.tensor([[len(ids) - 6 + i for i in range(6)]], device="mps")
    with torch.no_grad():
        h = model.encoder(input_ids=input_ids, attention_mask=attn).last_hidden_state
        h = h + model.type_emb(torch.zeros(1, dtype=torch.long, device="mps"))[:, None, :]
        gathered = torch.gather(h, 1, markers[:, :, None].expand(-1, -1, h.size(-1)))
        return torch.softmax(model.scorer(gathered).squeeze(-1).float(), -1)[0]


def predict(tokens, window):
    if LONG_WINDOW and len(tokens) > 32:
        window = LONG_WINDOW
    if len(tokens) <= window:
        return int(probabilities(tokens).argmax())
    stride = max(1, window // 2)
    best = None
    for start in range(0, max(len(tokens), 1), stride):
        chunk = tokens[start:start + window]
        probs = probabilities(chunk)
        best = probs if best is None else torch.maximum(best, probs)
    return int(best.argmax())


for window in (24, 32):
    for name in ("dev", "wild-handwritten-2026-11-09", "blind-2026-11-09"):
        items = [row for row in rows if row["set"] == name]
        correct = total = 0
        for row in items:
            tokens = row["ids"][1:-6]
            if not (MIN_LEN <= len(tokens) <= MAX_LEN):
                continue
            gold = row["gold"]
            if gold not in ORDER:
                continue
            correct += int(predict(tokens, window) == ORDER.index(gold))
            total += 1
        print(f"window {window} {name}: {correct/max(total,1):.3f} (n={total})")
