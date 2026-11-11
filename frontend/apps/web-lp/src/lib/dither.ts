/**
 * The Gleaph landing-page art: a photograph drawn as an ordered (8x8 Bayer)
 * dither in a small, print-like ink palette, with a soft band of light that
 * sweeps across it and repeats.
 *
 * Adapted for Gleaph from the CUI landing page's dither engine (MIT); this
 * module owns the painting only — the page owns the masks, the opacity and the
 * fade, and the theme lives on `<html data-theme>`.
 *
 * Light theme: ink where the photo is dark (blue in the sky, clay and sand in
 * the valley). Dark theme: pale ink where the photo is light. The same picture
 * therefore reads as a print on paper or on ink-dark. Every dot covers a whole
 * number of device pixels, so the pattern stays crisp at any pixel ratio, and
 * the sweep only re-thresholds those dots — it never blurs or recolours.
 */

export type DitherOptions = {
  src?: string; // default '/art/dunes.jpg'
  cell?: number; // CSS px per dot, default 2
  focusY?: number; // 0..1 cover-crop focus, default 0.42
  horizon?: number; // 0..1 of source where sky gives way to ground, default 0.30
  gamma?: number; // higher = fewer dots, default 1.0
  reveal?: boolean | 'view'; // fade in on first paint, or when scrolled into view; default true
  loop?: boolean; // soft band of light sweeps the art and repeats; default false
  reduced?: () => boolean; // default: matchMedia('(prefers-reduced-motion: reduce)').matches
};

const TAU = Math.PI * 2;
const PERIOD_MS = 14_000; // one sweep of the band
const FRAME_MS = 50; // the sweep repaints at most 20 times a second
const BAND_STEPS = 512; // resolution of the band lookup, in samples per cycle
const BAND_LIFT = 0.42; // how much ink the band takes away (light theme)
const BAND_ADD = 0.42; // how much light the band adds (dark theme)
const SWEEP_DELAY_MS = 1_100; // the page's 1.1 s canvas fade, so the sweep never competes with it
const SETTLE_MS = 140; // coalesce a run of layout changes into one redraw

// ---------------------------------------------------------------------------
// ordered dither
// ---------------------------------------------------------------------------

/** 8x8 Bayer matrix, normalised to the open thresholds a dot is tested against. */
function bayer8(): Float32Array {
  let m: number[][] = [[0]];
  while (m.length < 8) {
    const n = m.length;
    const next: number[][] = [];
    for (let y = 0; y < n * 2; y++) {
      const row: number[] = [];
      for (let x = 0; x < n * 2; x++) {
        const corner = (y >= n ? 2 : 0) + (x >= n ? 1 : 0);
        row.push(4 * m[y % n][x % n] + [0, 2, 3, 1][corner]);
      }
      next.push(row);
    }
    m = next;
  }
  return Float32Array.from(m.flat(), (v) => (v + 0.5) / 64);
}

const BAYER = bayer8();

// ---------------------------------------------------------------------------
// ink
// ---------------------------------------------------------------------------

type Ink = readonly [number, number, number];
/** The ink colour a photo luminance prints in; the ramp interpolates between stops. */
type InkStop = readonly [number, Ink];

/** A 256-entry ink ramp: luminance -> colour, linear between stops. */
function ramp(stops: readonly InkStop[]): Uint8ClampedArray {
  const first = stops[0];
  const last = stops[stops.length - 1];
  const out = new Uint8ClampedArray(256 * 3);
  for (let i = 0; i < 256; i++) {
    const t = i / 255;
    let from = first;
    let to = last;
    for (let s = 1; s < stops.length; s++) {
      if (t <= stops[s][0]) {
        from = stops[s - 1];
        to = stops[s];
        break;
      }
    }
    const span = to[0] - from[0];
    const k = span > 0 ? (t - from[0]) / span : 1;
    out[i * 3] = from[1][0] + (to[1][0] - from[1][0]) * k;
    out[i * 3 + 1] = from[1][1] + (to[1][1] - from[1][1]) * k;
    out[i * 3 + 2] = from[1][2] + (to[1][2] - from[1][2]) * k;
  }
  return out;
}

// Print inks, not a gradient: a dark blue in the sky, clay and sand on the
// ground, and a pale cloud where the snow and the dune crests catch the light.
const LIGHT_INK = {
  sky: ramp([
    [0, [0x16, 0x32, 0x88]],
    [0.5, [0x34, 0x60, 0xbe]],
    [1, [0x96, 0xb2, 0xe0]],
  ]),
  ground: ramp([
    [0, [0x82, 0x4a, 0x30]],
    [0.45, [0xb0, 0x74, 0x48]],
    [0.72, [0xd8, 0xb2, 0x7a]],
    [0.95, [0xc9, 0xd3, 0xdc]],
  ]),
};

const DARK_INK = {
  sky: ramp([
    [0, [0x8f, 0xa9, 0xe8]],
    [1, [0xdb, 0xe6, 0xf8]],
  ]),
  ground: ramp([
    [0, [0xc3, 0x9a, 0x74]],
    [1, [0xec, 0xd8, 0xb6]],
  ]),
};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

const images = new Map<string, Promise<HTMLImageElement>>();

function loadImage(src: string): Promise<HTMLImageElement> {
  let pending = images.get(src);
  if (!pending) {
    pending = new Promise((resolve, reject) => {
      const img = new Image();
      img.decoding = 'async';
      img.onload = () => resolve(img);
      img.onerror = () => reject(new Error(`dither: cannot load ${src}`));
      img.src = src;
    });
    images.set(src, pending);
  }
  return pending;
}

/** A canvas 2d context; there is no painting path without one. */
function context2d(canvas: HTMLCanvasElement, opts?: CanvasRenderingContext2DSettings): CanvasRenderingContext2D {
  const ctx = canvas.getContext('2d', opts);
  if (!ctx) throw new Error('dither: 2d canvas context unavailable');
  return ctx;
}

function prefersReducedMotion(): boolean {
  return typeof window.matchMedia === 'function' && window.matchMedia('(prefers-reduced-motion: reduce)').matches;
}

function clamp01(v: number): number {
  return v < 0 ? 0 : v > 1 ? 1 : v;
}

function smoothstep(edge0: number, edge1: number, x: number): number {
  const t = clamp01((x - edge0) / (edge1 - edge0));
  return t * t * (3 - 2 * t);
}

// ---------------------------------------------------------------------------
// mount
// ---------------------------------------------------------------------------

/**
 * Paint the dither art into `target` and return the cleanup function.
 *
 * The module appends `div.dither > canvas`; the page is expected to style the
 * wrapper (`position: absolute; inset: 0; overflow: hidden`), the canvas fade
 * (`.dither.shown canvas`), the masks and the opacity.
 */
export function mountDither(target: HTMLElement, opts: DitherOptions = {}): () => void {
  const conf = {
    src: opts.src ?? '/art/dunes.jpg',
    cell: opts.cell ?? 2,
    focusY: opts.focusY ?? 0.42,
    horizon: opts.horizon ?? 0.3,
    gamma: opts.gamma ?? 1,
    reveal: opts.reveal ?? true,
    loop: opts.loop ?? false,
    reduced: opts.reduced ?? prefersReducedMotion,
  };

  const wrap = document.createElement('div');
  wrap.className = 'dither';
  wrap.setAttribute('aria-hidden', 'true');
  const canvas = document.createElement('canvas');
  wrap.append(canvas);

  const ctx = context2d(canvas);
  const small = document.createElement('canvas');
  const smallCtx = context2d(small, { willReadFrequently: true });

  target.append(wrap);

  // The paint surface: a grid of dots, one dot per `cell` CSS pixels.
  let img: HTMLImageElement | null = null;
  let frame: ImageData | null = null;
  let density: Float32Array | null = null; // ink coverage per dot, gamma and theme applied
  let ink: Uint8ClampedArray | null = null; // the colour of each dot's ink
  let bandPos: Uint16Array | null = null; // where each dot sits on the light band, 0..BAND_STEPS-1
  const bandGain = new Float32Array(BAND_STEPS); // the band's effect on coverage this frame
  let gw = 0;
  let gh = 0;
  let k = 1; // device pixels per dot
  let dark = false;
  let lastW = 0;
  let lastH = 0;

  let stopped = false;
  let revealed = conf.reveal === false;
  let visible = conf.reveal !== 'view';
  let revealRaf = 0;
  let settle = 0;
  let loopRaf = 0;
  let loopLast = 0;
  let loopArmed = false;
  let loopReady = false;
  let loopDelay = 0;
  let loopT0 = 0;
  let inView = false;

  function prepare(): boolean {
    if (!img || stopped) return false;
    const { width, height } = wrap.getBoundingClientRect();
    if (width < 1 || height < 1) return false;

    const dpr = window.devicePixelRatio || 1;
    k = Math.max(1, Math.round(conf.cell * dpr));
    gw = Math.ceil((width * dpr) / k);
    gh = Math.ceil((height * dpr) / k);
    if (gw < 1 || gh < 1) return false;

    // Cover-crop the photo into the box: the longer side is cropped, never
    // stretched, and focusY decides which part survives when the box is
    // taller than the photo's own crop.
    const nw = img.naturalWidth;
    const nh = img.naturalHeight;
    const boxAspect = gw / gh;
    let sw = nw;
    let sh = nh;
    let sx = 0;
    let sy = 0;
    if (nw / nh > boxAspect) {
      sw = nh * boxAspect;
      sx = (nw - sw) / 2;
    } else {
      sh = nw / boxAspect;
      sy = (nh - sh) * clamp01(conf.focusY);
    }

    small.width = gw;
    small.height = gh;
    smallCtx.imageSmoothingEnabled = true;
    smallCtx.imageSmoothingQuality = 'high';
    smallCtx.drawImage(img, sx, sy, sw, sh, 0, 0, gw, gh);
    const px = smallCtx.getImageData(0, 0, gw, gh).data;

    dark = document.documentElement.dataset.theme === 'dark';
    const palette = dark ? DARK_INK : LIGHT_INK;
    const n = gw * gh;
    density = new Float32Array(n);
    ink = new Uint8ClampedArray(n * 3);
    bandPos = new Uint16Array(n);
    frame = smallCtx.createImageData(gw, gh);

    for (let y = 0; y < gh; y++) {
      // Where this row of dots lands in the photo, to know sky from ground.
      const rowY = (sy + ((y + 0.5) / gh) * sh) / nh;
      const groundWeight = smoothstep(conf.horizon - 0.05, conf.horizon + 0.05, rowY);
      for (let x = 0; x < gw; x++) {
        const i = y * gw + x;
        const p = i * 4;
        // Rec. 709 luma; the photo is greyscale, but the crop resamples it.
        const lum = (px[p] * 0.2126 + px[p + 1] * 0.7152 + px[p + 2] * 0.0722) / 255;
        density[i] = Math.pow(dark ? lum : 1 - lum, conf.gamma);

        const c = ((lum * 255) | 0) * 3;
        const j = i * 3;
        ink[j] = palette.sky[c] + (palette.ground[c] - palette.sky[c]) * groundWeight;
        ink[j + 1] = palette.sky[c + 1] + (palette.ground[c + 1] - palette.sky[c + 1]) * groundWeight;
        ink[j + 2] = palette.sky[c + 2] + (palette.ground[c + 2] - palette.sky[c + 2]) * groundWeight;

        // A diagonal plane across the box, wrapped so the band repeats.
        const u = (x / gw) * 0.9 + (y / gh) * 0.35;
        bandPos[i] = ((u % 1) * BAND_STEPS) | 0;
      }
    }

    canvas.width = gw * k;
    canvas.height = gh * k;
    canvas.style.width = `${(gw * k) / dpr}px`;
    canvas.style.height = `${(gh * k) / dpr}px`;
    lastW = width;
    lastH = height;
    return true;
  }

  /** Paint the dots. `sweep` is the light band's phase in radians, or null. */
  function paint(sweep: number | null): void {
    if (!frame || !density || !ink || !bandPos) return;

    if (sweep !== null) {
      for (let s = 0; s < BAND_STEPS; s++) {
        const w = 0.5 + 0.5 * Math.cos((s / BAND_STEPS) * TAU - sweep);
        const band = w * w * w * w * w * w;
        bandGain[s] = dark ? 1 + BAND_ADD * band : 1 - BAND_LIFT * band;
      }
    }

    const out = frame.data;
    for (let y = 0; y < gh; y++) {
      // The threshold pattern is fixed to the dot grid, not to the photo.
      const thresholds = (y & 7) * 8;
      const row = y * gw;
      for (let x = 0; x < gw; x++) {
        const i = row + x;
        const coverage = sweep === null ? density[i] : density[i] * bandGain[bandPos[i]];
        const j = i * 4;
        if (coverage > BAYER[thresholds + (x & 7)]) {
          const c = i * 3;
          out[j] = ink[c];
          out[j + 1] = ink[c + 1];
          out[j + 2] = ink[c + 2];
          out[j + 3] = 255;
        } else out[j + 3] = 0;
      }
    }

    smallCtx.putImageData(frame, 0, 0);
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    ctx.imageSmoothingEnabled = false;
    ctx.drawImage(small, 0, 0, canvas.width, canvas.height);
  }

  function sweepPhase(): number | null {
    if (!conf.loop || !loopReady || conf.reduced()) return null;
    return ((performance.now() - loopT0) / PERIOD_MS) * TAU;
  }

  // Paint once, then let the compositor run the CSS fade. The sweep only
  // starts after the fade, so there is one painter at a time.
  function draw(): void {
    if (stopped) return;
    if (!prepare()) return;
    paint(visible ? sweepPhase() : null);
    if (!visible) return;

    if (!revealed) {
      revealed = true;
      if (conf.reveal === false || conf.reduced()) wrap.classList.add('shown', 'instant');
      // Let the painted frame reach the compositor before the fade starts.
      else revealRaf = requestAnimationFrame(() => wrap.classList.add('shown'));
    }
    armLoop();
    startLoop();
  }

  function armLoop(): void {
    if (!conf.loop || conf.reduced() || loopArmed) return;
    loopArmed = true;
    loopDelay = window.setTimeout(() => {
      loopReady = true;
      loopT0 = performance.now();
      startLoop();
    }, SWEEP_DELAY_MS);
  }

  function startLoop(): void {
    if (stopped || loopRaf || !conf.loop || !loopReady || conf.reduced()) return;
    if (!inView || document.hidden) return;
    loopRaf = requestAnimationFrame(tick);
  }

  function tick(now: number): void {
    loopRaf = 0;
    if (stopped || !conf.loop || !loopReady || conf.reduced() || !inView || document.hidden) return;
    if (now - loopLast >= FRAME_MS) {
      loopLast = now;
      if (!wrap.isConnected) {
        stop();
        return;
      }
      paint(sweepPhase());
    }
    loopRaf = requestAnimationFrame(tick);
  }

  // ---- observations -------------------------------------------------------

  // 'view': hidden (but painted) until the art scrolls in.
  const view = new IntersectionObserver(
    (entries) => {
      if (!entries.some((entry) => entry.isIntersecting)) return;
      view.disconnect();
      if (visible) return;
      visible = true;
      draw();
    },
    { threshold: 0.35 },
  );

  // The sweep pauses while the art is off screen.
  const onScreen = new IntersectionObserver(
    (entries) => {
      inView = entries.some((entry) => entry.isIntersecting);
      if (inView) startLoop();
    },
    { threshold: 0 },
  );

  const onVisibility = (): void => startLoop();
  document.addEventListener('visibilitychange', onVisibility);

  // Flip the palette when the page flips the theme.
  const theme = new MutationObserver(() => {
    if (!wrap.isConnected) {
      stop();
      return;
    }
    if (img) draw();
  });

  // A run of layout changes (fonts, viewport, a settling stack) is one redraw.
  const resize = new ResizeObserver((entries) => {
    if (!wrap.isConnected) {
      stop();
      return;
    }
    if (!img) return;
    const { width, height } = entries[entries.length - 1].contentRect;
    if (Math.abs(width - lastW) < 1 && Math.abs(height - lastH) < 1) return;
    window.clearTimeout(settle);
    settle = window.setTimeout(draw, SETTLE_MS);
  });

  function stop(): void {
    if (stopped) return;
    stopped = true;
    view.disconnect();
    onScreen.disconnect();
    theme.disconnect();
    resize.disconnect();
    document.removeEventListener('visibilitychange', onVisibility);
    cancelAnimationFrame(revealRaf);
    cancelAnimationFrame(loopRaf);
    window.clearTimeout(settle);
    window.clearTimeout(loopDelay);
    wrap.remove();
  }

  resize.observe(wrap);
  theme.observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] });

  loadImage(conf.src)
    .then((loaded) => {
      if (stopped) return;
      img = loaded;
      if (conf.loop) {
        onScreen.observe(wrap);
      }
      if (!visible) view.observe(wrap);
      draw();
    })
    .catch(() => {});

  return stop;
}
