-- Only decoded balances can complete a day's pool flow summary.
DROP TRIGGER dirty_daily_flows_classified;
DROP TRIGGER dirty_daily_flows_reclassified;
UPDATE daily_pool_flows SET dirty=1;
PRAGMA user_version=7;
