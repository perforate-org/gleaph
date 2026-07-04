import { Show } from "solid-js";

export function ErrorCard(props: {
  title: string;
  message: string;
  onRetry?: () => void;
}) {
  return (
    <div class="rounded-xl border border-red-200 bg-red-50 p-4 shadow-sm">
      <h2 class="font-semibold text-red-800">{props.title}</h2>
      <p class="mt-1 whitespace-pre-wrap text-sm text-red-700">{props.message}</p>
      <Show when={props.onRetry}>
        <button
          type="button"
          onClick={props.onRetry}
          class="mt-3 rounded-lg bg-red-100 px-3 py-1.5 text-sm font-medium text-red-800 hover:bg-red-200"
        >
          Retry
        </button>
      </Show>
    </div>
  );
}
