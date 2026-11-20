use super::*;
use crate::activity::{CandidateBlock, Day, FLOW_WINDOW, Flow, FlowKey};
use chrono::{DateTime, Datelike, NaiveDate};
use serde_json::{Value, json};

const DAY: u64 = 86400;

// A block's compact classification accounts for Ironwood-only transactions without
// downloading their full bytes. A mixed transaction still needs its signed balance.
const DAY_COVERAGE: &str = "
    WITH decoded AS (
        SELECT b.height, b.activity_count, u.ironwood_only AS classified_only,
            COUNT(f.position) FILTER (WHERE f.ironwood_only=0) AS mixed,
            COUNT(f.position) FILTER (WHERE f.ironwood_only=1) AS only_decoded
        FROM blocks b LEFT JOIN block_usage u USING(height)
        JOIN leaves l USING(height) LEFT JOIN transaction_flows f USING(position)
        WHERE b.time>=?1 AND b.time<?2 AND l.action_index=0 AND l.tx_index>0
        GROUP BY b.height
    ), coverage AS (
        SELECT height, activity_count, mixed, only_decoded,
            CASE WHEN classified_only BETWEEN only_decoded AND activity_count-mixed
                THEN classified_only ELSE only_decoded END AS only_count
        FROM decoded
    )";

#[derive(Serialize)]
pub struct DailyFlows {
    pub transactions: u64,
    pub checked: u64,
    pub ironwood_only: u64,
    pub in_count: usize,
    pub out_count: usize,
    pub median_in_zat: Option<f64>,
    pub median_out_zat: Option<f64>,
}

#[derive(Default)]
struct FlowAmounts {
    deposits: Vec<u64>,
    withdrawals: Vec<u64>,
}

impl FlowAmounts {
    fn push(&mut self, net: i64) {
        if net < 0 {
            self.deposits.push(net.unsigned_abs());
        } else if net > 0 {
            self.withdrawals.push(net as u64);
        }
    }
}
const RECENT_TXS: &str = "SELECT position, height, txid FROM leaves
    WHERE action_index=0 AND tx_index>0 ORDER BY position DESC LIMIT ?1";

pub(super) fn median(values: &mut [u64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some((values[(values.len() - 1) / 2] as f64 + values[values.len() / 2] as f64) / 2.0)
}
fn times(mut values: Vec<u64>) -> Value {
    let mean = (!values.is_empty())
        .then(|| values.iter().map(|&seconds| seconds as f64).sum::<f64>() / values.len() as f64);
    let p50 = median(&mut values);
    json!({"count":values.len(), "mean":mean, "median":p50,
        "p90":values.get((values.len()*9).div_ceil(10).saturating_sub(1))})
}

impl Store {
    pub fn request_flow_day(&self, day: u64) {
        let mut queue = self.requested_flow_days.lock().unwrap();
        if queue.len() < 8 && !queue.contains(&day) {
            queue.push_back(day);
        }
    }

    pub fn next_flow_day(&self) -> Option<u64> {
        self.requested_flow_days.lock().unwrap().pop_front()
    }

    pub async fn daily_flows(&self, day: u64) -> Result<Option<DailyFlows>> {
        self.read(move |conn| {
            let Some((first, last)) = flow_day_range(conn)? else {return Ok(None)};
            if !day.is_multiple_of(DAY) || !(first..=last).contains(&day) {
                return Ok(None);
            }
            let saved = conn.query_row(
                "SELECT transactions, checked, ironwood_only, in_count, out_count, median_in_zat, median_out_zat
                 FROM daily_pool_flows WHERE day=?1 AND dirty=0",
                [day],
                |row| Ok(DailyFlows{
                    transactions:row.get(0)?, checked:row.get(1)?, ironwood_only:row.get(2)?, in_count:row.get(3)?,
                    out_count:row.get(4)?, median_in_zat:row.get(5)?, median_out_zat:row.get(6)?,
                })
            ).optional()?;
            Ok(Some(match saved { Some(result)=>result, None=>calculate_day_flows(conn,day)? }))
        }).await
    }

    pub async fn next_incomplete_flow_day(&self) -> Result<Option<u64>> {
        self.read(|conn| {
            let Some((first,last))=flow_day_range(conn)? else {return Ok(None)};
            let first = first.max(last.saturating_sub(364 * DAY));
            Ok(conn.query_row(
                "WITH RECURSIVE calendar(day) AS (VALUES (?2) UNION ALL SELECT day-86400 FROM calendar WHERE day>?1)
                 SELECT day FROM calendar LEFT JOIN daily_pool_flows f USING(day)
                 WHERE f.day IS NULL OR dirty=1 OR checked<transactions ORDER BY day DESC LIMIT 1",
                params![first,last], |r|r.get(0),
            ).optional()?)
        }).await
    }

    pub async fn save_daily_flows(&self, day: u64) -> Result<()> {
        self.writer.call_raw(move |conn| -> Result<()> {
            let tx=conn.transaction()?;
            let flows=calculate_day_flows(&tx,day)?;
            tx.execute("INSERT INTO daily_pool_flows VALUES (?1,?2,?3,?4,?5,?6,?7,?8,0)
                ON CONFLICT(day) DO UPDATE SET transactions=excluded.transactions, checked=excluded.checked,
                ironwood_only=excluded.ironwood_only, in_count=excluded.in_count, out_count=excluded.out_count,
                median_in_zat=excluded.median_in_zat, median_out_zat=excluded.median_out_zat, dirty=0",
                params![day,flows.transactions,flows.checked,flows.ironwood_only,
                    flows.in_count,flows.out_count,flows.median_in_zat,flows.median_out_zat])?;
            tx.commit()?;
            Ok(())
        }).await?
    }

    pub async fn flow_candidate_span(&self, day: u64) -> Result<Option<(u32, u32)>> {
        self.read(move |conn| {
            let (first, last): (Option<u32>, Option<u32>) = conn.query_row(
                "SELECT MIN(b.height),MAX(b.height) FROM blocks b JOIN block_usage u USING(height)
                 WHERE b.time>=?1 AND b.time<?2 AND b.activity_count>u.ironwood_only
                 AND NOT EXISTS (SELECT 1 FROM flow_candidate_blocks c WHERE c.height=b.height)",
                params![day, day + DAY],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            Ok(first.zip(last))
        })
        .await
    }

    pub async fn save_flow_candidates(&self, blocks: Vec<CandidateBlock>) -> Result<()> {
        self.writer
            .call_raw(move |conn| -> Result<()> {
                let tx = conn.transaction()?;
                for block in blocks {
                    let Some(expected):Option<usize> = tx.query_row(
                    "SELECT activity_count-ironwood_only FROM blocks JOIN block_usage USING(height)
                     WHERE height=?1 AND hash=?2",
                    params![block.height,block.hash],|r|r.get(0),
                ).optional()? else {continue};
                    if expected == 0 {
                        continue;
                    }
                    let mut stmt = tx.prepare_cached(
                        "SELECT position, txid FROM leaves
                    WHERE height=?1 AND action_index=0 AND tx_index>0",
                    )?;
                    let mut positions = Vec::with_capacity(expected);
                    for result in stmt.query_map([block.height], |r| {
                        Ok((r.get::<_, u64>(0)?, r.get::<_, Bytes32>(1)?))
                    })? {
                        let (position, txid) = result?;
                        if block.other_pool_txids.contains(&txid) {
                            positions.push(position)
                        }
                    }
                    ensure!(
                        positions.len() == expected,
                        "compact flow candidate count differs at block {}",
                        block.height
                    );
                    tx.execute(
                        "DELETE FROM flow_candidates WHERE position IN (
                    SELECT position FROM leaves WHERE height=?1 AND action_index=0 AND tx_index>0
                )",
                        [block.height],
                    )?;
                    let mut insert =
                        tx.prepare_cached("INSERT INTO flow_candidates VALUES (?1)")?;
                    for position in positions {
                        insert.execute([position])?;
                    }
                    tx.execute(
                        "INSERT OR IGNORE INTO flow_candidate_blocks VALUES (?1)",
                        [block.height],
                    )?;
                }
                tx.commit()?;
                Ok(())
            })
            .await?
    }

    pub async fn missing_daily_flows(&self, day: u64, limit: usize) -> Result<Vec<FlowKey>> {
        // Fill blocks with the largest share of undecoded mixed transactions first.
        self.read(move |conn| {
            let sql = format!(
                "{DAY_COVERAGE}
                SELECT l.position, l.height, l.txid
                FROM coverage c JOIN leaves l USING(height)
                LEFT JOIN transaction_flows f USING(position)
                LEFT JOIN flow_candidate_blocks known ON known.height=l.height
                LEFT JOIN flow_candidates candidate USING(position)
                WHERE c.mixed+c.only_count<c.activity_count AND f.position IS NULL
                    AND (known.height IS NULL OR candidate.position IS NOT NULL)
                    AND l.action_index=0 AND l.tx_index>0
                ORDER BY 1.0*(c.activity_count-c.mixed-c.only_count)
                    /(c.activity_count-c.mixed-c.only_decoded) DESC, l.position DESC LIMIT ?3"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let missing = stmt.query_map(params![day, day + DAY, limit], |row| {
                Ok(FlowKey {
                    position: row.get(0)?,
                    height: row.get(1)?,
                    txid: row.get(2)?,
                })
            })?;
            Ok(missing.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    pub async fn missing_flows(&self) -> Result<Vec<FlowKey>> {
        self.read(|conn| {
            let sql = format!("WITH recent AS ({RECENT_TXS})
                SELECT r.position, r.height, r.txid FROM recent r LEFT JOIN transaction_flows f USING(position)
                WHERE f.position IS NULL ORDER BY r.position DESC");
            let mut statement = conn.prepare_cached(&sql)?;
            let missing = statement.query_map([FLOW_WINDOW], |row| {
                Ok(FlowKey {
                    position: row.get(0)?,
                    height: row.get(1)?,
                    txid: row.get(2)?,
                })
            })?;
            Ok(missing.collect::<rusqlite::Result<Vec<_>>>()?)
        }).await
    }
    pub async fn save_flows(&self, rows: Vec<(FlowKey, Flow)>) -> Result<()> {
        self.writer.call_raw(move |conn| -> Result<()> {
            let tx = conn.transaction()?;
            for (key, flow) in rows {
                // A rollback can replace the leaf at this position during the network request.
                tx.prepare_cached("INSERT OR IGNORE INTO transaction_flows
                    SELECT position, ?4, ?5 FROM leaves WHERE position=?1 AND txid=?2 AND height=?3")?
                    .execute(params![key.position,key.txid,key.height,flow.net_zat,flow.ironwood_only])?;
            }
            tx.commit()?;
            Ok(())
        }).await?
    }
    pub async fn save_balance(&self, hash: Bytes32, amount: u64) -> Result<()> {
        self.writer.call_raw(move |conn| -> Result<()> {
            conn.execute("INSERT OR IGNORE INTO pool_balances SELECT height,?1 FROM blocks WHERE hash=?2",params![amount,hash])?;
            Ok(())
        }).await?
    }
    pub async fn observe_pending(&self, seen: u64, ids: Vec<Bytes32>) -> Result<()> {
        self.writer
            .call_raw(move |conn| -> Result<()> {
                let tx = conn.transaction()?;
                tx.execute(
                    "DELETE FROM pending_sightings WHERE first_seen < ?1",
                    [seen.saturating_sub(DAY)],
                )?;
                for id in ids {
                    tx.prepare_cached("INSERT OR IGNORE INTO pending_sightings VALUES (?1,?2)")?
                        .execute(params![id, seen])?;
                }
                tx.commit()?;
                Ok(())
            })
            .await?
    }
    pub async fn activity(&self) -> Result<Value> {
        let mut cached = self.activity_cache.lock().await;
        if let Some((at, data)) = &*cached
            && at.elapsed().as_secs() < 15
        {
            return Ok(data.clone());
        }
        let value = self.read(activity_snapshot).await?;
        *cached = Some((std::time::Instant::now(), value.clone()));
        Ok(value)
    }

    pub async fn activity_year(&self, year: i32) -> Result<Option<Vec<Day>>> {
        let (Some(start), Some(end)) = (year_start(year), year_start(year.saturating_add(1)))
        else {
            return Ok(None);
        };
        self.read(move |conn| {
            let Some((activation, tip)) = flow_day_range(conn)? else {
                return Ok(None);
            };
            let first = start.max(activation) / DAY;
            let last = (end - DAY).min(tip) / DAY;
            if first > last {
                return Ok(None);
            }
            let start_height: u32 =
                conn.query_row("SELECT start_height FROM metadata", [], |r| r.get(0))?;
            Ok(Some(read_days(conn, start_height, first, last)?))
        })
        .await
    }

    pub async fn usage_gap(&self) -> Result<Option<(u32, u32)>> {
        self.read(|conn| {
            let tip = tip(conn)?;
            let activation: u32 =
                conn.query_row("SELECT start_height FROM metadata", [], |r| r.get(0))?;
            let min: Option<u32> = conn.query_row(
                "SELECT MIN(height) FROM blocks WHERE time>=?1 AND height>=?2",
                params![(tip.time / DAY).saturating_sub(364) * DAY, activation],
                |r| r.get(0),
            )?;
            let Some(min) = min else { return Ok(None) };
            // A new tip may appear while we are traversing older ranges. Only fetch the
            // contiguous missing suffix ending at the newest hole.
            let end: Option<u32> = conn
                .query_row(
                    "SELECT b.height FROM blocks b LEFT JOIN block_usage u USING(height)
                WHERE b.height>=?1 AND u.height IS NULL ORDER BY b.height DESC LIMIT 1",
                    [min],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(end) = end else { return Ok(None) };
            let previous: Option<u32> = conn.query_row(
                "SELECT MAX(height) FROM block_usage WHERE height<?1",
                [end],
                |r| r.get(0),
            )?;
            Ok(Some((min.max(previous.map_or(min, |h| h + 1)), end)))
        })
        .await
    }
    pub async fn save_usage(&self, rows: Vec<crate::usage::BlockUsage>) -> Result<()> {
        self.writer.call_raw(move |conn| -> Result<()> {
            let tx = conn.transaction()?;
            for row in rows {
                let n = tx.prepare_cached("INSERT INTO block_usage SELECT height,?5,?6 FROM blocks
                    WHERE height=?1 AND hash=?2 AND activity_count=?3 AND leaf_count-first_position=?4
                    ON CONFLICT(height) DO UPDATE SET ironwood_only=excluded.ironwood_only,shielded_actions=excluded.shielded_actions")?
                    .execute(params![row.height,row.hash,row.transactions,row.ironwood_actions,row.ironwood_only,row.shielded_actions])?;
                ensure!(n == 1, "pool activity differs from indexed block {}", row.height);
            }
            tx.commit()?;
            Ok(())
        }).await?
    }
}

fn flow_day_range(conn: &rusqlite::Connection) -> Result<Option<(u64, u64)>> {
    let (activation, latest): (Option<u64>, u64) = conn.query_row(
        "SELECT (SELECT b.time FROM blocks b JOIN metadata m ON b.height=m.start_height), MAX(time) FROM blocks",
        [],
        |row| Ok((row.get(0)?,row.get(1)?)),
    )?;
    let last = latest / DAY;
    Ok(activation.map(|time| (time / DAY * DAY, last * DAY)))
}

fn calculate_day_flows(conn: &rusqlite::Connection, day: u64) -> Result<DailyFlows> {
    let sql = format!(
        "{DAY_COVERAGE}
        SELECT COALESCE(SUM(activity_count),0), COALESCE(SUM(mixed+only_count),0),
            COALESCE(SUM(only_count),0) FROM coverage"
    );
    let (transactions, checked, ironwood_only) =
        conn.query_row(&sql, params![day, day + DAY], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
    let mut stmt = conn.prepare_cached(
        "SELECT f.net_zat FROM blocks b JOIN leaves l USING(height)
        JOIN transaction_flows f USING(position)
        WHERE b.time>=?1 AND b.time<?2 AND f.ironwood_only=0",
    )?;
    let mut amounts = FlowAmounts::default();
    for net in stmt.query_map(params![day, day + DAY], |r| r.get(0))? {
        amounts.push(net?);
    }
    Ok(DailyFlows {
        transactions,
        checked,
        ironwood_only,
        in_count: amounts.deposits.len(),
        out_count: amounts.withdrawals.len(),
        median_in_zat: median(&mut amounts.deposits),
        median_out_zat: median(&mut amounts.withdrawals),
    })
}

fn year_start(year: i32) -> Option<u64> {
    NaiveDate::from_ymd_opt(year, 1, 1)?
        .and_hms_opt(0, 0, 0)?
        .and_utc()
        .timestamp()
        .try_into()
        .ok()
}

fn calendar_days(
    conn: &rusqlite::Connection,
    snap: &Snapshot,
    end: u64,
) -> Result<(Vec<Day>, Option<u64>)> {
    let start: Option<u64> = conn
        .query_row(
            "SELECT time FROM blocks WHERE height=?1",
            [snap.start_height],
            |r| r.get(0),
        )
        .optional()?;
    let Some(start) = start else {
        return Ok((Vec::new(), None));
    };
    let activation_day = start / DAY;
    let last = end / DAY;
    let year = DateTime::from_timestamp(i64::try_from(end)?, 0)
        .ok_or_else(|| anyhow::anyhow!("invalid indexed block time"))?
        .year();
    let current_year = year_start(year).unwrap_or_default() / DAY;
    let first = activation_day.max(current_year.min(last.saturating_sub(364)));
    Ok((
        read_days(conn, snap.start_height, first, last)?,
        Some(activation_day),
    ))
}

fn read_days(
    conn: &rusqlite::Connection,
    start_height: u32,
    first: u64,
    last: u64,
) -> Result<Vec<Day>> {
    let mut days: Vec<_> = (first..=last)
        .map(|d| Day {
            start: d * DAY,
            classified: true,
            ..Default::default()
        })
        .collect();
    let mut stmt = conn.prepare_cached(
        "SELECT time/3600, SUM(activity_count), SUM(COALESCE(ironwood_only,0)),
                MAX(u.height IS NULL AND activity_count>0)
         FROM blocks b LEFT JOIN block_usage u USING(height)
         WHERE time>=?1 AND time<?2 AND b.height>=?3 GROUP BY time/3600",
    )?;
    let mut rows = stmt.query(params![first * DAY, (last + 1) * DAY, start_height])?;
    while let Some(row) = rows.next()? {
        let hour: u64 = row.get(0)?;
        let day = &mut days[(hour / 24 - first) as usize];
        let slot = (hour % 24) as usize;
        day.hours[slot] = row.get(1)?;
        day.internal[slot] = row.get(2)?;
        day.classified &= !row.get::<_, bool>(3)?;
    }
    Ok(days)
}

fn weekday_medians(days: &[Day], activation: Option<u64>, internal: bool) -> Value {
    let complete_days = days
        .iter()
        .take(days.len().saturating_sub(1))
        .filter_map(|day| {
            let epoch_day = day.start / DAY;
            if Some(epoch_day) == activation || (internal && !day.classified) {
                return None;
            }
            let hours = if internal { day.internal } else { day.hours };
            // Unix epoch day zero was a Thursday, the fourth Monday-based weekday.
            Some((((epoch_day + 3) % 7) as usize, hours))
        });
    json!(weekdays::summarize_weekdays(complete_days))
}

fn recent_flows(conn: &rusqlite::Connection) -> Result<Value> {
    let sql = format!(
        "WITH recent AS ({RECENT_TXS}) SELECT net_zat,ironwood_only FROM recent LEFT JOIN transaction_flows USING(position)"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let mut rows = stmt.query([FLOW_WINDOW])?;
    let mut amounts = FlowAmounts::default();
    let (mut population, mut decoded, mut only) = (0, 0, 0);
    while let Some(row) = rows.next()? {
        population += 1;
        if let Some(amount) = row.get::<_, Option<i64>>(0)? {
            decoded += 1;
            if row.get::<_, bool>(1)? {
                only += 1;
            } else {
                amounts.push(amount);
            }
        }
    }
    Ok(
        json!({"window":FLOW_WINDOW, "transactions":population, "decoded":decoded, "ironwood_only":only,
        "in_count":amounts.deposits.len(),"out_count":amounts.withdrawals.len(),
        "median_in_zat":median(&mut amounts.deposits),"median_out_zat":median(&mut amounts.withdrawals)}),
    )
}

fn commitment_breakdown(
    conn: &rusqlite::Connection,
    days: &[Day],
    snapshot: &Snapshot,
) -> Result<Value> {
    let transactions: u64 = days.iter().flat_map(|day| day.hours).sum();
    let Some(first_day) = days.first() else {
        return Ok(
            json!({"from_calendar": 0, "from_coinbase": 0, "from_earlier": 0,
            "leaves": snapshot.checkpoint.leaf_count, "transactions":transactions, "most_common": []}),
        );
    };
    let (coinbase, from_calendar): (u64,u64) = conn.query_row(
        "SELECT COALESCE(SUM(coinbase_commitments),0),
            COALESCE(SUM(CASE WHEN time>=?1 THEN leaf_count-first_position-coinbase_commitments ELSE 0 END),0)
         FROM blocks WHERE height>=?2",
        params![first_day.start,snapshot.start_height],
        |row| Ok((row.get(0)?,row.get(1)?)),
    )?;
    let from_earlier = snapshot
        .checkpoint
        .leaf_count
        .checked_sub(from_calendar + coinbase)
        .ok_or_else(|| anyhow::anyhow!("commitment counts exceed indexed leaves"))?;
    let most_common = conn
        .prepare_cached(
            "WITH counts AS (
            SELECT actions, SUM(transactions) AS txs FROM block_transaction_actions
            JOIN blocks USING(height) WHERE time>=?1 GROUP BY actions
         )
         SELECT actions FROM counts WHERE txs=(SELECT MAX(txs) FROM counts) ORDER BY actions",
        )?
        .query_map([first_day.start], |row| row.get::<_, u64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(
        json!({"from_calendar":from_calendar,"from_coinbase":coinbase,"from_earlier":from_earlier,
        "leaves":snapshot.checkpoint.leaf_count,"transactions":transactions,"most_common":most_common}),
    )
}

fn activity_snapshot(conn: &rusqlite::Connection) -> Result<Value> {
    let snap = snapshot(conn, None)?;
    let end: u64 = conn.query_row("SELECT MAX(time) FROM blocks", [], |r| r.get(0))?;
    let (days, activation) = calendar_days(conn, &snap, end)?;
    let rolling_days = &days[days.len().saturating_sub(365)..];
    let commitments = commitment_breakdown(conn, rolling_days, &snap)?;
    let balance = conn
        .query_row(
            "SELECT amount_zat, height, time FROM pool_balances JOIN blocks USING(height)
         ORDER BY height DESC LIMIT 1",
            [],
            |row| {
                Ok(json!({
                    "amount_zat": row.get::<_, u64>(0)?,
                    "height": row.get::<_, u32>(1)?,
                    "time": row.get::<_, u64>(2)?,
                }))
            },
        )
        .optional()?;
    let intervals = conn
        .prepare_cached(
            "SELECT b.time-a.time FROM blocks a JOIN blocks b ON b.height=a.height+9
        WHERE a.time>=?1 AND b.time>=a.time AND a.height>=?2",
        )?
        .query_map(
            params![end.saturating_sub(7 * DAY), snap.start_height],
            |r| r.get(0),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let cutoff = unix_time().saturating_sub(7 * DAY);
    let waits = conn
        .prepare_cached("SELECT seconds FROM confirmation_times WHERE observed_at>=?1")?
        .query_map([cutoff], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let confirmed: u64 = conn.query_row(
        "SELECT COALESCE(SUM(activity_count),0) FROM blocks WHERE time>=?1",
        [cutoff],
        |r| r.get(0),
    )?;
    let (blocks, actions): (u32, Option<f64>) = conn.query_row(
        "SELECT COUNT(*), AVG(shielded_actions)
        FROM block_usage WHERE height>?1",
        [snap.checkpoint.height.saturating_sub(100)],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    // ZIP 218/259 introduced a shared shielded action budget of 330 per block at NU7.
    let nu7 = match snap.network.as_str() {
        "main" => Some(3_543_405),
        "test" => Some(4_465_026),
        _ => None,
    };
    let capacity = nu7
        .filter(|height| snap.checkpoint.height.saturating_sub(99) >= *height)
        .map(|_| 330);
    Ok(
        json!({"checkpoint":snap.checkpoint,"network":snap.network,"days":days,"activation_day":activation.map(|day| day * DAY),"commitments":commitments,
        "weekdays":weekday_medians(rolling_days,activation,false),"internal_weekdays":weekday_medians(rolling_days,activation,true),
        "flows":recent_flows(conn)?,"balance":balance,"ten_confirmations":times(intervals),
        "first_confirmation":times(waits),"confirmed_7d":confirmed,
        "load":{"blocks":blocks,"mean_actions":actions,"capacity":capacity}}),
    )
}
