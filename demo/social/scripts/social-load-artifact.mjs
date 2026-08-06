const I64_MIN = -(1n << 63n);
const I64_MAX = (1n << 63n) - 1n;

function isLeapYear(year) {
  return year % 4 === 0 && (year % 100 !== 0 || year % 400 === 0);
}

function daysInMonth(year, month) {
  return [
    31,
    isLeapYear(year) ? 29 : 28,
    31,
    30,
    31,
    30,
    31,
    31,
    30,
    31,
    30,
    31,
  ][month - 1];
}

// Howard Hinnant's proleptic-Gregorian civil-date algorithm, with Unix epoch offset.
function daysFromCivil(year, month, day) {
  const adjustedYear = year - (month <= 2 ? 1 : 0);
  const era = Math.floor(adjustedYear / 400);
  const yearOfEra = adjustedYear - era * 400;
  const adjustedMonth = month + (month > 2 ? -3 : 9);
  const dayOfYear = Math.floor((153 * adjustedMonth + 2) / 5) + day - 1;
  const dayOfEra =
    yearOfEra * 365 +
    Math.floor(yearOfEra / 4) -
    Math.floor(yearOfEra / 100) +
    dayOfYear;
  return BigInt(era * 146097 + dayOfEra - 719468);
}

/** Normalize an authored YYYYMMDDHHmm payload to the canonical UTC DateTime value. */
export function normalizeCreatedAt(payload) {
  if (typeof payload === "number" && !Number.isSafeInteger(payload)) {
    throw new Error("created_at numeric payload must be a safe integer");
  }
  if (
    typeof payload !== "string" &&
    typeof payload !== "number" &&
    typeof payload !== "bigint"
  ) {
    throw new Error("created_at must be a string or integer");
  }
  const text = String(payload);
  if (!/^[0-9]{12}$/.test(text)) {
    throw new Error(
      "created_at must contain exactly 12 ASCII digits (YYYYMMDDHHmm)",
    );
  }

  const year = Number(text.slice(0, 4));
  const month = Number(text.slice(4, 6));
  const day = Number(text.slice(6, 8));
  const hour = Number(text.slice(8, 10));
  const minute = Number(text.slice(10, 12));
  if (year < 1 || year > 9999)
    throw new Error("created_at year must be in 0001..=9999");
  if (month < 1 || month > 12) throw new Error("created_at month is invalid");
  if (day < 1 || day > daysInMonth(year, month)) {
    throw new Error("created_at calendar day is invalid");
  }
  // The digit grammar already constrains each overflow-capable field to 00..=99.
  if (hour < 0 || hour > 99 || minute < 0 || minute > 99) {
    throw new Error("created_at hour and minute must each be in 00..=99");
  }

  const seconds =
    daysFromCivil(year, month, day) * 86_400n +
    BigInt(hour * 60 + minute) * 60n;
  if (seconds < I64_MIN || seconds > I64_MAX) {
    throw new Error("created_at is outside the canonical DateTime i64 range");
  }
  return { DateTime: { seconds: Number(seconds), nanos: 0 } };
}

export function buildSocialLoadArtifact({
  graph,
  demoId,
  demoGraph = "social",
}) {
  const vertices = graph.nodes.map((node) => {
    const properties = {
      ...(node.gqlLabel === "User"
        ? { user_id: { Text: node.id } }
        : { demo_id: { Int64: Number(demoId(node.id)) } }),
      demo_graph: { Text: demoGraph },
      [node.property]: { Text: node.label },
    };
    if (node.kind === "post") {
      properties.created_at = normalizeCreatedAt(node.createdAt);
      properties.is_public = { Bool: Boolean(node.isPublic) };
    }
    return {
      source_id: node.id,
      labels: [node.gqlLabel],
      properties,
    };
  });

  const sourceIds = new Set(vertices.map(({ source_id }) => source_id));
  if (sourceIds.size !== vertices.length)
    throw new Error("social-load source_id values must be unique");

  const edges = graph.edges.map((edge) => {
    if (!sourceIds.has(edge.source) || !sourceIds.has(edge.target)) {
      throw new Error(`social-load edge ${edge.id} has an unknown endpoint`);
    }
    return {
      source: edge.source,
      target: edge.target,
      directed: true,
      label: edge.gqlLabel,
      inline_value: null,
      properties: {
        demo_edge_id: { Text: edge.id },
        demo_kind: { Text: edge.displayLabel },
      },
    };
  });

  // Post embeddings remain authored in the canonical manifest (social-graph.json) for the
  // future vector work; the Gleaph CLI load artifact carries vertices and edges only.
  const embeddings = {};
  for (const node of graph.nodes) {
    if (node.embedding) {
      if (node.kind !== "post")
        throw new Error(`Non-Post node ${node.id} has embedding`);
      embeddings[node.id] = node.embedding;
    }
  }
  const hasPostEmbeddings = Object.keys(embeddings).length > 0;
  if (hasPostEmbeddings) {
    for (const node of graph.nodes) {
      if (node.kind === "post" && !node.embedding) {
        throw new Error(`Post node ${node.id} is missing embedding`);
      }
    }
  }

  // `gleaph load` artifact rows: vertices (source_id/labels/properties) and edges
  // (source/target endpoints, label, directed). The CLI reads these as an NDJSON row
  // stream (one row per line), so build-config.mjs emits them as `vertices.jsonl` +
  // `edges.jsonl` instead of a single JSON document.
  return { format_version: 1, vertices, edges };
}
