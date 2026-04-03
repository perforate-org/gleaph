function hexToRgba(hex) {
  const h = hex.replace("#", "");
  const n = parseInt(h, 16);
  return {
    r: ((n >> 16) & 255) / 255,
    g: ((n >> 8) & 255) / 255,
    b: (n & 255) / 255,
    a: 1,
  };
}

const createdVariableIds = [];

function mkPrimColor(collection, modeId, name, hex, web) {
  const v = figma.variables.createVariable(name, collection, "COLOR");
  v.scopes = [];
  v.setValueForMode(modeId, hexToRgba(hex));
  v.setVariableCodeSyntax("WEB", web);
  createdVariableIds.push(v.id);
  return v;
}

function mkPrimFloat(collection, modeId, name, px, web) {
  const v = figma.variables.createVariable(name, collection, "FLOAT");
  v.scopes = name.includes("radius") ? ["CORNER_RADIUS"] : ["GAP"];
  v.setValueForMode(modeId, px);
  v.setVariableCodeSyntax("WEB", web);
  createdVariableIds.push(v.id);
  return v;
}

const primCol = figma.variables.createVariableCollection("Gleaph / Primitives");
const primMode = primCol.modes[0].modeId;
primCol.renameMode(primMode, "default");

const grays = [
  ["0", "#ffffff"],
  ["50", "#f6f7f9"],
  ["100", "#eceff3"],
  ["200", "#d8dde4"],
  ["300", "#b8c0cc"],
  ["400", "#8f9aad"],
  ["500", "#6b778c"],
  ["600", "#525f73"],
  ["700", "#3d4758"],
  ["800", "#2a3140"],
  ["900", "#1a1f28"],
  ["950", "#0b0d0f"],
];
for (const [stop, hex] of grays) {
  mkPrimColor(
    primCol,
    primMode,
    "color/gray/" + stop,
    hex,
    "var(--color-primitive-gray-" + stop + ")"
  );
}

mkPrimColor(primCol, primMode, "color/teal/300", "#4ff0d4", "var(--color-primitive-teal-300)");
mkPrimColor(primCol, primMode, "color/teal/400", "#1dd8bf", "var(--color-primitive-teal-400)");
mkPrimColor(primCol, primMode, "color/teal/500", "#0fb5a0", "var(--color-primitive-teal-500)");
mkPrimColor(primCol, primMode, "color/teal/600", "#0a9083", "var(--color-primitive-teal-600)");
mkPrimColor(primCol, primMode, "color/teal/700", "#0c736a", "var(--color-primitive-teal-700)");

mkPrimFloat(primCol, primMode, "space/4", 16, "var(--space-primitive-4)");
mkPrimFloat(primCol, primMode, "space/6", 24, "var(--space-primitive-6)");
mkPrimFloat(primCol, primMode, "radius/md", 6, "var(--radius-primitive-md)");

return {
  step: "primitives",
  collectionId: primCol.id,
  modeId: primMode,
  createdVariableIds,
};
