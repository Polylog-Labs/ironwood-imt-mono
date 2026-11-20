-- Materialize counts per block so the year view never scans every shielded action.
CREATE INDEX activity_transactions ON leaves(position DESC, height, txid)
    WHERE action_index = 0 AND tx_index > 0;
ALTER TABLE blocks ADD COLUMN activity_count INTEGER NOT NULL DEFAULT 0;
UPDATE blocks SET activity_count = (
    SELECT count(*) FROM leaves WHERE height = blocks.height AND action_index = 0 AND tx_index > 0
);
CREATE INDEX activity_block_time ON blocks(time, activity_count);
CREATE TABLE transaction_flows (
    position INTEGER PRIMARY KEY REFERENCES leaves(position) ON DELETE CASCADE,
    net_zat INTEGER NOT NULL,
    ironwood_only INTEGER NOT NULL CHECK (ironwood_only IN (0, 1))
) STRICT;
CREATE TABLE pool_balances (
    height INTEGER PRIMARY KEY REFERENCES blocks(height) ON DELETE CASCADE,
    amount_zat INTEGER NOT NULL CHECK (amount_zat >= 0)
) STRICT;
CREATE TABLE pending_sightings (
    txid BLOB PRIMARY KEY,
    first_seen INTEGER NOT NULL
) STRICT, WITHOUT ROWID;
CREATE TABLE confirmation_times (
    position INTEGER PRIMARY KEY REFERENCES leaves(position) ON DELETE CASCADE,
    observed_at INTEGER NOT NULL,
    seconds INTEGER NOT NULL CHECK (seconds >= 0)
) STRICT;
CREATE INDEX confirmation_observed ON confirmation_times(observed_at);
PRAGMA user_version = 2;
