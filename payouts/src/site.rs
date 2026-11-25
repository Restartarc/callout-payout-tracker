//! Builds the website's data files from the payout rows.
//!
//!   site/data/summary.json        batches, concentration, top wallets, bands
//!   site/data/w/<shard>.json      per-wallet history, rank and percentile
//!
//! Amounts stay in raw USDC units (6 decimals); the page divides. A wallet's
//! shard is the hex of its first two characters, which keeps file names safe
//! on case-insensitive file systems (base58 is case-sensitive).

use crate::{load_rows, utc};
use anyhow::Result;
use callout_board::ledger::USDC;
use callout_board::payouts::{batches, concentration, totals_by_recipient, Batch, PayoutRow, PAYOUT_WALLETS};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;

/// Payout-size bands, in whole USDC: [low, high).
const BANDS: [(f64, f64); 6] = [(0.0, 1.0), (1.0, 10.0), (10.0, 100.0), (100.0, 1_000.0), (1_000.0, 10_000.0), (10_000.0, f64::INFINITY)];

pub fn shard_of(wallet: &str) -> String {
    wallet.bytes().take(2).map(|b| format!("{b:02x}")).collect()
}

fn bands(amounts: &[u64]) -> Vec<Value> {
    BANDS.iter().map(|(lo, hi)| {
        let inb: Vec<u64> = amounts.iter().copied().filter(|a| { let u = *a as f64 / 1e6; u >= *lo && u < *hi }).collect();
        json!({ "from": lo, "to": if hi.is_finite() { json!(hi) } else { Value::Null },
                "payouts": inb.len(), "total": inb.iter().sum::<u64>() })
    }).collect()
}

fn batch_json(b: &Batch) -> Value {
    json!({ "date": &utc(b.start)[..10], "start": b.start, "end": b.end, "payouts": b.payouts, "wallets": b.recipients,
            "total": b.total, "median": b.median, "p90": b.p90, "max": b.max })
}

/// Rank (1 = most paid) and percentile for every wallet.
pub fn ranks(totals: &BTreeMap<String, u64>) -> BTreeMap<String, (usize, f64)> {
    let mut v: Vec<(&String, &u64)> = totals.iter().collect();
    v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    let n = v.len();
    let mut out = BTreeMap::new();
    let mut i = 0;
    while i < n {
        // Ties share the better rank.
        let mut j = i;
        while j < n && v[j].1 == v[i].1 { j += 1; }
        // Share of wallets paid the same or less.
        let pct = (n - i) as f64 / n as f64;
        for k in i..j { out.insert(v[k].0.clone(), (i + 1, pct)); }
        i = j;
    }
    out
}

pub fn build(out: &Path, site: &Path) -> Result<Value> {
    let rows = load_rows(out, None)?;
    let pays: Vec<&PayoutRow> = rows.iter().filter(|r| r.kind == "payout" && r.mint == USDC).collect();
    let data = site.join("data");
    let wdir = data.join("w");
    if wdir.exists() { std::fs::remove_dir_all(&wdir)?; }
    std::fs::create_dir_all(&wdir)?;

    let all_batches = batches(&rows, USDC, 1800);
    let totals = totals_by_recipient(&rows, USDC);
    let rank = ranks(&totals);

    // Latest complete batch: the newest one, unless it may still be running.
    let latest = all_batches.last();
    let latest_rows: Vec<&&PayoutRow> = match latest {
        Some(b) => pays.iter().filter(|r| r.block_time.map(|t| t >= b.start && t <= b.end).unwrap_or(false)).collect(),
        None => vec![],
    };
    let mut latest_totals: BTreeMap<String, u64> = BTreeMap::new();
    let mut latest_counts: BTreeMap<String, usize> = BTreeMap::new();
    for r in &latest_rows {
        *latest_totals.entry(r.counterparty.clone()).or_default() += r.amount;
        *latest_counts.entry(r.counterparty.clone()).or_default() += 1;
    }
    let mut all_counts: BTreeMap<String, usize> = BTreeMap::new();
    for r in &pays { *all_counts.entry(r.counterparty.clone()).or_default() += 1; }

    // Payout counts come from the same scope as the totals they sit beside.
    let top = |m: &BTreeMap<String, u64>, counts: &BTreeMap<String, usize>, k: usize| -> Vec<Value> {
        let mut v: Vec<(&String, &u64)> = m.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        v.into_iter().take(k).map(|(w, t)| json!({ "wallet": w, "total": t, "payouts": counts.get(w).copied().unwrap_or(0) })).collect()
    };
    let state: Value = std::fs::read(out.join(format!("state-{}.json", PAYOUT_WALLETS[0].0)))
        .ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);

    // The newest batch may still be running if its last payout falls within
    // one batch gap (30 min) of when payouts were last listed.
    let checked_at = state["head_checked_at"].as_i64();
    let in_progress = match (latest, checked_at) {
        (Some(b), Some(t)) => b.end > t - 1800,
        (Some(_), None) => true,
        _ => false,
    };

    let summary = json!({
        "generated_at": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs(),
        "payout_wallets": PAYOUT_WALLETS.iter().map(|(w, why)| json!({"wallet": w, "evidence": why})).collect::<Vec<_>>(),
        "coverage": {
            "from": pays.iter().filter_map(|r| r.block_time).min(),
            "to": pays.iter().filter_map(|r| r.block_time).max(),
            "history_complete": state["backfill_done"].as_bool().unwrap_or(false),
            "checked_at": checked_at,
            "payouts": pays.len(),
            "wallets": totals.len(),
        },
        "batches": all_batches.iter().map(batch_json).collect::<Vec<_>>(),
        "latest": latest.map(|b| json!({
            "batch": batch_json(b),
            "in_progress": in_progress,
            "concentration": concentration(&latest_totals),
            "bands": bands(&latest_rows.iter().map(|r| r.amount).collect::<Vec<_>>()),
            "top": top(&latest_totals, &latest_counts, 50),
        })),
        "all_time": {
            "concentration": concentration(&totals),
            "bands": bands(&pays.iter().map(|r| r.amount).collect::<Vec<_>>()),
            "top": top(&totals, &all_counts, 100),
        },
    });
    std::fs::write(data.join("summary.json"), serde_json::to_vec(&summary)?)?;

    // Per-wallet shards.
    let mut shards: BTreeMap<String, serde_json::Map<String, Value>> = BTreeMap::new();
    let mut by_wallet: BTreeMap<&str, Vec<&PayoutRow>> = BTreeMap::new();
    for r in &pays { by_wallet.entry(r.counterparty.as_str()).or_default().push(r); }
    for (w, list) in by_wallet {
        let (rk, pct) = rank.get(w).copied().unwrap_or((0, 0.0));
        let mut list = list;
        list.sort_by_key(|r| (r.block_time, r.slot));
        let entry = json!({
            "total": totals.get(w).copied().unwrap_or(0),
            "payouts": list.len(),
            "rank": rk,
            "of": totals.len(),
            "percentile": pct,
            "history": list.iter().map(|r| json!([r.block_time, r.amount, r.signature])).collect::<Vec<_>>(),
        });
        shards.entry(shard_of(w)).or_default().insert(w.to_string(), entry);
    }
    let n_shards = shards.len();
    for (s, m) in shards {
        std::fs::write(wdir.join(format!("{s}.json")), serde_json::to_vec(&Value::Object(m))?)?;
    }
    Ok(json!({ "payouts": pays.len(), "wallets": totals.len(), "batches": all_batches.len(), "shards": n_shards }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shard_names_are_case_safe() {
        assert_eq!(shard_of("Ab12"), "4162");
        assert_ne!(shard_of("Ab12"), shard_of("AB12"));
    }

    #[test]
    fn ranks_and_percentiles() {
        let mut t = BTreeMap::new();
        t.insert("a".to_string(), 50u64);
        t.insert("b".to_string(), 10);
        t.insert("c".to_string(), 10);
        t.insert("d".to_string(), 1);
        let r = ranks(&t);
        assert_eq!(r["a"], (1, 1.0));
        assert_eq!(r["b"].0, 2);
        assert_eq!(r["c"].0, 2, "ties share a rank");
        assert_eq!(r["d"], (4, 0.25));
    }

    #[test]
    fn bands_split_on_whole_usdc() {
        let b = bands(&[500_000, 1_000_000, 25_000_000, 12_000_000_000]);
        let counts: Vec<u64> = b.iter().map(|x| x["payouts"].as_u64().unwrap()).collect();
        assert_eq!(counts, vec![1, 1, 1, 0, 0, 1]);
    }
}
