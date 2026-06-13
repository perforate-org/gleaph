export type QueryDataSource = "live" | "preview";

export type QueryTiming = {
  startedAt: number;
  finishedAt: number;
  durationMs: number;
  rowCount?: number;
};

export type QueryRunState =
  | { status: "idle" }
  | { status: "loading"; startedAt: number; source: QueryDataSource }
  | {
      status: "ready";
      timing: QueryTiming;
      source: QueryDataSource;
    }
  | {
      status: "error";
      message: string;
      source: QueryDataSource;
      timing?: QueryTiming;
    };

export const formatDurationMs = (durationMs: number): string => {
  if (durationMs < 1) {
    return "< 1 ms";
  }
  if (durationMs < 100) {
    return `${durationMs.toFixed(1)} ms`;
  }
  if (durationMs < 1000) {
    return `${Math.round(durationMs)} ms`;
  }
  return `${(durationMs / 1000).toFixed(2)} s`;
};

export const elapsedMs = (startedAt: number, now = performance.now()): number =>
  Math.max(0, now - startedAt);
