import init, { format_gql_query } from "~/generated/gql_formatter/gql_formatter";

export type FormatterErrorKind = "parse" | "unsupported" | "invalid-options" | "adapter";

export type FormatterError = {
  kind: FormatterErrorKind;
  message: string;
};

export type FormatterOptions = {
  indentation?: string;
  lineWidth?: number;
  keywordCase?: "upper" | "lower";
  clauseBreaks?: "every-clause" | "compact";
  commaAfterBreak?: boolean;
  resultItemBreaks?: "every-item" | "compact";
};

let initialized: Promise<unknown> | undefined;

function ensureInitialized(): Promise<unknown> {
  initialized ??= init();
  return initialized;
}

function normalizeError(value: unknown): FormatterError {
  if (typeof value === "object" && value !== null) {
    const candidate = value as { kind?: unknown; message?: unknown };
    if (typeof candidate.kind === "string" && typeof candidate.message === "string") {
      const kind: FormatterErrorKind =
        candidate.kind === "parse" ||
        candidate.kind === "unsupported" ||
        candidate.kind === "invalid-options"
          ? candidate.kind
          : "adapter";
      return { kind, message: candidate.message };
    }
  }
  return {
    kind: "adapter",
    message: value instanceof Error ? value.message : "The GQL formatter could not run.",
  };
}

export async function formatGqlQuery(
  query: string,
  options: FormatterOptions = {},
): Promise<string | FormatterError> {
  try {
    await ensureInitialized();
    return format_gql_query(query, options);
  } catch (error) {
    return normalizeError(error);
  }
}

export function isFormatterError(value: string | FormatterError): value is FormatterError {
  return typeof value !== "string";
}
