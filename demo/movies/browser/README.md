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

The page imports a self-contained browser bundle, so serve `demo/movies` (not the repository root):

```bash
cd demo/movies
python3 -m http.server 8766
# open http://127.0.0.1:8766/browser/similar-films.html
```

Rebuild the bundle after changing the JS SDK:

```bash
node scripts/build-browser-sdk.mjs
```

`scripts/quickstart.sh` rebuilds it and writes `browser/config.json` with the local Router canister
id and gateway host after it brings the network up. The config is ignored by git. Without it, pass
the values in the URL:

```text
similar-films.html?router=<router-canister-id>&host=http://127.0.0.1:<gateway-port>
```

The page reads `../seeds/catalog.json` for the picker. The picker calls only
`films-similar-to-ja` and `films-similar-to-en`; the Ask form calls the Router's public
`ask(graph_name, text)` and executes whichever read-only prepared operation the Router selects.

## UX details

- The title picker is a searchable combobox over `title_ja`, `title_en` and year; it supports
  mouse selection and Arrow/Enter/Escape keyboard navigation.
- The selected anchor is shown in its own card and is filtered out of the result list.
- `lang`, `limit`, and the selected Japanese title are kept in the URL; `Copy link` shares that state.
- The result list uses stable row numbers, year metadata, and a model-free loading/empty/error state.
- The page is responsive at 390 px and preserves keyboard focus styling.


For a shareable deep link, pass `title` and optionally `lang`:

```text
similar-films.html?title=千と千尋の神隠し&lang=ja
similar-films.html?title=Spirited%20Away&lang=en
```

## Ask in words

The Ask form calls `client.ask("movies", question)` through the bundled JS SDK and renders the
returned rows generically. It supports `Answered`, `NeedsInput`, `Unsupported` and `Failed` states.

```text
千と千尋の神隠しに似ている映画は？
1990年代のSF映画を教えて
90年代のジャンプ作品の映画を見せて
今日の天気は？
```

Deep link:

```text
similar-films.html?ask=1990年代のSF映画を教えて
```

Ask may select `films-by-embedding` for a semantic-topic question; the page has no embedding provider,
so that returns a normal failed answer. Similar and structured questions are model-free.

`quickstart.sh` calls `scripts/check-browser-ask.mjs` after loading the graph, which exercises the same
JS SDK `ask` path anonymously.

## Topic search

The Topic search card is an **optional** semantic mode. The page does not load a model on startup or
for similar/ask. The user must press Load model once; the provider dynamically imports
Transformers.js from the pinned CDN and downloads the Bekko ONNX graph (~130 MB), then the browser
encodes the query and calls `films-by-embedding`.

```text
Load model
  → Transformers.js + pinned Bekko ONNX
  → encode query (384d, mean pooling, normalized)
  → prepared_query("films-by-embedding", { embedding })
```

The model is cached by the browser after the first load. The model-free paths remain fully usable
when the optional model cannot be downloaded.

## Live verification

On 2026-11-14 the optional topic path was exercised end to end in headless Chromium against the
local network:

```text
model: Bekko ONNX · 384d · loaded
status: Answered with 8 topic results
rows: タイムコップ / タイム・マシン 80万年後の世界へ / めぐりあう時間たち / ...
```

The deployment failure seen during an earlier verification was a stale globally installed
`gleaph` binary using the old single-request `install_code` path; rebuilding
`target/debug/gleaph` and using it from quickstart fixed the HTTP 413. No split upload was needed.

## Same magazine

Selecting a film enables **Same magazine as this film**, which runs `films-by-same-magazine`. The
prepared query walks the provenance graph instead of filtering a flat field:

```text
Film ──BASED_ON──▶ Work ──SERIALIZED_IN──▶ Magazine
      ◀──BASED_ON── Work ◀──SERIALIZED_IN──
```

For a Jump film it returns the other catalog films from the same magazine (for example Dragon
Ball and Fist of the North Star); films without provenance return an empty state.
