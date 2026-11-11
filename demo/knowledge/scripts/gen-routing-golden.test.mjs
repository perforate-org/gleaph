// Contract tests for the op-level routing golden set generator.
//
//   node --test scripts/gen-routing-golden.test.mjs
//
// These pin the properties the harness depends on: exact sizes, deterministic regeneration,
// family and entity isolation between splits, label validity against the prepared directory, and
// a usable per-operation / per-language balance.

import test from "node:test";
import assert from "node:assert/strict";

import {
  DEFAULTS,
  generateRoutingGolden,
  generateTrainingSet,
  readEntities,
  readPreparedOperations,
  serialiseSplit,
} from "./gen-routing-golden.mjs";

const SPLITS = ["train", "dev", "test"];

function generate() {
  return generateRoutingGolden();
}

function byOperation(items) {
  const counts = new Map();
  for (const item of items) {
    const key = item.expected_op ?? "out-of-scope";
    counts.set(key, (counts.get(key) ?? 0) + 1);
  }
  return counts;
}

test("sizes match the documented 900/200/400 shape at a 20% abstain ratio", () => {
  const result = generate();
  assert.deepEqual(
    SPLITS.map((split) => result.splits[split].length),
    [DEFAULTS.train, DEFAULTS.dev, DEFAULTS.test],
  );
  for (const split of SPLITS) {
    const items = result.splits[split];
    const abstain = items.filter((item) => item.expect_abstain).length;
    const positive = items.length - abstain;
    assert.equal(abstain, Math.round(items.length * DEFAULTS.outOfScopeRatio));
    assert.equal(positive, items.length - abstain);
  }
});

test("generation is deterministic and seed-sensitive", () => {
  const first = generate();
  const second = generate();
  assert.deepEqual(first.splits, second.splits);

  const reseeded = generateRoutingGolden({ seed: DEFAULTS.seed + 1 });
  assert.notDeepEqual(first.splits.test, reseeded.splits.test);
});

test("every positive label is a prepared operation and abstain items carry no op", () => {
  const result = generate();
  const prepared = readPreparedOperations();
  for (const split of SPLITS) {
    for (const item of result.splits[split]) {
      assert.ok(item.question.trim().length > 0, "question must not be empty");
      const hasOp = item.expected_op !== undefined;
      const abstains = item.expect_abstain === true;
      assert.notEqual(hasOp, abstains, `item must have exactly one label: ${item.question}`);
      if (hasOp) {
        assert.ok(
          prepared.includes(item.expected_op),
          `unknown operation ${item.expected_op} in ${item.question}`,
        );
      }
    }
  }
});

test("operations are balanced inside every split", () => {
  const result = generate();
  const prepared = readPreparedOperations();
  for (const split of SPLITS) {
    const items = result.splits[split];
    const counts = byOperation(items);
    const abstain = Math.round(items.length * 0.2);
    const expectedPerOperation = Math.floor((items.length - abstain) / prepared.length);
    for (const operation of prepared) {
      assert.equal(
        counts.get(operation),
        expectedPerOperation,
        `${split}/${operation} is not balanced`,
      );
    }
    assert.equal(counts.get("out-of-scope"), abstain);
  }
});

test("both languages and the abstraction slices appear in every split", () => {
  const result = generate();
  for (const split of SPLITS) {
    const items = result.splits[split];
    assert.ok(items.some((item) => item.lang === "en"), `${split} needs English items`);
    assert.ok(items.some((item) => item.lang === "ja"), `${split} needs Japanese items`);
    assert.ok(items.some((item) => item.abstraction === "graph-vocabulary"));
    assert.ok(items.some((item) => item.abstraction === "business-intent"));
  }
});

test("no template renders an unfilled value", () => {
  const result = generate();
  for (const split of SPLITS) {
    for (const item of result.splits[split]) {
      assert.ok(!/undefined|NaN/.test(item.question), `unfilled value: ${item.question}`);
    }
  }
});

test("question wording is unique across the whole dataset", () => {
  const result = generate();
  const seen = new Set();
  for (const split of SPLITS) {
    for (const item of result.splits[split]) {
      assert.ok(!seen.has(item.question), `duplicate question: ${item.question}`);
      seen.add(item.question);
    }
  }
  assert.equal(seen.size, DEFAULTS.train + DEFAULTS.dev + DEFAULTS.test);
});

test("families do not leak between splits", () => {
  const result = generate();
  const [train, dev, test] = SPLITS.map((split) => new Set(result.families[split]));
  for (const family of dev) {
    assert.ok(!train.has(family), `dev family leaked into train: ${family}`);
  }
  for (const family of test) {
    assert.ok(!train.has(family), `test family leaked into train: ${family}`);
    assert.ok(!dev.has(family), `test family leaked into dev: ${family}`);
  }
});

test("test split holds out entities that training never sees", () => {
  const result = generate();
  const entities = readEntities();
  for (const key of ["concept", "document"]) {
    const plural = key === "concept" ? "concepts" : "documents";
    const train = new Set(result.entities.train[key]);
    const test = new Set(result.entities.test[key]);
    const heldOut = entities[plural].filter((entity) => !train.has(entity));
    assert.ok(heldOut.length >= 1, `no held-out ${plural}`);
    for (const entity of heldOut) {
      assert.ok(test.has(entity), `held-out ${plural} ${entity} missing from test`);
    }
  }
});

test("serialised JSONL round-trips with the fields the CLI reads", () => {
  const result = generate();
  for (const split of SPLITS) {
    const lines = serialiseSplit(result.splits[split]).trim().split("\n");
    assert.equal(lines.length, result.splits[split].length);
    for (const line of lines) {
      const parsed = JSON.parse(line);
      assert.equal(typeof parsed.question, "string");
      assert.ok(parsed.expected_op !== undefined || parsed.expect_abstain === true);
      assert.equal(typeof parsed.lang, "string");
      assert.equal(typeof parsed.family, "string");
    }
  }
});

test("training set is balanced, unique, and disjoint from evaluation families", () => {
  const canonical = generateRoutingGolden();
  const training = generateTrainingSet({ train: 600 });
  assert.equal(training.items.length, 600);
  assert.equal(new Set(training.items.map((item) => item.question)).size, 600);
  const ops = new Map();
  for (const item of training.items) {
    const key = item.expected_op ?? "abstain";
    ops.set(key, (ops.get(key) ?? 0) + 1);
  }
  const prepared = readPreparedOperations();
  assert.equal(ops.size, prepared.length + 1);
  // generateTrainingSet raises the training-only abstain ratio to 30% (audit-driven); the
  // canonical splits keep their documented 20%.
  const abstain = Math.round(600 * 0.35);
  const perOperation = Math.floor((600 - abstain) / prepared.length);
  assert.equal(ops.get("abstain"), abstain);
  for (const operation of prepared) {
    assert.equal(ops.get(operation), perOperation, `${operation} is not balanced`);
  }
  const evaluationFamilies = new Set([...canonical.families.dev, ...canonical.families.test]);
  for (const item of training.items) {
    assert(!evaluationFamilies.has(item.family), `training family leaked into evaluation: ${item.family}`);
    assert(!item.question.includes("undefined") && !item.question.includes("NaN"));
  }
  assert(
    training.items.some((item) => item.family.includes("/xtrain-")),
    "extra training-only templates must appear",
  );
});

test("training set generation is deterministic", () => {
  const first = generateTrainingSet({ train: 200 });
  const second = generateTrainingSet({ train: 200 });
  assert.deepEqual(
    first.items.map((item) => item.question),
    second.items.map((item) => item.question),
  );
});
