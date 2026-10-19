use crate::{
    error::{Fault, Result},
    store::{Store, Usage},
};
use chrono::{Datelike, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;
use std::collections::BTreeMap;

pub const PRICE_SOURCE: &str = "https://platform.claude.com/docs/en/about-claude/pricing";
pub const PRICE_DATE: &str = "2026-10-18";

#[derive(Default, Serialize)]
struct AccountCycle {
    start: i64,
    end: i64,
    fallback_records: u64,
    totals: Totals,
}
#[derive(Default, Serialize)]
struct AccountCycles {
    current: Option<AccountCycle>,
    previous: Option<AccountCycle>,
}
#[cfg(test)]
mod account_cycle_tests {
    use super::*;
    #[test]
    fn account_windows_expire_restart_after_idle_and_keep_unknowns_explicit() {
        let mut cycles = AccountCycles::default();
        let mut u = Usage {
            started_at: Some(100),
            accounting_version: 1,
            status: 200,
            input_estimate: 10,
            output_estimate: 2,
            ..Default::default()
        };
        cycles.add(120, &u);
        u.started_at = Some(18099);
        u.internal = true;
        cycles.add(18120, &u);
        assert_eq!(cycles.current.as_ref().unwrap().start, 100);
        assert_eq!(cycles.current.as_ref().unwrap().totals.records, 2);
        assert_eq!(cycles.current.as_ref().unwrap().totals.internal_requests, 1);
        u.started_at = Some(18100);
        u.internal = false;
        cycles.add(18130, &u);
        assert_eq!(cycles.previous.as_ref().unwrap().totals.requests, 1);
        assert_eq!(cycles.current.as_ref().unwrap().start, 18100);
        cycles.expire(36100);
        assert!(cycles.current.is_none());
        assert_eq!(cycles.previous.as_ref().unwrap().start, 18100);
        u.started_at = Some(100000);
        cycles.add(100030, &u);
        assert_eq!(cycles.current.as_ref().unwrap().start, 100000);
        assert_eq!(cycles.previous.as_ref().unwrap().start, 18100);
        u.started_at = None;
        u.status = 502;
        cycles.add(100040, &u);
        assert_eq!(cycles.current.as_ref().unwrap().totals.records, 1);
        u.started_at = Some(100050);
        cycles.add(100060, &u);
        assert_eq!(
            cycles.current.as_ref().unwrap().totals.unmetered_requests,
            1
        );
        u.accounting_version = 0;
        u.started_at = None;
        u.status = 200;
        cycles.add(100070, &u);
        assert_eq!(cycles.current.as_ref().unwrap().fallback_records, 1);
        assert_eq!(cycles.current.as_ref().unwrap().totals.input_estimate, 30);
    }
}
impl AccountCycles {
    fn add(&mut self, at: i64, usage: &Usage) {
        // Old ledgers have only completion time. Never invent an upstream reset.
        if usage.accounting_version > 0 && usage.started_at.is_none() && usage.status != 200 {
            return;
        }
        let start = usage
            .started_at
            .filter(|v| *v > 0 && *v <= at)
            .unwrap_or(at);
        if self.current.as_ref().is_none_or(|c| start >= c.end) {
            self.previous = self.current.take().or(self.previous.take());
            self.current = Some(AccountCycle {
                start,
                end: start.saturating_add(18000),
                ..Default::default()
            });
        }
        if let Some(cycle) = &mut self.current {
            cycle.fallback_records += u64::from(usage.started_at.is_none());
            cycle.totals.add(usage);
        }
    }
    fn expire(&mut self, at: i64) {
        if self.current.as_ref().is_some_and(|c| c.end <= at) {
            self.previous = self.current.take();
        }
    }
}
impl Store {
    pub async fn account_cycles(
        &self,
        ids: Vec<String>,
        now: i64,
    ) -> Result<BTreeMap<String, Value>> {
        let mut cycles: BTreeMap<_, _> = ids
            .into_iter()
            .map(|id| (id, AccountCycles::default()))
            .collect();
        let mut rows = sqlx::query("SELECT at,data FROM usage WHERE at<=? ORDER BY at,seq")
            .bind(now)
            .fetch(&self.db);
        while let Some(row) = rows.try_next().await.map_err(|_| Fault::internal())? {
            let usage: Usage = serde_json::from_str(&row.get::<String, _>("data"))
                .map_err(|_| Fault::internal())?;
            if let Some(cycle) = cycles.get_mut(&usage.account_id) {
                cycle.add(row.get("at"), &usage);
            }
        }
        Ok(cycles.into_iter().map(|(id,mut value)| {
            value.expire(now);
            (id,json!({"current":value.current,"previous":value.previous,"duration_seconds":18000,"estimated":true,"basis":"local_generation_start","historical_fallback":"completion_time","at":now}))
        }).collect())
    }
}

/// USD per million tokens, expressed in millionths of a dollar for exact math.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rates {
    pub input: u64,
    pub output: u64,
    pub write_1h: u64,
    pub read: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Charge {
    pub price_date: String,
    pub rates_microusd_per_mtok: Rates,
    pub tier: String,
    pub input_nano_usd: u64,
    pub output_nano_usd: u64,
    pub cache_write_nano_usd: u64,
    pub cache_read_nano_usd: u64,
    pub total_nano_usd: u64,
    pub baseline_nano_usd: u64,
}
pub fn rates(model: &str, input: usize) -> Option<(Rates, &'static str)> {
    // Exact native IDs only: an alias must be resolved before accounting.
    let (base, output, read, tier) = match model {
        "claude-haiku-5-5" if input > 100_000 => (500_000, 2_500_000, 50_000, "over_100k"),
        "claude-haiku-5-5" => (100_000, 500_000, 10_000, "up_to_100k"),
        "claude-sonnet-5-5" => (2_000_000, 10_000_000, 100_000, "standard"),
        "claude-opus-5-5" => (4_000_000, 20_000_000, 200_000, "standard"),
        "claude-sonnet-5" => (2_000_000, 10_000_000, 200_000, "standard"),
        "claude-sonnet-4-6" | "claude-sonnet-4-5-20250929" => {
            (3_000_000, 15_000_000, 300_000, "standard")
        }
        "claude-haiku-4-5-20251001" => (1_000_000, 5_000_000, 100_000, "standard"),
        "claude-opus-5"
        | "claude-opus-4-8"
        | "claude-opus-4-7"
        | "claude-opus-4-6"
        | "claude-opus-4-5-20251101" => (5_000_000, 25_000_000, 500_000, "standard"),
        _ => return None,
    };
    Some((
        Rates {
            input: base,
            output,
            write_1h: base * 2,
            read,
        },
        tier,
    ))
}
fn amount(tokens: usize, rate: u64) -> u64 {
    // (token * microUSD/MTok) /1000 = nanoUSD. Round only at category boundary.
    ((tokens as u128 * rate as u128 + 500) / 1000).min(u64::MAX as u128) as u64
}
pub fn quote(u: &Usage) -> Option<Charge> {
    if u.status != 200 || u.accounting_version != 1 {
        return None;
    }
    let cache = u.simulated_cache?;
    let (rates, tier) = rates(&u.model, u.input_estimate)?;
    let read = if cache {
        u.cache_read_simulated.min(u.input_estimate)
    } else {
        0
    };
    let write = if cache {
        u.cache_creation_simulated.min(u.input_estimate - read)
    } else {
        0
    };
    let normal = u.input_estimate - read - write;
    let input = amount(normal, rates.input);
    let output = amount(u.output_estimate, rates.output);
    let creation = amount(write, rates.write_1h);
    let reading = amount(read, rates.read);
    Some(Charge {
        price_date: PRICE_DATE.to_owned(),
        tier: tier.to_owned(),
        input_nano_usd: input,
        output_nano_usd: output,
        cache_write_nano_usd: creation,
        cache_read_nano_usd: reading,
        total_nano_usd: input
            .saturating_add(output)
            .saturating_add(creation)
            .saturating_add(reading),
        baseline_nano_usd: amount(u.input_estimate, rates.input).saturating_add(output),
        rates_microusd_per_mtok: rates,
    })
}

#[derive(Clone, Default, Serialize)]
pub struct Totals {
    pub records: u64,
    pub requests: u64,
    pub internal_requests: u64,
    pub successful_requests: u64,
    pub unmetered_requests: u64,
    pub legacy_records: u64,
    pub priced_requests: u64,
    pub unpriced_requests: u64,
    pub cache_enabled_requests: u64,
    pub input_estimate: u64,
    pub output_estimate: u64,
    pub physical_estimate: u64,
    pub uncached_input: u64,
    pub cache_creation_simulated: u64,
    pub cache_read_simulated: u64,
    pub priced_uncached_input: u64,
    pub priced_output: u64,
    pub priced_cache_creation: u64,
    pub priced_cache_read: u64,
    pub total_nano_usd: u64,
    pub baseline_nano_usd: u64,
    pub input_nano_usd: u64,
    pub output_nano_usd: u64,
    pub cache_write_nano_usd: u64,
    pub cache_read_nano_usd: u64,
}
impl Totals {
    fn add(&mut self, u: &Usage) {
        self.records += 1;
        self.requests += u64::from(!u.internal);
        self.internal_requests += u64::from(u.internal);
        self.successful_requests += u64::from(u.status == 200);
        self.unmetered_requests += u64::from(u.status != 200);
        self.legacy_records += u64::from(u.accounting_version != 1);
        self.input_estimate = self.input_estimate.saturating_add(u.input_estimate as u64);
        self.output_estimate = self
            .output_estimate
            .saturating_add(u.output_estimate as u64);
        self.physical_estimate = self
            .physical_estimate
            .saturating_add(u.physical_estimate as u64);
        let read = u.cache_read_simulated.min(u.input_estimate);
        let write = u.cache_creation_simulated.min(u.input_estimate - read);
        self.uncached_input = self
            .uncached_input
            .saturating_add((u.input_estimate - read - write) as u64);
        self.cache_read_simulated = self.cache_read_simulated.saturating_add(read as u64);
        self.cache_creation_simulated = self.cache_creation_simulated.saturating_add(write as u64);
        self.cache_enabled_requests += u64::from(u.simulated_cache == Some(true));
        if let Some(c) = &u.charge {
            self.priced_requests += 1;
            self.priced_uncached_input = self
                .priced_uncached_input
                .saturating_add((u.input_estimate - read - write) as u64);
            self.priced_output = self.priced_output.saturating_add(u.output_estimate as u64);
            self.priced_cache_creation = self.priced_cache_creation.saturating_add(write as u64);
            self.priced_cache_read = self.priced_cache_read.saturating_add(read as u64);
            macro_rules! sums {($($field:ident),*)=>{$(self.$field=self.$field.saturating_add(c.$field);)*};}
            sums!(
                total_nano_usd,
                baseline_nano_usd,
                input_nano_usd,
                output_nano_usd,
                cache_write_nano_usd,
                cache_read_nano_usd
            );
        } else if u.status == 200 {
            self.unpriced_requests += 1;
        }
    }
    fn merge(&mut self, other: &Self) {
        macro_rules! sums {($($field:ident),*)=>{$(self.$field=self.$field.saturating_add(other.$field);)*};}
        sums!(
            records,
            requests,
            internal_requests,
            successful_requests,
            unmetered_requests,
            legacy_records,
            priced_requests,
            unpriced_requests,
            cache_enabled_requests,
            input_estimate,
            output_estimate,
            physical_estimate,
            uncached_input,
            cache_creation_simulated,
            cache_read_simulated,
            priced_uncached_input,
            priced_output,
            priced_cache_creation,
            priced_cache_read,
            total_nano_usd,
            baseline_nano_usd,
            input_nano_usd,
            output_nano_usd,
            cache_write_nano_usd,
            cache_read_nano_usd
        );
    }
    fn tokens(&self) -> u64 {
        self.input_estimate.saturating_add(self.output_estimate)
    }
}
#[derive(Serialize)]
pub struct Window {
    pub start: i64,
    pub end: i64,
    pub buckets: Vec<i64>,
}
fn midnight(date: NaiveDate, tz: Tz) -> Result<i64> {
    let local = date.and_hms_opt(0, 0, 0).ok_or_else(Fault::internal)?;
    // Earliest resolves a repeated midnight. For a skipped midnight, the next
    // valid civil hour is the start of that local day (including historical zones).
    for hours in 0..=24 {
        if let Some(v) = tz
            .from_local_datetime(&(local + chrono::Duration::hours(hours)))
            .earliest()
        {
            return Ok(v.timestamp());
        }
    }
    Err(Fault::invalid("cannot resolve calendar boundary"))
}
pub fn windows(
    range: &str,
    granularity: &str,
    timezone: &str,
    now: i64,
    earliest: i64,
) -> Result<(Window, Window)> {
    let tz: Tz = timezone
        .parse()
        .map_err(|_| Fault::invalid("analytics requires a valid IANA timezone"))?;
    let local = Utc
        .timestamp_opt(now, 0)
        .single()
        .ok_or_else(Fault::internal)?
        .with_timezone(&tz);
    let date = local.date_naive();
    let month =
        NaiveDate::from_ymd_opt(date.year(), date.month(), 1).ok_or_else(Fault::internal)?;
    let (start, end) = match range {
        "today" => (midnight(date, tz)?, now + 1),
        "24h" => (now - 86400, now + 1),
        "7d" => (now - 7 * 86400, now + 1),
        "month" => (midnight(month, tz)?, now + 1),
        "last_month" => {
            let previous = month.pred_opt().ok_or_else(Fault::internal)?;
            let first = NaiveDate::from_ymd_opt(previous.year(), previous.month(), 1)
                .ok_or_else(Fault::internal)?;
            (midnight(first, tz)?, midnight(month, tz)?)
        }
        "all" => (earliest.min(now), now + 1),
        _ => return Err(Fault::invalid("invalid usage range")),
    };
    let buckets: Vec<i64> = match granularity {
        "hour" => (0..=24).map(|i| now - 86400 + i * 3600).collect(),
        "day" => (0..=7).map(|i| now - 7 * 86400 + i * 86400).collect(),
        _ => return Err(Fault::invalid("invalid usage granularity")),
    };
    let mut trend = Window {
        start: buckets[0],
        end: now + 1,
        buckets,
    };
    // Include this second in the last rolling bucket, without creating a25th/8th point.
    *trend.buckets.last_mut().expect("nonempty fixed buckets") = now + 1;
    Ok((
        Window {
            start,
            end,
            buckets: vec![],
        },
        trend,
    ))
}
struct Group {
    name: String,
    totals: Totals,
    trend: Vec<Totals>,
}
impl Store {
    pub async fn analytics(
        &self,
        range: &str,
        granularity: &str,
        timezone: &str,
        now: i64,
    ) -> Result<Value> {
        // No LIMIT: totals must not silently become the newest10000 requests.
        let earliest: Option<i64> = sqlx::query_scalar("SELECT MIN(at) FROM usage")
            .fetch_one(&self.db)
            .await
            .map_err(|_| Fault::internal())?;
        let (window, trend) = windows(range, granularity, timezone, now, earliest.unwrap_or(now))?;
        let mut rows =
            sqlx::query("SELECT at,data FROM usage WHERE at>=? AND at<? ORDER BY at,seq")
                .bind(window.start.min(trend.start))
                .bind(window.end.max(trend.end))
                .fetch(&self.db);
        let mut totals = Totals::default();
        let mut accounts = BTreeMap::<String, Totals>::new();
        let mut groups = BTreeMap::<String, Group>::new();
        let points = trend.buckets.len() - 1;
        let mut grouped = false;
        while let Some(row) = rows.try_next().await.map_err(|_| Fault::internal())? {
            let at: i64 = row.get("at");
            let u: Usage = serde_json::from_str(&row.get::<String, _>("data"))
                .map_err(|_| Fault::internal())?;
            let mut id = if u.accounting_version == 1 {
                u.token_id
                    .clone()
                    .unwrap_or_else(|| "environment".to_owned())
            } else {
                "unassigned".to_owned()
            };
            let mut label = u.token_name.clone().unwrap_or_else(|| {
                if id == "environment" {
                    "环境网关密钥".to_owned()
                } else if id == "unassigned" {
                    "历史未归属".to_owned()
                } else {
                    format!("令牌 {}", &id[..id.len().min(8)])
                }
            });
            if groups.len() >= 2048 && !groups.contains_key(&id) {
                id = "other".to_owned();
                label = "其他历史令牌".to_owned();
                grouped = true;
            }
            let group = groups.entry(id).or_insert_with(|| Group {
                name: label.clone(),
                totals: Totals::default(),
                trend: vec![Totals::default(); points],
            });
            group.name = label;
            if at >= window.start && at < window.end {
                totals.add(&u);
                group.totals.add(&u);
                if accounts.len() < 1024 || accounts.contains_key(&u.account_id) {
                    accounts.entry(u.account_id.clone()).or_default().add(&u);
                } else {
                    accounts.entry("other".to_owned()).or_default().add(&u);
                }
            }
            if at >= trend.start && at < trend.end {
                let index = trend
                    .buckets
                    .partition_point(|v| *v <= at)
                    .saturating_sub(1)
                    .min(points - 1);
                group.trend[index].add(&u);
            }
        }
        let mut ranked: Vec<_> = groups.iter().collect();
        ranked.sort_by_key(|(id, g)| {
            (
                std::cmp::Reverse(g.trend.iter().map(Totals::tokens).sum::<u64>()),
                *id,
            )
        });
        let mut series: Vec<Value> = ranked
            .iter()
            .take(12)
            .map(|(id, g)| json!({"id":id,"name":g.name,"points":g.trend}))
            .collect();
        if ranked.len() > 12 {
            let mut rest = vec![Totals::default(); points];
            for (_, g) in ranked.iter().skip(12) {
                for (out, value) in rest.iter_mut().zip(&g.trend) {
                    out.merge(value);
                }
            }
            series.push(json!({"id":"other_series","name":"其余令牌","points":rest}));
        }
        let tokens: Vec<Value> = groups
            .iter()
            .filter(|(_, g)| g.totals.records > 0)
            .map(|(id, g)| json!({"id":id,"name":g.name,"totals":g.totals}))
            .collect();
        Ok(
            json!({"schema_version":1,"at":now,"range":range,"granularity":granularity,"timezone":timezone,
            "window":window,"trend_window":trend,"totals":totals,"tokens":tokens,"series":series,"by_account":accounts,
            "estimated":true,"currency":"USD","price_source":PRICE_SOURCE,"price_date":PRICE_DATE,"ledger_since":earliest,
            "truncated":false,"grouped_historical_tokens":grouped,"chart_series_limit":12,
            "formula":"uncached_input*input_rate + cache_write_1h*write_rate + cache_read*read_rate + output*output_rate",
            "limitations":"local token estimates; simulated API-equivalent cost, not provider billing; failures/disconnects may be unmetered; no retroactive pricing"}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample(cache: bool, read: usize, model: &str, input: usize) -> Usage {
        Usage {
            accounting_version: 1,
            input_estimate: input,
            output_estimate: 200,
            cache_read_simulated: read,
            cache_creation_simulated: if cache { input - read } else { 0 },
            simulated_cache: Some(cache),
            status: 200,
            model: model.to_owned(),
            ..Usage::default()
        }
    }
    #[test]
    fn cache_write_read_and_disabled_costs_have_exact_category_math() {
        let normal = quote(&sample(false, 0, "claude-sonnet-5-5", 1000)).unwrap();
        assert_eq!(normal.total_nano_usd, 4_000_000);
        let write = quote(&sample(true, 0, "claude-sonnet-5-5", 1000)).unwrap();
        assert_eq!(write.total_nano_usd, 6_000_000);
        assert_eq!(write.baseline_nano_usd, normal.total_nano_usd);
        let hit = quote(&sample(true, 900, "claude-sonnet-5-5", 1000)).unwrap();
        assert_eq!(hit.cache_read_nano_usd, 90_000);
        assert_eq!(hit.cache_write_nano_usd, 400_000);
        assert_eq!(hit.total_nano_usd, 2_490_000);
        assert_eq!(hit.input_nano_usd, 0);
        // Output tokens never also become creation input in this record.
        assert_eq!(hit.output_nano_usd, 2_000_000);
    }
    #[test]
    fn haiku_context_tier_uses_whole_input_and_unknown_or_failed_remains_unknown() {
        let short = quote(&sample(true, 100_000, "claude-haiku-5-5", 100_000)).unwrap();
        let long = quote(&sample(true, 100_000, "claude-haiku-5-5", 100_001)).unwrap();
        assert_eq!(short.tier, "up_to_100k");
        assert_eq!(long.tier, "over_100k");
        assert_eq!(long.cache_read_nano_usd, 5_000_000);
        assert!(quote(&sample(false, 0, "unknown-model", 1000)).is_none());
        let mut bad = sample(false, 0, "claude-sonnet-5-5", 1000);
        bad.status = 502;
        assert!(quote(&bad).is_none());
        bad.status = 200;
        bad.accounting_version = 0;
        assert!(quote(&bad).is_none());
    }
    fn ts(s: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp()
    }
    #[test]
    fn calendar_ranges_use_taipei_leap_month_and_exclusive_boundaries() {
        let now = ts("2024-03-01T00:30:00+08:00");
        let (today, trend) = windows("today", "hour", "Asia/Taipei", now, 0).unwrap();
        assert_eq!(today.start, ts("2024-03-01T00:00:00+08:00"));
        assert_eq!(trend.buckets.len(), 25);
        let (last, trend) = windows("last_month", "day", "Asia/Taipei", now, 0).unwrap();
        assert_eq!(last.start, ts("2024-02-01T00:00:00+08:00"));
        assert_eq!(last.end, today.start);
        assert_eq!(last.end - last.start, 29 * 86400);
        assert_eq!(trend.buckets.len(), 8);
        let (all, _) = windows("all", "hour", "Asia/Taipei", now, 1234).unwrap();
        assert_eq!(all.start, 1234);
        assert!(windows("wat", "day", "Asia/Taipei", now, 0).is_err());
        assert!(windows("24h", "wat", "Asia/Taipei", now, 0).is_err());
        assert!(windows("24h", "hour", "fakezone", now, 0).is_err());
    }
    #[test]
    fn dst_calendar_day_and_year_rollover_are_not_fixed_offsets() {
        let now = ts("2024-03-11T00:30:00-04:00");
        let (a, _) = windows("today", "day", "America/New_York", now, 0).unwrap();
        assert_eq!(a.start, ts("2024-03-11T00:00:00-04:00"));
        let (last, _) = windows(
            "last_month",
            "day",
            "Asia/Taipei",
            ts("2026-01-02T00:00:00+08:00"),
            0,
        )
        .unwrap();
        assert_eq!(last.start, ts("2025-12-01T00:00:00+08:00"));
        assert_eq!(last.end, ts("2026-01-01T00:00:00+08:00"));
    }
}
