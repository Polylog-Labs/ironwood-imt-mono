ALTER TABLE daily_pool_flows ADD COLUMN cross_pool_changes INTEGER NOT NULL DEFAULT 0 CHECK (cross_pool_changes >= 0);

UPDATE daily_pool_flows AS daily
SET cross_pool_changes = amounts.changes
FROM (
    SELECT b.time/86400*86400 AS day, COUNT(*) AS changes
    FROM transaction_flows f JOIN leaves l USING(position) JOIN blocks b USING(height)
    WHERE f.ironwood_only=0 AND f.net_zat!=0
    GROUP BY day
) AS amounts
WHERE daily.day=amounts.day;

PRAGMA user_version=8;
