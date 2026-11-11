// Contract tests for paraphrase generation and validation.
//
//   node --test scripts/paraphrase-routing.test.mjs

import test from "node:test";
import assert from "node:assert/strict";

import {
  requiredTokens,
  sampleItems,
  validateParaphrase,
} from "./paraphrase-routing.mjs";

const entities = ["Graph databases", "Vector search", "The GQL standard"];

test("required tokens keep relation labels, entities and numbers", () => {
  const tokens = requiredTokens(
    "Which documents are reachable from The GQL standard through CITES up to depth 2?",
    entities,
  );
  assert.deepEqual(tokens.sort(), ["2", "CITES", "GQL", "The GQL standard"].sort());
});

test("a genuine rewrite is accepted", () => {
  const result = validateParaphrase(
    "Which concepts reach Graph databases through RELATED_TO within 2 hops?",
    "Within 2 hops, which concepts can get to Graph databases by following RELATED_TO?",
    entities,
  );
  assert.equal(result.ok, true);
});

test("identical text is rejected", () => {
  const question = "Which concepts reach Graph databases through RELATED_TO within 2 hops?";
  const result = validateParaphrase(question, question, entities);
  assert.equal(result.ok, false);
  assert.equal(result.reason, "identical");
});

test("dropping a relation label or number is rejected", () => {
  const original = "Which concepts reach Graph databases through RELATED_TO within 2 hops?";
  const dropped = validateParaphrase(
    original,
    "Which concepts are near Graph databases within 2 hops?",
    entities,
  );
  assert.equal(dropped.ok, false);
  assert.match(dropped.reason, /^missing:/);

  const droppedNumber = validateParaphrase(
    original,
    "Which concepts reach Graph databases through RELATED_TO?",
    entities,
  );
  assert.equal(droppedNumber.ok, false);
});

test("a language change is rejected", () => {
  const result = validateParaphrase(
    "Vector search と Graph databases の最短の RELATED_TO 経路は？",
    "What is the shortest RELATED_TO path between Vector search and Graph databases?",
    entities,
  );
  assert.equal(result.ok, false);
  assert.equal(result.reason, "language-changed");
});

test("empty output is rejected", () => {
  const result = validateParaphrase("Which concepts reach X?", "   ", entities);
  assert.equal(result.ok, false);
  assert.equal(result.reason, "empty");
});

test("sampling is deterministic and bounded", () => {
  const items = Array.from({ length: 50 }, (_, index) => ({ question: `q${index}` }));
  const first = sampleItems(items, 10, 7);
  const second = sampleItems(items, 10, 7);
  assert.deepEqual(first, second);
  assert.equal(first.length, 10);
  assert.equal(new Set(first.map((item) => item.question)).size, 10);
  assert.equal(sampleItems(items, 1000, 7).length, 50);
});
