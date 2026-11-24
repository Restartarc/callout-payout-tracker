//! pump.fun callout reward payouts, read from the payout wallet's transfers,
//! and the statistics the payout ledger reports.
//!
//! The payout wallet was identified by matching a caller's in-app callout
//! reward to its on-chain transfer, to the cent. One match is the evidence
//! so far; matching more rewards would strengthen it.

use crate::decode::DecodedTx;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Wallets known to pay callout rewards, with the evidence for each.
pub const PAYOUT_WALLETS: &[(&str, &str)] = &[(
    "BNrYNyQZhdGRs8Rvi98WGTjhxab2wFTdGtVjW72RGyh4",
    "a caller's in-app callout reward matched its on-chain transfer from this wallet, to the cent",
)];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PayoutRow {
    pub slot: u64,
    pub tx_index: Option<u32>,
    pub block_time: Option<i64>,
    pub signature: String,
    pub ix_path: String,
    /// The payout wallet involved.
    pub payout_wallet: String,
    /// "payout": the payout wallet sent it. "funding": the payout wallet received it.
    pub kind: String,
    /// The other side: the recipient of a payout, or the funder.
    pub counterparty: String,
    pub counterparty_account: String,
    pub mint: String,
    pub amount: u64,
}

/// Transfers in a successful transaction that move value out of, or into,
/// one of `payout_wallets`.
pub fn payout_rows(d: &DecodedTx, payout_wallets: &[&str]) -> Vec<PayoutRow> {
    if d.failed {
        return vec![];
    }
    let mut out = vec![];
    for t in &d.transfers {
        let (kind, wallet, other, other_acct) = if payout_wallets.contains(&t.from_owner.as_str()) && t.to_owner != t.from_owner {
            ("payout", &t.from_owner, &t.to_owner, &t.to_account)
        } else if payout_wallets.contains(&t.to_owner.as_str()) && t.to_owner != t.from_owner {
            ("funding", &t.to_owner, &t.from_owner, &t.from_owner)
        } else {
            continue;
        };
        out.push(PayoutRow {
            slot: d.slot,
            tx_index: d.tx_index,
            block_time: d.block_time,
            signature: d.signature.clone(),
            ix_path: t.ix_path.clone(),
            payout_wallet: wallet.clone(),
            kind: kind.into(),
            counterparty: other.clone(),
            counterparty_account: other_acct.clone(),
            mint: t.mint.clone(),
            amount: t.amount,
        });
    }
    out
}

/// Drops repeats of the same transfer (a resumed ingest can write a page twice).
pub fn dedupe(rows: &mut Vec<PayoutRow>) {
    rows.sort_by(|a, b| (a.slot, &a.signature, &a.ix_path).cmp(&(b.slot, &b.signature, &b.ix_path)));
    rows.dedup_by(|a, b| a.signature == b.signature && a.ix_path == b.ix_path);
}

#[derive(Debug, Clone, Serialize)]
pub struct Batch {
    pub start: i64,
    pub end: i64,
    pub payouts: usize,
    pub recipients: usize,
    pub total: u64,
    pub median: u64,
    pub p90: u64,
    pub max: u64,
}

/// Groups payouts into batches: consecutive payouts with gaps under `gap_secs`.
pub fn batches(rows: &[PayoutRow], mint: &str, gap_secs: i64) -> Vec<Batch> {
    let mut p: Vec<(i64, u64, &str)> = rows.iter()
        .filter(|r| r.kind == "payout" && r.mint == mint)
        .filter_map(|r| r.block_time.map(|t| (t, r.amount, r.counterparty.as_str())))
        .collect();
    p.sort();
    let mut out = vec![];
    let mut i = 0;
    while i < p.len() {
        let mut j = i + 1;
        while j < p.len() && p[j].0 - p[j - 1].0 <= gap_secs {
            j += 1;
        }
        let mut amts: Vec<u64> = p[i..j].iter().map(|x| x.1).collect();
        amts.sort();
        let recips: std::collections::BTreeSet<&str> = p[i..j].iter().map(|x| x.2).collect();
        out.push(Batch {
            start: p[i].0,
            end: p[j - 1].0,
            payouts: j - i,
            recipients: recips.len(),
            total: amts.iter().sum(),
            median: amts[amts.len() / 2],
            p90: amts[(amts.len() * 9 / 10).min(amts.len() - 1)],
            max: *amts.last().unwrap(),
        });
        i = j;
    }
    out
}

/// Total received per recipient.
pub fn totals_by_recipient(rows: &[PayoutRow], mint: &str) -> BTreeMap<String, u64> {
    let mut m = BTreeMap::new();
    for r in rows.iter().filter(|r| r.kind == "payout" && r.mint == mint) {
        *m.entry(r.counterparty.clone()).or_default() += r.amount;
    }
    m
}

#[derive(Debug, Clone, Serialize)]
pub struct Concentration {
    pub recipients: usize,
    pub total: u64,
    pub top_1pct_share: f64,
    pub top_10pct_share: f64,
    pub top_10_share: f64,
    /// 0 = everyone got the same, 1 = one wallet got everything.
    pub gini: f64,
}

pub fn concentration(totals: &BTreeMap<String, u64>) -> Option<Concentration> {
    let mut v: Vec<u64> = totals.values().copied().collect();
    if v.is_empty() {
        return None;
    }
    v.sort_unstable_by(|a, b| b.cmp(a));
    let total: u64 = v.iter().sum();
    let n = v.len();
    let share = |k: usize| v.iter().take(k.max(1)).sum::<u64>() as f64 / total as f64;
    // Gini from the ascending order: sum((2i - n - 1) * x_i) / (n * sum x).
    let asc: Vec<u64> = v.iter().rev().copied().collect();
    let g: f64 = asc.iter().enumerate().map(|(i, x)| (2.0 * (i as f64 + 1.0) - n as f64 - 1.0) * *x as f64).sum::<f64>()
        / (n as f64 * total as f64);
    Some(Concentration {
        recipients: n,
        total,
        top_1pct_share: share(n / 100),
        top_10pct_share: share(n / 10),
        top_10_share: share(10),
        gini: g,
    })
}

/// Share of recipients whose total is at or below `amount`: the percentile
/// a wallet sits at.
pub fn percentile(totals: &BTreeMap<String, u64>, amount: u64) -> f64 {
    if totals.is_empty() {
        return 0.0;
    }
    totals.values().filter(|v| **v <= amount).count() as f64 / totals.len() as f64
}
