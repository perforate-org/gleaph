// The wasm graph session, wrapped so the view never touches raw bindings.
//
// The module is served from `/graph-view/` (built by `wasm-bindgen --target web`, optimised with
// `wasm-opt -Oz`) and loaded only when the demo scrolls into view: the landing page should not pay
// for a renderer until someone looks at it.

import type { DemoPreset } from "./presets";

interface GraphSessionHandle {
  frame(): [Float32Array, Float32Array, Float32Array];
  pan(dx: number, dy: number): void;
  zoom_at(x: number, y: number, factor: number): void;
  fit(): void;
  resize(width: number, height: number): void;
  step(iterations: number): boolean;
  pick(x: number, y: number): number;
  free(): void;
}

interface GraphSessionConstructor {
  new (
    positions: Float32Array,
    edges: Uint32Array,
    labelSlots: Uint32Array,
    width: number,
    height: number,
  ): GraphSessionHandle;
}

let modulePromise: Promise<GraphSessionConstructor> | undefined;

async function loadModule(): Promise<GraphSessionConstructor> {
  modulePromise ??= (async () => {
    const module = await import(/* @vite-ignore */ "/graph-view/graph_view_web.js");
    await module.default({ module_or_path: "/graph-view/graph_view_web_bg.wasm" });
    return module.GraphSession as GraphSessionConstructor;
  })();
  return modulePromise;
}

/** A frame in the view's terms: nodes and edges as flat planes, labels already placed. */
export interface Frame {
  nodes: Float32Array;
  edges: Float32Array;
  labels: Float32Array;
}

export class GraphSession {
  private constructor(
    private readonly handle: GraphSessionHandle,
    readonly preset: DemoPreset,
  ) {}

  static async create(
    preset: DemoPreset,
    width: number,
    height: number,
  ): Promise<GraphSession> {
    const GraphSessionClass = await loadModule();
    // Start from a ring so the first frame is a graph even before the layout settles.
    const positions = new Float32Array(preset.nodes.length * 2);
    preset.nodes.forEach((_, index) => {
      const angle = (index / preset.nodes.length) * Math.PI * 2;
      positions[index * 2] = Math.cos(angle) * 100;
      positions[index * 2 + 1] = Math.sin(angle) * 100;
    });
    const edges = new Uint32Array(preset.edges.length * 2);
    preset.edges.forEach(([source, target], index) => {
      edges[index * 2] = source;
      edges[index * 2 + 1] = target;
    });
    const slots = new Uint32Array(preset.nodes.map((node) => node.slot));
    const handle = new GraphSessionClass(positions, edges, slots, width, height);
    return new GraphSession(handle, preset);
  }

  frame(): Frame {
    const [nodes, edges, labels] = this.handle.frame();
    return { nodes, edges, labels };
  }

  step(iterations: number): boolean {
    return this.handle.step(iterations);
  }

  pan(dx: number, dy: number): void {
    this.handle.pan(dx, dy);
  }

  zoomAt(x: number, y: number, factor: number): void {
    this.handle.zoom_at(x, y, factor);
  }

  fit(): void {
    this.handle.fit();
  }

  resize(width: number, height: number): void {
    this.handle.resize(width, height);
  }

  pick(x: number, y: number): number {
    return this.handle.pick(x, y);
  }

  /** The preset node behind a plane index (`nodes` carries its index in the fifth column). */
  nodeAt(index: number): DemoPreset["nodes"][number] | undefined {
    return this.preset.nodes[index];
  }

  destroy(): void {
    this.handle.free();
  }
}
