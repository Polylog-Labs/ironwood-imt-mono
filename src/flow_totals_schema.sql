ALTER TABLE daily_pool_flows ADD COLUMN total_in_zat INTEGER NOT NULL DEFAULT 0 CHECK (total_in_zat >= 0);
ALTER TABLE daily_pool_flows ADD COLUMN total_out_zat INTEGER NOT NULL DEFAULT 0 CHECK (total_out_zat >= 0);

UPDATE daily_pool_flows AS daily
SET total_in_zat = amounts.total_in_zat, total_out_zat = amounts.total_out_zat
FROM (
    SELECT b.time/86400*86400 AS day,
        COALESCE(SUM(-f.net_zat) FILTER (WHERE f.net_zat < 0), 0) AS total_in_zat,
        COALESCE(SUM(f.net_zat) FILTER (WHERE f.net_zat > 0), 0) AS total_out_zat
    FROM transaction_flows f JOIN leaves l USING(position) JOIN blocks b USING(height)
    WHERE f.ironwood_only=0
    GROUP BY day
) AS amounts
WHERE daily.day=amounts.day;

PRAGMA user_version=6;
