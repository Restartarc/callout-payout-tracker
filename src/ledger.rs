//! In-order, all-in PnL for one wallet in one coin.
//!
//! Every number comes from the wallet's own balance changes in each
//! transaction, so router fees, tips, gas paid back to a sponsor and rent all
//! count without being modeled. Cost basis is average cost, kept per currency
//! (SOL, USDC...) with no price conversion, since converting needs a price
//! source and the design keeps that separate (Pyth, later).

use crate::decode::{DecodedTx, SOL, WSOL};
use serde::Serialize;
use std::collections::BTreeMap;

pub const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

/// Maps a balance-change key to the currency it is counted in, or None if it
/// is not cash. Native SOL and wSOL are the same currency.
fn cash(mint: &str) -> Option<&'static str> {
    match mint {
        SOL | WSOL => Some("SOL"),
        USDC => Some("USDC"),
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Fill {
    pub signature: String,
    pub slot: u64,
    pub block_time: Option<i64>,
    pub qty_delta: i128,
    /// All-in cash change for the wallet in this tx, per currency (raw units).
    pub cash: BTreeMap<String, i128>,
    pub rent_locked_delta: i128,
    /// Part of the cash change that went back to a separate fee payer (gas sponsor).
    pub sponsor_reimbursement: BTreeMap<String, u64>,
    /// Network fee, if this wallet was the fee payer.
    pub network_fee_lamports: u64,
    /// Fees named in the pump/PumpSwap events, in the event's quote mint.
    pub swap_fees: BTreeMap<String, u64>,
    pub realized: BTreeMap<String, i128>,
    pub memo_lengths: Vec<usize>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Position {
    pub wallet: String,
    pub mint: String,
    pub qty: i128,
    /// Remaining cost basis per currency; can be negative when a buy also
    /// returned cash (e.g. a rent refund).
    pub cost: BTreeMap<String, i128>,
    pub realized: BTreeMap<String, i128>,
    /// Lamports sitting as rent in this wallet's token accounts, recoverable on close.
    pub rent_locked: i128,
    pub fills: Vec<Fill>,
    pub warnings: Vec<String>,
}

pub fn build_position(wallet: &str, mint: &str, txs: &[DecodedTx]) -> Position {
    let mut txs: Vec<&DecodedTx> = txs.iter().filter(|t| !t.failed).collect();
    // Order by slot, then position in the block. RPC results have no position;
    // for those, same-slot fills fall back to signature order and are flagged.
    txs.sort_by(|a, b| (a.slot, a.tx_index, &a.signature).cmp(&(b.slot, b.tx_index, &b.signature)));
    let mut p = Position { wallet: wallet.into(), mint: mint.into(), ..Default::default() };
    let mut last_slot = None;
    for t in txs {
        let Some(flow) = t.wallet_flows.get(wallet) else { continue };
        let qty_delta = flow.tokens.get(mint).copied().unwrap_or(0);
        if qty_delta == 0 {
            continue;
        }
        if last_slot == Some(t.slot) && t.tx_index.is_none() {
            p.warnings.push(format!("{}: same slot as previous fill and no in-block index; order is a guess", t.signature));
        }
        last_slot = Some(t.slot);
        let others: Vec<&String> = flow.tokens.keys().filter(|m| *m != mint && cash(m).is_none()).collect();
        if !others.is_empty() {
            p.warnings.push(format!("{}: wallet also moved other coins {:?}; all cash in this tx is charged to {}", t.signature, others, mint));
        }

        let mut cash_d: BTreeMap<String, i128> = BTreeMap::new();
        if flow.sol_lamports != 0 {
            *cash_d.entry("SOL".into()).or_default() += flow.sol_lamports;
        }
        for (m, d) in &flow.tokens {
            if let Some(c) = cash(m) {
                *cash_d.entry(c.into()).or_default() += d;
            }
        }
        let mut sponsor: BTreeMap<String, u64> = BTreeMap::new();
        if t.fee_payer != wallet {
            for tr in t.transfers.iter().filter(|x| x.from_owner == wallet && x.to_owner == t.fee_payer) {
                let c = cash(&tr.mint).unwrap_or(&tr.mint).to_string();
                *sponsor.entry(c).or_default() += tr.amount;
            }
        }
        let mut swap_fees: BTreeMap<String, u64> = BTreeMap::new();
        for tr in t.trades.iter().filter(|x| x.mint == mint) {
            for (k, v) in &tr.fees {
                *swap_fees.entry(format!("{k} ({})", cash(&tr.quote_mint).unwrap_or(&tr.quote_mint))).or_default() += v;
            }
        }

        let mut realized_now: BTreeMap<String, i128> = BTreeMap::new();
        if qty_delta > 0 {
            for (c, d) in &cash_d {
                *p.cost.entry(c.clone()).or_default() -= d;
            }
            p.qty += qty_delta;
        } else {
            let sold = -qty_delta;
            if sold > p.qty {
                p.warnings.push(format!("{}: sold {} but only {} known; tokens came from outside the trades seen", t.signature, sold, p.qty));
            }
            let held = p.qty.max(sold);
            let currencies: Vec<String> = p.cost.keys().chain(cash_d.keys()).cloned().collect();
            for c in currencies {
                if realized_now.contains_key(&c) {
                    continue;
                }
                let basis = p.cost.get(&c).copied().unwrap_or(0);
                // Integer division rounds toward zero; on a full exit the
                // fraction is exactly 1 so nothing is lost.
                let removed = if sold == held { basis } else { basis * sold / held };
                let proceeds = cash_d.get(&c).copied().unwrap_or(0);
                realized_now.insert(c.clone(), proceeds - removed);
                *p.cost.entry(c.clone()).or_default() -= removed;
                *p.realized.entry(c).or_default() += proceeds - removed;
            }
            p.qty -= sold;
        }
        p.rent_locked += flow.token_account_rent_lamports;
        p.fills.push(Fill {
            signature: t.signature.clone(),
            slot: t.slot,
            block_time: t.block_time,
            qty_delta,
            cash: cash_d,
            rent_locked_delta: flow.token_account_rent_lamports,
            sponsor_reimbursement: sponsor,
            network_fee_lamports: if t.fee_payer == wallet { t.fee_lamports } else { 0 },
            swap_fees,
            realized: realized_now,
            memo_lengths: t.features.memo_lengths.clone(),
        });
    }
    p.cost.retain(|_, v| *v != 0);
    p
}
