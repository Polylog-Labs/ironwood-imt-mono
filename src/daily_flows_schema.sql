CREATE TABLE daily_pool_flows (
    day INTEGER PRIMARY KEY,
    transactions INTEGER NOT NULL,
    checked INTEGER NOT NULL CHECK (checked BETWEEN 0 AND transactions),
    ironwood_only INTEGER NOT NULL,
    in_count INTEGER NOT NULL,
    out_count INTEGER NOT NULL,
    median_in_zat REAL,
    median_out_zat REAL,
    dirty INTEGER NOT NULL DEFAULT 0 CHECK (dirty IN (0, 1))
) STRICT;

CREATE TABLE flow_candidate_blocks (
    height INTEGER PRIMARY KEY REFERENCES blocks(height) ON DELETE CASCADE
) STRICT;
CREATE TABLE flow_candidates (
    position INTEGER PRIMARY KEY REFERENCES leaves(position) ON DELETE CASCADE
) STRICT;

CREATE TRIGGER dirty_daily_flows_block AFTER UPDATE OF activity_count ON blocks
WHEN NEW.activity_count != OLD.activity_count BEGIN
    UPDATE daily_pool_flows SET dirty=1 WHERE day=NEW.time/86400*86400;
END;
CREATE TRIGGER dirty_daily_flows_reorg AFTER DELETE ON blocks BEGIN
    UPDATE daily_pool_flows SET dirty=1 WHERE day=OLD.time/86400*86400;
END;
CREATE TRIGGER dirty_daily_flows_decoded AFTER INSERT ON transaction_flows BEGIN
    UPDATE daily_pool_flows SET dirty=1 WHERE day=(
        SELECT b.time/86400*86400 FROM leaves l JOIN blocks b USING(height) WHERE l.position=NEW.position
    );
END;
CREATE TRIGGER dirty_daily_flows_classified AFTER INSERT ON block_usage BEGIN
    UPDATE daily_pool_flows SET dirty=1 WHERE day=(SELECT time/86400*86400 FROM blocks WHERE height=NEW.height);
END;
CREATE TRIGGER dirty_daily_flows_reclassified AFTER UPDATE OF ironwood_only ON block_usage
WHEN NEW.ironwood_only != OLD.ironwood_only BEGIN
    UPDATE daily_pool_flows SET dirty=1 WHERE day=(SELECT time/86400*86400 FROM blocks WHERE height=NEW.height);
    DELETE FROM flow_candidate_blocks WHERE height=NEW.height;
END;
PRAGMA user_version=4;
