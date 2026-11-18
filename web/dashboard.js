import { TreeView } from "./tree.js";
import {
  DEPTH,
  CAPACITY,
  EMPTY_LEAF,
  nodeInfo,
  fillProjection,
  leafFillProjection,
  syncProgress,
} from "./tree-model.mjs";

const number = new Intl.NumberFormat("en-US");
const percent = new Intl.NumberFormat("en-US", {
  minimumFractionDigits: 2,
  maximumFractionDigits: 2,
  signDisplay: "exceptZero",
});
const calendar = new Intl.DateTimeFormat("en-GB", {
  day: "2-digit",
  month: "short",
  year: "numeric",
  timeZone: "UTC",
});
const clock = new Intl.DateTimeFormat("en-GB", {
  day: "2-digit",
  month: "short",
  year: "numeric",
  hour: "2-digit",
  minute: "2-digit",
  second: "2-digit",
  hour12: false,
  timeZone: "UTC",
});
const byId = (id) => document.getElementById(id);
const text = (id, value) => {
  byId(id).textContent = value;
};
const time = (timestamp) => `${clock.format(new Date(timestamp * 1000))} UTC`;
let payload = null;
let timer;
let inFlight = false;
let retryNotBefore = 0;
let selected = { depth: 0, index: 0 };
let selectionRequest;
let selectionVersion = 0;

const tree = new TreeView(byId("tree-canvas"), {
  onSelect: (node) => {
    selected = node;
    inspect();
  },
  onSnapshotGone: () => {
    text(
      "error",
      "The chain changed. Refreshing the tree to its current branch.",
    );
    byId("error").hidden = false;
    refresh();
  },
});

function status(label, kind = "") {
  text("status", label);
  byId("status").className = `status ${kind}`;
}

function renderSync(next, unavailable = false) {
  const panel = byId("sync-panel");
  panel.hidden = next?.ready === true;
  if (panel.hidden) return;
  const snapshot = next?.data.snapshot;
  const progress = syncProgress(
    snapshot?.start_height,
    snapshot?.checkpoint.height,
    next?.sync.node_height,
  );
  const bar = byId("sync-progress");
  if (progress) {
    bar.value = progress.percent;
    // Keep 100% reserved for completion, even with only one block remaining.
    text("sync-percent", `${Math.min(progress.percent, progress.remaining ? 99.99 : 100).toFixed(2)}%`);
    text("sync-detail", unavailable
      ? `Last reported at ${time(next.observed_at)} · reconnecting automatically`
      : `${number.format(progress.indexed)} / ${number.format(progress.total)} Ironwood blocks · ${number.format(progress.remaining)} remaining · block ${number.format(snapshot.checkpoint.height)} / ${number.format(next.sync.node_height)}`);
  } else {
    bar.removeAttribute("value");
    text("sync-percent", unavailable ? "Waiting" : "Connecting…");
    text("sync-detail", unavailable
      ? "Waiting for the API to report progress. Reconnecting automatically."
      : "Discovering the upstream chain tip.");
  }
  text("sync-title", unavailable ? "Sync status unavailable"
    : next?.sync.failed ? "Sync retrying"
    : progress?.remaining === 0 ? "Verifying latest chain state"
    : next ? "API syncing" : "API sync");
}

function render(next) {
  const previousHash = payload?.data.snapshot.checkpoint.hash;
  payload = next;
  const { data, mempool, sync } = payload;
  const tip = data.snapshot.checkpoint;
  const growth = data.growth_24h;
  renderSync(next);
  document.body.dataset.hasSnapshot = "true";
  document.body.dataset.connection = "online";
  byId("tree-waiting").hidden = true;
  byId("explorer-toolbar").inert = false;
  byId("tree-canvas").inert = false;
  text("root-label", "CURRENT ROOT");
  text(
    "network",
    data.snapshot.network === "main"
      ? "MAINNET"
      : data.snapshot.network.toUpperCase(),
  );
  text("usage", `${data.used_percent.toFixed(5)}%`);
  byId("capacity-bar").value = data.used_percent;
  text(
    "leaf-count",
    `${number.format(tip.leaf_count)} / ${number.format(data.capacity)} leaves`,
  );
  text(
    "growth",
    payload.chain_fresh && growth.percent !== null
      ? `${percent.format(growth.percent)}%`
      : "—",
  );
  text(
    "growth-detail",
    !payload.chain_fresh
      ? "Waiting for a fresh chain snapshot"
      : growth.added === null
        ? "24-hour baseline unavailable"
        : `+${number.format(growth.added)} leaves / day`,
  );
  text(
    "growth-window",
    growth.added !== null && growth.percent === null
      ? "No percentage: baseline is empty"
      : "Rolling 24-hour window",
  );
  const projection = fillProjection(
    tip.leaf_count,
    growth.added,
    payload.observed_at,
    payload.chain_fresh,
  );
  text(
    "fill-date",
    projection
      ? projection.days === 0
        ? "Tree is full"
        : calendar.format(new Date(projection.timestamp * 1000)).toUpperCase()
      : "No estimate",
  );
  text(
    "fill-detail",
    projection
      ? projection.days === 0
        ? "All leaf positions are occupied"
        : `At ${number.format(growth.added)} new leaves per day`
      : !payload.chain_fresh
        ? "Waiting for a fresh chain snapshot"
        : growth.added === 0
          ? "No additions in the last 24 hours"
          : "24-hour growth is unavailable",
  );
  text(
    "mempool",
    mempool.actions === null ? "—" : number.format(mempool.actions),
  );
  text(
    "mempool-time",
    mempool.observed_at === null
      ? "Not yet sampled"
      : `${mempool.fresh ? "Observed" : "Stale sample"} ${new Date(mempool.observed_at * 1000).toISOString().slice(11, 19)} UTC`,
  );
  text("root", tip.root);
  text("tree-snapshot", `VERIFIED AT BLOCK ${number.format(tip.height)}`);
  text(
    "updated",
    `Updated ${new Date(payload.observed_at * 1000).toISOString().slice(11, 19)} UTC · 10s refresh`,
  );
  if (payload.ready) status("Synced", "live");
  else if (sync.failed) status("Sync retrying", "warning");
  else if (!payload.caught_up) status("API syncing", "warning");
  else status("Awaiting fresh data", "warning");
  byId("error").hidden = true;
  tree.setSnapshot(data.snapshot, data.latest_leaves);
  // Keep the camera and selection when new blocks arrive, and refresh pinned details.
  if (previousHash !== tip.hash) {
    tree.selected = nodeInfo(selected.depth, selected.index, tip.leaf_count);
    inspect();
    renderRecent(data.latest_leaves);
  } else {
    renderLeafEstimate(selected);
  }
}

function renderLeafEstimate(node) {
  const emptyLeaf = node.depth === DEPTH && node.occupied === 0;
  byId("node-fill-row").hidden = !emptyLeaf;
  if (!emptyLeaf) return;
  const added = payload.data.growth_24h.added;
  const estimate = leafFillProjection(
    node.index,
    payload.data.snapshot.checkpoint.leaf_count,
    added,
    payload.observed_at,
    payload.chain_fresh,
  );
  text(
    "node-fill",
    estimate
      ? `${calendar.format(new Date(estimate.timestamp * 1000))} UTC`
      : "No estimate",
  );
  const explanation = estimate
    ? `Estimated at ${number.format(added)} leaves/day, continuing the last 24 hours’ rate.`
    : !payload.chain_fresh
      ? "A fresh chain snapshot is needed to estimate its fill date."
      : added === null
        ? "A 24-hour growth baseline is needed to estimate its fill date."
        : added === 0
          ? "No fill date can be estimated without new leaves."
          : "The estimated date is outside the supported calendar range.";
  text(
    "node-note",
    `${explanation} This leaf is currently empty (field value 2).`,
  );
}

function renderRecent(leaves) {
  const cards = leaves.slice(0, 8).map((leaf) => {
    const card = document.createElement("button");
    card.type = "button";
    card.className = "leaf-card";
    card.setAttribute(
      "aria-label",
      `Inspect leaf ${number.format(leaf.position)}, added ${time(leaf.time)}`,
    );
    const top = document.createElement("span");
    top.className = "leaf-top";
    const name = document.createElement("span");
    name.textContent = `# ${number.format(leaf.position)}`;
    const arrow = document.createElement("span");
    arrow.textContent = "↗";
    arrow.setAttribute("aria-hidden", "true");
    top.append(name, arrow);
    const hash = document.createElement("code");
    hash.textContent = leaf.cmx;
    const date = document.createElement("span");
    date.className = "leaf-date";
    date.textContent = `${calendar.format(new Date(leaf.time * 1000))} · ${new Date(leaf.time * 1000).toISOString().slice(11, 19)} UTC`;
    card.append(top, hash, date);
    card.addEventListener("click", () => {
      tree.goLeaf(leaf.position);
      byId("tree-stage").scrollIntoView({
        block: "center",
        behavior: tree.reducedMotion.matches ? "instant" : "smooth",
      });
      byId("tree-canvas").focus({ preventScroll: true });
    });
    return card;
  });
  if (!cards.length) {
    const empty = document.createElement("p");
    empty.className = "hint";
    empty.textContent = "No confirmed leaves yet.";
    cards.push(empty);
  }
  byId("recent-leaves").replaceChildren(...cards);
}

async function inspect() {
  if (!payload) return;
  selectionRequest?.abort();
  const controller = new AbortController();
  selectionRequest = controller;
  const version = ++selectionVersion;
  const tip = payload.data.snapshot.checkpoint;
  const node = nodeInfo(selected.depth, selected.index, tip.leaf_count);
  selected = node;
  const isLeaf = node.depth === DEPTH;
  text(
    "node-kind",
    node.depth === 0
      ? "TREE ORIGIN"
      : isLeaf
        ? "NOTE COMMITMENT"
        : "MERKLE SUBTREE",
  );
  text(
    "node-title",
    node.depth === 0
      ? "Root node"
      : isLeaf
        ? `Leaf #${number.format(node.index)}`
        : `Branch #${number.format(node.index)}`,
  );
  text(
    "node-state",
    node.occupied === 0
      ? "Uncommitted · empty"
      : node.occupied === node.capacity
        ? "Fully occupied"
        : "Partially occupied",
  );
  text("node-depth", `${node.depth} / ${DEPTH}`);
  text(
    "node-range",
    isLeaf
      ? number.format(node.first)
      : `${number.format(node.first)} — ${number.format(node.last)}`,
  );
  text(
    "node-occupied",
    `${number.format(node.occupied)} / ${number.format(node.capacity)}`,
  );
  byId("node-added-row").hidden = !isLeaf || !node.occupied;
  byId("node-block-row").hidden = !isLeaf || !node.occupied;
  byId("node-tx-row").hidden = true;
  byId("node-action-row").hidden = true;
  byId("node-tx").replaceChildren();
  byId("node-transaction-link").hidden = true;
  byId("node-transaction-link").removeAttribute("href");
  text("node-added", "Loading…");
  text("node-block", "Loading…");
  text("node-hash-label", isLeaf ? "LEAF COMMITMENT" : "NODE HASH");
  text("node-verification", "LOADING");
  text("node-hash", "Loading from this block’s tree…");
  text(
    "node-note",
    !node.occupied
      ? "Empty leaves use field value 2. Empty branch hashes are derived from those leaves."
      : isLeaf
        ? "Added time is the block timestamp (UTC). Spending a note does not remove its commitment."
        : "Click a child node or explore this branch to reveal the leaves below it.",
  );
  renderLeafEstimate(node);
  byId("explore-node").disabled = false;
  text("explore-node", isLeaf ? "Locate leaf ↗" : "Explore branch ↗");
  byId("parent-node").disabled = node.depth === 0;
  byId("node-error").hidden = true;
  const timeout = setTimeout(() => controller.abort(), 8000);
  try {
    let hash;
    let leaf;
    if (node.depth === 0) hash = tip.root;
    else if (isLeaf && !node.occupied) hash = EMPTY_LEAF;
    else if (isLeaf) {
      leaf = tree.leaves.get(node.index);
      if (!leaf) {
        const page = await read(
          `/v1/leaves?from=${node.index}&limit=1&at=${tip.hash}`,
          controller.signal,
        );
        leaf = page.items[0];
        if (!leaf || leaf.position !== node.index)
          throw new Error("Leaf missing from snapshot");
      }
      hash = leaf.cmx;
    } else {
      const result = await read(
        `/v1/tree/nodes/${DEPTH - node.depth}/${node.index}?at=${tip.hash}`,
        controller.signal,
      );
      hash = result.hash;
    }
    if (version !== selectionVersion || controller.signal.aborted) return;
    text("node-hash", hash);
    text("node-verification", node.occupied ? "VERIFIED TREE" : "UNCOMMITTED");
    if (leaf) {
      text("node-added", time(leaf.time));
      text("node-block", number.format(leaf.height));
      if (/^[a-f0-9]{64}$/i.test(leaf.txid)) {
        const mainnet = payload.data.snapshot.network === "main";
        const source = document.createElement(mainnet ? "a" : "code");
        source.textContent = `${leaf.txid.slice(0, 10)}…${leaf.txid.slice(-10)}${mainnet ? " ↗" : ""}`;
        source.title = leaf.txid;
        if (mainnet) {
          source.href = `https://www.blockgrey.com/zcash/tx/${leaf.txid}`;
          source.target = "_blank";
          source.rel = "noopener noreferrer";
          source.setAttribute("aria-label", `View transaction ${leaf.txid} on BlockGrey`);
          byId("node-transaction-link").href = source.href;
          byId("node-transaction-link").hidden = false;
        }
        byId("node-tx").append(source);
        byId("node-tx-row").hidden = false;
      }
      if (Number.isSafeInteger(leaf.action_index) && leaf.action_index >= 0) {
        text("node-action", `${number.format(leaf.action_index)} (zero-based)`);
        byId("node-action-row").hidden = false;
      }
    }
  } catch (error) {
    if (version !== selectionVersion) return;
    text("node-hash", "Details unavailable");
    text("node-verification", "UNAVAILABLE");
    text("node-added", "Unavailable");
    text("node-block", "Unavailable");
    text(
      "node-error",
      error.message === "snapshot-gone"
        ? "This block was reorganized. Refreshing…"
        : "Could not load this node. Select it again or refresh to retry.",
    );
    byId("node-error").hidden = false;
  } finally {
    clearTimeout(timeout);
  }
}

async function read(url, signal) {
  if (Date.now() < retryNotBefore) throw new Error("rate-limited");
  const response = await fetch(url, { signal, cache: "no-store" });
  if (response.status === 429) {
    const seconds = Number(response.headers.get("Retry-After"));
    retryNotBefore = Date.now() + (Number.isFinite(seconds) && seconds > 0 ? seconds : 10) * 1000;
    throw new Error("rate-limited");
  }
  if (response.status === 409) {
    refresh();
    throw new Error("snapshot-gone");
  }
  if (!response.ok) throw new Error(`HTTP ${response.status}`);
  return response.json();
}

async function refresh() {
  if (inFlight || document.hidden) return false;
  if (Date.now() < retryNotBefore) {
    clearTimeout(timer);
    timer = setTimeout(refresh, retryNotBefore - Date.now());
    return false;
  }
  clearTimeout(timer);
  inFlight = true;
  byId("refresh").disabled = true;
  byId("retry-connection").disabled = true;
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 8000);
  try {
    render(await read("/v1/dashboard", controller.signal));
    return true;
  } catch (error) {
    renderSync(payload, true);
    document.body.dataset.connection = payload ? "stale" : "offline";
    const limited = error.message === "rate-limited";
    status(limited ? "Retrying shortly" : payload ? "Last known state" : "On standby", "offline");
    if (payload) {
      text("root-label", "LAST VERIFIED ROOT");
      text(
        "error",
        `Live updates paused. Showing the verified snapshot from ${time(payload.observed_at)}. Reconnecting automatically.`,
      );
      text(
        "mempool-time",
        payload.mempool.observed_at === null
          ? "No previous observation"
          : `Last observed ${time(payload.mempool.observed_at)}`,
      );
      byId("error").hidden = false;
    } else {
      text("waiting-title", limited ? "Reconnecting shortly" : "Awaiting chain data");
      text(
        "waiting-detail",
        limited
          ? "Taking a short pause between requests. This view will retry automatically."
          : "Live data is temporarily unavailable. This view will reconnect automatically.",
      );
      text("updated", "Waiting for connection");
      byId("error").hidden = true;
    }
    return false;
  } finally {
    clearTimeout(timeout);
    inFlight = false;
    byId("refresh").disabled = false;
    byId("retry-connection").disabled = false;
    if (!document.hidden) timer = setTimeout(refresh, Math.max(10_000, retryNotBefore - Date.now()));
  }
}

byId("view-whole").addEventListener("click", () => tree.goWhole());
byId("view-active").addEventListener("click", () => tree.goActive());
byId("reset-view").addEventListener("click", () => tree.goActive());
byId("view-latest").addEventListener("click", () =>
  tree.goLeaf(Math.max(0, tree.count - 1), true),
);
byId("zoom-in").addEventListener("click", () => tree.zoom(1));
byId("zoom-out").addEventListener("click", () => tree.zoom(-1));
byId("explore-node").addEventListener("click", () =>
  selected.depth === DEPTH
    ? tree.goLeaf(selected.index)
    : tree.explore(selected),
);
byId("parent-node").addEventListener("click", () => {
  if (selected.depth === 0) return;
  const node = nodeInfo(
    selected.depth - 1,
    Math.floor(selected.index / 2),
    tree.count,
  );
  tree.select(node);
  tree.explore(node);
});
byId("leaf-search").addEventListener("submit", (event) => {
  event.preventDefault();
  const raw = byId("leaf-index").value.replace(/[ ,]/g, "");
  const index = Number(raw);
  if (
    !/^\d+$/.test(raw) ||
    !Number.isSafeInteger(index) ||
    index < 0 ||
    index >= CAPACITY
  ) {
    text(
      "search-error",
      `Enter a leaf index from 0 to ${number.format(CAPACITY - 1)}.`,
    );
    byId("search-error").hidden = false;
    return;
  }
  byId("search-error").hidden = true;
  tree.goLeaf(index);
  byId("tree-canvas").focus({ preventScroll: true });
});
async function retry() {
  if (await refresh()) inspect();
}
byId("refresh").addEventListener("click", retry);
byId("retry-connection").addEventListener("click", retry);
document.addEventListener("visibilitychange", () => {
  clearTimeout(timer);
  if (!document.hidden) refresh();
});
refresh();
