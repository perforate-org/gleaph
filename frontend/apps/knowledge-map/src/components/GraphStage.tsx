import { For, Show, createEffect, createMemo, createSignal, onCleanup } from "solid-js";

import {
  type QueryRunState,
  elapsedMs,
  formatDurationMs,
} from "~/api/queryTiming";
import type { DemoEdge, DemoNode, KnowledgeMapViewModel, NodeKind, PlaybackStatus } from "~/types";

type GraphStageProps = {
  viewModel?: KnowledgeMapViewModel;
  queryRun: QueryRunState;
  activeStepIndex: number;
  playbackStatus: PlaybackStatus;
};

export function GraphStage(props: GraphStageProps) {
  const nodePositions = createMemo(() => {
    const viewModel = props.viewModel;
    if (!viewModel) {
      return new Map<string, { x: number; y: number }>();
    }

    return new Map(
      viewModel.nodes.map((node) => {
        const [sourceX, sourceY] = node.positionHint ?? [0, 0, 0];
        const x = 50 + sourceX * 8.2;
        const y = 50 - sourceY * 14.5;
        return [node.id, { x, y }];
      }),
    );
  });

  const resultNodeIds = createMemo(() => {
    const viewModel = props.viewModel;
    if (!viewModel || props.playbackStatus !== "complete") {
      return new Set<string>();
    }
    return new Set(
      viewModel.results.flatMap((result) => (result.nodeId ? [result.nodeId] : [])),
    );
  });

  const edgeById = createMemo(() => {
    const viewModel = props.viewModel;
    return new Map((viewModel?.edges ?? []).map((edge) => [edge.id, edge]));
  });

  const activeStep = createMemo(() => props.viewModel?.storySteps[props.activeStepIndex]);

  const visitedNodeIds = createMemo(() => {
    const viewModel = props.viewModel;
    const visited = new Set<string>();
    if (!viewModel) {
      return visited;
    }
    for (let index = 0; index <= props.activeStepIndex; index += 1) {
      const nodeId = viewModel.storySteps[index]?.nodeId;
      if (nodeId) {
        visited.add(nodeId);
      }
    }
    return visited;
  });

  const visitedEdgeIds = createMemo(() => {
    const viewModel = props.viewModel;
    const visited = new Set<string>();
    if (!viewModel) {
      return visited;
    }
    for (let index = 0; index <= props.activeStepIndex; index += 1) {
      const edgeId = viewModel.storySteps[index]?.edgeId;
      if (edgeId) {
        visited.add(edgeId);
      }
    }
    return visited;
  });

  return (
    <section class="relative min-h-[520px] overflow-hidden rounded-md border border-slate-200/80 bg-white shadow-[0_24px_80px_rgba(15,23,42,0.08)]">
      <div class="absolute inset-0 bg-[radial-gradient(circle_at_18%_18%,rgba(14,165,233,0.16),transparent_28%),radial-gradient(circle_at_80%_18%,rgba(124,58,237,0.13),transparent_26%),linear-gradient(180deg,rgba(255,255,255,0.96),rgba(241,247,255,0.88))]" />
      <Show when={props.viewModel}>
        {(viewModel) => (
          <svg
            class="absolute inset-0 h-full w-full"
            role="img"
            aria-label="Animated relationship map"
            viewBox="0 0 100 100"
            preserveAspectRatio="xMidYMid meet"
          >
            <defs>
              <marker
                id="knowledge-map-arrow"
                viewBox="0 0 10 10"
                refX="8"
                refY="5"
                markerWidth="4"
                markerHeight="4"
                orient="auto-start-reverse"
              >
                <path d="M 0 0 L 10 5 L 0 10 z" fill="rgba(71, 85, 105, 0.42)" />
              </marker>
            </defs>

            <g opacity="0.28">
              <For each={Array.from({ length: 15 })}>
                {(_, index) => (
                  <circle
                    cx={10 + ((index() * 17) % 82)}
                    cy={8 + ((index() * 29) % 84)}
                    r="0.18"
                    fill="#94a3b8"
                  />
                )}
              </For>
            </g>

            <g>
              <For each={viewModel().edges}>
                {(edge) => (
                  <GraphEdge
                    edge={edge}
                    positions={nodePositions()}
                    active={activeStep()?.edgeId === edge.id}
                    visited={visitedEdgeIds().has(edge.id)}
                    contextual={visitedNodeIds().has(edge.source) || visitedNodeIds().has(edge.target)}
                  />
                )}
              </For>
            </g>

            <ActiveTrail
              edge={activeStep()?.edgeId ? edgeById().get(activeStep()?.edgeId ?? "") : undefined}
              positions={nodePositions()}
              playbackStatus={props.playbackStatus}
            />

            <g>
              <For each={viewModel().nodes}>
                {(node) => (
                  <GraphNode
                    node={node}
                    position={nodePositions().get(node.id)}
                    active={activeStep()?.nodeId === node.id}
                    visited={visitedNodeIds().has(node.id)}
                    result={resultNodeIds().has(node.id)}
                    contextual={
                      visitedNodeIds().has(node.id) ||
                      resultNodeIds().has(node.id) ||
                      node.kind === "person" ||
                      node.kind === "project"
                    }
                  />
                )}
              </For>
            </g>
          </svg>
        )}
      </Show>
      <QueryLoadingOverlay queryRun={props.queryRun} />
      <div class="pointer-events-none absolute left-4 top-4 max-w-[280px] rounded-md border border-slate-200 bg-white/88 px-3 py-2 shadow-[0_12px_34px_rgba(15,23,42,0.08)]">
        <p class="text-xs font-semibold uppercase tracking-[0.16em] text-sky-700">
          Relationship path
        </p>
        <p class="mt-1 text-sm text-slate-600">
          <Show
            when={props.queryRun.status === "ready"}
            fallback="Run a query to load the fan-out graph from Gleaph."
          >
            Full graph context with the active path and result fan-out highlighted.
          </Show>
        </p>
      </div>
    </section>
  );
}

type GraphEdgeProps = {
  edge: DemoEdge;
  positions: Map<string, { x: number; y: number }>;
  active: boolean;
  visited: boolean;
  contextual: boolean;
};

function GraphEdge(props: GraphEdgeProps) {
  const source = () => props.positions.get(props.edge.source);
  const target = () => props.positions.get(props.edge.target);
  const mid = () => {
    const a = source();
    const b = target();
    return a && b ? { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 } : undefined;
  };

  return (
    <Show when={source() && target()}>
      <g>
        <line
          class="knowledge-map-edge"
          x1={source()?.x}
          y1={source()?.y}
          x2={target()?.x}
          y2={target()?.y}
          stroke={props.active ? "#2563eb" : props.visited ? "#06b6d4" : "#94a3b8"}
          stroke-width={props.active ? 0.62 : props.visited ? 0.38 : props.contextual ? 0.22 : 0.16}
          stroke-opacity={props.active ? 0.92 : props.visited ? 0.72 : props.contextual ? 0.34 : 0.18}
          marker-end={props.active || props.visited ? "url(#knowledge-map-arrow)" : undefined}
        />
        <Show when={props.active || props.visited}>
          <text
            x={mid()?.x}
            y={mid()?.y}
            text-anchor="middle"
            dominant-baseline="middle"
            class="select-none fill-slate-700 text-[2.2px] font-semibold"
            paint-order="stroke"
            stroke="rgba(255,255,255,0.94)"
            stroke-width="0.85"
          >
            {props.edge.label}
          </text>
        </Show>
      </g>
    </Show>
  );
}

type ActiveTrailProps = {
  edge?: DemoEdge;
  positions: Map<string, { x: number; y: number }>;
  playbackStatus: PlaybackStatus;
};

function ActiveTrail(props: ActiveTrailProps) {
  const source = () => (props.edge ? props.positions.get(props.edge.source) : undefined);
  const target = () => (props.edge ? props.positions.get(props.edge.target) : undefined);
  return (
    <Show when={props.edge && source() && target() && props.playbackStatus !== "complete"}>
      <line
        class="knowledge-map-trail"
        x1={source()?.x}
        y1={source()?.y}
        x2={target()?.x}
        y2={target()?.y}
        stroke="#38bdf8"
        stroke-width="0.72"
        stroke-linecap="round"
        stroke-dasharray="1.2 4.8"
        stroke-opacity="0.92"
      />
    </Show>
  );
}

type GraphNodeProps = {
  node: DemoNode;
  position?: { x: number; y: number };
  active: boolean;
  visited: boolean;
  result: boolean;
  contextual: boolean;
};

function GraphNode(props: GraphNodeProps) {
  const color = () => nodeColor(props.node.kind);
  const radius = () => {
    if (props.node.kind === "person") {
      return 2.85;
    }
    if (props.node.kind === "project") {
      return 2.55;
    }
    if (props.node.kind === "document") {
      return 2.15;
    }
    return 1.95;
  };
  const labelOpacity = () => {
    if (props.active || props.result) {
      return 1;
    }
    if (props.visited || props.contextual) {
      return 0.82;
    }
    return 0.34;
  };

  return (
    <Show when={props.position}>
      {(position) => (
        <g
          class="knowledge-map-node-group"
          transform={`translate(${position().x} ${position().y})`}
          opacity={props.active || props.visited || props.result ? 1 : props.contextual ? 0.72 : 0.38}
        >
          <circle
            class="knowledge-map-node-halo"
            r={radius() + (props.active ? 1.8 : props.result ? 1.2 : 0.6)}
            fill={color()}
            opacity={props.active ? 0.18 : props.result ? 0.14 : props.visited ? 0.09 : 0.04}
          />
          <circle
            class="knowledge-map-node-core"
            r={radius()}
            fill={color()}
            stroke={props.active ? "#0f172a" : "rgba(255,255,255,0.92)"}
            stroke-width={props.active ? 0.55 : 0.25}
          />
          <text
            y={radius() + 4.2}
            text-anchor="middle"
            class="select-none fill-slate-950 text-[2.5px] font-semibold"
            paint-order="stroke"
            stroke="rgba(255,255,255,0.94)"
            stroke-width="0.92"
            opacity={labelOpacity()}
          >
            {props.node.label}
          </text>
          <text
            y={radius() + 7}
            text-anchor="middle"
            class="select-none fill-slate-500 text-[1.75px] font-medium uppercase"
            paint-order="stroke"
            stroke="rgba(255,255,255,0.92)"
            stroke-width="0.74"
            opacity={labelOpacity() * 0.86}
          >
            {props.node.kind}
          </text>
        </g>
      )}
    </Show>
  );
}

function nodeColor(kind: NodeKind) {
  switch (kind) {
    case "person":
      return "#0891b2";
    case "post":
      return "#f59e0b";
    case "topic":
      return "#7c3aed";
    case "project":
      return "#10b981";
    case "document":
      return "#2563eb";
  }
}

type QueryLoadingOverlayProps = {
  queryRun: QueryRunState;
};

function QueryLoadingOverlay(props: QueryLoadingOverlayProps) {
  const [liveElapsedMs, setLiveElapsedMs] = createSignal(0);

  createEffect(() => {
    const run = props.queryRun;
    if (run.status !== "loading") {
      setLiveElapsedMs(0);
      return;
    }

    const tick = () => setLiveElapsedMs(elapsedMs(run.startedAt));
    tick();
    const timer = window.setInterval(tick, 32);
    onCleanup(() => window.clearInterval(timer));
  });

  return (
    <Show when={props.queryRun.status === "loading" || props.queryRun.status === "error"}>
      <div class="absolute inset-0 z-10 flex items-center justify-center bg-white/45">
        <Show
          when={props.queryRun.status === "loading"}
          fallback={
            <div class="max-w-sm rounded-md border border-rose-200 bg-white px-5 py-4 text-center shadow-[0_18px_50px_rgba(15,23,42,0.08)]">
              <p class="text-sm font-semibold text-rose-800">Query failed</p>
              <p class="mt-2 text-sm leading-6 text-slate-600">
                <Show
                  when={
                    props.queryRun.status === "error"
                      ? props.queryRun
                      : undefined
                  }
                >
                  {(run) => run().message}
                </Show>
              </p>
            </div>
          }
        >
          <div class="max-w-sm rounded-md border border-sky-200 bg-white px-6 py-5 text-center shadow-[0_18px_50px_rgba(15,23,42,0.08)]">
            <div class="mx-auto flex size-12 items-center justify-center rounded-full border border-sky-200 bg-sky-50">
              <span class="inline-block size-5 animate-spin rounded-full border-2 border-sky-200 border-t-sky-600" />
            </div>
            <p class="mt-4 text-sm font-semibold text-slate-950">
              Querying Gleaph Router
            </p>
            <p class="mt-2 text-4xl font-semibold tabular-nums tracking-tight text-sky-700">
              {formatDurationMs(liveElapsedMs())}
            </p>
            <p class="mt-2 text-sm leading-6 text-slate-600">
              Query round trip in progress. The graph stays visible underneath.
            </p>
          </div>
        </Show>
      </div>
    </Show>
  );
}
