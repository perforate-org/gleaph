/* tslint:disable */
/* eslint-disable */

export class GraphSession {
    free(): void;
    [Symbol.dispose](): void;
    fit(): void;
    /**
     * `[nodePlane, edgePlane, labelPlane]` as `Float32Array`s: no object per node, no strings
     * per frame, and nothing to parse on the hot path.
     */
    frame(): Array<any>;
    constructor(positions: Float32Array, edges: Uint32Array, label_slots: Uint32Array, width: number, height: number);
    pan(dx: number, dy: number): void;
    pick(x: number, y: number): number;
    resize(width: number, height: number): void;
    step(iterations: number): boolean;
    zoom_at(x: number, y: number, factor: number): void;
}

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly __wbg_graphsession_free: (a: number, b: number) => void;
    readonly graphsession_fit: (a: number) => void;
    readonly graphsession_frame: (a: number) => any;
    readonly graphsession_new: (a: number, b: number, c: number, d: number, e: number, f: number, g: number, h: number) => number;
    readonly graphsession_pan: (a: number, b: number, c: number) => void;
    readonly graphsession_pick: (a: number, b: number, c: number) => number;
    readonly graphsession_resize: (a: number, b: number, c: number) => void;
    readonly graphsession_step: (a: number, b: number) => number;
    readonly graphsession_zoom_at: (a: number, b: number, c: number, d: number) => void;
    readonly __externref_table_alloc: () => number;
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __wbindgen_malloc: (a: number, b: number) => number;
    readonly __wbindgen_start: () => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
 * Instantiates the given `module`, which can either be bytes or
 * a precompiled `WebAssembly.Module`.
 *
 * @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
 *
 * @returns {InitOutput}
 */
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request and
 * for everything else, calls `WebAssembly.instantiate` directly.
 *
 * @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
 *
 * @returns {Promise<InitOutput>}
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
