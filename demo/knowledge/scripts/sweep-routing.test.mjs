// Contract tests for the abstain-threshold sweep over shadow records.
//
//   node --test scripts/sweep-routing.test.mjs

import test from "node:test";
import assert from "node:assert/strict";

import { acceptanceScore, renderTable, sweep } from "./sweep-routing.mjs";

function record(expectedOp, scores) {
  return {
    expected_op: expectedOp ?? undefined,
    model: {
      hits: scores.map((score, index) => ({ name: index === 0 ? expectedOp ?? "x" : `other${index}`, score })),
      abstained: false,
    },
  };
}

const records = [
  record("citation-reach", [0.9, 0.05]),
  record("shortest-path", [0.5, 0.2]),
  record("variable-length-reach", [0.2, 0.15]),
  record(null, [0.3, 0.1]),
  record(null, [0.8, 0.1]),
];

test("acceptance score is the top probability or the top margin", () => {
  assert.equal(acceptanceScore(records[0]), 0.9);
  assert.equal(acceptanceScore(records[1], "margin"), 0.3);
  assert.equal(acceptanceScore({ model: { hits: [] } }), 0);
});

test("sweep reports coverage, precision and out-of-scope recall", () => {
  const rows = sweep(records, [0.4, 0.6]);
  const low = rows[0];
  assert.equal(low.inScope, 3);
  assert.equal(low.answered, 2);
  assert.equal(low.correct, 2);
  assert.equal(low.outOfScope, 2);
  assert.equal(low.oosAbstained, 1);
  assert.equal(low.coverage.toFixed(3), "0.667");
  assert.equal(low.precision, 1);
  assert.equal(low.oosRecall, 0.5);

  const high = rows[1];
  assert.equal(high.answered, 1);
  assert.equal(high.oosAbstained, 1);
});

test("table renders one row per threshold", () => {
  const table = renderTable(sweep(records, [0.4, 0.6]));
  const lines = table.split("\n");
  assert.equal(lines.length, 4);
  assert.match(lines[2], /^\| 0\.40 \| 2\/3 \| 66\.7% \| 100\.0% \| 50\.0%/);
});
