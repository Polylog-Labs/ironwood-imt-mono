ALTER TABLE blocks ADD COLUMN coinbase_commitments INTEGER NOT NULL DEFAULT 0;

UPDATE blocks SET coinbase_commitments = (
    SELECT COUNT(*) FROM leaves WHERE leaves.height=blocks.height AND tx_index=0
);

CREATE TABLE block_transaction_actions (
    height INTEGER NOT NULL REFERENCES blocks(height) ON DELETE CASCADE,
    actions INTEGER NOT NULL CHECK (actions > 0),
    transactions INTEGER NOT NULL CHECK (transactions > 0),
    PRIMARY KEY (height, actions)
) STRICT, WITHOUT ROWID;

INSERT INTO block_transaction_actions
SELECT height, actions, COUNT(*) FROM (
    SELECT height, COUNT(*) AS actions FROM leaves WHERE tx_index>0 GROUP BY height, tx_index
) GROUP BY height, actions;

PRAGMA user_version=5;
