// Geometry uses leaf positions as coordinates. Never enumerate a complete level:
// the visible interval and five levels of detail keep work independent of capacity.
export const DEPTH = 32;
export const CAPACITY = 2 ** DEPTH;
export const MAX_ZOOM = 31;
export const EMPTY_LEAF = "02" + "00".repeat(31);
export const clamp = (value, min, max) => Math.max(min, Math.min(max, value));

// Indexing starts at Ironwood activation, not at genesis. The baseline has zero work done.
export function syncProgress(startHeight, indexedHeight, targetHeight) {
  if (
    ![startHeight, indexedHeight, targetHeight].every(Number.isSafeInteger) ||
    startHeight < 1 ||
    indexedHeight < startHeight - 1 ||
    targetHeight < startHeight ||
    indexedHeight > targetHeight
  ) return null;
  const total = targetHeight - startHeight + 1;
  const indexed = indexedHeight - startHeight + 1;
  return {
    indexed,
    total,
    remaining: total - indexed,
    percent: (indexed / total) * 100,
  };
}

export function camera(center, zoom, panY = 0) {
  zoom = clamp(zoom, 0, MAX_ZOOM);
  const half = CAPACITY / 2 ** zoom / 2;
  return {
    center: clamp(center, half, CAPACITY - half),
    zoom,
    panY: clamp(panY, -100, 100),
  };
}
export function activeCamera(count) {
  return camera(count / 2, Math.log2(CAPACITY / Math.max(count * 1.35, 32)));
}
export function leafCamera(index) {
  return camera(index + 0.5, 29);
}
export function nodeInfo(depth, index, count) {
  const capacity = 2 ** (DEPTH - depth);
  const first = index * capacity;
  return {
    depth,
    index,
    capacity,
    first,
    last: first + capacity - 1,
    occupied: clamp(count - first, 0, capacity),
  };
}
export function zoomAt(view, delta, x, width) {
  const usable = Math.max(1, width - 80);
  const offset = (x - width / 2) / usable;
  const anchor = view.center + (offset * CAPACITY) / 2 ** view.zoom;
  const zoom = clamp(view.zoom + delta, 0, MAX_ZOOM);
  return camera(anchor - (offset * CAPACITY) / 2 ** zoom, zoom, view.panY);
}
export function layout(view, width, height, count) {
  const usable = Math.max(1, width - 80);
  const span = CAPACITY / 2 ** view.zoom;
  const top = Math.min(27, Math.max(0, view.zoom - 0.7));
  const gap = Math.max(32, (height - (width < 440 ? 240 : 190)) / 5.5);
  const firstDepth = Math.max(0, Math.floor(top) - 1);
  const lastDepth = Math.min(DEPTH, Math.floor(view.zoom) + 5);
  const xAt = (position) =>
    ((position - view.center) / span) * usable + width / 2;
  const yAt = (depth) => 82 + (depth - top) * gap + view.panY;
  const left = clamp(
    view.center - ((width / 2 + 35) / usable) * span,
    0,
    CAPACITY,
  );
  const right = clamp(
    view.center + ((width / 2 + 35) / usable) * span,
    0,
    CAPACITY,
  );
  const nodes = [];
  for (let depth = firstDepth; depth <= lastDepth; depth++) {
    const size = 2 ** (DEPTH - depth);
    const start = Math.max(0, Math.floor(left / size));
    const end = Math.min(2 ** depth - 1, Math.floor(right / size));
    for (let index = start; index <= end; index++) {
      const node = nodeInfo(depth, index, count);
      nodes.push({
        ...node,
        x: xAt(node.first + size / 2),
        y: yAt(depth),
        spacing: (size / span) * usable,
      });
    }
  }
  return {
    nodes,
    firstDepth,
    lastDepth,
    left: Math.floor(left),
    right: Math.min(CAPACITY - 1, Math.ceil(right)),
    xAt,
    yAt,
    span,
  };
}
// Linear projection of the observed absolute additions per rolling 24 hours.
// No compounding; a missing/zero rate or stale chain has no finite estimate.
export function fillProjection(count, added, observedAt, fresh) {
  return projectAdditions(
    Math.max(0, CAPACITY - count),
    added,
    observedAt,
    fresh,
  );
}

export function leafFillProjection(index, count, added, observedAt, fresh) {
  if (
    !Number.isSafeInteger(index) ||
    index < count ||
    index >= CAPACITY ||
    index < 0
  )
    return null;
  // Filling index N requires N + 1 confirmed leaves, including that leaf itself.
  return projectAdditions(index + 1 - count, added, observedAt, fresh);
}

function projectAdditions(remaining, added, observedAt, fresh) {
  if (!fresh || added === null || !Number.isFinite(added) || added < 0)
    return null;
  if (remaining === 0) return { timestamp: observedAt, days: 0 };
  if (added === 0) return null;
  const days = remaining / added;
  const timestamp = observedAt + days * 86_400;
  if (!Number.isFinite(timestamp) || timestamp * 1000 > 8.64e15) return null;
  return { timestamp, days };
}
