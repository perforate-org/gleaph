function ep(name) {
  let p = figma.root.children.find((c) => c.name === name);
  if (!p) {
    p = figma.createPage();
    p.name = name;
  }
  return p;
}

let cover = figma.root.children.find((c) => c.name === "00 Cover");
if (!cover) {
  const p1 = figma.root.children.find((c) => c.name === "Page 1");
  if (p1) {
    p1.name = "00 Cover";
    cover = p1;
  }
}
if (!cover) cover = ep("00 Cover");

const foundations = ep("01 Foundations");
const components = ep("02 Components");

await figma.setCurrentPageAsync(cover);

let maxRight = 0;
for (const n of cover.children) {
  maxRight = Math.max(maxRight, n.x + n.width);
}
const ox = maxRight > 0 ? maxRight + 80 : 80;
const oy = 80;

const coverFrame = figma.createFrame();
coverFrame.name = "Cover / Gleaph Design System";
coverFrame.resize(960, 520);
coverFrame.x = ox;
coverFrame.y = oy;
cover.appendChild(coverFrame);
coverFrame.layoutMode = "VERTICAL";
coverFrame.primaryAxisAlignItems = "CENTER";
coverFrame.counterAxisAlignItems = "CENTER";
coverFrame.paddingTop = 48;
coverFrame.paddingBottom = 48;
coverFrame.paddingLeft = 48;
coverFrame.paddingRight = 48;
coverFrame.itemSpacing = 16;

const cols = await figma.variables.getLocalVariableCollectionsAsync();
const semCol = cols.find((c) => c.name === "Gleaph / Semantic");
const primCol = cols.find((c) => c.name === "Gleaph / Primitives");
if (semCol && semCol.modes.length >= 1) {
  const lm = semCol.modes.find((m) => m.name === "light");
  const lightModeId = lm ? lm.modeId : semCol.modes[0].modeId;
  coverFrame.setExplicitVariableModeForCollection(semCol, lightModeId);
} else if (primCol && primCol.modes[0]) {
  coverFrame.setExplicitVariableModeForCollection(primCol, primCol.modes[0].modeId);
}

await figma.loadFontAsync({ family: "Inter", style: "Semi Bold" });
await figma.loadFontAsync({ family: "Inter", style: "Regular" });

let frameFills = [{ type: "SOLID", color: { r: 0.96, g: 0.97, b: 0.98 } }];
if (semCol) {
  const canvasVar = (await figma.variables.getLocalVariablesAsync("COLOR")).find(
    (v) => v.variableCollectionId === semCol.id && v.name === "color/bg/canvas"
  );
  if (canvasVar) {
    frameFills = [
      figma.variables.setBoundVariableForPaint(
        { type: "SOLID", color: { r: 0, g: 0, b: 0 } },
        "color",
        canvasVar
      ),
    ];
  }
}
coverFrame.fills = frameFills;

let textFills = [{ type: "SOLID", color: { r: 0.1, g: 0.12, b: 0.16 } }];
if (semCol) {
  const tp = (await figma.variables.getLocalVariablesAsync("COLOR")).find(
    (v) => v.variableCollectionId === semCol.id && v.name === "color/text/primary"
  );
  if (tp) {
    textFills = [
      figma.variables.setBoundVariableForPaint(
        { type: "SOLID", color: { r: 0, g: 0, b: 0 } },
        "color",
        tp
      ),
    ];
  }
}

const title = figma.createText();
title.characters = "Gleaph Design System";
title.fontSize = 36;
title.fontName = { family: "Inter", style: "Semi Bold" };
title.fills = textFills;
coverFrame.appendChild(title);
title.layoutSizingHorizontal = "HUG";
title.layoutSizingVertical = "HUG";

const sub = figma.createText();
sub.characters = "Tokens v0 - Primitives + Semantic (light / dark)";
sub.fontSize = 16;
sub.fontName = { family: "Inter", style: "Regular" };
sub.fills = textFills;
coverFrame.appendChild(sub);
sub.layoutSizingHorizontal = "HUG";
sub.layoutSizingVertical = "HUG";

const placeholder = figma.createFrame();
placeholder.name = "Placeholder / Foundations";
placeholder.resize(720, 120);
placeholder.layoutMode = "HORIZONTAL";
placeholder.primaryAxisAlignItems = "CENTER";
placeholder.counterAxisAlignItems = "CENTER";
placeholder.paddingLeft = 24;
placeholder.paddingRight = 24;
placeholder.paddingTop = 16;
placeholder.paddingBottom = 16;
let phStrokes = [{ type: "SOLID", color: { r: 0.85, g: 0.87, b: 0.9 } }];
if (semCol) {
  const bv = (await figma.variables.getLocalVariablesAsync("COLOR")).find(
    (v) => v.variableCollectionId === semCol.id && v.name === "color/border/default"
  );
  if (bv) {
    phStrokes = [
      figma.variables.setBoundVariableForPaint(
        { type: "SOLID", color: { r: 0, g: 0, b: 0 } },
        "color",
        bv
      ),
    ];
  }
}
placeholder.strokes = phStrokes;
placeholder.strokeWeight = 1;
placeholder.cornerRadius = 8;
coverFrame.appendChild(placeholder);
placeholder.layoutSizingHorizontal = "FILL";

const phText = figma.createText();
phText.fontName = { family: "Inter", style: "Regular" };
phText.characters = "Add color ramps on page 01 Foundations";
phText.fontSize = 14;
phText.fills = textFills;
placeholder.appendChild(phText);
phText.layoutSizingHorizontal = "HUG";
phText.layoutSizingVertical = "HUG";

return {
  mutatedPageIds: [cover.id, foundations.id, components.id],
  createdNodeIds: [coverFrame.id, title.id, sub.id, placeholder.id, phText.id],
};
