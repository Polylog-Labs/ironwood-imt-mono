// JSON responses from src/api.rs, src/model.rs, src/store.rs, and src/sync.rs.
// Hashes are hex strings; timestamps are Unix seconds.
export interface Checkpoint {
  height: number;
  hash: string;
  time: number;
  leaf_count: number;
  root: string;
}

export interface Snapshot {
  network: string;
  start_height: number;
  generation: number;
  checkpoint: Checkpoint;
}

export interface Leaf {
  position: number;
  height: number;
  time: number;
  txid: string;
  tx_index: number;
  action_index: number;
  cmx: string;
  nullifier: string;
}

export interface Page<T> {
  snapshot: Snapshot;
  items: T[];
  next: number | null;
}

export interface TreeNode {
  snapshot: Snapshot;
  level: number;
  index: number;
  hash: string;
  first_position: number;
  capacity: number;
  occupied: number;
}

export interface DashboardResponse {
  observed_at: number;
  tree_kind: "ironwood_note_commitment";
  data: {
    snapshot: Snapshot;
    capacity: number;
    used_percent: number;
    growth_24h: {
      window_start: number;
      window_end: number;
      baseline: Checkpoint | null;
      added: number | null;
      percent: number | null;
    };
    latest_leaves: Leaf[];
  };
  sync: {
    node_height: number | null;
    node_hash: string | null;
    last_success: number | null;
    failed: boolean;
  };
  chain_fresh: boolean;
  caught_up: boolean;
  ready: boolean;
  mempool: {
    actions: number | null;
    transactions: number | null;
    observed_at: number | null;
    fresh: boolean;
  };
}
