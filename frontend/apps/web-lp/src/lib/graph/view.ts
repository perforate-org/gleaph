// The canvas view: draws the frame the wasm session hands over, places the DOM labels, and turns
// pointers into pan / zoom / hover / pick.
//
// Colour and type come from the page, not from this file: every value is read from CSS custom
// properties on the root element the view is mounted in, so the demo follows the landing page's
// tokens (and its dark/light switch) without a second palette.

import type { DemoPreset } from "./presets";
import { GraphSession, type Frame } from "./session";

export interface GraphTheme {
  canvas: string;
  ink: string;
  muted: string;
  line: string;
  accent: string;
  classes: string[];
}

const NODE_STRIDE = 5;
const EDGE_STRIDE = 4;
const LABEL_STRIDE = 4;

export function readTheme(root: HTMLElement): GraphTheme {
  const style = getComputedStyle(root);
  const read = (name: string, fallback: string) =>
    style.getPropertyValue(name).trim() || fallback;
  return {
    canvas: read("--gd-canvas", "#f5f3ef"),
    ink: read("--gd-ink", "#212121"),
    muted: read("--gd-muted", "#606060"),
    line: read("--gd-line", "#d1d1d1"),
    accent: read("--gd-accent", "#284dbe"),
    classes: [1, 2, 3, 4].map((index) =>
      read(`--gd-node-${index}`, index === 1 ? "#284dbe" : "#808080"),
    ),
  };
}

export class GraphView {
  private session?: GraphSession;
  private frame?: Frame;
  private theme: GraphTheme;
  private hovered = -1;
  private selected = -1;
  private raf = 0;
  private dragging = false;
  private pointer = { x: 0, y: 0 };
  private readonly labels: HTMLElement[] = [];
  private readonly labelLayer: HTMLElement;

  constructor(
    private readonly root: HTMLElement,
    private readonly canvas: HTMLCanvasElement,
    labelLayer: HTMLElement,
    private readonly status: HTMLElement,
  ) {
    this.labelLayer = labelLayer;
    this.theme = readTheme(root);
    this.bind();
  }

  async load(preset: DemoPreset): Promise<void> {
    this.session?.destroy();
    const size = this.size();
    this.session = await GraphSession.create(preset, size.width, size.height);
    this.hovered = -1;
    this.selected = -1;
    this.frame = this.session.frame();
    this.resize();
    this.draw();
    this.animate();
  }

  resize(): void {
    const ratio = window.devicePixelRatio || 1;
    const size = this.size();
    this.canvas.width = Math.round(size.width * ratio);
    this.canvas.height = Math.round(size.height * ratio);
    this.canvas.style.width = `${size.width}px`;
    this.canvas.style.height = `${size.height}px`;
    this.session?.resize(size.width, size.height);
    this.draw();
  }

  refreshTheme(): void {
    this.theme = readTheme(this.root);
    this.draw();
  }

  private size(): { width: number; height: number } {
    const rect = this.canvas.parentElement?.getBoundingClientRect();
    return {
      width: Math.max(240, Math.round(rect?.width ?? 640)),
      height: Math.max(200, Math.round(rect?.height ?? 420)),
    };
  }

  private bind(): void {
    const canvas = this.canvas;
    canvas.addEventListener("pointerdown", (event) => {
      this.dragging = true;
      this.pointer = { x: event.clientX, y: event.clientY };
      canvas.setPointerCapture(event.pointerId);
    });
    canvas.addEventListener("pointermove", (event) => {
      const rect = canvas.getBoundingClientRect();
      const local = { x: event.clientX - rect.left, y: event.clientY - rect.top };
      if (this.dragging) {
        this.session?.pan(event.clientX - this.pointer.x, event.clientY - this.pointer.y);
        this.pointer = { x: event.clientX, y: event.clientY };
        this.draw();
        return;
      }
      const hit = this.session?.pick(local.x, local.y) ?? -1;
      if (hit !== this.hovered) {
        this.hovered = hit;
        this.report();
        this.draw();
      }
    });
    const end = () => {
      this.dragging = false;
    };
    canvas.addEventListener("pointerup", end);
    canvas.addEventListener("pointercancel", end);
    canvas.addEventListener("pointerleave", () => {
      this.hovered = -1;
      this.report();
      this.draw();
    });
    canvas.addEventListener("click", (event) => {
      const rect = canvas.getBoundingClientRect();
      const hit = this.session?.pick(event.clientX - rect.left, event.clientY - rect.top) ?? -1;
      this.selected = hit === this.selected ? -1 : hit;
      this.report();
      this.draw();
    });
    canvas.addEventListener(
      "wheel",
      (event) => {
        event.preventDefault();
        const rect = canvas.getBoundingClientRect();
        const factor = Math.exp(-event.deltaY * 0.0015);
        this.session?.zoomAt(event.clientX - rect.left, event.clientY - rect.top, factor);
        this.draw();
      },
      { passive: false },
    );
    canvas.addEventListener("dblclick", () => {
      this.session?.fit();
      this.draw();
    });
    const observer = new ResizeObserver(() => this.resize());
    observer.observe(this.canvas.parentElement ?? this.canvas);
  }

  private animate(): void {
    cancelAnimationFrame(this.raf);
    const tick = () => {
      if (!this.session) return;
      const moving = this.session.step(1);
      this.frame = this.session.frame();
      this.draw();
      if (moving && !matchMedia("(prefers-reduced-motion: reduce)").matches) {
        this.raf = requestAnimationFrame(tick);
      } else if (moving) {
        // Reduced motion: settle at a coarse cadence instead of animating every frame.
        this.raf = window.setTimeout(tick, 120);
      }
    };
    this.raf = requestAnimationFrame(tick);
  }

  private report(): void {
    const node = this.hovered >= 0 ? this.session?.nodeAt(this.nodeIndex(this.hovered)) : undefined;
    this.status.textContent = node ? node.name : "";
    this.status.dataset.hovered = node ? "true" : "false";
  }

  private nodeIndex(planeIndex: number): number {
    const nodes = this.frame?.nodes;
    if (!nodes || planeIndex < 0 || planeIndex * NODE_STRIDE + 4 >= nodes.length) return -1;
    return nodes[planeIndex * NODE_STRIDE + 4];
  }

  private draw(): void {
    const context = this.canvas.getContext("2d");
    const frame = this.frame;
    if (!context || !frame) return;
    const ratio = window.devicePixelRatio || 1;
    context.setTransform(ratio, 0, 0, ratio, 0, 0);
    const { width, height } = this.size();
    context.clearRect(0, 0, width, height);
    context.fillStyle = this.theme.canvas;
    context.fillRect(0, 0, width, height);

    // Edges first, under the markers: the graph's structure reads through the nodes.
    context.strokeStyle = this.theme.line;
    context.lineWidth = 1;
    context.beginPath();
    const edges = frame.edges;
    for (let at = 0; at + EDGE_STRIDE <= edges.length; at += EDGE_STRIDE) {
      context.moveTo(edges[at], edges[at + 1]);
      context.lineTo(edges[at + 2], edges[at + 3]);
    }
    context.stroke();

    const nodes = frame.nodes;
    for (let at = 0; at + NODE_STRIDE <= nodes.length; at += NODE_STRIDE) {
      const index = at / NODE_STRIDE;
      const slot = nodes[at + 3];
      const selected = index === this.selected;
      const hovered = index === this.hovered;
      context.beginPath();
      context.arc(nodes[at], nodes[at + 1], nodes[at + 2] + (hovered ? 1.5 : 0), 0, Math.PI * 2);
      context.fillStyle = this.theme.classes[slot % this.theme.classes.length];
      context.globalAlpha = this.hovered >= 0 && !hovered && !selected ? 0.45 : 1;
      context.fill();
      context.globalAlpha = 1;
      if (selected) {
        context.beginPath();
        context.arc(nodes[at], nodes[at + 1], nodes[at + 2] + 4, 0, Math.PI * 2);
        context.strokeStyle = this.theme.accent;
        context.lineWidth = 1.5;
        context.stroke();
      }
    }
    this.placeLabels();
  }

  private placeLabels(): void {
    const frame = this.frame;
    if (!frame) return;
    const pixels = frame.labels;
    const count = Math.min(Math.floor(pixels.length / LABEL_STRIDE), 220);
    while (this.labels.length < count) {
      const element = document.createElement("span");
      element.className = "gd-label";
      this.labelLayer.append(element);
      this.labels.push(element);
    }
    for (let index = 0; index < this.labels.length; index++) {
      const element = this.labels[index];
      if (index >= count) {
        element.hidden = true;
        continue;
      }
      const at = index * LABEL_STRIDE;
      const node = this.session?.nodeAt(pixels[at + 3]);
      if (!node) {
        element.hidden = true;
        continue;
      }
      element.hidden = false;
      element.textContent = node.name;
      element.style.transform = `translate(${pixels[at]}px, ${pixels[at + 1]}px)`;
      element.dataset.slot = String(pixels[at + 2]);
      element.classList.toggle("is-hovered", node === this.session?.nodeAt(this.nodeIndex(this.hovered)));
    }
  }
}
