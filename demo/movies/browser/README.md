# Browser embedding probe

`bekko-probe.html` checks that the Bekko ONNX encoder runs in a browser through Transformers.js.

It is a probe, not the final browser client:

- The model is loaded from the Hugging Face hub on first use (~130 MB ONNX + tokenizer).
- The page measures cold load and warm embedding latency in the browser.
- It does not call Router or the vector index.
- The production client must keep embedding optional and must not download a model on SDK import;
  this page exists to validate the deployment path.

Open it from any static file server, for example:

```bash
cd demo/movies
python3 -m http.server 8766
# open http://127.0.0.1:8766/browser/bekko-probe.html
```

Last probe (headless Chromium, WASM path):

| Metric | Result |
| --- | ---: |
| cold load including download | 15,974 ms |
| warm embedding | 36.3 ms |
| dimensions | 384 |
| first values | matches the ONNX/PyTorch vector |

Local-asset packaging and WebGPU measurement are still open.
