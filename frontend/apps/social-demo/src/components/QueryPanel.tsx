import { For, Show, createEffect, createMemo, createSignal, onCleanup } from "solid-js";

import type { ScenarioDefinition } from "~/data/scenarios";
import { QUERY_ANNOTATIONS, type QueryAnnotation } from "~/data/queryAnnotations";
import {
  formatGqlQuery,
  isFormatterError,
  type FormatterError,
} from "~/formatter/gqlFormatter";

type TokenType =
  | "keyword"
  | "label"
  | "property"
  | "number"
  | "string"
  | "variable"
  | "function"
  | "symbol"
  | "space"
  | "other";

type Token = {
  type: TokenType;
  text: string;
};

const KEYWORDS = new Set([
  "match",
  "optional",
  "where",
  "return",
  "order",
  "by",
  "limit",
  "search",
  "in",
  "for",
  "distance",
  "as",
  "asc",
  "desc",
  "and",
  "or",
  "not",
  "is",
  "null",
  "true",
  "false",
]);

const TOKEN_CLASSES: Record<TokenType, string> = {
  keyword: "text-purple-700 font-semibold",
  label: "text-emerald-700",
  property: "text-amber-700",
  number: "text-blue-600",
  string: "text-rose-600",
  variable: "text-cyan-700",
  function: "text-pink-700",
  symbol: "text-slate-500",
  space: "",
  other: "text-slate-800",
};

function tokenize(text: string): Token[] {
  const tokens: Token[] = [];
  let i = 0;

  while (i < text.length) {
    const c = text[i];

    if (/\s/.test(c)) {
      let j = i;
      while (j < text.length && /\s/.test(text[j])) j++;
      tokens.push({ type: "space", text: text.slice(i, j) });
      i = j;
      continue;
    }

    if (c === "'" || c === '"') {
      const quote = c;
      let j = i + 1;
      while (j < text.length && text[j] !== quote) {
        if (text[j] === "\\") j++;
        j++;
      }
      if (j < text.length) j++;
      tokens.push({ type: "string", text: text.slice(i, j) });
      i = j;
      continue;
    }

    if (/\d/.test(c) || (c === "-" && /\d/.test(text[i + 1] ?? ""))) {
      let j = i;
      if (c === "-") j++;
      while (j < text.length && /[\d.]/.test(text[j])) j++;
      tokens.push({ type: "number", text: text.slice(i, j) });
      i = j;
      continue;
    }

    if (c === "$") {
      let j = i + 1;
      while (j < text.length && /[a-zA-Z0-9_]/.test(text[j])) j++;
      tokens.push({ type: "variable", text: text.slice(i, j) });
      i = j;
      continue;
    }

    if (c === ":" && /[a-zA-Z_]/.test(text[i + 1] ?? "")) {
      let j = i + 1;
      while (j < text.length && /[a-zA-Z0-9_]/.test(text[j])) j++;
      tokens.push({ type: "label", text: text.slice(i, j) });
      i = j;
      continue;
    }

    if (/[a-zA-Z_]/.test(c)) {
      let j = i;
      while (j < text.length && /[a-zA-Z0-9_.]/.test(text[j])) {
        if (text[j] === "." && !/[a-zA-Z0-9_]/.test(text[j + 1] ?? "")) break;
        j++;
      }
      const word = text.slice(i, j);
      const lower = word.toLowerCase();
      if (KEYWORDS.has(lower)) {
        tokens.push({ type: "keyword", text: word });
      } else if (
        word.toUpperCase() === "ELEMENT_ID" ||
        (word.includes(".") && word.toUpperCase().startsWith("GLEAPH."))
      ) {
        tokens.push({ type: "function", text: word });
      } else if (word.includes(".")) {
        tokens.push({ type: "property", text: word });
      } else {
        tokens.push({ type: "other", text: word });
      }
      i = j;
      continue;
    }

    tokens.push({ type: "symbol", text: c });
    i++;
  }

  return tokens;
}

type Segment = {
  text: string;
  annotation?: QueryAnnotation;
  lineBreakAfter?: boolean;
};

function buildSegments(query: string, annotations: QueryAnnotation[]): Segment[] {
  const segments: Segment[] = [];
  let index = 0;

  for (const annotation of annotations) {
    const found = query.indexOf(annotation.queryText, index);
    if (found === -1) {
      continue;
    }

    if (found > index) {
      segments.push({ text: query.slice(index, found) });
    }

    segments.push({ text: annotation.queryText, annotation });
    index = found + annotation.queryText.length;
  }

  if (index < query.length) {
    segments.push({ text: query.slice(index) });
  }

  return segments;
}

function renderSegments(segments: Segment[]): Segment[] {
  const result: Segment[] = [];

  for (const segment of segments) {
    const parts = segment.text.split("\n");
    for (let i = 0; i < parts.length; i++) {
      result.push({
        text: parts[i],
        annotation: segment.annotation,
        lineBreakAfter: i < parts.length - 1,
      });
    }
  }

  return result;
}

function QueryTokens(props: {
  text: string;
  annotation?: QueryAnnotation;
  isActive?: boolean;
  onEnter?: (event: MouseEvent, annotation: QueryAnnotation) => void;
  onLeave?: () => void;
}) {
  const tokens = () => tokenize(props.text);
  const baseClass = () =>
    props.annotation
      ? "rounded-sm border-b border-dashed border-indigo-400 transition-colors hover:bg-indigo-100"
      : "";

  return (
    <span
      class={baseClass()}
      classList={{ "bg-indigo-100": props.isActive }}
      onMouseEnter={(event) => props.annotation && props.onEnter?.(event, props.annotation)}
      onMouseLeave={props.onLeave}
    >
      <For each={tokens()}>
        {(token) => <span class={TOKEN_CLASSES[token.type]}>{token.text}</span>}
      </For>
    </span>
  );
}

export function QueryPanel(props: { definition: ScenarioDefinition }) {
  type QueryFormatState =
    | { status: "loading" }
    | { status: "ready"; query: string }
    | { status: "error"; error: FormatterError };

  const [active, setActive] = createSignal<QueryAnnotation | null>(null);
  const [anchor, setAnchor] = createSignal<DOMRect | null>(null);
  const [expanded, setExpanded] = createSignal(false);
  const [formatState, setFormatState] = createSignal<QueryFormatState>({ status: "loading" });

  createEffect(() => {
    const query = props.definition.preparedQuery;
    let cancelled = false;
    setFormatState({ status: "loading" });

    void formatGqlQuery(query).then((result) => {
      if (cancelled) return;
      if (isFormatterError(result)) {
        setFormatState({ status: "error", error: result });
      } else {
        setFormatState({ status: "ready", query: result });
      }
    });

    onCleanup(() => {
      cancelled = true;
    });
  });

  const formattedQuery = () => {
    const state = formatState();
    return state.status === "ready" ? state.query : "";
  };
  const annotations = () => QUERY_ANNOTATIONS[props.definition.id] ?? [];
  const rawSegments = () => buildSegments(formattedQuery(), annotations());
  const segments = () => renderSegments(rawSegments());
  const formatStateMessage = () => {
    const state = formatState();
    if (state.status === "loading") return "Formatting query…";
    if (state.status === "error") return `${state.error.kind}: ${state.error.message}`;
    return "";
  };

  const handleEnter = (event: MouseEvent, annotation: QueryAnnotation) => {
    setActive(annotation);
    setAnchor((event.currentTarget as HTMLSpanElement).getBoundingClientRect());
  };

  const handleLeave = () => {
    setActive(null);
    setAnchor(null);
  };

  const tooltipStyle = createMemo(() => {
    const rect = anchor();
    if (!rect) {
      return { display: "none" };
    }

    const gap = 8;
    const maxWidth = 320;
    const padding = 16;
    let x = rect.left;
    let y = rect.bottom + gap;

    if (typeof window !== "undefined") {
      x = Math.max(padding, Math.min(x, window.innerWidth - maxWidth - padding));
      if (y + 120 > window.innerHeight) {
        y = Math.max(padding, rect.top - 120);
      }
    }

    return {
      left: `${x}px`,
      top: `${y}px`,
      maxWidth: `${maxWidth}px`,
    };
  });

  const ExpandIcon = () => (
    <svg
      xmlns="http://www.w3.org/2000/svg"
      width="16"
      height="16"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      stroke-width="2"
      stroke-linecap="round"
      stroke-linejoin="round"
    >
      <path d="M15 3h6v6" />
      <path d="M9 21H3v-6" />
      <path d="m21 3-7 7" />
      <path d="m3 21 7-7" />
    </svg>
  );

  const CloseIcon = () => (
    <svg
      xmlns="http://www.w3.org/2000/svg"
      width="20"
      height="20"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      stroke-width="2"
      stroke-linecap="round"
      stroke-linejoin="round"
    >
      <path d="M18 6 6 18" />
      <path d="m6 6 12 12" />
    </svg>
  );

  return (
    <div>
      <div class="flex items-center justify-between">
        <h3 class="text-xs font-semibold uppercase tracking-wide text-slate-500">GQL query</h3>
      </div>
      <p class="mt-1 text-xs text-slate-500">Hover highlighted parts to see what they do.</p>

      <div class="relative mt-2">
        <button
          type="button"
          class="absolute right-2 top-2 z-10 rounded-md p-1.5 text-slate-400 transition-all duration-150 ease-out hover:bg-slate-200 hover:text-slate-700 active:scale-95"
          onClick={() => setExpanded(true)}
          title="Expand query"
          aria-label="Expand query"
        >
          <ExpandIcon />
        </button>

        <div class="cursor-default overflow-x-auto rounded-lg border border-slate-200 bg-slate-50 p-3 pr-10 font-mono text-xs leading-relaxed whitespace-pre-wrap text-slate-800">
          <Show
            when={formatState().status === "ready"}
            fallback={<div class="text-slate-500">{formatStateMessage()}</div>}
          >
            <For each={segments()}>
              {(segment) => (
                <>
                  <QueryTokens
                    text={segment.text}
                    annotation={segment.annotation}
                    isActive={
                      segment.annotation !== undefined &&
                      active()?.queryText === segment.annotation.queryText
                    }
                    onEnter={handleEnter}
                    onLeave={handleLeave}
                  />
                  {segment.lineBreakAfter && <br />}
                </>
              )}
            </For>
          </Show>
        </div>
      </div>

      <Show when={active()}>
        <div
          class="fixed z-[60] pointer-events-none rounded-lg border border-slate-200 bg-white p-3 shadow-lg"
          style={tooltipStyle()}
        >
          <div class="text-xs font-semibold text-indigo-900">{active()!.label}</div>
          <div class="mt-1 text-xs leading-relaxed text-slate-700">{active()!.description}</div>
        </div>
      </Show>

      <div
        class="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto p-4 transition-all duration-300 ease-out sm:items-center"
        classList={{
          "pointer-events-auto bg-black/50 opacity-100 backdrop-blur-sm": expanded(),
          "pointer-events-none bg-black/0 opacity-0 backdrop-blur-none": !expanded(),
        }}
        aria-hidden={expanded() ? "false" : "true"}
        onClick={() => setExpanded(false)}
      >
        <div
          class="relative my-auto w-full max-w-4xl transform transition-all duration-300 ease-out"
          classList={{
            "scale-100 opacity-100": expanded(),
            "scale-95 opacity-0": !expanded(),
          }}
          onClick={(event) => event.stopPropagation()}
        >
          <div class="rounded-xl border border-slate-200 bg-white p-4 shadow-2xl sm:p-6">
            <div class="flex items-start justify-between gap-4">
              <div>
                <h3 class="text-sm font-semibold uppercase tracking-wide text-slate-500">
                  GQL query
                </h3>
                <p class="mt-1 text-xs text-slate-500">
                  Hover highlighted parts to see what they do.
                </p>
              </div>
              <button
                type="button"
                class="shrink-0 rounded-md p-2 text-slate-400 transition-all duration-150 ease-out hover:bg-slate-100 hover:text-slate-700 active:scale-95"
                onClick={() => setExpanded(false)}
                title="Close"
                aria-label="Close"
              >
                <CloseIcon />
              </button>
            </div>

            <div class="cursor-default relative mt-4 overflow-x-auto rounded-lg border border-slate-200 bg-slate-50 p-4 font-mono text-sm leading-relaxed whitespace-pre-wrap text-slate-800 sm:p-6">
              <Show
                when={formatState().status === "ready"}
                fallback={<div class="text-slate-500">{formatStateMessage()}</div>}
              >
                <For each={segments()}>
                  {(segment) => (
                    <>
                      <QueryTokens
                        text={segment.text}
                        annotation={segment.annotation}
                        isActive={
                          segment.annotation !== undefined &&
                          active()?.queryText === segment.annotation.queryText
                        }
                        onEnter={handleEnter}
                        onLeave={handleLeave}
                      />
                      {segment.lineBreakAfter && <br />}
                    </>
                  )}
                </For>
              </Show>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}
