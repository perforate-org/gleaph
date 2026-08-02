import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { buildSocialLoadArtifact, normalizeCreatedAt } from "./social-load-artifact.mjs";

const buildConfigPath = fileURLToPath(new URL("./build-config.mjs", import.meta.url));

const runGenerator = (output) => {
  const env = { ...process.env, GLEAPH_DEMO_OUTPUT_ROOT: output };
  delete env.SOCIAL_DEMO_USER_SCALE;
  delete env.SOCIAL_DEMO_POST_SCALE;
  return spawnSync(process.execPath, [buildConfigPath], {
    encoding: "utf8",
    env,
  });
};

test("created_at accepts calendar dates and normalizes HH/mm overflow deterministically", () => {
  const before = normalizeCreatedAt("202607140984").DateTime.seconds;
  const overflow = normalizeCreatedAt("202607140985").DateTime.seconds;
  const after = normalizeCreatedAt("202607140986").DateTime.seconds;
  assert.equal(overflow, Math.trunc(Date.parse("2026-07-14T10:25:00Z") / 1000));
  assert.ok(before < overflow && overflow < after);
  assert.deepEqual(normalizeCreatedAt("200002290000"), {
    DateTime: { seconds: 951782400, nanos: 0 },
  });
});

test("created_at rejects invalid grammar, dates, bounds, and unsafe numeric input", () => {
  for (const value of [
    "20260714000",
    "2026071400000",
    "2026-071400",
    "000001010000",
    "1000001010000",
    "202302290000",
    "202613010000",
    Number.MAX_SAFE_INTEGER + 1,
  ]) {
    assert.throws(() => normalizeCreatedAt(value), undefined, String(value));
  }
});

test("typed artifact preserves vertex and edge order and source-id closure", () => {
  const graph = {
    nodes: [
      { id: "u", gqlLabel: "User", property: "name", label: "User", kind: "user" },
      {
        id: "p",
        gqlLabel: "Post",
        property: "body",
        label: "Post",
        kind: "post",
        createdAt: "202607140985",
        isPublic: true,
        embedding: { values: [0.5] },
      },
    ],
    edges: [
      { id: "posted", source: "u", target: "p", gqlLabel: "POSTED", displayLabel: "posted" },
    ],
  };
  const ids = new Map([["p", 7n]]);
  const artifact = buildSocialLoadArtifact({ graph, demoId: (id) => ids.get(id) });
  assert.deepEqual(artifact.vertices.map(({ source_id }) => source_id), ["u", "p"]);
  assert.deepEqual(artifact.edges.map(({ source_id, target_id }) => [source_id, target_id]), [["u", "p"]]);
  assert.deepEqual(Object.keys(artifact.embeddings), ["p"]);
  assert.throws(() => buildSocialLoadArtifact({
    graph: { ...graph, edges: [{ ...graph.edges[0], target: "missing" }] },
    demoId: (id) => ids.get(id),
  }), /unknown endpoint/);
});

test("full generator emits byte-identical typed load artifacts in isolated roots", () => {
  const root = mkdtempSync(join(tmpdir(), "gleaph-social-load-contract-"));
  const outputs = [join(root, "first"), join(root, "second")];
  for (const output of outputs) {
    mkdirSync(output, { recursive: true });
    const result = runGenerator(output);
    assert.equal(result.status, 0, result.stderr);
  }
  assert.deepEqual(
    readFileSync(join(outputs[0], "seeds", "social-load.json")),
    readFileSync(join(outputs[1], "seeds", "social-load.json")),
  );
});

test("default generator preserves the tracked 5x/20x workload and endpoint closure", () => {
  const root = mkdtempSync(join(tmpdir(), "gleaph-social-scale-contract-"));
  mkdirSync(root, { recursive: true });
  const result = runGenerator(root);
  assert.equal(result.status, 0, result.stderr);

  const graph = JSON.parse(
    readFileSync(join(root, "seeds", "social-graph.json"), "utf8"),
  );
  const load = JSON.parse(
    readFileSync(join(root, "seeds", "social-load.json"), "utf8"),
  );
  assert.equal(graph.nodes.length, 7244);
  assert.equal(graph.edges.length, 47230);
  assert.equal(load.vertices.length, 7244);
  assert.equal(load.edges.length, 47230);
  assert.equal(Object.keys(load.embeddings).length, 7100);

  const vertexIds = new Set(load.vertices.map(({ source_id }) => source_id));
  for (const edge of load.edges) {
    assert.ok(vertexIds.has(edge.source_id), `missing source ${edge.source_id}`);
    assert.ok(vertexIds.has(edge.target_id), `missing target ${edge.target_id}`);
  }
  for (const embeddingId of Object.keys(load.embeddings)) {
    assert.ok(vertexIds.has(embeddingId), `missing embedding vertex ${embeddingId}`);
  }
});
