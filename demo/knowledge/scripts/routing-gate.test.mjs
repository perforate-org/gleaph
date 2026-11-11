import assert from "node:assert/strict";
import { test } from "node:test";
import { ABSTAIN, buildCriteria, evaluate, exitCodeFor, gate, leadingDocComment } from "./routing-gate.mjs";

const REAL = ["citation-reach", "document-text-search", "shortest-path", "team-readable-documents", "variable-length-reach"];

function probabilities(chosen, confidence = 0.9) {
  const out = {};
  const rest = (1 - confidence) / REAL.length;
  for (const name of REAL) out[name] = rest;
  out[chosen] += confidence - rest;
  out[ABSTAIN] = chosen === ABSTAIN ? confidence : 1 - confidence;
  return out;
}

function row(question, expected_op) {
  return { question, expected_op };
}

const rows = [
  row("q1", "citation-reach"),
  row("q2", "document-text-search"),
  row("q3", "shortest-path"),
  row("q4", "team-readable-documents"),
  row("q5", "variable-length-reach"),
  row("q6", null),
];

const perfect = (question) => {
  const match = rows.find((entry) => entry.question === question);
  return probabilities(match.expected_op ?? ABSTAIN);
};

test("doc comment parser joins leading /// lines and stops at the statement", () => {
  assert.equal(leadingDocComment("/// one\n///  two\nMATCH (n) RETURN n"), "one two");
  assert.equal(leadingDocComment("MATCH (n) RETURN n"), "");
});

test("criteria come from prepared files and end with the abstain option", () => {
  const criteria = buildCriteria();
  assert.deepEqual(Object.keys(criteria).sort(), [...REAL, ABSTAIN].sort());
  for (const description of Object.values(criteria)) {
    assert.ok(description.length > 0);
  }
});

test("perfect predictions pass every gate floor", () => {
  const stats = evaluate(rows, perfect);
  assert.equal(stats.accuracy, 1);
  assert.equal(stats.oosRefusal, 1);
  const verdict = gate(stats);
  assert.equal(verdict.passed, true);
});

test("a wrong positive route lowers precision and fails the floor", () => {
  const predict = (question) => probabilities(question === "q1" ? "shortest-path" : (rows.find((entry) => entry.question === question).expected_op ?? ABSTAIN));
  const stats = evaluate(rows, predict);
  assert.equal(stats.correct, 4);
  assert.equal(stats.precision, 4 / 5);
  assert.equal(gate(stats, { precision: 0.9 }).passed, false);
});

test("an OOS question routed to a real operation lowers accuracy and OOS refusal", () => {
  const predict = (question) => probabilities(question === "q6" ? "citation-reach" : (rows.find((entry) => entry.question === question).expected_op ?? ABSTAIN));
  const stats = evaluate(rows, predict);
  assert.equal(stats.oosRefused, 0);
  assert.equal(stats.accuracy, 5 / 6);
  assert.equal(gate(stats, { oosRefusal: 0.9 }).passed, false);
});

test("frozen set verifies against its sha256 sidecar and fails after mutation", async () => {
  const { mkdtempSync, writeFileSync } = await import("node:fs");
  const { tmpdir } = await import("node:os");
  const { join } = await import("node:path");
  const { sha256File, verifyFrozen } = await import("./routing-gate.mjs");
  const dir = mkdtempSync(join(tmpdir(), "routing-gate-"));
  const file = join(dir, "blind.jsonl");
  writeFileSync(file, '{"question":"q","expected_op":null}\n');
  writeFileSync(`${file}.sha256`, `${sha256File(file)}\n`);
  assert.equal(verifyFrozen(file).ok, true);
  writeFileSync(file, '{"question":"q2","expected_op":null}\n');
  assert.equal(verifyFrozen(file).ok, false);
});

test("report-only keeps a failing blind gate from failing the process", () => {
  const failing = { passed: false, checks: [] };
  assert.equal(exitCodeFor(failing), 1);
  assert.equal(exitCodeFor(failing, true), 0);
  assert.equal(exitCodeFor({ passed: true }, false), 0);
});
