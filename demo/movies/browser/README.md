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

# Similar-films demo

`similar-films.html` is the model-free browser client for the two title-bound similarity
operations:

```text
film picker (title_ja / title_en)
  → films-similar-to-ja or films-similar-to-en
  → Router reads the stored anchor embedding
  → Vector canister runs the search
```

It does not import an embedding model and does not download model assets. The Router and Vector
canister own the anchor embedding; the browser sends only a title.

## Serve it

The page imports the local JS SDK and its browser ESM dependencies, so serve the repository root:

```bash
cd /path/to/gleaph
python3 -m http.server 8766
# open http://127.0.0.1:8766/demo/movies/browser/similar-films.html
```

`scripts/quickstart.sh` writes `browser/config.json` with the local Router canister id and gateway
host after it brings the network up. The file is ignored by git. Without it, you can pass the
values in the URL:

```text
similar-films.html?router=<router-canister-id>&host=http://127.0.0.1:<gateway-port>
```

The page reads `../seeds/catalog.json` for the picker. It is a read-only client: the only calls are
`films-similar-to-ja` and `films-similar-to-en`.

For a shareable deep link, pass `title` and optionally `lang`:

```text
similar-films.html?title=千と千尋の神隠し&lang=ja
similar-films.html?title=Spirited%20Away&lang=en
```
