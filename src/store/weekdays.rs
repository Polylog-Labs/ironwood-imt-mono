use super::*;
use chrono::{DateTime, Datelike, Days, Timelike};
use chrono_tz::Tz;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Serialize, Default)]
pub struct WeekdaySummary {
    pub days: usize,
    pub median: Option<f64>,
    pub peak_hour: Option<u8>,
}

pub(super) struct WeekdayCacheEntry {
    zone: Tz,
    updated: Instant,
    summaries: [WeekdaySummary; 7],
}

pub(super) fn summarize_weekdays(
    days: impl IntoIterator<Item = (usize, [u64; 24])>,
) -> [WeekdaySummary; 7] {
    let mut by_weekday: [Vec<[u64; 24]>; 7] = Default::default();
    for (weekday, hours) in days {
        by_weekday[weekday].push(hours);
    }
    by_weekday.map(|days| {
        let mut daily: Vec<u64> = days.iter().map(|hours| hours.iter().sum()).collect();
        let median = activity::median(&mut daily);
        let mut peak = 0.0;
        let mut peak_hour = None;
        for hour in 0..24 {
            let mut hourly: Vec<_> = days.iter().map(|day| day[hour]).collect();
            let typical = activity::median(&mut hourly).unwrap_or_default();
            if typical > peak {
                peak = typical;
                peak_hour = Some(hour as u8);
            }
        }
        WeekdaySummary {
            days: days.len(),
            median,
            peak_hour,
        }
    })
}

impl Store {
    pub async fn zoned_weekdays(&self, zone: Tz) -> Result<[WeekdaySummary; 7]> {
        let mut cache = self.weekday_cache.lock().await;
        cache.retain(|entry| entry.updated.elapsed() < Duration::from_secs(60));
        if let Some(entry) = cache.iter().find(|entry| entry.zone == zone) {
            return Ok(entry.summaries);
        }
        let summaries = self.read(move |conn| local_weekdays(conn, zone)).await?;
        if cache.len() >= 32 {
            cache.pop_front();
        }
        cache.push_back(WeekdayCacheEntry {
            zone,
            updated: Instant::now(),
            summaries,
        });
        Ok(summaries)
    }
}

fn local_weekdays(conn: &rusqlite::Connection, zone: Tz) -> Result<[WeekdaySummary; 7]> {
    let snap = snapshot(conn, None)?;
    let activation: Option<i64> = conn
        .query_row(
            "SELECT time FROM blocks WHERE height=?1",
            [snap.start_height],
            |row| row.get(0),
        )
        .optional()?;
    let Some(activation) = activation else {
        return Ok(Default::default());
    };
    let end: i64 = conn.query_row("SELECT MAX(time) FROM blocks", [], |row| row.get(0))?;
    let in_zone = |time| {
        DateTime::from_timestamp(time, 0)
            .map(|stamp| stamp.with_timezone(&zone))
            .ok_or_else(|| anyhow::anyhow!("invalid indexed block time"))
    };
    let activation_date = in_zone(activation)?.date_naive();
    let last = in_zone(end)?.date_naive();
    // Match the calendar's range while excluding the first activation day and the latest day.
    let first = activation_date + Days::new(1);
    let first = first.max(last - Days::new(364));
    let days = (last - first).num_days().max(0) as usize;
    let mut counts = vec![[0_u64; 24]; days];

    let mut stmt = conn.prepare_cached(
        "SELECT time, activity_count FROM blocks
         WHERE time>=?1 AND height>=?2 AND activity_count>0",
    )?;
    // The extra days cover time-zone offsets and daylight-saving transitions at the boundary.
    let mut rows = stmt.query(params![
        activation.max(end - 367 * 86_400),
        snap.start_height
    ])?;
    while let Some(row) = rows.next()? {
        let local = in_zone(row.get(0)?)?;
        let index = (local.date_naive() - first).num_days();
        if let Ok(index) = usize::try_from(index)
            && let Some(day) = counts.get_mut(index)
        {
            // Both occurrences of a repeated clock hour contribute to that day's hour.
            day[local.hour() as usize] += row.get::<_, u64>(1)?;
        }
    }

    let weekday = first.weekday().num_days_from_monday() as usize;
    Ok(summarize_weekdays(
        counts
            .into_iter()
            .enumerate()
            .map(|(i, hours)| ((weekday + i) % 7, hours)),
    ))
}
