import type { PlaybackStatus } from "~/types";

type DemoHeaderProps = {
  playbackStatus: PlaybackStatus;
  technicalMode: boolean;
  onPlay: () => void;
  onPause: () => void;
  onReset: () => void;
  onToggleTechnical: () => void;
};

export function DemoHeader(props: DemoHeaderProps) {
  return (
    <header class="flex flex-col gap-3 rounded-md border border-slate-200/80 bg-white/80 px-4 py-3 shadow-[0_24px_70px_rgba(15,23,42,0.08)] backdrop-blur md:flex-row md:items-center md:justify-between">
      <div>
        <p class="text-xs font-semibold uppercase tracking-[0.18em] text-sky-700">
          Gleaph demo
        </p>
        <h1 class="mt-1 text-2xl font-semibold text-slate-950">Knowledge Map</h1>
      </div>
      <div class="flex flex-wrap items-center gap-2">
        <button
          class="rounded-md border border-slate-300 bg-white px-3 py-2 text-sm font-medium text-slate-950 transition hover:border-sky-300 hover:bg-sky-50"
          type="button"
          onClick={props.playbackStatus === "playing" ? props.onPause : props.onPlay}
        >
          {props.playbackStatus === "playing" ? "Pause" : "Play"}
        </button>
        <button
          class="rounded-md border border-slate-300 bg-white px-3 py-2 text-sm font-medium text-slate-950 transition hover:border-sky-300 hover:bg-sky-50"
          type="button"
          onClick={props.onReset}
        >
          Replay
        </button>
        <button
          class="rounded-md border border-indigo-200 bg-indigo-50 px-3 py-2 text-sm font-medium text-indigo-800 transition hover:border-indigo-300 hover:bg-indigo-100"
          type="button"
          aria-pressed={props.technicalMode}
          onClick={props.onToggleTechnical}
        >
          {props.technicalMode ? "Hide technical flow" : "Show technical flow"}
        </button>
      </div>
    </header>
  );
}
