import { GleaphSdkError } from "./errors.ts";
import type {
  PreparedManifest,
  PreparedManifestColumn,
  PreparedManifestParameter,
  PreparedManifestRecordField,
  PreparedSemanticType,
} from "./types.ts";

/**
 * Raw IDL-decoded shapes of the prepared manifest.
 *
 * The Router's Candid interface encodes manifest types as Candid variants and
 * `opt` fields: `semantic_type` arrives as a variant object (`{ Text: null }`,
 * `{ List: { element: ... } }`, ...), `kind` as `{ Query: null } | { Update:
 * null }`, and optional strings as `[] | [string]`. These interfaces mirror
 * that raw shape so `listPrepared` can normalize it into the typed
 * [`PreparedManifest`] API.
 */
export interface RawPreparedManifestRecordField {
  name: string;
  type: unknown;
  nullable: boolean;
}

export interface RawPreparedManifestColumn {
  name: string;
  type: unknown;
  nullable: boolean;
}

export interface RawPreparedManifestParameter {
  name: string;
  description: [] | [string];
  required: boolean;
  nullable: boolean;
  type: unknown;
}

export interface RawPreparedManifestSortKey {
  key: string;
  label: [] | [string];
}

export interface RawPreparedManifestOperation {
  name: string;
  description: [] | [string];
  kind: { Query: null } | { Update: null };
  parameters: RawPreparedManifestParameter[];
  result: { columns: RawPreparedManifestColumn[] };
  supports_consistency: boolean;
  supports_idempotency: boolean;
  allowed_sorts: RawPreparedManifestSortKey[];
}

export interface RawPreparedManifest {
  manifest_version: number;
  graph: { id: string; name: [] | [string] };
  operations: RawPreparedManifestOperation[];
}

const SCALAR_SEMANTIC_TAGS: ReadonlySet<string> = new Set([
  "Null",
  "Bool",
  "Int8",
  "Int16",
  "Int32",
  "Int64",
  "Int128",
  "Int256",
  "Uint8",
  "Uint16",
  "Uint32",
  "Uint64",
  "Uint128",
  "Uint256",
  "Float16",
  "Float32",
  "Float64",
  "Float128",
  "Float256",
  "Decimal",
  "Text",
  "Bytes",
  "Date",
  "Time",
  "LocalTime",
  "DateTime",
  "LocalDateTime",
  "ZonedDateTime",
  "ZonedTime",
  "Duration",
  "Principal",
  "Path",
]);

const manifestMismatch = (message: string): GleaphSdkError =>
  new GleaphSdkError(message, "GLEAPH_MANIFEST_MISMATCH");

/**
 * Normalize one `semantic_type` value into the typed representation.
 *
 * Accepts both the raw Candid variant object form (as decoded by the Router
 * manifest IDL) and an already-normalized flat string, so the function can be
 * applied at the manifest boundary or composed over authored specs.
 */
export function normalizeSemanticType(raw: unknown): PreparedSemanticType {
  if (typeof raw === "string") {
    return raw as PreparedSemanticType;
  }
  if (!raw || typeof raw !== "object") {
    throw manifestMismatch(`invalid semantic type: ${String(raw)}`);
  }
  const entries = Object.entries(raw as Record<string, unknown>);
  const entry = entries[0];
  if (entries.length !== 1 || entry === undefined) {
    throw manifestMismatch("semantic type variant must have exactly one tag");
  }
  const tag = entry[0];
  if (tag === "List") {
    const payload = (raw as { List: { element: unknown } }).List;
    return { List: { element: normalizeSemanticType(payload.element) } };
  }
  if (tag === "Record") {
    const fields = (raw as { Record: { fields: RawPreparedManifestRecordField[] } }).Record.fields;
    return { Record: { fields: fields.map(normalizeRecordField) } };
  }
  if (!SCALAR_SEMANTIC_TAGS.has(tag)) {
    throw manifestMismatch(`unknown semantic type tag: ${tag}`);
  }
  return tag as PreparedSemanticType;
}

function normalizeRecordField(raw: RawPreparedManifestRecordField): PreparedManifestRecordField {
  return {
    name: raw.name,
    nullable: raw.nullable,
    semantic_type: normalizeSemanticType(raw.type),
  };
}

function normalizeColumn(raw: RawPreparedManifestColumn): PreparedManifestColumn {
  return {
    name: raw.name,
    nullable: raw.nullable,
    semantic_type: normalizeSemanticType(raw.type),
  };
}

function normalizeParameter(raw: RawPreparedManifestParameter): PreparedManifestParameter {
  return {
    name: raw.name,
    description: raw.description[0] ?? null,
    required: raw.required,
    nullable: raw.nullable,
    semantic_type: normalizeSemanticType(raw.type),
  };
}

/**
 * Normalize a raw (Candid-decoded) prepared manifest into the typed
 * [`PreparedManifest`] API: variant objects become flat/structured semantic
 * types, `kind` variants become strings, and `opt` strings become `null`.
 */
export function normalizeManifest(raw: RawPreparedManifest): PreparedManifest {
  return {
    manifest_version: raw.manifest_version,
    graph: {
      id: raw.graph.id,
      name: raw.graph.name[0] ?? null,
    },
    operations: raw.operations.map((operation) => ({
      name: operation.name,
      description: operation.description[0] ?? null,
      kind: "Query" in operation.kind ? "Query" : "Update",
      parameters: operation.parameters.map(normalizeParameter),
      result: { columns: operation.result.columns.map(normalizeColumn) },
      supports_consistency: operation.supports_consistency,
      supports_idempotency: operation.supports_idempotency,
      allowed_sorts: operation.allowed_sorts.map((sort) => ({
        key: sort.key,
        label: sort.label[0] ?? null,
      })),
    })),
  };
}
