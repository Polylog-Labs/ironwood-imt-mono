CREATE TABLE metadata (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    network TEXT NOT NULL,
    genesis BLOB NOT NULL CHECK (length(genesis) = 32),
    start_height INTEGER NOT NULL CHECK (start_height > 0),
    generation INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE TABLE blocks (
    height INTEGER PRIMARY KEY,
    hash BLOB NOT NULL UNIQUE CHECK (length(hash) = 32),
    time INTEGER NOT NULL,
    first_position INTEGER NOT NULL,
    leaf_count INTEGER NOT NULL CHECK (leaf_count BETWEEN 0 AND 4294967296),
    root BLOB NOT NULL CHECK (length(root) = 32)
) STRICT;

CREATE TABLE leaves (
    position INTEGER PRIMARY KEY CHECK (position BETWEEN 0 AND 4294967295),
    height INTEGER NOT NULL REFERENCES blocks(height) ON DELETE CASCADE,
    txid BLOB NOT NULL CHECK (length(txid) = 32),
    tx_index INTEGER NOT NULL,
    action_index INTEGER NOT NULL,
    cmx BLOB NOT NULL CHECK (length(cmx) = 32),
    nullifier BLOB NOT NULL UNIQUE CHECK (length(nullifier) = 32)
) STRICT;
CREATE INDEX leaves_height ON leaves(height);

-- Only full subtrees are stored. They never change unless their block is orphaned.
-- Leaves live in `leaves`; empty and right-edge partial subtrees are derived.
CREATE TABLE nodes (
    level INTEGER NOT NULL CHECK (level BETWEEN 1 AND 32),
    node_index INTEGER NOT NULL,
    hash BLOB NOT NULL CHECK (length(hash) = 32),
    height INTEGER NOT NULL REFERENCES blocks(height) ON DELETE CASCADE,
    PRIMARY KEY (level, node_index)
) STRICT, WITHOUT ROWID;
CREATE INDEX nodes_height ON nodes(height);

PRAGMA user_version = 1;
