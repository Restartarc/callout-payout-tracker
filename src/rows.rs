//! Compact, table-shaped rows for storage (ClickHouse JSONEachRow), and the
//! reverse conversion back into `DecodedTx` for the ledger.
//!
//! What is kept, and why:
//! - every transaction, failed ones included: followers pay fees on failures;
//! - trades without the full event: fees, fee rates and pool reserves are
//!   kept (needed for liquidation value and replay), the rest is on Solscan;
//! - balance flows only for wallets that signed or traded (not pool accounts);
//! - transfers the decoder tags "other" (tips, router fees, sponsor paybacks,
//!   refunds), not swap legs or self-wraps, which trades and flows already hold;
//! - the signature once, in `txs`. Other tables key on (slot, tx_index), which
//!   is unique; they carry the signature only when the index is unknown (RPC).
//!
//! `from_rows(to_rows(tx))` gives the ledger the same answer as `tx`
//! (tests/rows.rs).

use crate::decode::{AppFeatures, DecodedTx, Trade, Transfer, WalletFlow};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TxRow {
    pub slot: u64,
    pub tx_index: Option<u32>,
    pub block_time: Option<i64>,
    pub signature: String,
    pub failed: bool,
    pub fee_lamports: u64,
    pub fee_payer: String,
    pub signers: Vec<String>,
    pub top_level_programs: Vec<String>,
    pub has_jitodontfront: bool,
    pub compute_budget_ixs: u32,
    pub memos: Vec<String>,
    pub idl_commit: String,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TradeRow {
    pub slot: u64,
    pub tx_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    pub block_time: Option<i64>,
    pub ix_path: String,
    pub program: String,
    pub ix_name: String,
    pub event_name: String,
    pub user: String,
    pub mint: String,
    pub quote_mint: String,
    pub is_buy: bool,
    pub routing_hop: bool,
    pub base_amount: u64,
    pub pool_quote_amount: u64,
    pub user_quote_amount: Option<u64>,
    pub fees: BTreeMap<String, u64>,
    /// Every `*_basis_points` / `*_bps` field of the event.
    pub fee_bps: BTreeMap<String, u64>,
    /// Pool or curve reserves after the trade, as strings (some are u128).
    pub reserves: BTreeMap<String, String>,
    /// Bytes past the last IDL field; > 0 means the program is ahead of the IDL.
    pub trailing_bytes: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlowRow {
    pub slot: u64,
    pub tx_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    pub block_time: Option<i64>,
    pub wallet: String,
    pub sol_lamports: i128,
    pub tokens: BTreeMap<String, i128>,
    pub token_account_rent_lamports: i128,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransferRow {
    pub slot: u64,
    pub tx_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    pub ix_path: String,
    pub mint: String,
    pub from_owner: String,
    pub to_owner: String,
    pub to_account: String,
    pub amount: u64,
    pub kind: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Rows {
    pub txs: Vec<TxRow>,
    pub trades: Vec<TradeRow>,
    pub flows: Vec<FlowRow>,
    pub transfers: Vec<TransferRow>,
}

impl Rows {
    pub fn extend(&mut self, other: Rows) {
        self.txs.extend(other.txs);
        self.trades.extend(other.trades);
        self.flows.extend(other.flows);
        self.transfers.extend(other.transfers);
    }
}

fn event_maps(ev: &Value) -> (BTreeMap<String, u64>, BTreeMap<String, String>, u32) {
    let mut bps = BTreeMap::new();
    let mut reserves = BTreeMap::new();
    if let Some(o) = ev.as_object() {
        for (k, v) in o {
            if k.ends_with("_basis_points") || k.ends_with("_bps") {
                if let Some(n) = v.as_u64() {
                    bps.insert(k.clone(), n);
                }
            } else if k.contains("reserves") && !k.starts_with("user_") {
                let s = v.as_str().map(String::from).or_else(|| v.as_u64().map(|n| n.to_string()));
                if let Some(s) = s {
                    reserves.insert(k.clone(), s);
                }
            }
        }
    }
    let trailing = ev.get("_trailing_bytes").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    (bps, reserves, trailing)
}

/// How child rows point at their transaction.
fn child_sig(d: &DecodedTx) -> Option<String> {
    d.tx_index.is_none().then(|| d.signature.clone())
}

pub fn to_rows(d: &DecodedTx) -> Rows {
    let keep: BTreeSet<&str> = d.signers.iter().map(String::as_str)
        .chain(std::iter::once(d.fee_payer.as_str()))
        .chain(d.trades.iter().map(|t| t.user.as_str()))
        .collect();
    let tx = TxRow {
        slot: d.slot,
        tx_index: d.tx_index,
        block_time: d.block_time,
        signature: d.signature.clone(),
        failed: d.failed,
        fee_lamports: d.fee_lamports,
        fee_payer: d.fee_payer.clone(),
        signers: d.signers.clone(),
        top_level_programs: d.features.top_level_programs.clone(),
        has_jitodontfront: d.features.has_jitodontfront,
        compute_budget_ixs: d.features.compute_budget_ixs as u32,
        memos: d.memos.clone(),
        idl_commit: d.idl_commit.clone(),
        // Trailing-bytes notes are per trade (TradeRow.trailing_bytes).
        warnings: d.warnings.iter().filter(|w| !w.contains("past the last IDL field")).cloned().collect(),
    };
    let trades = d.trades.iter().map(|t| {
        let (fee_bps, reserves, trailing_bytes) = event_maps(&t.event);
        TradeRow {
            slot: d.slot, tx_index: d.tx_index, block_time: d.block_time, signature: child_sig(d),
            ix_path: t.ix_path.clone(), program: t.program.clone(), ix_name: t.ix_name.clone(),
            event_name: t.event_name.clone(), user: t.user.clone(), mint: t.mint.clone(),
            quote_mint: t.quote_mint.clone(), is_buy: t.is_buy, routing_hop: t.routing_hop,
            base_amount: t.base_amount, pool_quote_amount: t.pool_quote_amount,
            user_quote_amount: t.user_quote_amount, fees: t.fees.clone(), fee_bps, reserves, trailing_bytes,
        }
    }).collect();
    let flows = d.wallet_flows.iter().filter(|(w, _)| keep.contains(w.as_str())).map(|(w, f)| FlowRow {
        slot: d.slot, tx_index: d.tx_index, block_time: d.block_time, signature: child_sig(d),
        wallet: w.clone(), sol_lamports: f.sol_lamports, tokens: f.tokens.clone(),
        token_account_rent_lamports: f.token_account_rent_lamports,
    }).collect();
    let transfers = d.transfers.iter()
        .filter(|t| t.kind == "other" && (keep.contains(t.from_owner.as_str()) || keep.contains(t.to_owner.as_str())))
        .map(|t| TransferRow {
            slot: d.slot, tx_index: d.tx_index, signature: child_sig(d), ix_path: t.ix_path.clone(),
            mint: t.mint.clone(), from_owner: t.from_owner.clone(), to_owner: t.to_owner.clone(),
            to_account: t.to_account.clone(), amount: t.amount, kind: t.kind.clone(),
        }).collect();
    Rows { txs: vec![tx], trades, flows, transfers }
}

/// Rebuilds transactions from rows. Trades get an `event` holding only the
/// kept fee rates and reserves; flows exist only for the kept wallets.
pub fn from_rows(rows: Rows) -> Vec<DecodedTx> {
    // Children find their transaction by (slot, tx_index), or by signature
    // when the index is unknown.
    let key = |slot: u64, idx: Option<u32>, sig: Option<&str>| -> String {
        match idx { Some(i) => format!("{slot}:{i}"), None => sig.unwrap_or("").to_string() }
    };
    let mut by_key: BTreeMap<String, DecodedTx> = BTreeMap::new();
    for t in rows.txs {
        let k = key(t.slot, t.tx_index, Some(&t.signature));
        by_key.insert(k, DecodedTx {
            signature: t.signature,
            slot: t.slot,
            tx_index: t.tx_index,
            block_time: t.block_time,
            failed: t.failed,
            fee_lamports: t.fee_lamports,
            fee_payer: t.fee_payer,
            features: AppFeatures {
                top_level_programs: t.top_level_programs,
                has_jitodontfront: t.has_jitodontfront,
                compute_budget_ixs: t.compute_budget_ixs as usize,
                memo_lengths: t.memos.iter().map(String::len).collect(),
                signer_count: t.signers.len(),
            },
            signers: t.signers,
            idl_commit: t.idl_commit,
            trades: vec![],
            wallet_flows: BTreeMap::new(),
            transfers: vec![],
            memos: t.memos,
            warnings: t.warnings,
        });
    }
    for r in rows.trades {
        let Some(d) = by_key.get_mut(&key(r.slot, r.tx_index, r.signature.as_deref())) else { continue };
        let mut ev = Map::new();
        for (k, v) in &r.fee_bps { ev.insert(k.clone(), Value::from(*v)); }
        for (k, v) in &r.reserves { ev.insert(k.clone(), Value::from(v.clone())); }
        d.trades.push(Trade {
            ix_path: r.ix_path, program: r.program, ix_name: r.ix_name, event_name: r.event_name,
            user: r.user, mint: r.mint, quote_mint: r.quote_mint, is_buy: r.is_buy, routing_hop: r.routing_hop,
            base_amount: r.base_amount, pool_quote_amount: r.pool_quote_amount,
            user_quote_amount: r.user_quote_amount, fees: r.fees, event: Value::Object(ev),
        });
    }
    for r in rows.flows {
        let Some(d) = by_key.get_mut(&key(r.slot, r.tx_index, r.signature.as_deref())) else { continue };
        d.wallet_flows.insert(r.wallet, WalletFlow {
            sol_lamports: r.sol_lamports, tokens: r.tokens, token_account_rent_lamports: r.token_account_rent_lamports,
        });
    }
    for r in rows.transfers {
        let Some(d) = by_key.get_mut(&key(r.slot, r.tx_index, r.signature.as_deref())) else { continue };
        d.transfers.push(Transfer {
            ix_path: r.ix_path, mint: r.mint, from_owner: r.from_owner, to_owner: r.to_owner,
            to_account: r.to_account, amount: r.amount, kind: r.kind,
        });
    }
    by_key.into_values().collect()
}
