const LAYER_X = [-4.4, -2.5, -0.5, 1.5, 3.5] as const;

export function layerPosition(
  layer: number,
  index: number,
  count: number,
): [number, number, number] {
  const x = LAYER_X[layer] ?? -4.4 + layer * 2;
  if (count <= 1) {
    return [x, 0, 0];
  }
  const spread = Math.min(6.4, 1.2 + count * 0.72);
  const y = -spread / 2 + (index / (count - 1)) * spread;
  return [x, y, 0];
}

export function liveFanOutPosition(
  index: number,
  count: number,
  role: "source" | "target",
): [number, number, number] {
  if (role === "source") {
    return [-3.6, 0, 0];
  }
  return layerPosition(2, index, Math.max(count, 1));
}
