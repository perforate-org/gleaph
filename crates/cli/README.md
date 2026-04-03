# gleaph-cli

**gleaph-codegen** generates **TypeScript**, **JavaScript** (`.js` + `.d.ts`), and **Rust** helpers for calling Gleaph **prepared queries**. Point it at your **live canister** (recommended) or a **JSON snapshot** of the same metadata your service lists for clients.

## Generate from a canister (recommended)

The tool performs an **anonymous** canister **query** (no wallet). Your canister must allow that call for the method you name.

- Default method: **`list_prepared`**, invoked with **no arguments**.
- The reply should be either a plain list of prepared-query records, or `Result` with a text error—matching what your Gleaph canister exposes for clients.

**Mainnet** (TypeScript to a file):

```bash
gleaph-codegen codegen \
  --canister YOUR_CANISTER_PRINCIPAL \
  --output src/generated/gleaph.prepared.ts \
  --source-label production-canister
```

**Local replica** (e.g. `http://127.0.0.1:4943`): add `--fetch-root-key`.

```bash
gleaph-codegen codegen \
  --canister YOUR_CANISTER_PRINCIPAL \
  --replica-url http://127.0.0.1:4943 \
  --fetch-root-key \
  --output src/generated/gleaph.prepared.ts
```

The principal may include an optional **`ic:`** prefix. If your list method is not named `list_prepared`, set **`--query-method`**.

### Several languages into one folder

```bash
mkdir -p generated
gleaph-codegen codegen \
  --canister YOUR_CANISTER_PRINCIPAL \
  --lang ts \
  --lang rust \
  --output-dir generated \
  --source-label my-service
```

## Generate from a JSON file

Use **`--input`** when you already have a metadata file (same content shape as the canister list). Supported top-level forms:

- a **JSON array** of prepared definitions, or  
- an object **`{ "statements": [ ... ] }`**

```bash
gleaph-codegen codegen \
  --input prepared.json \
  --lang ts \
  --output src/generated/gleaph.prepared.ts \
  --source-label prepared-snapshot
```

See [JSON snapshot example](#json-snapshot-example) below.

## Command reference

Use **either** `--input` **or** `--canister` (not both).

```text
gleaph-codegen codegen (--input <FILE> | --canister <PRINCIPAL>) [OPTIONS]
```

| Option | Description |
|--------|-------------|
| `-i`, `--input` | Path to JSON: array of prepared definitions or `{ "statements": [...] }` |
| `--canister` | Canister principal (text). Anonymous query with `()` to `--query-method` |
| `--replica-url` | IC replica URL (default: `https://ic0.app`) |
| `--fetch-root-key` | Use before querying a **local** replica |
| `--query-method` | Query method name with no args (default: `list_prepared`) |
| `--lang` | `ts`, `js` / `javascript`, `rust` / `rs` (repeat; default: `ts`) |
| `-o`, `--output` | Output file for a **single** language (defaults listed below). For `js`, also writes a matching `.d.ts` |
| `--output-dir` | Output directory when `--lang` is passed **more than once** |
| `--js-param-style` | `camel` (default) or `preserve` (keep wire names as object keys in TS/JS) |
| `--source-label` | Short label in the generated file header |

### Default output names (when `--output` is omitted)

- TypeScript: `gleaph.prepared.ts`
- JavaScript: `gleaph.prepared.js` and `gleaph.prepared.d.ts`
- Rust: `gleaph_prepared.rs`

With multiple `--lang` values, use **`--output-dir`** (or point `--output` at an existing directory).

## What gets generated

### TypeScript (`gleaph.prepared.ts`)

Expects `GraphClient` from **`@gleaph/sdk`** and exports **`createPreparedClient(graph)`**. Prepared calls use **`executePrepared`** / **`executePreparedMutation`**. Execution results are typed loosely; map them to your app’s response shape. Parameters that are Internet Computer principals use **`Principal`** from **`@icp-sdk/core/principal`**.

### JavaScript (`gleaph.prepared.js` + `gleaph.prepared.d.ts`)

Exports **`createPreparedClient(graph)`** from the `.js` file; the `.d.ts` file types the same API. Methods are annotated with **`//`** comments (query text, columns, parameters) for quick reference in the editor.

### Rust (`gleaph_prepared.rs`)

Adds a small **`PreparedClient`** built on a trait you implement for your graph layer (**`gleaph_execute_prepared_query`** / **`gleaph_execute_prepared_mutation`**). Plan on **`serde_json`** for JSON values; add **`candid`** if generated parameter types include **`Principal`**.

## JSON snapshot example

One prepared query as a single-element array (add fields such as `parameters` when your queries take arguments):

```json
[
  {
    "name": "all_users",
    "kind": "Query",
    "requires_caller": false,
    "extension_types": [],
    "source": "MATCH (u:User) RETURN u.id AS id, u.name AS name",
    "columns": [
      { "name": "id", "expr": "u.id", "aliased": true },
      { "name": "name", "expr": "u.name", "aliased": true }
    ],
    "parameters": [],
    "type_warnings": [],
    "explain": "",
    "summary": {
      "estimated_rows": null,
      "estimated_cost": null,
      "has_dml": false,
      "dml_error_count": 0,
      "dml_warning_count": 0,
      "type_warning_count": 0
    }
  }
]
```

## License

MIT (see workspace `license`).
