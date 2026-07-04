import { IDL } from "@icp-sdk/core/candid";

const IcWirePathElement = IDL.Variant({
  Vertex: IDL.Vec(IDL.Nat8),
  Edge: IDL.Vec(IDL.Nat8),
});

const IcWireValue: IDL.Type = IDL.Rec();
const IcWireValueVariant = IDL.Variant({
  Null: IDL.Null,
  Bool: IDL.Bool,
  Int64: IDL.Int64,
  Uint64: IDL.Nat64,
  Int128: IDL.Int,
  Uint128: IDL.Nat,
  Int256: IDL.Text,
  Uint256: IDL.Text,
  Float64: IDL.Float64,
  Decimal: IDL.Text,
  Text: IDL.Text,
  Bytes: IDL.Vec(IDL.Nat8),
  Date: IDL.Int32,
  Time: IDL.Nat64,
  LocalTime: IDL.Nat64,
  DateTime: IDL.Record({
    seconds: IDL.Int64,
    nanos: IDL.Nat32,
  }),
  LocalDateTime: IDL.Record({
    seconds: IDL.Int64,
    nanos: IDL.Nat32,
  }),
  ZonedDateTime: IDL.Record({
    seconds: IDL.Int64,
    nanos: IDL.Nat32,
    offset_seconds: IDL.Int32,
  }),
  ZonedTime: IDL.Record({
    nanos: IDL.Nat64,
    offset_seconds: IDL.Int32,
  }),
  Duration: IDL.Record({
    months: IDL.Int32,
    nanos: IDL.Int64,
  }),
  Principal: IDL.Principal,
  ExtensionLeaf: IDL.Record({
    type_name: IDL.Text,
    payload: IDL.Vec(IDL.Nat8),
  }),
  ValueBinary: IDL.Vec(IDL.Nat8),
  List: IDL.Vec(IcWireValue),
  Path: IDL.Vec(IcWirePathElement),
  Record: IDL.Vec(IDL.Tuple(IDL.Text, IcWireValue)),
});

(IcWireValue as unknown as { fill: (value: IDL.Type) => void }).fill(IcWireValueVariant);

const IcWirePlanQueryResult = IDL.Record({
  rows: IDL.Vec(
    IDL.Record({
      columns: IDL.Vec(IDL.Tuple(IDL.Text, IcWireValue)),
    }),
  ),
});

type WireValue =
  | { Text: string }
  | { Int64: bigint }
  | { Uint64: bigint }
  | { Float64: number }
  | { DateTime: { seconds: bigint; nanos: number } }
  | { Date: number }
  | { Time: bigint }
  | { Null: null };

type WireRow = {
  columns: [string, WireValue][];
};

type WireResult = {
  rows: WireRow[];
};

const toUint8Array = (bytes: number[] | Uint8Array): Uint8Array =>
  bytes instanceof Uint8Array ? bytes : Uint8Array.from(bytes);

export const decodeWireRows = (rowsBlob: number[] | Uint8Array): WireResult => {
  const [decoded] = IDL.decode([IcWirePlanQueryResult], toUint8Array(rowsBlob));
  return decoded as WireResult;
};

export const rowToColumnMap = (row: WireRow): Map<string, WireValue> => {
  const map = new Map<string, WireValue>();
  for (const [name, value] of row.columns) {
    map.set(name, value);
  }
  return map;
};

export const expectText = (map: Map<string, WireValue>, column: string): string => {
  const value = map.get(column);
  if (!value || !("Text" in value)) {
    throw new Error(`Missing or non-text column: ${column}`);
  }
  return value.Text;
};

export const expectFloat64 = (map: Map<string, WireValue>, column: string): number => {
  const value = map.get(column);
  if (!value || !("Float64" in value)) {
    throw new Error(`Missing or non-float64 column: ${column}`);
  }
  return value.Float64;
};

export const expectDateTimeSeconds = (
  map: Map<string, WireValue>,
  column: string,
): bigint => {
  const value = map.get(column);
  if (value && "DateTime" in value) {
    return value.DateTime.seconds;
  }
  if (value && "Time" in value) {
    return value.Time;
  }
  if (value && "Int64" in value) {
    return value.Int64;
  }
  throw new Error(`Missing or unsupported date column: ${column}`);
};
