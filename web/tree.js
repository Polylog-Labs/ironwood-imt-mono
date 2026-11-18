import {
  DEPTH,
  CAPACITY,
  camera,
  activeCamera,
  leafCamera,
  nodeInfo,
  zoomAt,
  layout,
  clamp,
} from "./tree-model.mjs";

const number = new Intl.NumberFormat("en-US");
const compact = new Intl.NumberFormat("en-US", {
  notation: "compact",
  maximumFractionDigits: 1,
});
const shortDate = (timestamp) =>
  new Date(timestamp * 1000).toLocaleDateString("en-GB", {
    day: "2-digit",
    month: "short",
    year: "numeric",
    timeZone: "UTC",
  });
const utcTime = (timestamp) =>
  `${new Date(timestamp * 1000).toISOString().slice(11, 19)} UTC`;
const byId = (id) => document.getElementById(id);
const sameNode = (a, b) => a && b && a.depth === b.depth && a.index === b.index;

export class TreeView {
  constructor(canvas, { onSelect, onSnapshotGone }) {
    this.canvas = canvas;
    this.ctx = canvas.getContext("2d");
    this.onSelect = onSelect;
    this.onSnapshotGone = onSnapshotGone;
    this.view = camera(CAPACITY / 2, 0);
    this.target = { ...this.view };
    this.count = 0;
    this.leaves = new Map();
    this.pointers = new Map();
    this.reducedMotion = matchMedia("(prefers-reduced-motion: reduce)");
    this.width = 1;
    this.height = 1;
    this.frameId = 0;
    this.selected = null;
    this.hover = null;
    this.resizeObserver = new ResizeObserver(() => this.resize());
    this.resizeObserver.observe(canvas);
    this.bindInput();
  }

  resize() {
    const rect = this.canvas.getBoundingClientRect();
    this.width = rect.width;
    this.height = rect.height;
    this.dpr = Math.min(devicePixelRatio || 1, 2);
    this.canvas.width = Math.round(this.width * this.dpr);
    this.canvas.height = Math.round(this.height * this.dpr);
    this.invalidate();
    this.scheduleLoad();
  }

  setSnapshot(snapshot, recentLeaves) {
    const changed = this.snapshot?.checkpoint.hash !== snapshot.checkpoint.hash;
    const first = !this.snapshot;
    this.snapshot = snapshot;
    this.count = snapshot.checkpoint.leaf_count;
    if (changed) {
      this.leafRequest?.abort();
      this.leaves.clear();
      this.requestKey = null;
    }
    for (const leaf of recentLeaves) this.leaves.set(leaf.position, leaf);
    if (first) this.goActive(false);
    this.invalidate();
    this.scheduleLoad();
  }

  select(node) {
    this.selected = nodeInfo(node.depth, node.index, this.count);
    this.onSelect(this.selected);
    this.invalidate();
  }

  moveTo(next, animate = true) {
    this.target = next;
    if (!animate || this.reducedMotion.matches) this.view = { ...next };
    this.hideTooltip();
    this.invalidate();
    this.scheduleLoad();
  }

  markView(name) {
    for (const mode of ["whole", "active", "latest"]) {
      const button = byId(`view-${mode}`);
      button.classList.toggle("active", name === mode);
      button.setAttribute("aria-pressed", String(name === mode));
    }
  }

  goWhole() {
    this.markView("whole");
    this.moveTo(camera(CAPACITY / 2, 0));
  }
  goActive(animate = true) {
    this.markView("active");
    this.moveTo(activeCamera(this.count), animate);
  }
  goLeaf(index, latest = false) {
    this.markView(latest ? "latest" : null);
    this.moveTo(leafCamera(index));
    this.select(nodeInfo(DEPTH, index, this.count));
  }
  explore(node) {
    this.markView(null);
    this.moveTo(
      camera(node.first + node.capacity / 2, Math.min(31, node.depth + 1)),
    );
  }
  zoom(delta, x = this.width / 2) {
    this.markView(null);
    this.moveTo(zoomAt(this.target, delta, x, this.width));
  }
  pan(dx, dy) {
    const span = CAPACITY / 2 ** this.target.zoom;
    this.moveTo(
      camera(
        this.target.center - (dx / Math.max(1, this.width - 80)) * span,
        this.target.zoom,
        this.target.panY + dy,
      ),
      false,
    );
    this.markView(null);
  }

  invalidate() {
    if (!this.frameId) this.frameId = requestAnimationFrame(() => this.draw());
  }

  draw() {
    this.frameId = 0;
    const span = CAPACITY / 2 ** this.target.zoom;
    const moving =
      Math.abs(this.view.zoom - this.target.zoom) > 0.0005 ||
      Math.abs(this.view.center - this.target.center) > span / 10000 ||
      Math.abs(this.view.panY - this.target.panY) > 0.1;
    if (moving) {
      for (const key of ["zoom", "center", "panY"])
        this.view[key] += (this.target[key] - this.view[key]) * 0.22;
    } else {
      this.view = { ...this.target };
    }
    const ctx = this.ctx;
    ctx.setTransform(this.dpr, 0, 0, this.dpr, 0, 0);
    ctx.clearRect(0, 0, this.width, this.height);
    if (!this.snapshot) {
      this.hits = [];
      return;
    }
    const frame = layout(this.view, this.width, this.height, this.count);
    this.frame = frame;
    const bottom = this.width < 440 ? this.height - 125 : this.height - 85;
    const lastVisibleDepth = Math.max(
      0,
      ...frame.nodes.filter((n) => n.y <= bottom).map((n) => n.depth),
    );
    const selected = this.selected;
    const latestPosition = this.count - 1;
    const onLatestPath = (node) =>
      this.count > 0 &&
      node.first <= latestPosition &&
      node.last >= latestPosition;
    const onPath = (node) =>
      selected &&
      node.depth <= selected.depth &&
      node.first <= selected.first &&
      node.last >= selected.last;

    // A maximum of a few hundred primitives, even when viewing four billion leaves.
    for (const n of frame.nodes) {
      if (!n.depth || n.y < -100 || n.y > bottom) continue;
      const parent = nodeInfo(n.depth - 1, Math.floor(n.index / 2), this.count);
      const px = frame.xAt(parent.first + parent.capacity / 2);
      const py = frame.yAt(n.depth - 1);
      ctx.beginPath();
      ctx.moveTo(px, py + 4);
      ctx.bezierCurveTo(
        px,
        py + (n.y - py) * 0.48,
        n.x,
        n.y - (n.y - py) * 0.48,
        n.x,
        n.y - 5,
      );
      ctx.strokeStyle = onLatestPath(n)
        ? "#8cdef2b3"
        : onPath(n)
          ? "#c0f780aa"
          : n.occupied
            ? "#8eb46270"
            : "#41553965";
      ctx.lineWidth = onLatestPath(n) || onPath(n) ? 1.3 : 0.85;
      ctx.stroke();
    }

    this.hits = [];
    for (const n of frame.nodes) {
      if (n.x < -20 || n.x > this.width + 20 || n.y < 45 || n.y > bottom)
        continue;
      this.hits.push(n);
      const chosen = sameNode(selected, n);
      const hovered = sameNode(this.hover, n);
      const latest = onLatestPath(n);
      const leaf = n.depth === DEPTH;
      const r = chosen || hovered ? 6 : leaf ? 4 : 3.5;
      ctx.save();
      if (chosen || hovered || latest) {
        ctx.shadowBlur = chosen || hovered ? 14 : 5;
        ctx.shadowColor = latest ? "#8cdef255" : "#c0f78080";
      }
      ctx.strokeStyle = latest
        ? "#a4e8fa"
        : chosen || hovered
          ? "#e2ffb3"
          : n.occupied
            ? "#b4e77d"
            : "#668258";
      ctx.fillStyle = latest ? "#8cdef2" : n.occupied ? "#b4e77d" : "#10170f";
      ctx.lineWidth = 1;
      ctx.beginPath();
      if (leaf) ctx.rect(n.x - r, n.y - r, r * 2, r * 2);
      else {
        ctx.moveTo(n.x, n.y - r);
        ctx.lineTo(n.x + r, n.y);
        ctx.lineTo(n.x, n.y + r);
        ctx.lineTo(n.x - r, n.y);
        ctx.closePath();
      }
      ctx.fill();
      ctx.stroke();
      ctx.shadowBlur = 0;
      if (chosen) {
        ctx.strokeStyle = latest ? "#8cdef255" : "#c0f78055";
        ctx.strokeRect(n.x - 11, n.y - 11, 22, 22);
      }
      if (n.occupied && n.occupied < n.capacity) {
        ctx.beginPath();
        ctx.arc(n.x, n.y, r + 4, 0, Math.PI * 2);
        ctx.strokeStyle = latest ? "#8cdef255" : "#9cb97255";
        ctx.stroke();
      }
      ctx.font = "9px ui-monospace, SFMono-Regular, Menlo, monospace";
      ctx.textAlign = "center";
      ctx.fillStyle = n.occupied ? "#a6be91" : "#718764";
      if (leaf && n.spacing >= 68) {
        ctx.fillText(`#${number.format(n.index)}`, n.x, n.y + 23);
        if (n.spacing >= 100) {
          ctx.font = "8px ui-monospace, SFMono-Regular, Menlo, monospace";
          ctx.fillStyle = "#7c9368";
          const info = this.leaves.get(n.index);
          ctx.fillText(
            n.occupied
              ? info
                ? shortDate(info.time)
                : "Loading…"
              : "Uncommitted",
            n.x,
            n.y + 39,
          );
          if (n.occupied && info) {
            ctx.fillText(utcTime(info.time), n.x, n.y + 51);
          }
        }
      } else if (!leaf && n.spacing >= 105) {
        ctx.fillText(
          n.depth === 0 ? "ROOT" : `${compact.format(n.capacity)} leaves`,
          n.x,
          n.y + 22,
        );
      }
      if (!leaf && n.depth === lastVisibleDepth && n.occupied) {
        ctx.strokeStyle = "#a6cd7040";
        ctx.beginPath();
        ctx.moveTo(n.x - 3, n.y + 11);
        ctx.lineTo(n.x, n.y + 14);
        ctx.lineTo(n.x + 3, n.y + 11);
        ctx.stroke();
      }
      ctx.restore();
    }
    // Depth guides remain legible at any magnification.
    for (
      let depth = Math.ceil(Math.min(27, Math.max(0, this.view.zoom - 0.7)));
      depth <= frame.lastDepth;
      depth++
    ) {
      const y = frame.yAt(depth);
      if (y < 50 || y > bottom) continue;
      ctx.fillStyle = "#0e140def";
      ctx.fillRect(0, y - 7, 34, 14);
      ctx.font = "7px ui-monospace, SFMono-Regular, Menlo, monospace";
      ctx.textAlign = "left";
      ctx.fillStyle = "#647d54";
      ctx.fillText(String(depth).padStart(2, "0"), 15, y + 3);
    }
    const visible = this.hits;
    byId("view-depth").textContent = visible.length
      ? `DEPTH ${String(visible[0].depth).padStart(2, "0")} → ${String(visible.at(-1).depth).padStart(2, "0")}`
      : "DRAG TO REPOSITION";
    byId("view-scale").textContent =
      frame.lastDepth === DEPTH ? "INDIVIDUAL LEAVES" : "AGGREGATED BRANCHES";
    byId("view-range").textContent =
      `LEAVES ${number.format(frame.left)} — ${number.format(frame.right)}`;
    byId("zoom-label").textContent = `${compact.format(2 ** this.view.zoom)}×`;
    this.canvas.dataset.renderedNodes = String(frame.nodes.length);
    this.canvas.dataset.zoom = this.view.zoom.toFixed(2);
    this.canvas.dataset.moving = String(moving);
    if (moving) this.invalidate();
    else this.scheduleLoad();
  }

  hit(x, y) {
    let nearest = null;
    let distance = 18;
    for (const node of this.hits || []) {
      const d = Math.hypot(node.x - x, node.y - y);
      if (d < distance) {
        nearest = node;
        distance = d;
      }
    }
    return nearest;
  }
  hideTooltip() {
    byId("tree-tooltip").hidden = true;
    this.hover = null;
  }
  showTooltip(node, x, y) {
    this.hover = node;
    const tooltip = byId("tree-tooltip");
    if (!node) {
      tooltip.hidden = true;
      return;
    }
    const leaf = this.leaves.get(node.index);
    tooltip.textContent =
      node.depth === DEPTH
        ? `Leaf #${number.format(node.index)}\n${node.occupied ? (leaf ? `Block time: ${shortDate(leaf.time)} ${utcTime(leaf.time)}\nBlock ${number.format(leaf.height)}` : "Confirmed commitment · loading block time…") : "Empty · field value 2"}`
        : `Depth ${node.depth} · branch ${number.format(node.index)}\n${number.format(node.occupied)} / ${number.format(node.capacity)} occupied\nDouble-click to explore`;
    tooltip.hidden = false;
    tooltip.style.left = `${clamp(x + 14, 8, Math.max(8, this.width - tooltip.offsetWidth - 8))}px`;
    tooltip.style.top = `${clamp(y + 17, 8, this.height - tooltip.offsetHeight - 8)}px`;
  }

  bindInput() {
    const local = (event) => {
      const rect = this.canvas.getBoundingClientRect();
      return { x: event.clientX - rect.left, y: event.clientY - rect.top };
    };
    this.canvas.addEventListener(
      "wheel",
      (event) => {
        event.preventDefault();
        const horizontal =
          (event.shiftKey && event.deltaX === 0 ? event.deltaY : event.deltaX) *
          (event.deltaMode === 1 ? 16 : event.deltaMode === 2 ? this.width : 1);
        const delta =
          event.deltaY *
          (event.deltaMode === 1
            ? 16
            : event.deltaMode === 2
              ? this.height
              : 1);
        if (
          !event.ctrlKey &&
          (event.shiftKey || Math.abs(horizontal) > Math.abs(delta))
        ) {
          this.pan(-horizontal, 0);
          return;
        }
        this.zoom(
          clamp(-delta * (event.ctrlKey ? 0.012 : 0.004), -1.5, 1.5),
          local(event).x,
        );
      },
      { passive: false },
    );
    this.canvas.addEventListener("pointerdown", (event) => {
      if (event.button !== 0) return;
      this.canvas.focus({ preventScroll: true });
      this.canvas.setPointerCapture(event.pointerId);
      this.view = { ...this.target };
      this.pointers.set(event.pointerId, local(event));
      if (this.pointers.size === 1) {
        this.startPointer = local(event);
        this.dragged = false;
      } else this.dragged = true;
      this.canvas.classList.add("dragging");
      this.hideTooltip();
    });
    this.canvas.addEventListener("pointermove", (event) => {
      const point = local(event);
      const previous = this.pointers.get(event.pointerId);
      if (!previous) {
        this.showTooltip(this.hit(point.x, point.y), point.x, point.y);
        this.invalidate();
        return;
      }
      const old = [...this.pointers.values()];
      this.pointers.set(event.pointerId, point);
      const next = [...this.pointers.values()];
      if (next.length === 2) {
        const distance = (pair) =>
          Math.hypot(pair[0].x - pair[1].x, pair[0].y - pair[1].y);
        const midpoint = (pair) => ({
          x: (pair[0].x + pair[1].x) / 2,
          y: (pair[0].y + pair[1].y) / 2,
        });
        const a = midpoint(old),
          b = midpoint(next);
        this.pan(b.x - a.x, b.y - a.y);
        if (distance(old) > 0 && distance(next) > 0) {
          this.moveTo(
            zoomAt(
              this.target,
              Math.log2(distance(next) / distance(old)),
              b.x,
              this.width,
            ),
            false,
          );
        }
      } else if (next.length === 1) {
        if (
          Math.hypot(
            point.x - this.startPointer.x,
            point.y - this.startPointer.y,
          ) > 4
        )
          this.dragged = true;
        if (this.dragged) this.pan(point.x - previous.x, point.y - previous.y);
      }
    });
    const release = (event) => {
      const tracked = this.pointers.delete(event.pointerId);
      if (!tracked) return;
      if (!this.pointers.size) {
        this.canvas.classList.remove("dragging");
        if (!this.dragged && event.type === "pointerup") {
          const point = local(event),
            node = this.hit(point.x, point.y);
          if (node) this.select(node);
        }
        this.scheduleLoad();
      } else this.startPointer = [...this.pointers.values()][0];
    };
    this.canvas.addEventListener("pointerup", release);
    this.canvas.addEventListener("pointercancel", release);
    this.canvas.addEventListener("lostpointercapture", release);
    this.canvas.addEventListener("pointerleave", () => {
      this.hideTooltip();
      this.invalidate();
    });
    this.canvas.addEventListener("dblclick", (event) => {
      const point = local(event),
        node = this.hit(point.x, point.y);
      if (node) {
        this.select(node);
        this.explore(node);
      }
    });
    this.canvas.addEventListener("keydown", (event) => {
      switch (event.key) {
        case "+":
        case "=":
          this.zoom(1);
          break;
        case "-":
        case "_":
          this.zoom(-1);
          break;
        case "ArrowLeft":
          this.pan(80, 0);
          break;
        case "ArrowRight":
          this.pan(-80, 0);
          break;
        case "ArrowUp":
          this.pan(0, 35);
          break;
        case "ArrowDown":
          this.pan(0, -35);
          break;
        case "Home":
          this.goWhole();
          break;
        case "Enter":
          if (this.selected) this.explore(this.selected);
          break;
        default:
          return;
      }
      event.preventDefault();
    });
  }

  scheduleLoad() {
    clearTimeout(this.loadTimer);
    this.loadTimer = setTimeout(() => this.loadLeaves(), 160);
  }
  async loadLeaves() {
    if (!this.snapshot) return;
    const frame = layout(this.target, this.width, this.height, this.count);
    const missing = frame.nodes.filter(
      (n) => n.depth === DEPTH && n.occupied && !this.leaves.has(n.index),
    );
    if (!missing.length) return;
    const from = missing[0].index;
    const limit = missing.at(-1).index - from + 1;
    if (limit > 128) return; // Hard upper bound, independent of future layout changes.
    const hash = this.snapshot.checkpoint.hash;
    const key = `${hash}:${from}:${limit}`;
    if (this.requestKey === key) return;
    this.leafRequest?.abort();
    const controller = new AbortController();
    this.leafRequest = controller;
    this.requestKey = key;
    const timeout = setTimeout(() => controller.abort(), 8000);
    try {
      const response = await fetch(
        `/v1/leaves?from=${from}&limit=${limit}&at=${hash}`,
        { signal: controller.signal },
      );
      if (response.status === 409) {
        this.onSnapshotGone();
        return;
      }
      if (!response.ok) throw new Error("Leaf details unavailable");
      const page = await response.json();
      if (controller.signal.aborted || this.snapshot.checkpoint.hash !== hash)
        return;
      for (const leaf of page.items) this.leaves.set(leaf.position, leaf);
      // Bounded cache; panning through millions of leaves never retains them all.
      while (this.leaves.size > 256)
        this.leaves.delete(this.leaves.keys().next().value);
      this.invalidate();
    } catch {
      if (!controller.signal.aborted || this.leafRequest === controller) {
        byId("view-scale").textContent =
          "LEAF DATES UNAVAILABLE · REFRESH TO RETRY";
      }
    } finally {
      clearTimeout(timeout);
      if (this.leafRequest === controller) this.requestKey = null;
    }
  }
}
