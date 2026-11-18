import { test } from "node:test";
import assert from "node:assert/strict";
import {
  CAPACITY,
  EMPTY_LEAF,
  camera,
  activeCamera,
  leafCamera,
  layout,
  nodeInfo,
  zoomAt,
  fillProjection,
  leafFillProjection,
} from "../web/tree-model.mjs";

test("a depth-32 tree has bounded rendering work at every scale and boundary", () => {
  for (const width of [320, 800, 2000])
    for (let zoom = 0; zoom <= 31; zoom += 0.25) {
      for (const center of [0, 1_338_859, CAPACITY / 2, CAPACITY]) {
        const frame = layout(camera(center, zoom), width, 550, 1_338_859);
        assert.ok(
          frame.nodes.length < 250,
          `render budget: ${frame.nodes.length}`,
        );
        for (const n of frame.nodes) {
          assert.ok(n.depth >= 0 && n.depth <= 32);
          assert.ok(n.first >= 0 && n.last < CAPACITY);
          assert.ok(n.occupied >= 0 && n.occupied <= n.capacity);
        }
      }
    }
});
test("leaf navigation reaches both ends, and occupancy is exact at the frontier", () => {
  for (const index of [0, 1_338_858, 1_338_859, CAPACITY - 1]) {
    const frame = layout(leafCamera(index), 1000, 550, 1_338_859);
    const leaf = frame.nodes.find((n) => n.depth === 32 && n.index === index);
    assert.ok(leaf && leaf.x >= 0 && leaf.x <= 1000);
    assert.equal(leaf.occupied, Number(index < 1_338_859));
  }
  assert.equal(nodeInfo(0, 0, 1_338_859).occupied, 1_338_859);
  assert.equal(EMPTY_LEAF.length, 64);
  assert.equal(EMPTY_LEAF.slice(0, 2), "02");
  const mobileLeaf = layout(leafCamera(100), 360, 465, 200).nodes.find(
    (n) => n.depth === 32 && n.index === 100,
  );
  assert.ok(
    mobileLeaf.y + 51 < 350,
    "leaf date must clear mobile navigation controls",
  );
});
test("zoom keeps the pointed leaf fixed away from boundaries", () => {
  const before = camera(CAPACITY / 2, 12);
  const after = zoomAt(before, 0.75, 700, 1000);
  const world = (view) =>
    view.center + (((700 - 500) / 920) * CAPACITY) / 2 ** view.zoom;
  assert.ok(Math.abs(world(before) - world(after)) < 0.000001);
  assert.equal(activeCamera(0).zoom, 27);
});
test("fill date uses constant absolute growth, including no-rate and full-tree cases", () => {
  assert.deepEqual(fillProjection(CAPACITY - 100, 10, 1000, true), {
    days: 10,
    timestamp: 865000,
  });
  for (const rate of [null, 0, -1, NaN])
    assert.equal(fillProjection(10, rate, 1000, true), null);
  assert.equal(fillProjection(10, 10, 1000, false), null);
  assert.equal(fillProjection(CAPACITY, 0, 1000, true).days, 0);
});

test("an empty leaf fills only after its own position is appended", () => {
  assert.deepEqual(leafFillProjection(100, 100, 24, 1000, true), {
    days: 1 / 24,
    timestamp: 4600,
  });
  assert.equal(leafFillProjection(123, 100, 24, 1000, true).days, 1);
  assert.deepEqual(
    leafFillProjection(CAPACITY - 1, 100, 24, 1000, true),
    fillProjection(100, 24, 1000, true),
  );
  for (const index of [99, -1, CAPACITY, 100.5])
    assert.equal(leafFillProjection(index, 100, 24, 1000, true), null);
  for (const rate of [null, 0])
    assert.equal(leafFillProjection(100, 100, rate, 1000, true), null);
  assert.equal(leafFillProjection(100, 100, 24, 1000, false), null);
});
