// build-component-library.js — populate Foundations + Components pages
// Usage: node build-component-library.js [output.fig]   (default: overwrite source)
import { parseFig, encodeFigParts, assembleCanvasFig, createFigZip } from 'openfig-core';
import zstdPkg from "zstd-codec";
const zstdRun = zstdPkg.ZstdCodec.run;
import { readFileSync, writeFileSync } from 'fs';

const SRC = new URL('../gleaph-design-system.fig', import.meta.url).pathname;
const OUT = process.argv[2] ?? SRC;
const SID = 555001001;
let nextLocal = 100;
const newGuid = () => ({ sessionID: SID, localID: nextLocal++ });

const doc = parseFig(new Uint8Array(readFileSync(SRC)));

// ---------- token maps ----------
const primByName = new Map();
const semByName = new Map();
const primLightColor = new Map();
for (const n of doc.nodes) {
  if (n.type !== 'VARIABLE') continue;
  if (n.guid.sessionID === 4) {
    primByName.set(n.name, n.guid);
    const c = n.variableDataValues?.entries?.[0]?.variableData?.value?.colorValue;
    if (c) primLightColor.set(n.name, c);
  } else if (n.guid.sessionID === 6) semByName.set(n.name, n.guid);
}
const SEM_FALLBACK = {
  'color/background/canvas': 'color/sand/100',
  'color/background/surface': 'color/sand/0',
  'color/text/primary': 'color/sand/900',
  'color/text/secondary': 'color/sand/600',
  'color/accent/primary': 'color/blue/500',
  'color/border/default': 'color/sand/200',
  'color/focus/ring': 'color/blue/300',
};
function boundPaint(guid, primName) {
  return { type: 'SOLID', visible: true, opacity: 1, blendMode: 'NORMAL',
    color: primLightColor.get(primName),
    colorVar: { value: { alias: { guid } }, dataType: 'ALIAS', resolvedDataType: 'COLOR' } };
}
const sem = (name) => boundPaint(semByName.get(name), SEM_FALLBACK[name]);
const solidPrim = (name) => ({ type: 'SOLID', visible: true, opacity: 1, blendMode: 'NORMAL',
  color: primLightColor.get(name) });

// ---------- fonts ----------
const UI = { family: 'Helvetica Neue', style: 'Regular', postscript: 'HelveticaNeue' };
const SERIF_M = { family: 'Noto Serif', style: 'Medium', postscript: 'NotoSerif-Medium' };

// ---------- node factories ----------
const nodeChanges = doc.message.nodeChanges;
function maxSiblingPos(key) {
  let max = null;
  for (const p of doc.childrenMap.get(key) ?? []) {
    const pos = p.parentIndex?.position ?? '';
    if (max === null || pos > max) max = pos;
  }
  return max;
}
class Alloc {
  constructor(parentGuid) {
    this.prefix = maxSiblingPos(parentGuid.sessionID + ':' + parentGuid.localID);
    this.n = 0;
  }
  next() {
    this.n += 1;
    const idx = String(this.n).padStart(4, '0');
    return this.prefix ? `${this.prefix}~${idx}` : `u${idx}`;
  }
}
function base(type, name, x, y, w, h, parent, alloc) {
  const pg = parent && parent.guid ? parent.guid : parent;
  const n = { guid: newGuid(), phase: 'CREATED',
    parentIndex: { guid: pg, position: alloc.next() },
    type, name, visible: true, opacity: 1,
    size: { x: w, y: h },
    transform: { m00: 1, m01: 0, m02: x, m10: 0, m11: 1, m12: y },
    mask: false };
  nodeChanges.push(n);
  return n;
}
function frame(parent, alloc, o) {
  const n = base('FRAME', o.name, o.x, o.y, o.w, o.h, parent, alloc);
  if (o.fills?.length) n.fillPaints = o.fills;
  if (o.stroke) {
    n.strokePaints = [o.stroke];
    n.strokeWeight = o.strokeWeight ?? 1;
    n.strokeAlign = 'INSIDE';
    n.strokeJoin = 'MITER';
  }
  if (o.radius) n.cornerRadius = o.radius;
  n.clipsContent = true;
  return n;
}
function text(parent, alloc, o) {
  const lines = o.str.split('\n').map(() => ({
    lineType: 'PLAIN', styleId: 0, indentationLevel: 0,
    sourceDirectionality: 'AUTO', listStartOffset: 0, isFirstLineOfList: false }));
  const n = base('TEXT', o.name ?? o.str.split('\n')[0].slice(0, 40), o.x, o.y, o.w, o.h, parent, alloc);
  Object.assign(n, {
    fontSize: o.size,
    textAlignHorizontal: o.alignH ?? 'LEFT',
    textAlignVertical: o.alignV ?? 'TOP',
    lineHeight: { value: o.lh ?? 100, units: 'PERCENT' },
    fontName: { family: o.font.family, style: o.font.style, postscript: o.font.postscript },
    textData: { characters: o.str, lines },
    fillPaints: [o.paint] });
  return n;
}
const onAccent = () => sem('color/text/on-accent');

// ---------- new semantic variable color/text/on-accent ----------
function addOnAccentVariable() {
  const canvas = { sessionID: 0, localID: 2 };
  const g = newGuid();
  const pos = `${maxSiblingPos('0:2') ?? 'u'}~9999`;
  const version = `${g.sessionID}:${g.localID}`;
  const entry = (modeLocalID) => ({
    modeID: { sessionID: 6, localID: modeLocalID },
    variableData: { value: { alias: { guid: primByName.get('color/sand/0') } },
      dataType: 'ALIAS', resolvedDataType: 'COLOR' } });
  nodeChanges.push({
    guid: g, phase: 'CREATED',
    parentIndex: { guid: canvas, position: pos },
    type: 'VARIABLE', name: 'color/text/on-accent',
    isPublishable: true, version, userFacingVersion: version,
    sortPosition: pos,
    variableSetID: { guid: { sessionID: 6, localID: 2 } },
    variableResolvedType: 'COLOR',
    variableDataValues: { entries: [entry(0), entry(1)] },
    variableScopes: ['TEXT_FILL', 'SHAPE_FILL'],
    codeSyntax: { entries: [{ platform: 'WEB', value: 'var(--color-text-on-accent)' }] } });
  semByName.set('color/text/on-accent', g);
  SEM_FALLBACK['color/text/on-accent'] = 'color/sand/0';
}

// ---------- component builders ----------
function button(parent, alloc, { name, str, x, y, w, h = 52, variant }) {
  const fills =
    variant === 'primary' ? [sem('color/accent/primary')] :
    variant === 'secondary' ? [sem('color/background/surface')] : [];
  const stroke = variant === 'secondary' ? sem('color/border/default') : undefined;
  const f = frame(parent, alloc, { name, x, y, w, h, fills, stroke, radius: 4 });
  const a = new Alloc(f.guid);
  text(f, a, { str, name: 'Label', x: 0, y: 0, w, h, size: 16,
    alignH: 'CENTER', alignV: 'CENTER', font: UI,
    paint: variant === 'primary' ? onAccent()
      : variant === 'ghost' ? sem('color/accent/primary')
      : sem('color/text/primary') });
  return f;
}
function inputField(parent, alloc, { name, str, x, y, w, h = 52, focused = false }) {
  const f = frame(parent, alloc, { name, x, y, w, h,
    fills: [sem('color/background/surface')],
    stroke: focused ? sem('color/focus/ring') : sem('color/border/default'),
    strokeWeight: focused ? 2 : 1, radius: 4 });
  const a = new Alloc(f.guid);
  text(f, a, { str, name: focused ? 'Value' : 'Placeholder',
    x: 16, y: 0, w: w - 32, h, size: 15, alignV: 'CENTER', font: UI,
    paint: focused ? sem('color/text/primary') : sem('color/text/secondary') });
  return f;
}
function card(parent, alloc, { name, title, body, link, x, y, w, h }) {
  const f = frame(parent, alloc, { name, x, y, w, h,
    fills: [sem('color/background/surface')],
    stroke: sem('color/border/default'), radius: 8 });
  const a = new Alloc(f.guid);
  text(f, a, { str: title, name: 'Title', x: 24, y: 24, w: w - 48, h: 28,
    size: 20, font: SERIF_M, paint: sem('color/text/primary') });
  text(f, a, { str: body, name: 'Body', x: 24, y: 62, w: w - 48, h: 72,
    size: 14, lh: 145, font: UI, paint: sem('color/text/secondary') });
  text(f, a, { str: link, name: 'Link', x: 24, y: h - 52, w: 160, h: 20,
    size: 14, font: UI, paint: sem('color/accent/primary') });
  return f;
}
function badge(parent, alloc, { str, x, y, w, style }) {
  const h = 28;
  let fills, stroke, labelPaint;
  if (style === 'accent') { fills = [sem('color/accent/primary')]; labelPaint = onAccent(); }
  else if (style === 'tinted') { fills = [solidPrim('color/clay/400')]; labelPaint = sem('color/text/primary'); }
  else { fills = [sem('color/background/surface')]; stroke = sem('color/border/default'); labelPaint = sem('color/text/secondary'); }
  const f = frame(parent, alloc, { name: `Badge / ${str}`, x, y, w, h, fills, stroke, radius: 14 });
  const a = new Alloc(f.guid);
  text(f, a, { str, name: 'Label', x: 0, y: 0, w, h, size: 12,
    alignH: 'CENTER', alignV: 'CENTER', font: UI, paint: labelPaint });
  return f;
}
function sectionLabel(parent, alloc, str, x, y) {
  text(parent, alloc, { str, name: `Section / ${str}`, x, y, w: 260, h: 14,
    size: 11, font: UI, paint: sem('color/text/secondary') });
}

// ---------- galleries ----------
const COMPONENTS = { sessionID: 7, localID: 3 };
const FOUNDATIONS = { sessionID: 7, localID: 2 };

function buildDesktopGallery(pageAlloc) {
  const g = frame(COMPONENTS, pageAlloc, {
    name: 'Gallery / Desktop · Landscape', x: 80, y: 200, w: 1440, h: 900,
    fills: [sem('color/background/canvas')], radius: 8 });
  const a = new Alloc(g.guid);
  text(g, a, { str: 'Desktop · Landscape 1440×900', name: 'Gallery Title',
    x: 64, y: 56, w: 520, h: 34, size: 26, font: SERIF_M, paint: sem('color/text/primary') });
  text(g, a, { str: 'Buttons · Inputs · Card · Badges — every fill is bound to a semantic token',
    name: 'Gallery Subtitle', x: 64, y: 98, w: 640, h: 18, size: 13, font: UI,
    paint: sem('color/text/secondary') });

  sectionLabel(g, a, 'BUTTONS', 64, 152);
  button(g, a, { name: 'Button / Primary', str: 'Get started', x: 64, y: 180, w: 190, variant: 'primary' });
  button(g, a, { name: 'Button / Secondary', str: 'Documentation', x: 270, y: 180, w: 190, variant: 'secondary' });
  button(g, a, { name: 'Button / Ghost', str: 'Learn more', x: 476, y: 180, w: 150, variant: 'ghost' });

  sectionLabel(g, a, 'INPUTS', 680, 152);
  inputField(g, a, { name: 'Input / Placeholder', str: 'Graph name', x: 680, y: 180, w: 300 });
  inputField(g, a, { name: 'Input / Focused', str: 'gleaph-mainnet', x: 680, y: 250, w: 300, focused: true });

  sectionLabel(g, a, 'CARD', 1040, 152);
  card(g, a, { name: 'Card / Default', title: 'Sovereignty',
    body: 'Run your graph database on the Internet Computer,\nwithout depending on a centralized service.',
    link: 'Read more →', x: 1040, y: 180, w: 336, h: 200 });

  sectionLabel(g, a, 'BADGES', 64, 330);
  badge(g, a, { str: 'v0.9', x: 64, y: 358, w: 64, style: 'tinted' });
  badge(g, a, { str: 'Beta', x: 144, y: 358, w: 72, style: 'outlined' });
  badge(g, a, { str: 'ICP native', x: 232, y: 358, w: 110, style: 'accent' });
}

function buildMobileGallery(pageAlloc) {
  const g = frame(COMPONENTS, pageAlloc, {
    name: 'Gallery / Mobile · Portrait', x: 1560, y: 200, w: 390, h: 844,
    fills: [sem('color/background/canvas')], radius: 8 });
  const a = new Alloc(g.guid);
  text(g, a, { str: 'Mobile · Portrait 390×844', name: 'Gallery Title',
    x: 24, y: 32, w: 340, h: 28, size: 20, font: SERIF_M, paint: sem('color/text/primary') });
  text(g, a, { str: 'Same tokens · same components', name: 'Gallery Subtitle',
    x: 24, y: 66, w: 300, h: 16, size: 12, font: UI, paint: sem('color/text/secondary') });

  button(g, a, { name: 'Button / Primary', str: 'Get started', x: 24, y: 120, w: 342, variant: 'primary' });
  button(g, a, { name: 'Button / Secondary', str: 'Documentation', x: 24, y: 184, w: 342, variant: 'secondary' });
  button(g, a, { name: 'Button / Ghost', str: 'Learn more', x: 24, y: 248, w: 342, variant: 'ghost' });

  inputField(g, a, { name: 'Input / Placeholder', str: 'Graph name', x: 24, y: 330, w: 342 });
  inputField(g, a, { name: 'Input / Focused', str: 'gleaph-mainnet', x: 24, y: 394, w: 342, focused: true });

  card(g, a, { name: 'Card / Default', title: 'Sovereignty',
    body: 'Run your graph database on the Internet\nComputer without centralized services.',
    link: 'Read more →', x: 24, y: 470, w: 342, h: 190 });

  sectionLabel(g, a, 'BADGES', 24, 692);
  badge(g, a, { str: 'v0.9', x: 24, y: 716, w: 64, style: 'tinted' });
  badge(g, a, { str: 'Beta', x: 104, y: 716, w: 72, style: 'outlined' });
  badge(g, a, { str: 'ICP native', x: 192, y: 716, w: 110, style: 'accent' });
}

function buildFoundationsBoard(pageAlloc) {
  const b = frame(FOUNDATIONS, pageAlloc, {
    name: 'Board / Color Tokens', x: 80, y: 80, w: 1240, h: 720,
    fills: [sem('color/background/canvas')], radius: 8 });
  const a = new Alloc(b.guid);
  text(b, a, { str: 'Foundations · Color Tokens', name: 'Board Title',
    x: 48, y: 40, w: 500, h: 38, size: 28, font: SERIF_M, paint: sem('color/text/primary') });

  sectionLabel(b, a, 'SEMANTIC — LIGHT / DARK ADAPTIVE', 48, 116);
  [['background/canvas','color/background/canvas'],
   ['background/surface','color/background/surface'],
   ['text/primary','color/text/primary'],
   ['text/secondary','color/text/secondary'],
   ['accent/primary','color/accent/primary'],
   ['border/default','color/border/default'],
   ['focus/ring','color/focus/ring']].forEach(([labelText, token], i) => {
    const x = 48 + i * 92;
    frame(b, a, { name: `Swatch / ${token}`, x, y: 144, w: 56, h: 56,
      fills: [sem(token)], radius: 4 });
    text(b, a, { str: labelText, name: 'Swatch Label', x, y: 206, w: 84, h: 28,
      size: 10, lh: 120, font: UI, paint: sem('color/text/secondary') });
  });

  const scaleRow = (y, title, names) => {
    sectionLabel(b, a, title, 48, y);
    names.forEach((name, i) => {
      const x = 48 + i * 64;
      frame(b, a, { name: `Swatch / ${name}`, x, y: y + 28, w: 48, h: 48,
        fills: [solidPrim(name)], radius: 4 });
      text(b, a, { str: name.split('/').pop(), name: 'Swatch Label', x, y: y + 82, w: 48, h: 12,
        size: 9, alignH: 'CENTER', font: UI, paint: sem('color/text/secondary') });
    });
  };
  scaleRow(270, 'PRIMITIVES — SAND', ['color/sand/0','color/sand/50','color/sand/100','color/sand/200','color/sand/300','color/sand/400','color/sand/500','color/sand/600','color/sand/700','color/sand/800','color/sand/900','color/sand/950']);
  scaleRow(430, 'PRIMITIVES — BLUE', ['color/blue/300','color/blue/400','color/blue/500','color/blue/600','color/blue/700']);
  scaleRow(570, 'PRIMITIVES — CLAY', ['color/clay/300','color/clay/400','color/clay/500']);
}

// ---------- main ----------
addOnAccentVariable();
{
  const pageComponents = new Alloc(COMPONENTS);
  text(COMPONENTS, pageComponents, { str: 'Component Gallery', name: 'Page Title',
    x: 80, y: 80, w: 600, h: 42, size: 32, font: SERIF_M, paint: sem('color/text/primary') });
  text(COMPONENTS, pageComponents, { str: 'Bound to Gleaph semantic tokens · light/dark adaptive',
    name: 'Page Subtitle', x: 80, y: 130, w: 520, h: 20, size: 14, font: UI,
    paint: sem('color/text/secondary') });
  buildDesktopGallery(pageComponents);
  buildMobileGallery(pageComponents);
  buildFoundationsBoard(new Alloc(FOUNDATIONS));
}

const parts = encodeFigParts(doc);
zstdRun((zstd) => {
  const simple = new zstd.Simple();
  const messageCompressed = simple.compress(parts.messageRaw, 3);
  const canvasFig = assembleCanvasFig({
    prelude: parts.prelude,
    version: parts.version,
    schemaCompressed: parts.schemaCompressed,
    messageCompressed,
    passThrough: parts.passThrough });
  const figZip = createFigZip({ canvasFig, meta: doc.meta,
    thumbnail: doc.thumbnail, images: doc.images });
  writeFileSync(OUT, figZip);
});
console.log(`OK wrote ${OUT} (${nodeChanges.length} nodes total)`);
