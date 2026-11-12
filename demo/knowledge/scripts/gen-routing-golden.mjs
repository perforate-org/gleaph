// Deterministic op-level routing golden set for the knowledge demo.
//
// Run:
//   node scripts/gen-routing-golden.mjs            # writes eval/routing/{train,dev,test}.jsonl
//
// Why a generator and not a hand-written file: the routing evaluation needs enough items for
// per-operation and per-language slices (900/200/400 as recorded in
// design/investigations/2026-11-09-gleaph-decision-model-research.md §4.4), and the labels must be
// reproducible from the checked-in seeds. This first slice labels the *operation* only — the demo's
// prepared queries are literal-bound, so slot values do not exist yet. Every item is a template
// instance over real seed entities; an LLM paraphrase pass can be layered on later without
// changing the split logic.
//
// Split discipline: items are grouped into families (operation × template × language ×
// abstraction). Whole families are assigned to train/dev/test, and entities are held out per
// split, so a test question shares neither its wording family nor its entity with training.

import { readdirSync, readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const DEMO_ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const PREPARED_DIR = join(DEMO_ROOT, "prepared");
const SEEDS = join(DEMO_ROOT, "seeds", "vertices.jsonl");
const OUTPUT_DIR = join(DEMO_ROOT, "eval", "routing");

export const DEFAULTS = {
  seed: 20261109,
  train: 900,
  dev: 200,
  test: 400,
  outOfScopeRatio: 0.2,
};

/** Mulberry32: small, deterministic, good enough for shuffling fixtures. */
function rng(seed) {
  let state = seed >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let t = state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function shuffle(items, random) {
  const copy = [...items];
  for (let index = copy.length - 1; index > 0; index -= 1) {
    const swap = Math.floor(random() * (index + 1));
    [copy[index], copy[swap]] = [copy[swap], copy[index]];
  }
  return copy;
}

function readJsonl(path) {
  return readFileSync(path, "utf8")
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line.length > 0)
    .map((line) => JSON.parse(line));
}

/** Prepared operations are the single source of truth for routable names. */
export function readPreparedOperations(root = PREPARED_DIR) {
  return readdirSync(root)
    .filter((name) => name.endsWith(".gql"))
    .map((name) => name.slice(0, -".gql".length))
    .sort();
}

/**
 * Extract the demo entities the templates may reference.
 *
 * Seed vertex properties are `{ Text: string }` wrappers; labels are the `labels` array.
 */
export function readEntities(path = SEEDS) {
  const entities = { concepts: [], documents: [], persons: [], teams: [] };
  for (const vertex of readJsonl(path)) {
    const labels = vertex.labels ?? [];
    const properties = vertex.properties ?? {};
    const text = (key) => properties[key]?.Text;
    if (labels.includes("Concept") && text("name")) {
      entities.concepts.push(text("name"));
    }
    if (labels.includes("Document") && text("title")) {
      entities.documents.push(text("title"));
    }
    if (labels.includes("Person") && text("name")) {
      entities.persons.push(text("name"));
    }
    if (labels.includes("Team") && text("name")) {
      entities.teams.push(text("name"));
    }
  }
  for (const key of Object.keys(entities)) {
    entities[key] = [...new Set(entities[key])].sort();
  }
  return entities;
}

/**
 * Template bank.
 *
 * `needs` lists the entity keys a template consumes; `build` renders one question. Templates are
 * deliberately written against what the prepared query actually does (for example, citation reach
 * follows CITES outward from a document), not against the `///` comment prose.
 */
const PLACES = ["Paris", "Tokyo", "Berlin", "Osaka", "London", "Toronto", "Singapore", "Zurich"];
const ITEMS = ["documents", "concepts", "teams", "people", "citations", "indexes"];
const ROLES = ["researcher", "engineer", "writer", "analyst"];

function templates() {
  return [
    // ---- variable-length-reach: concepts that reach the anchor over incoming RELATED_TO ----
    {
      op: "variable-length-reach",
      id: "reach-incoming-hops",
      lang: "en",
      abstraction: "graph-vocabulary",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `Which concepts reach ${concept} through RELATED_TO within ${hops} hops?`,
    },
    {
      op: "variable-length-reach",
      id: "reach-lead-in",
      lang: "en",
      abstraction: "domain",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `List the concepts that lead to ${concept} over up to ${hops} RELATED_TO relations.`,
    },
    {
      op: "variable-length-reach",
      id: "reach-traverse",
      lang: "en",
      abstraction: "business-intent",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `Which topics sit within ${hops} steps of ${concept} by following related concepts?`,
    },
    {
      op: "variable-length-reach",
      id: "reach-ja-hops",
      lang: "ja",
      abstraction: "graph-vocabulary",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `${concept} に RELATED_TO で ${hops} ホップ以内に到達する概念は？`,
    },
    {
      op: "variable-length-reach",
      id: "reach-ja-lead",
      lang: "ja",
      abstraction: "domain",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `${concept} へ RELATED_TO をたどって ${hops} ホップ以内で到達できる概念を列挙して`,
    },
    {
      op: "variable-length-reach",
      id: "reach-ja-topic",
      lang: "ja",
      abstraction: "business-intent",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `${concept} と関連するテーマを ${hops} ステップ以内で洗い出したい`,
    },
    // ---- shortest-path: shortest RELATED_TO chain between two concepts ----
    {
      op: "shortest-path",
      id: "shortest-between",
      lang: "en",
      abstraction: "graph-vocabulary",
      needs: ["concept", "concept2"],
      build: ({ concept, concept2 }) =>
        `What is the shortest RELATED_TO chain between ${concept} and ${concept2}?`,
    },
    {
      op: "shortest-path",
      id: "shortest-route",
      lang: "en",
      abstraction: "domain",
      needs: ["concept", "concept2"],
      build: ({ concept, concept2 }) =>
        `Find the shortest path from ${concept} to ${concept2} following RELATED_TO edges.`,
    },
    {
      op: "shortest-path",
      id: "shortest-ja",
      lang: "ja",
      abstraction: "graph-vocabulary",
      needs: ["concept", "concept2"],
      build: ({ concept, concept2 }) =>
        `${concept} と ${concept2} の最短の RELATED_TO 経路は？`,
    },
    {
      op: "shortest-path",
      id: "shortest-ja-route",
      lang: "ja",
      abstraction: "business-intent",
      needs: ["concept", "concept2"],
      build: ({ concept, concept2 }) =>
        `${concept} から ${concept2} へ最短でたどる関連の連鎖を教えて`,
    },
    // ---- citation-reach: documents reachable from a document through outgoing CITES ----
    {
      op: "citation-reach",
      id: "cites-depth",
      lang: "en",
      abstraction: "graph-vocabulary",
      needs: ["document", "depth"],
      build: ({ document, depth }) =>
        `Which documents are reachable from ${document} through CITES up to depth ${depth}?`,
    },
    {
      op: "citation-reach",
      id: "cites-chain",
      lang: "en",
      abstraction: "domain",
      needs: ["document", "depth"],
      build: ({ document, depth }) =>
        `Show the citation trail starting at ${document} down to depth ${depth}.`,
    },
    {
      op: "citation-reach",
      id: "cites-ja",
      lang: "ja",
      abstraction: "graph-vocabulary",
      needs: ["document", "depth"],
      build: ({ document, depth }) =>
        `${document} から CITES を ${depth} 階層までたどって到達できる文書は？`,
    },
    {
      op: "citation-reach",
      id: "cites-ja-trail",
      lang: "ja",
      abstraction: "business-intent",
      needs: ["document", "depth"],
      build: ({ document, depth }) =>
        `${document} が引用している文書を ${depth} 階層まで追いかけたい`,
    },
    // ---- team-readable-documents: vector search constrained to public Platform-owned documents ----
    {
      op: "team-readable-documents",
      id: "team-readable-topic",
      lang: "en",
      abstraction: "graph-vocabulary",
      needs: ["topic"],
      build: ({ topic }) =>
        `Which Platform owned or public documents are semantically similar to "${topic}"?`,
    },
    {
      op: "team-readable-documents",
      id: "team-readable-intent",
      lang: "en",
      abstraction: "business-intent",
      needs: ["topic"],
      build: ({ topic }) =>
        `What can the Platform team read about ${topic}?`,
    },
    {
      op: "team-readable-documents",
      id: "team-ja-topic",
      lang: "ja",
      abstraction: "domain",
      needs: ["topic"],
      build: ({ topic }) =>
        `Platform チームが読める、${topic} に関連する文書をベクトル類似で探して`,
    },
    {
      op: "team-readable-documents",
      id: "team-ja-intent",
      lang: "ja",
      abstraction: "business-intent",
      needs: ["topic"],
      build: ({ topic }) =>
        `${topic} について、Platform チームが見られる資料はどれ？`,
    },
    // ---- additional wordings: more families per operation keep train/dev/test disjoint ----
    {
      op: "variable-length-reach",
      id: "reach-incoming-order",
      lang: "en",
      abstraction: "graph-vocabulary",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `${concept} is reachable from which concepts through RELATED_TO in at most ${hops} hops?`,
    },
    {
      op: "variable-length-reach",
      id: "reach-backwards",
      lang: "en",
      abstraction: "domain",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `Trace RELATED_TO edges backwards from ${concept} for up to ${hops} hops and list the concepts reached.`,
    },
    {
      op: "variable-length-reach",
      id: "reach-ja-incoming",
      lang: "ja",
      abstraction: "graph-vocabulary",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `${concept} に RELATED_TO で到達する概念を ${hops} ホップまで列挙して`,
    },
    {
      op: "variable-length-reach",
      id: "reach-ja-backwards",
      lang: "ja",
      abstraction: "domain",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `${concept} を終点とする RELATED_TO の連鎖を ${hops} ホップまで逆にたどると何がある？`,
    },
    {
      op: "shortest-path",
      id: "shortest-connect",
      lang: "en",
      abstraction: "graph-vocabulary",
      needs: ["concept", "concept2"],
      build: ({ concept, concept2 }) =>
        `Shortest RELATED_TO route connecting ${concept} and ${concept2}.`,
    },
    {
      op: "shortest-path",
      id: "shortest-connect-intent",
      lang: "en",
      abstraction: "business-intent",
      needs: ["concept", "concept2"],
      build: ({ concept, concept2 }) =>
        `How do ${concept} and ${concept2} connect through the shortest RELATED_TO chain?`,
    },
    {
      op: "shortest-path",
      id: "shortest-steps",
      lang: "en",
      abstraction: "domain",
      needs: ["concept", "concept2"],
      build: ({ concept, concept2 }) =>
        `Minimum number of RELATED_TO steps from ${concept} to ${concept2}: which path is shortest?`,
    },
    {
      op: "shortest-path",
      id: "shortest-ja-chain",
      lang: "ja",
      abstraction: "graph-vocabulary",
      needs: ["concept", "concept2"],
      build: ({ concept, concept2 }) =>
        `${concept} と ${concept2} をつなぐ最短の RELATED_TO チェーンは？`,
    },
    {
      op: "shortest-path",
      id: "shortest-ja-steps",
      lang: "ja",
      abstraction: "domain",
      needs: ["concept", "concept2"],
      build: ({ concept, concept2 }) =>
        `${concept} から ${concept2} まで RELATED_TO を最短でたどる経路を知りたい`,
    },
    {
      op: "shortest-path",
      id: "shortest-ja-search",
      lang: "ja",
      abstraction: "business-intent",
      needs: ["concept", "concept2"],
      build: ({ concept, concept2 }) =>
        `${concept} と ${concept2} の間の最短経路を RELATED_TO で探して`,
    },
    {
      op: "citation-reach",
      id: "cites-within",
      lang: "en",
      abstraction: "graph-vocabulary",
      needs: ["document", "depth"],
      build: ({ document, depth }) =>
        `Starting from ${document}, which documents can be reached through CITES within ${depth} hops?`,
    },
    {
      op: "citation-reach",
      id: "cites-follow",
      lang: "en",
      abstraction: "domain",
      needs: ["document", "depth"],
      build: ({ document, depth }) =>
        `Follow CITES from ${document} for depth ${depth} and list the documents reached.`,
    },
    {
      op: "citation-reach",
      id: "cites-transitive",
      lang: "en",
      abstraction: "business-intent",
      needs: ["document", "depth"],
      build: ({ document, depth }) =>
        `What is cited transitively from ${document} up to depth ${depth}?`,
    },
    {
      op: "citation-reach",
      id: "cites-ja-follow",
      lang: "ja",
      abstraction: "graph-vocabulary",
      needs: ["document", "depth"],
      build: ({ document, depth }) =>
        `${document} から CITES を ${depth} ホップたどった先の文書を列挙して`,
    },
    {
      op: "citation-reach",
      id: "cites-ja-chain",
      lang: "ja",
      abstraction: "domain",
      needs: ["document", "depth"],
      build: ({ document, depth }) =>
        `${document} が直接的・間接的に引用している文書を ${depth} 階層分見たい`,
    },
    {
      op: "citation-reach",
      id: "cites-ja-trail2",
      lang: "ja",
      abstraction: "business-intent",
      needs: ["document", "depth"],
      build: ({ document, depth }) =>
        `${document} を起点に CITES の連鎖を ${depth} まで追って文書を出して`,
    },
    {
      op: "team-readable-documents",
      id: "team-readable-similar",
      lang: "en",
      abstraction: "domain",
      needs: ["topic"],
      build: ({ topic }) => `Documents similar to ${topic} that the Platform team can read.`,
    },
    {
      op: "team-readable-documents",
      id: "team-readable-embeddings",
      lang: "en",
      abstraction: "graph-vocabulary",
      needs: ["topic"],
      build: ({ topic }) =>
        `Which Platform-accessible documents are closest to ${topic} in embedding space?`,
    },
    {
      op: "team-readable-documents",
      id: "team-readable-match",
      lang: "en",
      abstraction: "business-intent",
      needs: ["topic"],
      build: ({ topic }) =>
        `Find readable documents for the Platform team that match the meaning of ${topic}.`,
    },
    {
      op: "team-readable-documents",
      id: "team-ja-similar",
      lang: "ja",
      abstraction: "domain",
      needs: ["topic"],
      build: ({ topic }) =>
        `${topic} に意味が近い文書のうち Platform チームが閲覧できるもの`,
    },
    {
      op: "team-readable-documents",
      id: "team-ja-embed",
      lang: "ja",
      abstraction: "graph-vocabulary",
      needs: ["topic"],
      build: ({ topic }) =>
        `Platform チームの権限で読める ${topic} 関連の文書を類似度順に出して`,
    },
    {
      op: "team-readable-documents",
      id: "team-ja-access",
      lang: "ja",
      abstraction: "business-intent",
      needs: ["topic"],
      build: ({ topic }) =>
        `${topic} に近い内容で、Platform チームがアクセスできる文書は？`,
    },
    // ---- out of scope: no prepared operation answers these ----
    {
      op: null,
      id: "oos-weather",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["place"],
      build: ({ place }) => `What is the weather in ${place} tomorrow?`,
    },
    {
      op: null,
      id: "oos-weather-ja",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: ["place"],
      build: ({ place }) => `${place} の明日の天気は？`,
    },
    {
      op: null,
      id: "oos-write",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["document"],
      build: ({ document }) => `Create a new document titled "${document} draft" and publish it.`,
    },
    {
      op: null,
      id: "oos-write-ja",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: ["document"],
      build: ({ document }) => `「${document}」という文書を作成して公開して`,
    },
    {
      op: null,
      id: "oos-delete",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["document"],
      build: ({ document }) => `Delete ${document} and every document that cites it.`,
    },
    {
      op: null,
      id: "oos-delete-ja",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: ["document"],
      build: ({ document }) => `${document} と、それを引用している文書を削除して`,
    },
    {
      op: null,
      id: "oos-aggregate",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["item"],
      build: ({ item }) => `How many ${item} are there in total?`,
    },
    {
      op: null,
      id: "oos-aggregate-ja",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: ["item"],
      build: ({ item }) => `${item} は全部で何件ある？`,
    },
    {
      op: null,
      id: "oos-people",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["role"],
      build: ({ role }) => `List every ${role} and their email address.`,
    },
    {
      op: null,
      id: "oos-permission",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["document"],
      build: ({ document }) =>
        `Who has permission to delete ${document}?`,
    },
    {
      op: null,
      id: "oos-room",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["place"],
      build: ({ place }) => `Book a meeting room in ${place} for tomorrow morning.`,
    },
    {
      op: null,
      id: "oos-room-ja",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: ["place"],
      build: ({ place }) => `${place} の会議室を明日の朝に予約して`,
    },
    {
      op: null,
      id: "oos-compound",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["topic", "concept"],
      build: ({ topic, concept }) =>
        `Summarise ${topic} and ${concept} together and suggest what to read next.`,
    },
    {
      op: null,
      id: "oos-compound-ja",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: ["concept", "document"],
      build: ({ concept, document }) =>
        `${concept} と「${document}」の両方をまとめて要約して`,
    },
    {
      op: null,
      id: "oos-stock",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["item"],
      build: ({ item }) => `What is the current price of ${item}?`,
    },
    {
      op: null,
      id: "oos-stock-ja",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: ["item"],
      build: ({ item }) => `${item} の現在価格は？`,
    },
    {
      op: null,
      id: "oos-email",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["concept"],
      build: ({ concept }) => `Draft an email explaining ${concept} to a new colleague.`,
    },
    {
      op: null,
      id: "oos-translate",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["document"],
      build: ({ document }) => `Translate ${document} into Japanese and rewrite the summary.`,
    },
    {
      op: null,
      id: "oos-math",
      lang: "en",
      abstraction: "out-of-scope",
      needs: [],
      build: () => `What is 17% of 482, rounded to one decimal?`,
    },
    {
      op: null,
      id: "oos-code",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["item"],
      build: ({ item }) => `Write a Python function that counts ${item} in a list.`,
    },
    {
      op: null,
      id: "oos-hotel",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["place"],
      build: ({ place }) => `Book a hotel in ${place} for next weekend.`,
    },
    {
      op: null,
      id: "oos-news",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["place"],
      build: ({ place }) => `What happened in ${place} today?`,
    },
    {
      op: null,
      id: "oos-recipe",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["item"],
      build: ({ item }) => `Give me a recipe for ${item}.`,
    },
    {
      op: null,
      id: "oos-count-edges",
      lang: "en",
      abstraction: "out-of-scope",
      needs: [],
      build: () => `How many CITES edges are in the graph in total?`,
    },
    {
      op: null,
      id: "oos-drop-graph",
      lang: "en",
      abstraction: "out-of-scope",
      needs: [],
      build: () => `Drop the entire knowledge graph and remove all documents.`,
    },
    {
      op: null,
      id: "oos-translate-ja",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: ["document"],
      build: ({ document }) => `${document} を日本語に翻訳して要約してください。`,
    },
    {
      op: null,
      id: "oos-math-ja",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: [],
      build: () => `482 の 17% はいくつですか？`,
    },
    {
      op: null,
      id: "oos-mail-ja",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: ["concept"],
      build: ({ concept }) => `${concept} について同僚への説明メールを書いてください。`,
    },
    {
      op: null,
      id: "oos-news-ja",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: ["place"],
      build: ({ place }) => `${place} で今日あったニュースを教えて。`,
    },
    {
      op: null,
      id: "oos-count-ja",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: [],
      build: () => `このグラフには CITES の辺が全部で何本ありますか？`,
    },
    // ---- document-text-search: full-text keyword search over Document bodies ----
    {
      op: "document-text-search",
      id: "text-search-en-contains",
      lang: "en",
      abstraction: "graph-vocabulary",
      needs: ["topic"],
      build: ({ topic }) =>
        `Which documents contain the word ${topic} in their body text?`,
    },
    {
      op: "document-text-search",
      id: "text-search-en-score",
      lang: "en",
      abstraction: "domain",
      needs: ["topic"],
      build: ({ topic }) =>
        `Rank the documents by text search score for the term ${topic}.`,
    },
    {
      op: "document-text-search",
      id: "text-search-en-find",
      lang: "en",
      abstraction: "business-intent",
      needs: ["topic"],
      build: ({ topic }) =>
        `Find documents whose body mentions ${topic}, best matches first.`,
    },
    {
      op: "document-text-search",
      id: "text-search-en-lexical",
      lang: "en",
      abstraction: "plain-language",
      needs: ["topic"],
      build: ({ topic }) =>
        `I want a keyword search over the document text for ${topic}.`,
    },
    {
      op: "document-text-search",
      id: "text-search-en-match",
      lang: "en",
      abstraction: "graph-vocabulary",
      needs: ["topic"],
      build: ({ topic }) =>
        `Which body texts match ${topic}? Show the matching documents ranked.`,
    },
    {
      op: "document-text-search",
      id: "text-search-ja-contain",
      lang: "ja",
      abstraction: "domain",
      needs: ["topic"],
      build: ({ topic }) =>
        `${topic} という語を含む文書を、本文検索で見つけてください。`,
    },
    {
      op: "document-text-search",
      id: "text-search-ja-rank",
      lang: "ja",
      abstraction: "business-intent",
      needs: ["topic"],
      build: ({ topic }) =>
        `${topic} に言及している文書をテキスト検索で探し、スコア順に表示して。`,
    },
    {
      op: "document-text-search",
      id: "text-search-ja-fulltext",
      lang: "ja",
      abstraction: "plain-language",
      needs: ["topic"],
      build: ({ topic }) =>
        `本文の全文検索で「${topic}」がヒットする文書を教えてください。`,
    },
    {
      op: "document-text-search",
      id: "text-search-ja-keyterm",
      lang: "ja",
      abstraction: "graph-vocabulary",
      needs: ["topic"],
      build: ({ topic }) =>
        `キーワード ${topic} で本文を検索し、関連する文書を返してください。`,
    },

    {
      op: "document-text-search",
      id: "text-search-en-keyword",
      lang: "en",
      abstraction: "graph-vocabulary",
      needs: ["topic"],
      build: ({ topic }) =>
        `Full text search: which documents mention ${topic} in their body?`,
    },
    {
      op: "document-text-search",
      id: "text-search-en-terms",
      lang: "en",
      abstraction: "business-intent",
      needs: ["topic"],
      build: ({ topic }) =>
        `Search the document bodies for the keyword ${topic} and rank the matches.`,
    },
    {
      op: "document-text-search",
      id: "text-search-ja-keyword",
      lang: "ja",
      abstraction: "graph-vocabulary",
      needs: ["topic"],
      build: ({ topic }) =>
        `${topic} を本文に含む文書を全文検索して、関連度順に並べてください。`,
    },
    {
      op: "document-text-search",
      id: "text-search-ja-mention",
      lang: "ja",
      abstraction: "domain",
      needs: ["topic"],
      build: ({ topic }) =>
        `本文中に「${topic}」という語が出てくる文書をキーワード検索してください。`,
    },
  ];
}

/**
 * Training-only templates.
 *
 * These families never enter dev/test; they exist to give fine-tuning more wording diversity,
 * especially for the weak citation and team slices. The operation labels match the prepared
 * queries, not the comment prose.
 */
function extraTrainingTemplates() {
  return [
    {
      op: "document-text-search",
      id: "xtrain-text-plain",
      lang: "en",
      abstraction: "plain-language",
      needs: ["topic"],
      build: ({ topic }) =>
        `Which documents talk about ${topic}? Search the text of their bodies.`,
    },
    {
      op: "document-text-search",
      id: "xtrain-text-ja-plain",
      lang: "ja",
      abstraction: "plain-language",
      needs: ["topic"],
      build: ({ topic }) =>
        `${topic} について書かれている文書を、本文のキーワードで探したい。`,
    },
    // ---- citation-reach ----
    {
      op: "citation-reach",
      id: "xtrain-cite-out",
      lang: "en",
      abstraction: "graph-vocabulary",
      needs: ["document", "depth"],
      build: ({ document, depth }) =>
        `Which documents are cited by ${document} through CITES up to depth ${depth}?`,
    },
    {
      op: "citation-reach",
      id: "xtrain-cite-in",
      lang: "en",
      abstraction: "domain",
      needs: ["document", "depth"],
      build: ({ document, depth }) =>
        `What does ${document} cite directly or indirectly through CITES within ${depth} links?`,
    },
    {
      op: "citation-reach",
      id: "xtrain-cite-ja-1",
      lang: "ja",
      abstraction: "graph-vocabulary",
      needs: ["document", "depth"],
      build: ({ document, depth }) =>
        `${document} から CITES を ${depth} 段までたどると、どの文書に到達しますか？`,
    },
    {
      op: "citation-reach",
      id: "xtrain-cite-ja-2",
      lang: "ja",
      abstraction: "domain",
      needs: ["document", "depth"],
      build: ({ document, depth }) =>
        `${document} の引用関係を CITES で深さ ${depth} まで追跡して`,
    },
    // ---- team-readable-documents ----
    {
      op: "team-readable-documents",
      id: "xtrain-team-en-1",
      lang: "en",
      abstraction: "business-intent",
      needs: ["topic"],
      build: ({ topic }) => `Which documents can the Platform team read that are about ${topic}?`,
    },
    {
      op: "team-readable-documents",
      id: "xtrain-team-en-2",
      lang: "en",
      abstraction: "domain",
      needs: ["topic"],
      build: ({ topic }) =>
        `Find readable Platform team documents similar to ${topic} using vector search.`,
    },
    {
      op: "team-readable-documents",
      id: "xtrain-team-ja-1",
      lang: "ja",
      abstraction: "business-intent",
      needs: ["topic"],
      build: ({ topic }) => `Platform チームが読める ${topic} に関する文書を類似度で探して`,
    },
    {
      op: "team-readable-documents",
      id: "xtrain-team-ja-2",
      lang: "ja",
      abstraction: "domain",
      needs: ["topic"],
      build: ({ topic }) => `Platform チームがアクセスできる、${topic} に近い内容の文書は？`,
    },
    // ---- variable-length-reach ----
    {
      op: "variable-length-reach",
      id: "xtrain-reach-en",
      lang: "en",
      abstraction: "graph-vocabulary",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `Which concepts can be reached from ${concept} through RELATED_TO in at most ${hops} hops?`,
    },
    {
      op: "variable-length-reach",
      id: "xtrain-reach-ja",
      lang: "ja",
      abstraction: "graph-vocabulary",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `${concept} から RELATED_TO で最大 ${hops} ホップで到達できる概念は？`,
    },
    // ---- variable-length-reach: plain-language coverage (observed in out-of-template eval) ----
    {
      op: "variable-length-reach",
      id: "xtrain-reach-near",
      lang: "en",
      abstraction: "domain",
      needs: ["concept"],
      build: ({ concept }) => `What topics are near ${concept} in the graph?`,
    },
    {
      op: "variable-length-reach",
      id: "xtrain-reach-connected",
      lang: "en",
      abstraction: "domain",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `Which concepts are connected to ${concept} within ${hops} hops?`,
    },
    {
      op: "variable-length-reach",
      id: "xtrain-reach-related",
      lang: "en",
      abstraction: "business-intent",
      needs: ["concept"],
      build: ({ concept }) => `Show me related topics around ${concept}.`,
    },
    {
      op: "variable-length-reach",
      id: "xtrain-reach-neighborhood",
      lang: "en",
      abstraction: "business-intent",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `What concepts are related to ${concept} through up to ${hops} relations?`,
    },
    {
      op: "variable-length-reach",
      id: "xtrain-reach-ja-near",
      lang: "ja",
      abstraction: "domain",
      needs: ["concept"],
      build: ({ concept }) => `${concept} に関連するテーマを教えて`,
    },
    {
      op: "variable-length-reach",
      id: "xtrain-reach-ja-connected",
      lang: "ja",
      abstraction: "domain",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `${concept} につながる概念を ${hops} ホップ以内で見たい`,
    },
    {
      op: "variable-length-reach",
      id: "xtrain-reach-ja-related",
      lang: "ja",
      abstraction: "business-intent",
      needs: ["concept"],
      build: ({ concept }) => `${concept} の周辺にある概念は？`,
    },
    {
      op: "variable-length-reach",
      id: "xtrain-reach-ja-links",
      lang: "ja",
      abstraction: "business-intent",
      needs: ["concept"],
      build: ({ concept }) => `${concept} にたどり着ける概念を関連リンクで探して`,
    },
    // ---- citation-reach: plain-language coverage ----
    {
      op: "citation-reach",
      id: "xtrain-cite-reference",
      lang: "en",
      abstraction: "domain",
      needs: ["document"],
      build: ({ document }) => `Which documents reference ${document}?`,
    },
    {
      op: "citation-reach",
      id: "xtrain-cite-ja-reference",
      lang: "ja",
      abstraction: "domain",
      needs: ["document"],
      build: ({ document }) => `${document} を参照している文書は？`,
    },
    // ---- shortest-path ----
    {
      op: "shortest-path",
      id: "xtrain-shortest-en",
      lang: "en",
      abstraction: "graph-vocabulary",
      needs: ["concept", "concept2"],
      build: ({ concept, concept2 }) =>
        `Find the shortest RELATED_TO path connecting ${concept} and ${concept2}.`,
    },
    {
      op: "shortest-path",
      id: "xtrain-shortest-ja",
      lang: "ja",
      abstraction: "graph-vocabulary",
      needs: ["concept", "concept2"],
      build: ({ concept, concept2 }) =>
        `${concept} と ${concept2} を結ぶ最短の RELATED_TO 経路を教えて`,
    },
    // ---- positive cross-family expansion (training only) ----
    {
      op: "variable-length-reach",
      id: "xtrain-reach-neighborhood",
      lang: "en",
      abstraction: "domain",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `Show the neighborhood of ${concept} out to ${hops} RELATED_TO edges.`,
    },
    {
      op: "variable-length-reach",
      id: "xtrain-reach-ja-neighborhood",
      lang: "ja",
      abstraction: "domain",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `${concept} の周辺概念を RELATED_TO で ${hops} ホップまで見せてください。`,
    },
    {
      op: "variable-length-reach",
      id: "xtrain-reach-en-cluster",
      lang: "en",
      abstraction: "business-intent",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `Which related concepts cluster around ${concept} within ${hops} hops?`,
    },
    {
      op: "variable-length-reach",
      id: "xtrain-reach-ja-cluster",
      lang: "ja",
      abstraction: "business-intent",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `${concept} を中心に ${hops} ホップ以内の関連概念をまとめてください。`,
    },
    {
      op: "citation-reach",
      id: "xtrain-citation-en-forward",
      lang: "en",
      abstraction: "domain",
      needs: ["document", "depth"],
      build: ({ document, depth }) =>
        `Follow the outgoing citations of ${document} up to ${depth} levels.`,
    },
    {
      op: "citation-reach",
      id: "xtrain-citation-ja-forward",
      lang: "ja",
      abstraction: "domain",
      needs: ["document", "depth"],
      build: ({ document, depth }) =>
        `${document} の引用元を CITES で ${depth} 階層までたどってください。`,
    },
    {
      op: "citation-reach",
      id: "xtrain-citation-en-cites",
      lang: "en",
      abstraction: "graph-vocabulary",
      needs: ["document", "depth"],
      build: ({ document, depth }) =>
        `Enumerate CITES targets reachable from ${document} within depth ${depth}.`,
    },
    {
      op: "citation-reach",
      id: "xtrain-citation-ja-cites",
      lang: "ja",
      abstraction: "graph-vocabulary",
      needs: ["document", "depth"],
      build: ({ document, depth }) =>
        `${document} から CITES で深さ ${depth} 以内に届く文献を列挙して。`,
    },
    {
      op: "shortest-path",
      id: "xtrain-shortest-en-route",
      lang: "en",
      abstraction: "domain",
      needs: ["concept", "concept2"],
      build: ({ concept, concept2 }) =>
        `What is the shortest RELATED_TO route from ${concept} to ${concept2}?`,
    },
    {
      op: "shortest-path",
      id: "xtrain-shortest-ja-route",
      lang: "ja",
      abstraction: "domain",
      needs: ["concept", "concept2"],
      build: ({ concept, concept2 }) =>
        `${concept} から ${concept2} への最短 RELATED_TO ルートは？`,
    },
    {
      op: "shortest-path",
      id: "xtrain-shortest-en-chain",
      lang: "en",
      abstraction: "plain-language",
      needs: ["concept", "concept2"],
      build: ({ concept, concept2 }) =>
        `How do I get from ${concept} to ${concept2} in the fewest RELATED_TO steps?`,
    },
    {
      op: "shortest-path",
      id: "xtrain-shortest-ja-chain",
      lang: "ja",
      abstraction: "plain-language",
      needs: ["concept", "concept2"],
      build: ({ concept, concept2 }) =>
        `${concept} から ${concept2} まで、RELATED_TO の最少ステップで行く道は？`,
    },
    {
      op: "team-readable-documents",
      id: "xtrain-team-en-permitted",
      lang: "en",
      abstraction: "domain",
      needs: ["topic"],
      build: ({ topic }) =>
        `Which readable documents cover ${topic} for the Platform team?`,
    },
    {
      op: "team-readable-documents",
      id: "xtrain-team-ja-permitted",
      lang: "ja",
      abstraction: "domain",
      needs: ["topic"],
      build: ({ topic }) =>
        `Platform チームが閲覧できる範囲で、${topic} を扱う文書を探してください。`,
    },
    {
      op: "team-readable-documents",
      id: "xtrain-team-en-access",
      lang: "en",
      abstraction: "business-intent",
      needs: ["topic"],
      build: ({ topic }) =>
        `Show ${topic} documents that respect the Platform access policy.`,
    },
    {
      op: "team-readable-documents",
      id: "xtrain-team-ja-access",
      lang: "ja",
      abstraction: "business-intent",
      needs: ["topic"],
      build: ({ topic }) =>
        `アクセス制御を守った上で、${topic} に関する文書を Platform チーム向けに出して。`,
    },
    {
      op: "document-text-search",
      id: "xtrain-text-en-body",
      lang: "en",
      abstraction: "domain",
      needs: ["topic"],
      build: ({ topic }) =>
        `Use body-text scoring to find documents about ${topic}.`,
    },
    {
      op: "document-text-search",
      id: "xtrain-text-ja-body",
      lang: "ja",
      abstraction: "domain",
      needs: ["topic"],
      build: ({ topic }) =>
        `本文スコアで ${topic} に関する文書を検索してください。`,
    },
    {
      op: "document-text-search",
      id: "xtrain-text-en-hit",
      lang: "en",
      abstraction: "business-intent",
      needs: ["topic"],
      build: ({ topic }) =>
        `Which document bodies score highest for the term ${topic}?`,
    },
    {
      op: "document-text-search",
      id: "xtrain-text-ja-hit",
      lang: "ja",
      abstraction: "business-intent",
      needs: ["topic"],
      build: ({ topic }) =>
        `「${topic}」の本文スコアが高い文書を上位から出してください。`,
    },
    // ---- natural phrasings (training only; not the hand-written eval questions) ----
    {
      op: "variable-length-reach",
      id: "xtrain-natural-reach-en",
      lang: "en",
      abstraction: "plain-language",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `What sits within ${hops} RELATED_TO hops of ${concept}?`,
    },
    {
      op: "variable-length-reach",
      id: "xtrain-natural-reach-ja",
      lang: "ja",
      abstraction: "plain-language",
      needs: ["concept"],
      build: ({ concept }) => `${concept} って、どこから繋がってるの？`,
    },
    {
      op: "citation-reach",
      id: "xtrain-natural-citation-en",
      lang: "en",
      abstraction: "plain-language",
      needs: ["document", "depth"],
      build: ({ document, depth }) =>
        `If I start at ${document}, which papers do I reach through ${depth} citation hops?`,
    },
    {
      op: "citation-reach",
      id: "xtrain-natural-citation-ja",
      lang: "ja",
      abstraction: "plain-language",
      needs: ["document"],
      build: ({ document }) => `${document} から引用をたどると何が出てくる？`,
    },
    {
      op: "shortest-path",
      id: "xtrain-natural-shortest-en",
      lang: "en",
      abstraction: "plain-language",
      needs: ["concept", "concept2"],
      build: ({ concept, concept2 }) =>
        `How do ${concept} and ${concept2} connect with the fewest steps?`,
    },
    {
      op: "shortest-path",
      id: "xtrain-natural-shortest-ja",
      lang: "ja",
      abstraction: "plain-language",
      needs: ["concept", "concept2"],
      build: ({ concept, concept2 }) =>
        `${concept} から ${concept2} までどう繋がるのが最短？`,
    },
    {
      op: "team-readable-documents",
      id: "xtrain-natural-team-en",
      lang: "en",
      abstraction: "plain-language",
      needs: ["topic"],
      build: ({ topic }) =>
        `Which documents about ${topic} can the Platform team actually read?`,
    },
    {
      op: "team-readable-documents",
      id: "xtrain-natural-team-ja",
      lang: "ja",
      abstraction: "plain-language",
      needs: ["topic"],
      build: ({ topic }) =>
        `Platform チームが読める範囲で ${topic} の文書ある？`,
    },
    {
      op: "document-text-search",
      id: "xtrain-natural-text-en",
      lang: "en",
      abstraction: "plain-language",
      needs: ["topic"],
      build: ({ topic }) =>
        `Do any document bodies mention ${topic}?`,
    },
    {
      op: "document-text-search",
      id: "xtrain-natural-text-ja",
      lang: "ja",
      abstraction: "plain-language",
      needs: ["topic"],
      build: ({ topic }) =>
        `本文に ${topic} が出てくる文書ある？`,
    },
    // ---- hard out-of-scope negatives that mention catalog entities or teams ----
    {
      op: null,
      id: "xtrain-hard-oos-permission",
      lang: "en",
      abstraction: "out-of-scope",
      needs: [],
      build: () => `Grant the Platform team admin access to the knowledge workspace.`,
    },
    {
      op: null,
      id: "xtrain-hard-oos-permission-ja",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: [],
      build: () => `Platform チームに管理者権限を付与してください。`,
    },
    {
      op: null,
      id: "xtrain-hard-oos-delete",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["document"],
      build: ({ document }) => `Delete ${document} and its citations.`,
    },
    {
      op: null,
      id: "xtrain-hard-oos-translate",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: ["document"],
      build: ({ document }) => `${document} をフランス語に翻訳して。`,
    },
    {
      op: null,
      id: "xtrain-hard-oos-add",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["document"],
      build: ({ document }) => `Add a new document called ${document} overview.`,
    },
    {
      op: null,
      id: "xtrain-hard-oos-count-docs",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: ["concept"],
      build: ({ concept }) => `${concept} に言及する文書の件数を数えて。`,
    },
    {
      op: null,
      id: "xtrain-hard-oos-count-edges",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["concept"],
      build: ({ concept }) => `Count the RELATED_TO edges involving ${concept}.`,
    },
    {
      op: null,
      id: "xtrain-hard-oos-disable",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: [],
      build: () => `Vector search を無効にしてください。`,
    },
    {
      op: null,
      id: "xtrain-hard-oos-owner",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["document"],
      build: ({ document }) => `Change the owner of ${document} to the Platform team.`,
    },
    {
      op: null,
      id: "xtrain-hard-oos-reindex",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: [],
      build: () => `Indexing を再構築して、既存の文書を全部入れ直して。`,
    },
    // ---- blind-failure categories (training only; wording deliberately different from blind set) ----
    {
      op: "citation-reach",
      id: "xtrain-cat-citation-trail",
      lang: "en",
      abstraction: "domain",
      needs: ["document", "depth"],
      build: ({ document, depth }) =>
        `Walk forward through the reference trail from ${document} for ${depth} levels.`,
    },
    {
      op: "citation-reach",
      id: "xtrain-cat-citation-network",
      lang: "en",
      abstraction: "business-intent",
      needs: ["document", "depth"],
      build: ({ document, depth }) =>
        `Expand the reference network out of ${document} up to ${depth} steps.`,
    },
    {
      op: "citation-reach",
      id: "xtrain-cat-citation-ja",
      lang: "ja",
      abstraction: "domain",
      needs: ["document", "depth"],
      build: ({ document, depth }) =>
        `${document} の参考文献ネットワークを深さ ${depth} まで展開して。`,
    },
    {
      op: "document-text-search",
      id: "xtrain-cat-text-lookthrough",
      lang: "en",
      abstraction: "plain-language",
      needs: ["topic"],
      build: ({ topic }) =>
        `Look through the document bodies for ${topic} and rank the matches.`,
    },
    {
      op: "document-text-search",
      id: "xtrain-cat-text-scan",
      lang: "en",
      abstraction: "graph-vocabulary",
      needs: ["topic"],
      build: ({ topic }) =>
        `Scan the body text of the documents for the term ${topic}.`,
    },
    {
      op: "document-text-search",
      id: "xtrain-cat-text-ja",
      lang: "ja",
      abstraction: "domain",
      needs: ["topic"],
      build: ({ topic }) =>
        `本文を走査して ${topic} に一致する文書を順位付けして。`,
    },
    {
      op: "variable-length-reach",
      id: "xtrain-cat-reach-feed",
      lang: "en",
      abstraction: "graph-vocabulary",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `Which concepts feed into ${concept} within ${hops} RELATED_TO hops?`,
    },
    {
      op: "variable-length-reach",
      id: "xtrain-cat-reach-ja-upstream",
      lang: "ja",
      abstraction: "domain",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `${concept} の上流にあたる概念を ${hops} ホップまで。`,
    },
    {
      op: "variable-length-reach",
      id: "xtrain-cat-reach-above",
      lang: "en",
      abstraction: "business-intent",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `List concepts above ${concept} in the RELATED_TO graph out to ${hops} hops.`,
    },
    {
      op: null,
      id: "xtrain-cat-oos-summarize",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["document"],
      build: ({ document }) => `Summarize ${document} in three bullet points.`,
    },
    {
      op: null,
      id: "xtrain-cat-oos-summarize-ja",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: ["document"],
      build: ({ document }) => `${document} を3行で要約して。`,
    },
    {
      op: null,
      id: "xtrain-cat-oos-export",
      lang: "en",
      abstraction: "out-of-scope",
      needs: [],
      build: () => `Export all readable documents to a PDF file.`,
    },
    {
      op: null,
      id: "xtrain-cat-oos-rename",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["document"],
      build: ({ document }) => `Rename ${document} to ${document} v2.`,
    },
    {
      op: null,
      id: "xtrain-cat-oos-owner",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["document"],
      build: ({ document }) => `Who owns the ${document} document?`,
    },
    {
      op: null,
      id: "xtrain-cat-oos-sql-ja",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: ["concept"],
      build: ({ concept }) => `${concept} に関する SQL を書いて。`,
    },
    // ---- remaining blind-category coverage (training only; no blind wording) ----
    {
      op: "document-text-search",
      id: "xtrain-cat2-text-token",
      lang: "en",
      abstraction: "domain",
      needs: ["topic"],
      build: ({ topic }) =>
        `Which document bodies contain the token ${topic}? Please rank them.`,
    },
    {
      op: "document-text-search",
      id: "xtrain-cat2-text-across",
      lang: "en",
      abstraction: "business-intent",
      needs: ["topic"],
      build: ({ topic }) =>
        `Search across the body text of the documents for mentions of ${topic}.`,
    },
    {
      op: "document-text-search",
      id: "xtrain-cat2-text-pass",
      lang: "en",
      abstraction: "graph-vocabulary",
      needs: ["topic"],
      build: ({ topic }) =>
        `Do a full-text pass over the document bodies and surface ${topic} matches.`,
    },
    {
      op: "document-text-search",
      id: "xtrain-cat2-text-relevance",
      lang: "en",
      abstraction: "plain-language",
      needs: ["topic"],
      build: ({ topic }) =>
        `Find documents whose body includes ${topic} and order by relevance.`,
    },
    {
      op: "document-text-search",
      id: "xtrain-cat2-text-ja-scan",
      lang: "ja",
      abstraction: "domain",
      needs: ["topic"],
      build: ({ topic }) =>
        `文書の本文を全文走査して、${topic} を含むものを順位付けしてください。`,
    },
    {
      op: "document-text-search",
      id: "xtrain-cat2-text-ja-phrase",
      lang: "ja",
      abstraction: "graph-vocabulary",
      needs: ["topic"],
      build: ({ topic }) =>
        `本文中に ${topic} という語句がある文書を検索。`,
    },
    {
      op: "document-text-search",
      id: "xtrain-cat2-text-ja-appear",
      lang: "ja",
      abstraction: "plain-language",
      needs: ["topic"],
      build: ({ topic }) =>
        `${topic} が本文に現れる文書を、テキスト一致で抽出して。`,
    },
    {
      op: "document-text-search",
      id: "xtrain-cat2-text-ja-exec",
      lang: "ja",
      abstraction: "business-intent",
      needs: ["topic"],
      build: ({ topic }) =>
        `本文を対象に ${topic} のキーワード検索を実行して。`,
    },
    {
      op: "variable-length-reach",
      id: "xtrain-cat2-reach-above",
      lang: "en",
      abstraction: "graph-vocabulary",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `Which concepts sit above ${concept} along RELATED_TO edges, within ${hops} hops?`,
    },
    {
      op: "variable-length-reach",
      id: "xtrain-cat2-reach-incoming",
      lang: "en",
      abstraction: "domain",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `Trace incoming RELATED_TO links into ${concept} for ${hops} hops and list the concepts.`,
    },
    {
      op: "variable-length-reach",
      id: "xtrain-cat2-reach-arrive",
      lang: "en",
      abstraction: "business-intent",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `What concepts can arrive at ${concept} over RELATED_TO in ${hops} steps?`,
    },
    {
      op: "variable-length-reach",
      id: "xtrain-cat2-reach-feed",
      lang: "en",
      abstraction: "plain-language",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `List the concepts that feed ${concept} through RELATED_TO, up to ${hops} hops.`,
    },
    {
      op: "variable-length-reach",
      id: "xtrain-cat2-reach-ja-flow",
      lang: "ja",
      abstraction: "domain",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `${concept} へ RELATED_TO で流入する概念を ${hops} ホップ分挙げて。`,
    },
    {
      op: "variable-length-reach",
      id: "xtrain-cat2-reach-ja-upstream",
      lang: "ja",
      abstraction: "graph-vocabulary",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `${concept} の上流方向に RELATED_TO を ${hops} ホップたどった概念は？`,
    },
    {
      op: "variable-length-reach",
      id: "xtrain-cat2-reach-ja-reverse",
      lang: "ja",
      abstraction: "plain-language",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `RELATED_TO を逆にたどって ${concept} に届く概念を ${hops} ホップ以内で。`,
    },
    {
      op: "variable-length-reach",
      id: "xtrain-cat2-reach-ja-gather",
      lang: "ja",
      abstraction: "business-intent",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `${concept} に集まる概念を ${hops} ホップまで RELATED_TO で列挙。`,
    },
    // ---- iteration-2 OOS categories (counts, edge writes, summaries, scoring) ----
    {
      op: null,
      id: "xtrain-oos-count-documents-en",
      lang: "en",
      abstraction: "out-of-scope",
      needs: [],
      build: () => `How many documents exist in the graph right now?`,
    },
    {
      op: null,
      id: "xtrain-oos-count-documents-ja",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: [],
      build: () => `このグラフには文書が全部で何件ありますか？`,
    },
    {
      op: null,
      id: "xtrain-oos-delete-edge",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["document"],
      build: ({ document }) => `Delete the CITES edge from ${document} to its target.`,
    },
    {
      op: null,
      id: "xtrain-oos-insert-edge-ja",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: ["concept"],
      build: ({ concept }) => `${concept} から他の概念へ RELATED_TO の辺を追加してください。`,
    },
    {
      op: null,
      id: "xtrain-oos-score-popularity",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["concept"],
      build: ({ concept }) => `Score how popular ${concept} is in the graph.`,
    },
    {
      op: null,
      id: "xtrain-oos-score-popularity-ja",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: ["concept"],
      build: ({ concept }) => `${concept} の人気度を数値化してください。`,
    },
    {
      op: null,
      id: "xtrain-oos-summarize-short",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["document"],
      build: ({ document }) => `Give me a two-sentence summary of ${document}.`,
    },
    // ---- iteration-2 text-search phrasings (reduce text-vs-citation confusion) ----
    {
      op: "document-text-search",
      id: "xtrain-text-rank-bodies",
      lang: "en",
      abstraction: "domain",
      needs: ["topic"],
      build: ({ topic }) => `Search the document bodies for ${topic} and rank the results.`,
    },
    {
      op: "document-text-search",
      id: "xtrain-text-term-bodies",
      lang: "en",
      abstraction: "graph-vocabulary",
      needs: ["topic"],
      build: ({ topic }) => `Text-search the bodies for the term ${topic}.`,
    },
    {
      op: "document-text-search",
      id: "xtrain-text-contains",
      lang: "en",
      abstraction: "plain-language",
      needs: ["topic"],
      build: ({ topic }) => `Which document bodies contain ${topic}?`,
    },
    {
      op: "document-text-search",
      id: "xtrain-text-ja-contain",
      lang: "ja",
      abstraction: "domain",
      needs: ["topic"],
      build: ({ topic }) => `本文に ${topic} を含む文書を検索して。`,
    },
    {
      op: "document-text-search",
      id: "xtrain-text-ja-appear",
      lang: "ja",
      abstraction: "graph-vocabulary",
      needs: ["topic"],
      build: ({ topic }) => `「${topic}」が本文に出てくる文書を検索。`,
    },
    {
      op: "document-text-search",
      id: "xtrain-text-ja-search",
      lang: "ja",
      abstraction: "plain-language",
      needs: ["topic"],
      build: ({ topic }) => `本文検索で ${topic} を探して。`,
    },
    // ---- iteration-3 text-search vocabulary and upstream reach (training only) ----
    {
      op: "document-text-search",
      id: "xtrain-term-0-en",
      lang: "en",
      abstraction: "domain",
      needs: [],
      build: () => `Search the document bodies for the term graph and rank the matches.`,
    },
    {
      op: "document-text-search",
      id: "xtrain-term-1-en",
      lang: "en",
      abstraction: "plain-language",
      needs: [],
      build: () => `Search the document bodies for the term traversal and rank the matches.`,
    },
    {
      op: "document-text-search",
      id: "xtrain-term-2-en",
      lang: "en",
      abstraction: "domain",
      needs: [],
      build: () => `Search the document bodies for the term indexes and rank the matches.`,
    },
    {
      op: "document-text-search",
      id: "xtrain-term-3-en",
      lang: "en",
      abstraction: "plain-language",
      needs: [],
      build: () => `Search the document bodies for the term nodes and rank the matches.`,
    },
    {
      op: "document-text-search",
      id: "xtrain-term-4-en",
      lang: "en",
      abstraction: "domain",
      needs: [],
      build: () => `Search the document bodies for the term language and rank the matches.`,
    },
    {
      op: "document-text-search",
      id: "xtrain-term-5-en",
      lang: "en",
      abstraction: "plain-language",
      needs: [],
      build: () => `Search the document bodies for the term similarity and rank the matches.`,
    },
    {
      op: "document-text-search",
      id: "xtrain-term-6-en",
      lang: "en",
      abstraction: "domain",
      needs: [],
      build: () => `Search the document bodies for the term context and rank the matches.`,
    },
    {
      op: "document-text-search",
      id: "xtrain-term-7-en",
      lang: "en",
      abstraction: "plain-language",
      needs: [],
      build: () => `Search the document bodies for the term adjacency and rank the matches.`,
    },
    {
      op: "document-text-search",
      id: "xtrain-term-8-en",
      lang: "en",
      abstraction: "domain",
      needs: [],
      build: () => `Search the document bodies for the term shards and rank the matches.`,
    },
    {
      op: "document-text-search",
      id: "xtrain-term-9-en",
      lang: "en",
      abstraction: "plain-language",
      needs: [],
      build: () => `Search the document bodies for the term embedding and rank the matches.`,
    },
    {
      op: "document-text-search",
      id: "xtrain-term-0-ja",
      lang: "ja",
      abstraction: "domain",
      needs: [],
      build: () => `本文に graph を含む文書を検索して、スコア順に並べてください。`,
    },
    {
      op: "document-text-search",
      id: "xtrain-term-1-ja",
      lang: "ja",
      abstraction: "business-intent",
      needs: [],
      build: () => `本文に traversal を含む文書を検索して、スコア順に並べてください。`,
    },
    {
      op: "document-text-search",
      id: "xtrain-term-2-ja",
      lang: "ja",
      abstraction: "domain",
      needs: [],
      build: () => `本文に indexes を含む文書を検索して、スコア順に並べてください。`,
    },
    {
      op: "document-text-search",
      id: "xtrain-term-3-ja",
      lang: "ja",
      abstraction: "business-intent",
      needs: [],
      build: () => `本文に nodes を含む文書を検索して、スコア順に並べてください。`,
    },
    {
      op: "document-text-search",
      id: "xtrain-term-4-ja",
      lang: "ja",
      abstraction: "domain",
      needs: [],
      build: () => `本文に language を含む文書を検索して、スコア順に並べてください。`,
    },
    {
      op: "document-text-search",
      id: "xtrain-term-5-ja",
      lang: "ja",
      abstraction: "business-intent",
      needs: [],
      build: () => `本文に similarity を含む文書を検索して、スコア順に並べてください。`,
    },
    {
      op: "document-text-search",
      id: "xtrain-term-6-ja",
      lang: "ja",
      abstraction: "domain",
      needs: [],
      build: () => `本文に context を含む文書を検索して、スコア順に並べてください。`,
    },
    {
      op: "document-text-search",
      id: "xtrain-term-7-ja",
      lang: "ja",
      abstraction: "business-intent",
      needs: [],
      build: () => `本文に adjacency を含む文書を検索して、スコア順に並べてください。`,
    },
    {
      op: "document-text-search",
      id: "xtrain-term-8-ja",
      lang: "ja",
      abstraction: "domain",
      needs: [],
      build: () => `本文に shards を含む文書を検索して、スコア順に並べてください。`,
    },
    {
      op: "document-text-search",
      id: "xtrain-term-9-ja",
      lang: "ja",
      abstraction: "business-intent",
      needs: [],
      build: () => `本文に embedding を含む文書を検索して、スコア順に並べてください。`,
    },    {
      op: "variable-length-reach",
      id: "xtrain-upstream-path",
      lang: "en",
      abstraction: "graph-vocabulary",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `Which concepts have a RELATED_TO link path into ${concept} within ${hops} hops?`,
    },
    {
      op: "variable-length-reach",
      id: "xtrain-upstream-ja",
      lang: "ja",
      abstraction: "graph-vocabulary",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `${concept} に流入する RELATED_TO 経路を ${hops} ホップ分たどって概念を列挙して。`,
    },
    {
      op: "variable-length-reach",
      id: "xtrain-upstream-en-arrive",
      lang: "en",
      abstraction: "business-intent",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `List concepts that arrive at ${concept} over at most ${hops} RELATED_TO edges.`,
    },
    {
      op: "variable-length-reach",
      id: "xtrain-upstream-ja-above",
      lang: "ja",
      abstraction: "plain-language",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `${concept} より上流の概念を RELATED_TO で ${hops} ホップまで見たい。`,
    },
    // ---- iteration-4 OOS categories (aggregates, edge writes, permissions, summaries) ----
    {
      op: null,
      id: "xtrain-oos-count-documents2",
      lang: "en",
      abstraction: "out-of-scope",
      needs: [],
      build: () => `What is the total number of documents stored in this graph?`,
    },
    {
      op: null,
      id: "xtrain-oos-count-documents2-ja",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: [],
      build: () => `グラフ内の文書の総数を教えてください。`,
    },
    {
      op: null,
      id: "xtrain-oos-count-edges2-ja",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: [],
      build: () => `このグラフの辺の総数は何本ですか？`,
    },
    {
      op: null,
      id: "xtrain-oos-delete-edge2",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["concept"],
      build: ({ concept }) => `Remove the RELATED_TO edge that touches ${concept}.`,
    },
    {
      op: null,
      id: "xtrain-oos-permission-revoke",
      lang: "en",
      abstraction: "out-of-scope",
      needs: [],
      build: () => `Revoke the Platform team's read permission on the knowledge graph.`,
    },
    {
      op: null,
      id: "xtrain-oos-permission-revoke-ja",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: [],
      build: () => `Platform チームの読み取り権限を取り消してください。`,
    },
    {
      op: null,
      id: "xtrain-oos-translate2",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["document"],
      build: ({ document }) => `Translate the abstract of ${document} into Spanish.`,
    },
    {
      op: null,
      id: "xtrain-oos-summarize2-ja",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: ["document"],
      build: ({ document }) => `${document} の要点を箇条書きでまとめてください。`,
    },
    // ---- iteration-5 operation-contrast hard negatives (catalog cue + unsupported verb) ----
    {
      op: null,
      id: "xtrain-contrast-count-citations-ja",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: [],
      build: () => `The GQL standard を引用している文書は何件ありますか？`,
    },
    {
      op: null,
      id: "xtrain-contrast-count-citations",
      lang: "en",
      abstraction: "out-of-scope",
      needs: [],
      build: () => `How many documents cite The GQL standard? Give me only the count.`,
    },
    {
      op: null,
      id: "xtrain-contrast-count-hops",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["concept"],
      build: ({ concept }) => `How many RELATED_TO hops separate ${concept} from Graph databases?`,
    },
    {
      op: null,
      id: "xtrain-contrast-count-team-docs",
      lang: "en",
      abstraction: "out-of-scope",
      needs: [],
      build: () => `Count the documents the Platform team is allowed to read.`,
    },
    {
      op: null,
      id: "xtrain-contrast-permission-add",
      lang: "en",
      abstraction: "out-of-scope",
      needs: [],
      build: () => `Add read permission for the Platform team to every document.`,
    },
    {
      op: null,
      id: "xtrain-contrast-permission-remove-ja",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: [],
      build: () => `Platform チームから文書の閲覧権限を外してください。`,
    },
    {
      op: null,
      id: "xtrain-contrast-export-team-docs",
      lang: "en",
      abstraction: "out-of-scope",
      needs: [],
      build: () => `Export the documents readable by the Platform team as a PDF.`,
    },
    {
      op: null,
      id: "xtrain-contrast-export-citation",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["document"],
      build: ({ document }) => `Export every document reachable from ${document} through CITES to CSV.`,
    },
    {
      op: null,
      id: "xtrain-contrast-summarize-cited-ja",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: ["document"],
      build: ({ document }) => `${document} から引用でたどれる文書を要約してください。`,
    },
    {
      op: null,
      id: "xtrain-contrast-delete-cited",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["document"],
      build: ({ document }) => `Delete the citations of ${document} from the graph.`,
    },
    {
      op: null,
      id: "xtrain-contrast-translate-team",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: [],
      build: () => `Platform チームが読める文書を日本語に翻訳してください。`,
    },
    {
      op: null,
      id: "xtrain-contrast-reach-count-ja",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: ["concept"],
      build: ({ concept }) => `${concept} に到達する概念の総数を数えて。`,
    },
    {
      op: null,
      id: "xtrain-contrast-team-score",
      lang: "en",
      abstraction: "out-of-scope",
      needs: [],
      build: () => `Score the Platform team's documents by popularity and rank them.`,
    },
    {
      op: null,
      id: "xtrain-contrast-shortest-count-ja",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: ["concept", "concept2"],
      build: ({ concept, concept2 }) => `${concept} と ${concept2} の最短経路の長さを数値で教えて。`,
    },
    // ---- iteration-6 text-intent contrast pairs (training only) ----
    {
      op: "document-text-search",
      id: "xtrain-contrast-text-mention",
      lang: "en",
      abstraction: "graph-vocabulary",
      needs: ["topic"],
      build: ({ topic }) => `Which documents mention ${topic} in the body text?`,
    },
    {
      op: "document-text-search",
      id: "xtrain-contrast-text-contains",
      lang: "en",
      abstraction: "plain-language",
      needs: ["topic"],
      build: ({ topic }) => `Find documents whose body contains ${topic}.`,
    },
    {
      op: "document-text-search",
      id: "xtrain-contrast-text-inside",
      lang: "en",
      abstraction: "domain",
      needs: ["topic"],
      build: ({ topic }) => `Search inside the document bodies for ${topic}.`,
    },
    {
      op: "document-text-search",
      id: "xtrain-contrast-text-ja-contain",
      lang: "ja",
      abstraction: "domain",
      needs: ["topic"],
      build: ({ topic }) => `本文中に ${topic} を含む文書を挙げてください。`,
    },
    {
      op: "document-text-search",
      id: "xtrain-contrast-text-ja-keyword",
      lang: "ja",
      abstraction: "business-intent",
      needs: ["topic"],
      build: ({ topic }) =>
        `本文をキーワード検索して ${topic} に触れている文書を探して。`,
    },
    {
      op: "team-readable-documents",
      id: "xtrain-contrast-team-readable",
      lang: "en",
      abstraction: "domain",
      needs: ["topic"],
      build: ({ topic }) => `Which documents about ${topic} are readable by the Platform team?`,
    },
    {
      op: "team-readable-documents",
      id: "xtrain-contrast-team-ja-readable",
      lang: "ja",
      abstraction: "domain",
      needs: ["topic"],
      build: ({ topic }) => `Platform チームが閲覧できる ${topic} に関する文書は？`,
    },
    {
      op: "team-readable-documents",
      id: "xtrain-contrast-team-permission",
      lang: "en",
      abstraction: "business-intent",
      needs: ["topic"],
      build: ({ topic }) =>
        `With Platform access, find semantically similar documents about ${topic}.`,
    },
    {
      op: "citation-reach",
      id: "xtrain-contrast-citation-direct",
      lang: "en",
      abstraction: "graph-vocabulary",
      needs: ["document"],
      build: ({ document }) => `Which documents does ${document} cite directly?`,
    },
    {
      op: "citation-reach",
      id: "xtrain-contrast-citation-ja-direct",
      lang: "ja",
      abstraction: "graph-vocabulary",
      needs: ["document"],
      build: ({ document }) => `${document} が直接引用している文書は？`,
    },
    {
      op: "variable-length-reach",
      id: "xtrain-contrast-reach-ja",
      lang: "ja",
      abstraction: "graph-vocabulary",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `${concept} に ${hops} ホップで到達する概念は？`,
    },
    {
      op: "variable-length-reach",
      id: "xtrain-contrast-reach-en",
      lang: "en",
      abstraction: "graph-vocabulary",
      needs: ["concept", "hops"],
      build: ({ concept, hops }) =>
        `Which concepts reach ${concept} within ${hops} RELATED_TO hops?`,
    },
    {
      op: null,
      id: "xtrain-contrast-count-mentions",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["topic"],
      build: ({ topic }) => `Count the documents that mention ${topic}.`,
    },
    {
      op: null,
      id: "xtrain-contrast-count-mentions-ja",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: ["topic"],
      build: ({ topic }) => `本文に ${topic} を含む文書は何件ありますか？`,
    },
    {
      op: null,
      id: "xtrain-contrast-team-owner",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["topic"],
      build: ({ topic }) => `Which team owns the documents about ${topic}?`,
    },
    {
      op: null,
      id: "xtrain-contrast-export-topic-ja",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: ["topic"],
      build: ({ topic }) => `${topic} に関する文書の一覧を CSV で出力して。`,
    },
    // ---- out of scope ----
    {
      op: null,
      id: "xtrain-oos-capital",
      lang: "en",
      abstraction: "out-of-scope",
      needs: ["place"],
      build: ({ place }) => `What is the capital of ${place}?`,
    },
    {
      op: null,
      id: "xtrain-oos-capital-ja",
      lang: "ja",
      abstraction: "out-of-scope",
      needs: ["place"],
      build: ({ place }) => `${place} の首都はどこ？`,
    },
  ];
}

/** Wider surface variants used only by {@link generateTrainingSet}. */
const TRAIN_SURFACE_PREFIXES = {
  en: [
    "",
    "Please answer: ",
    "Could you tell me: ",
    "I want to know: ",
    "Quick question: ",
    "Help me with this: ",
    "Question: ",
    "Tell me this: ",
    "I need to know: ",
    "Answer this: ",
  ],
  ja: [
    "",
    "お願いします。",
    "教えてください: ",
    "知りたいのは: ",
    "質問です: ",
    "次を教えて: ",
    "確認したい: ",
    "回答してください: ",
    "知りたいこと: ",
    "質問: ",
  ],
};
const TRAIN_SURFACE_SUFFIXES = {
  en: ["?", "? Return the rows.", "? Keep it short.", "? List them."],
  ja: ["？", "？ 結果だけ返してください。", "？ 簡潔に。", "？ 一覧で。"],
};

/** Deterministic entity and family splits. */
function splitEntities(entities, random) {
  // Train and dev see the shared entities; test sees the shared set plus a held-out quarter, so
  // every test slice covers both seen and unseen entities while nothing test-only leaks into
  // training.
  const splits = {};
  for (const [key, values] of Object.entries(entities)) {
    const shuffled = shuffle(values, random);
    const holdoutCount = Math.max(1, Math.round(shuffled.length * 0.25));
    const holdout = shuffled.slice(0, holdoutCount);
    const shared = shuffled.slice(holdoutCount);
    splits[key] = {
      train: shared,
      dev: shared,
      test: [...shared, ...holdout],
    };
  }
  return splits;
}

function splitFamilies(families, random) {
  const splits = { train: [], dev: [], test: [] };
  const groups = new Map();
  for (const family of families) {
    const key = family.op ?? "out-of-scope";
    if (!groups.has(key)) {
      groups.set(key, []);
    }
    groups.get(key).push(family);
  }
  for (const group of groups.values()) {
    const shuffled = shuffle(group, random);
    const total = shuffled.length;
    if (total < 3) {
      throw new Error(
        `operation group needs at least three templates to keep train/dev/test families distinct`,
      );
    }
    const testCount = Math.max(1, Math.floor(total * 0.25));
    const devCount = Math.max(1, Math.floor(total * 0.2));
    const trainCount = total - testCount - devCount;
    if (trainCount < 1) {
      throw new Error(`operation group needs more templates to keep train families: ${total}`);
    }
    splits.train.push(...shuffled.slice(0, trainCount));
    splits.dev.push(...shuffled.slice(trainCount, trainCount + devCount));
    splits.test.push(...shuffled.slice(trainCount + devCount));
  }
  return splits;
}

const SURFACE_PREFIXES = {
  en: ["", "Please answer: ", "Could you tell me: ", "I want to know: "],
  ja: ["", "お願いします。", "教えてください: ", "知りたいのは: "],
};
const SURFACE_SUFFIXES = {
  en: ["?", "? Return the rows.", "? Keep it short."],
  ja: ["？", "？ 結果だけ返してください。", "？ 簡潔に。"],
};

function applySurface(question, lang, random, surfaces = null) {
  const prefixes = surfaces?.prefixes[lang] ?? SURFACE_PREFIXES[lang] ?? SURFACE_PREFIXES.en;
  const suffixes = surfaces?.suffixes[lang] ?? SURFACE_SUFFIXES[lang] ?? SURFACE_SUFFIXES.en;
  const prefix = prefixes[Math.floor(random() * prefixes.length)];
  const suffix = suffixes[Math.floor(random() * suffixes.length)];
  const body = question.replace(/[.。!?！？]+\s*$/, "");
  return `${prefix}${body}${suffix}`;
}

function pickEntity(splitEntitiesForSplit, key, random) {
  const values = splitEntitiesForSplit[key];
  if (!values || values.length === 0) {
    throw new Error(`no entity available for ${key}`);
  }
  return values[Math.floor(random() * values.length)];
}

function pickPair(splitEntitiesForSplit, allConcepts, random) {
  const local = splitEntitiesForSplit.concept ?? [];
  const seen = local.length > 0 ? local : allConcepts;
  const first = seen[Math.floor(random() * seen.length)];
  const others = allConcepts.filter((concept) => concept !== first);
  if (others.length === 0) {
    return null;
  }
  return [first, others[Math.floor(random() * others.length)]];
}

function materialise(template, splitEntitiesForSplit, allConcepts, random) {
  const values = {};
  for (const key of template.needs) {
    if (key === "concept2") {
      continue;
    }
    if (key === "concept") {
      values.concept = pickEntity(splitEntitiesForSplit, "concept", random);
    } else if (key === "document") {
      values.document = pickEntity(splitEntitiesForSplit, "document", random);
    } else if (key === "topic") {
      const pool = [
        ...(splitEntitiesForSplit.concept ?? []),
        ...(splitEntitiesForSplit.document ?? []),
      ];
      values.topic = pool[Math.floor(random() * pool.length)];
    } else if (key === "depth") {
      values.depth = 1 + Math.floor(random() * 3);
    } else if (key === "hops") {
      values.hops = 2 + Math.floor(random() * 2);
    } else if (key === "place") {
      values.place = PLACES[Math.floor(random() * PLACES.length)];
    } else if (key === "item") {
      values.item = ITEMS[Math.floor(random() * ITEMS.length)];
    } else if (key === "role") {
      values.role = ROLES[Math.floor(random() * ROLES.length)];
    } else {
      values[key] = pickEntity(splitEntitiesForSplit, key, random);
    }
  }
  if (template.needs.includes("concept2")) {
    const pair = pickPair(splitEntitiesForSplit, allConcepts, random);
    if (!pair) {
      return null;
    }
    values.concept = pair[0];
    values.concept2 = pair[1];
  }
  return values;
}

function buildItem(template, splitEntitiesForSplit, allConcepts, random, surfaces = null) {
  const values = materialise(template, splitEntitiesForSplit, allConcepts, random);
  if (values === null) {
    return null;
  }
  const rendered = template.build(values);
  if (rendered.includes("undefined") || rendered.includes("NaN")) {
    throw new Error(`template ${template.id} rendered an unfilled value: ${rendered}`);
  }
  const question = applySurface(rendered, template.lang, random, surfaces);
  const slots = {};
  if (template.op === "shortest-path") {
    slots.from = values.concept;
    slots.to = values.concept2;
  } else if (values.concept) {
    slots.anchor = values.concept;
  }
  if (values.document) {
    slots.anchor = values.document;
  }
  if (values.topic) {
    slots.topic = values.topic;
  }
  if (values.depth) {
    slots.depth = values.depth;
  }
  if (values.hops) {
    slots.hops = values.hops;
  }
  if (template.op === "team-readable-documents") {
    slots.team = "Platform";
  }
  const item = {
    question,
    lang: template.lang,
    family: `${template.op ?? "out-of-scope"}/${template.id}`,
    abstraction: template.abstraction,
    slots,
  };
  if (template.op === null) {
    item.expect_abstain = true;
  } else {
    item.expected_op = template.op;
  }
  return item;
}

function targetFor(splitCount, ratio) {
  const positives = Math.round(splitCount * (1 - ratio));
  return { positives, outOfScope: splitCount - positives };
}

/**
 * Generate the dataset in memory. Pure: the same arguments produce the same items.
 */
export function generateRoutingGolden(options = {}) {
  const config = { ...DEFAULTS, ...options };
  const random = rng(config.seed);
  const entities = readEntities();
  const prepared = readPreparedOperations();
  const bank = templates();

  for (const template of bank) {
    if (template.op !== null && !prepared.includes(template.op)) {
      throw new Error(`template ${template.id} references unknown prepared op ${template.op}`);
    }
  }

  const allConcepts = entities.concepts;
  const entitySplits = splitEntities(entities, random);
  const familySplits = splitFamilies(bank, random);
  const sizes = { train: config.train, dev: config.dev, test: config.test };
  const result = { prepared, splits: {}, families: {}, entities: {} };

  for (const split of ["train", "dev", "test"]) {
    const target = targetFor(sizes[split], config.outOfScopeRatio);
    const positiveOps = prepared;
    const perOp = Math.floor(target.positives / positiveOps.length);
    const items = [];
    const seen = new Set();

    const emit = (template) => {
      const item = buildItem(template, splitEntitiesForSplit, allConcepts, random);
      if (item === null || seen.has(item.question)) {
        return false;
      }
      seen.add(item.question);
      items.push(item);
      return true;
    };

    const splitEntitiesForSplit = {
      concept: entitySplits.concepts[split],
      document: entitySplits.documents[split],
      topic: undefined,
    };
    const families = familySplits[split];
    const positiveFamilies = families.filter((family) => family.op !== null);
    const negativeFamilies = families.filter((family) => family.op === null);

    // Positives: round-robin the split's families, capping each operation at its quota.
    const counts = new Map(positiveOps.map((op) => [op, 0]));
    let guard = 0;
    while ([...counts.values()].some((count) => count < perOp) && guard < 10000) {
      guard += 1;
      let progressed = false;
      for (const family of positiveFamilies) {
        if ((counts.get(family.op) ?? 0) >= perOp) {
          continue;
        }
        if (emit(family)) {
          counts.set(family.op, (counts.get(family.op) ?? 0) + 1);
          progressed = true;
        }
      }
      if (!progressed) {
        break;
      }
    }

    // Out-of-scope: fill up to the target ratio.
    let negativeCount = 0;
    guard = 0;
    while (negativeCount < target.outOfScope && guard < 10000) {
      guard += 1;
      let progressed = false;
      for (const family of negativeFamilies) {
        if (negativeCount >= target.outOfScope) {
          break;
        }
        if (emit(family)) {
          negativeCount += 1;
          progressed = true;
        }
      }
      if (!progressed) {
        break;
      }
    }

    result.splits[split] = items;
    result.families[split] = [...new Set(items.map((item) => item.family))].sort();
    result.entities[split] = splitEntitiesForSplit;
  }

  return result;
}

/**
 * Generate a larger training-only split for fine-tuning.
 *
 * The canonical dev/test splits are untouched: extra families never enter them, and the base
 * family/entity split uses the same seed as {@link generateRoutingGolden}, so training rows stay
 * disjoint from the evaluation rows.
 */
export function generateTrainingSet(options = {}) {
  const config = {
    train: 3000,
    // Training-only negatives are raised above the canonical 20%: the audit showed overconfident
    // positives leaking hard out-of-scope questions, so the checkpoint needs a stronger
    // abstain signal. Canonical dev/test keep their documented 20% shape.
    outOfScopeRatio: 0.35,
    seed: 20261110,
    familySeed: DEFAULTS.seed,
    ...options,
  };
  const random = rng(config.seed);
  const entities = readEntities();
  const prepared = readPreparedOperations();
  const bank = templates();
  const extras = extraTrainingTemplates();
  for (const template of [...bank, ...extras]) {
    if (template.op !== null && !prepared.includes(template.op)) {
      throw new Error(`template ${template.id} references unknown prepared op ${template.op}`);
    }
  }

  const allConcepts = entities.concepts;
  // One RNG in the same consumption order as `generateRoutingGolden`, so the base families and
  // entities assigned to train are exactly the ones the canonical splits treat as training data.
  const splitRandom = rng(config.familySeed);
  const entitySplits = splitEntities(entities, splitRandom);
  const familySplits = splitFamilies(bank, splitRandom);
  const families = [...familySplits.train, ...extras];
  const splitEntitiesForSplit = {
    concept: entitySplits.concepts.train,
    document: entitySplits.documents.train,
    topic: undefined,
  };
  const surfaces = { prefixes: TRAIN_SURFACE_PREFIXES, suffixes: TRAIN_SURFACE_SUFFIXES };
  const target = targetFor(config.train, config.outOfScopeRatio);
  const perOp = Math.floor(target.positives / prepared.length);
  const items = [];
  const seen = new Set();

  const emit = (template) => {
    const item = buildItem(template, splitEntitiesForSplit, allConcepts, random, surfaces);
    if (item === null || seen.has(item.question)) {
      return false;
    }
    seen.add(item.question);
    items.push(item);
    return true;
  };

  const positiveFamilies = families.filter((family) => family.op !== null);
  const negativeFamilies = families.filter((family) => family.op === null);
  const counts = new Map(prepared.map((op) => [op, 0]));
  let guard = 0;
  while ([...counts.values()].some((count) => count < perOp) && guard < 200000) {
    guard += 1;
    let progressed = false;
    for (const family of positiveFamilies) {
      if ((counts.get(family.op) ?? 0) >= perOp) {
        continue;
      }
      if (emit(family)) {
        counts.set(family.op, (counts.get(family.op) ?? 0) + 1);
        progressed = true;
      }
    }
    if (!progressed) {
      break;
    }
  }
  let negativeCount = 0;
  guard = 0;
  while (negativeCount < target.outOfScope && guard < 200000) {
    guard += 1;
    let progressed = false;
    for (const family of negativeFamilies) {
      if (negativeCount >= target.outOfScope) {
        break;
      }
      if (emit(family)) {
        negativeCount += 1;
        progressed = true;
      }
    }
    if (!progressed) {
      break;
    }
  }
  if (items.length < config.train) {
    throw new Error(
      `training set reached only ${items.length}/${config.train} items; add entity pools or surfaces`,
    );
  }
  return { items, families: [...new Set(items.map((item) => item.family))].sort() };
}

/** Normalise the split arrays into the on-disk JSONL shape (stable ordering). */
export function serialiseSplit(items, options = {}) {
  return items
    .map((item) =>
      JSON.stringify({
        question: item.question,
        ...(item.expected_op === undefined ? {} : { expected_op: item.expected_op }),
        ...(item.expect_abstain ? { expect_abstain: true } : {}),
        lang: item.lang,
        family: item.family,
        abstraction: item.abstraction,
        ...(options.slots === true ? { slots: item.slots } : {}),
      }),
    )
    .join("\n")
    .concat("\n");
}

export function writeDataset(result, outputDir = OUTPUT_DIR) {
  mkdirSync(outputDir, { recursive: true });
  for (const split of ["train", "dev", "test"]) {
    writeFileSync(join(outputDir, `${split}.jsonl`), serialiseSplit(result.splits[split]));
  }
  return outputDir;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const trainingIndex = process.argv.indexOf("--training-only");
  if (trainingIndex >= 0) {
    const count = Number.parseInt(process.argv[trainingIndex + 1] ?? "3000", 10);
    const outputIndex = process.argv.indexOf("--output");
    const output =
    outputIndex >= 0
      ? process.argv[outputIndex + 1]
      : `${process.env.GLEV_MODEL_DIR ?? "../../target/glev-model"}/train-large.jsonl`;
    mkdirSync(dirname(output), { recursive: true });
    const result = generateTrainingSet({ train: count });
    writeFileSync(output, serialiseSplit(result.items));
    if (process.argv.includes("--slots")) {
      const slotOutput = output.replace(/\.jsonl$/, "-slots.jsonl");
      writeFileSync(slotOutput, serialiseSplit(result.items, { slots: true }));
      console.log(`wrote ${slotOutput} (slot-labelled)`);
    }
    const positives = result.items.filter((item) => item.expected_op).length;
    console.log(
      `wrote ${output}: ${result.items.length} (${positives} positive, ` +
        `${result.items.length - positives} abstain, ${result.families.length} families)`,
    );
    process.exit(0);
  }
  const result = generateRoutingGolden();
  const slotsIndex = process.argv.indexOf("--slots");
  if (slotsIndex >= 0) {
    const outputIndex = process.argv.indexOf("--output-dir");
    const directory = outputIndex >= 0 ? process.argv[outputIndex + 1] : join(OUTPUT_DIR, "slots");
    mkdirSync(directory, { recursive: true });
    for (const split of ["train", "dev", "test"]) {
      writeFileSync(join(directory, `${split}.jsonl`), serialiseSplit(result.splits[split], { slots: true }));
    }
    console.log(`wrote ${directory} (slot-labelled)`);
    process.exit(0);
  }
  const directory = writeDataset(result);
  const summary = ["train", "dev", "test"].map((split) => {
    const items = result.splits[split];
    const positives = items.filter((item) => item.expected_op).length;
    return `${split}: ${items.length} (${positives} positive, ${items.length - positives} abstain)`;
  });
  console.log(`wrote ${directory}`);
  console.log(summary.join("\n"));
}
