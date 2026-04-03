const cols = await figma.variables.getLocalVariableCollectionsAsync();
const primCol = cols.find((c) => c.name === "Gleaph / Primitives");
if (!primCol) {
  throw new Error("Missing collection Gleaph / Primitives — run primitives step first");
}

const allVars = await figma.variables.getLocalVariablesAsync();
const primByName = {};
for (const v of allVars) {
  if (v.variableCollectionId === primCol.id) primByName[v.name] = v;
}

const createdVariableIds = [];
const semCol = figma.variables.createVariableCollection("Gleaph / Semantic");
const lightId = semCol.modes[0].modeId;
semCol.renameMode(lightId, "light");
const darkId = semCol.addMode("dark");

function mkSem(name, scopes, web, lightPrim, darkPrim) {
  const v = figma.variables.createVariable(name, semCol, "COLOR");
  v.scopes = scopes;
  v.setVariableCodeSyntax("WEB", web);
  v.setValueForMode(lightId, {
    type: "VARIABLE_ALIAS",
    id: primByName[lightPrim].id,
  });
  v.setValueForMode(darkId, {
    type: "VARIABLE_ALIAS",
    id: primByName[darkPrim].id,
  });
  createdVariableIds.push(v.id);
}

mkSem(
  "color/bg/canvas",
  ["FRAME_FILL", "SHAPE_FILL"],
  "var(--color-bg-canvas)",
  "color/gray/50",
  "color/gray/950"
);
mkSem(
  "color/bg/surface",
  ["FRAME_FILL", "SHAPE_FILL"],
  "var(--color-bg-surface)",
  "color/gray/0",
  "color/gray/900"
);
mkSem(
  "color/text/primary",
  ["TEXT_FILL"],
  "var(--color-text-primary)",
  "color/gray/900",
  "color/gray/50"
);
mkSem(
  "color/text/secondary",
  ["TEXT_FILL"],
  "var(--color-text-secondary)",
  "color/gray/600",
  "color/gray/300"
);
mkSem(
  "color/accent/primary",
  ["FRAME_FILL", "SHAPE_FILL"],
  "var(--color-accent-primary)",
  "color/teal/600",
  "color/teal/400"
);
mkSem(
  "color/border/default",
  ["STROKE_COLOR"],
  "var(--color-border-default)",
  "color/gray/200",
  "color/gray/700"
);
mkSem(
  "color/focus/ring",
  ["STROKE_COLOR"],
  "var(--color-focus-ring)",
  "color/teal/500",
  "color/teal/300"
);

return {
  step: "semantic",
  collectionId: semCol.id,
  semanticLightId: lightId,
  semanticDarkId: darkId,
  createdVariableIds,
};
