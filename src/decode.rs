//! Turns one RPC `getTransaction` (encoding=jsonParsed) result into trades,
//! per-wallet balance flows, transfers and app-fingerprint features.

use crate::idl::{Registry, EVENT_IX_TAG};
use anyhow::{anyhow, Result};
use base64::Engine;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

pub const WSOL: &str = "So11111111111111111111111111111111111111112";
pub const SOL: &str = "SOL";
const MEMO_PROGRAMS: [&str; 2] = ["MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr", "Memo1UhkJRfHyvLMcVucJwxXeuD728EqVDDwQDxFMNo"];
const COMPUTE_BUDGET: &str = "ComputeBudget111111111111111111111111111111";

#[derive(Debug, Clone, Serialize)]
pub struct Trade {
    /// Position in execution order: "top" or "top.inner".
    pub ix_path: String,
    pub program: String,
    pub ix_name: String,
    pub event_name: String,
    pub user: String,
    pub mint: String,
    pub quote_mint: String,
    pub is_buy: bool,
    /// True for a multi-hop leg where the user never held the base coin: the
    /// pool sent it straight to the next pool. Not a position for the user.
    pub routing_hop: bool,
    pub base_amount: u64,
    /// Quote amount as the event reports it, pool side.
    pub pool_quote_amount: u64,
    /// Quote the user actually paid or received at the swap. Only set when the
    /// IDL names a user-side field (PumpSwap); pump curve events don't have one.
    pub user_quote_amount: Option<u64>,
    pub fees: BTreeMap<String, u64>,
    pub event: Value,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct WalletFlow {
    /// Change in the wallet's own lamports (tx fee, tips, rent refunds...).
    pub sol_lamports: i128,
    /// Raw token amount change per mint, across all token accounts the wallet owns.
    pub tokens: BTreeMap<String, i128>,
    /// Change in lamports held as rent by token accounts the wallet owns.
    /// Recoverable by closing the account, so the ledger tracks it apart from cash.
    pub token_account_rent_lamports: i128,
}

#[derive(Debug, Clone, Serialize)]
pub struct Transfer {
    pub ix_path: String,
    /// "SOL" for system transfers, otherwise the SPL mint.
    pub mint: String,
    pub from_owner: String,
    pub to_owner: String,
    pub to_account: String,
    pub amount: u64,
    /// "swap_leg": to or from an account of a pump/PumpSwap trade instruction
    /// (pool, curve, protocol fee and creator vaults), already in the trade's
    /// amounts and fees. "self": between the same owner's accounts (wrapping
    /// SOL). "other": everything else, i.e. tips, router fees, gas paid back
    /// to a sponsor, refunds, funding.
    pub kind: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AppFeatures {
    pub top_level_programs: Vec<String>,
    pub has_jitodontfront: bool,
    pub compute_budget_ixs: usize,
    pub memo_lengths: Vec<usize>,
    pub signer_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct DecodedTx {
    pub signature: String,
    pub slot: u64,
    /// Position in the block. Present in Old Faithful backfill and the live
    /// stream; absent from RPC getTransaction.
    pub tx_index: Option<u32>,
    pub block_time: Option<i64>,
    pub failed: bool,
    pub fee_lamports: u64,
    pub fee_payer: String,
    pub signers: Vec<String>,
    pub idl_commit: String,
    pub trades: Vec<Trade>,
    pub wallet_flows: BTreeMap<String, WalletFlow>,
    pub transfers: Vec<Transfer>,
    pub memos: Vec<String>,
    pub features: AppFeatures,
    pub warnings: Vec<String>,
}

#[derive(Clone)]
struct DecodedIx {
    label: String,
    name: String,
    accts: Vec<(String, String)>,
    path: String,
}

fn note_layout(warnings: &mut Vec<String>, label: &str, ev_name: &str, path: &str, ev: &Value, idl_commit: &str) {
    if let Some(n) = ev.get("_trailing_bytes") {
        warnings.push(format!("{label} {ev_name} at {path}: {n} bytes past the last IDL field; the deployed program is ahead of IDL {idl_commit}"));
    }
    if let Some(m) = ev.get("_missing") {
        warnings.push(format!("{label} {ev_name} at {path}: fields absent on-chain {m}; older event layout"));
    }
}

/// For transactions without stack heights (old RPC data): pair the n-th
/// trade-named instruction of a program with its n-th trade event, from
/// self-CPI events or, failing that, "Program data:" logs.
fn legacy_pairing(meta: &Value, reg: &Registry, tok: &HashMap<String, String>, transfers: &[Transfer],
                  ixs: &HashMap<String, Vec<DecodedIx>>, cpi_events: &HashMap<String, Vec<(String, Value)>>,
                  trades: &mut Vec<Trade>, warnings: &mut Vec<String>) {
    let mut log_events: HashMap<String, Vec<(String, Value)>> = HashMap::new();
    let mut stack: Vec<String> = vec![];
    for l in meta["logMessages"].as_array().into_iter().flatten().filter_map(|l| l.as_str()) {
        let parts: Vec<&str> = l.split_whitespace().collect();
        if parts.len() >= 3 && parts[0] == "Program" && parts[2] == "invoke" {
            stack.push(parts[1].to_string());
        } else if parts.len() >= 3 && parts[0] == "Program" && (parts[2] == "success" || parts[2] == "failed:") {
            stack.pop();
        } else if let Some(b64) = l.strip_prefix("Program data: ") {
            if let (Some(top), Ok(bytes)) = (stack.last(), base64::engine::general_purpose::STANDARD.decode(b64)) {
                if let Some(prog) = reg.get(top) {
                    if let Some((n, ev)) = prog.decode_event(&bytes) {
                        if matches!(n.as_str(), "TradeEvent" | "BuyEvent" | "SellEvent") {
                            log_events.entry(prog.label.clone()).or_default().push((n, ev));
                        }
                    }
                }
            }
        }
    }
    for (label, list) in ixs {
        let evs = cpi_events.get(label).filter(|e| !e.is_empty()).or_else(|| log_events.get(label)).cloned().unwrap_or_default();
        if evs.len() != list.len() {
            warnings.push(format!("{label}: {} trade instructions but {} trade events (no stack heights); unpaired ones skipped", list.len(), evs.len()));
        }
        for (d, (ev_name, ev)) in list.iter().zip(evs.iter()) {
            note_layout(warnings, label, ev_name, &d.path, ev, &reg.idl_commit);
            trades.push(build_trade(label, &d.path, &d.name, &d.accts, ev_name, ev, tok, transfers));
        }
    }
}

struct Ix<'a> {
    path: String,
    v: &'a Value,
}

fn u(v: &Value) -> u64 {
    v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())).unwrap_or(0)
}

pub fn decode_tx(tx: &Value, reg: &Registry) -> Result<DecodedTx> {
    let msg = &tx["transaction"]["message"];
    let meta = &tx["meta"];
    let signature = tx["transaction"]["signatures"][0].as_str().ok_or_else(|| anyhow!("no signature"))?.to_string();
    let mut warnings = vec![];

    // Full account list. jsonParsed already lists lookup-table keys with a
    // "source" field; older responses need loadedAddresses appended.
    let ak = msg["accountKeys"].as_array().ok_or_else(|| anyhow!("no accountKeys"))?;
    let mut keys: Vec<String> = ak.iter().map(|a| a["pubkey"].as_str().or(a.as_str()).unwrap_or("").to_string()).collect();
    if !ak.iter().any(|a| a.get("source").is_some()) {
        for part in ["writable", "readonly"] {
            for k in meta["loadedAddresses"][part].as_array().into_iter().flatten() {
                keys.push(k.as_str().unwrap_or("").to_string());
            }
        }
    }
    let signers: Vec<String> = ak.iter().filter(|a| a["signer"].as_bool() == Some(true))
        .map(|a| a["pubkey"].as_str().unwrap_or("").to_string()).collect();
    let fee_payer = keys.first().cloned().unwrap_or_default();

    // Instructions in execution order: top-level i, then its inner instructions.
    let mut order: Vec<Ix> = vec![];
    let inner: HashMap<u64, &Vec<Value>> = meta["innerInstructions"].as_array().into_iter().flatten()
        .filter_map(|g| Some((g["index"].as_u64()?, g["instructions"].as_array()?))).collect();
    for (i, ix) in msg["instructions"].as_array().into_iter().flatten().enumerate() {
        order.push(Ix { path: i.to_string(), v: ix });
        if let Some(list) = inner.get(&(i as u64)) {
            for (j, ix) in list.iter().enumerate() {
                order.push(Ix { path: format!("{i}.{j}"), v: ix });
            }
        }
    }

    // Token account -> (owner, mint), from the balance snapshots.
    let mut tok: HashMap<String, (String, String)> = HashMap::new();
    for side in ["preTokenBalances", "postTokenBalances"] {
        for b in meta[side].as_array().into_iter().flatten() {
            let idx = b["accountIndex"].as_u64().unwrap_or(0) as usize;
            if let Some(k) = keys.get(idx) {
                tok.insert(k.clone(), (b["owner"].as_str().unwrap_or("").into(), b["mint"].as_str().unwrap_or("").into()));
            }
        }
    }

    // Token accounts created and closed inside the transaction (multi-hop
    // intermediates) are missing from the balance snapshots; learn their mints
    // from the instructions that name them.
    let mut acct_mint: HashMap<String, String> = tok.iter().map(|(k, (_, m))| (k.clone(), m.clone())).collect();
    // Same for owners: initializeAccount names the owner, ATA creation the wallet.
    let mut acct_owner: HashMap<String, String> = tok.iter().map(|(k, (o, _))| (k.clone(), o.clone())).collect();
    for Ix { v, .. } in &order {
        let info = &v["parsed"]["info"];
        if let (Some(a), Some(o)) = (info["account"].as_str(), info["owner"].as_str().or(info["wallet"].as_str())) {
            acct_owner.entry(a.to_string()).or_insert_with(|| o.to_string());
        }
        let mint = info["mint"].as_str().unwrap_or("");
        if mint.is_empty() {
            continue;
        }
        for field in ["account", "source", "destination"] {
            if let Some(a) = info[field].as_str() {
                acct_mint.entry(a.to_string()).or_insert_with(|| mint.to_string());
            }
        }
    }

    // --- Decode instructions and events.
    // Each self-CPI event is paired with the instruction that emitted it: the
    // latest instruction one level up the call stack (stackHeight - 1), which
    // must belong to the same program. One instruction can emit several events
    // (multi-hop swaps, buyback-and-burn), so this is not a 1:1 pairing.
    // Failed transactions emit no events and yield no trades.
    let failed = !meta["err"].is_null();
    let has_heights = order.iter().all(|ix| !ix.path.contains('.') || ix.v["stackHeight"].as_u64().is_some());
    let mut last_at: BTreeMap<u64, Option<DecodedIx>> = BTreeMap::new();
    let mut trade_named: Vec<(String, String, String)> = vec![]; // (path, label, name)
    let mut trade_ix_accounts: std::collections::HashSet<String> = Default::default();
    let mut emitted_from: std::collections::HashSet<String> = Default::default();
    let mut trades = vec![];
    let mut pending_trades: Vec<(DecodedIx, String, Value)> = vec![];
    // Legacy pairing inputs, used only for transactions without stack heights.
    let mut legacy_ixs: HashMap<String, Vec<DecodedIx>> = HashMap::new();
    let mut cpi_events: HashMap<String, Vec<(String, Value)>> = HashMap::new();
    let mut memos = vec![];
    let mut transfers = vec![];
    let mut features = AppFeatures { signer_count: signers.len(), ..Default::default() };
    features.has_jitodontfront = keys.iter().any(|k| k.starts_with("jitodontfront"));
    for ix in msg["instructions"].as_array().into_iter().flatten() {
        let p = ix["programId"].as_str().unwrap_or("").to_string();
        if p == COMPUTE_BUDGET { features.compute_budget_ixs += 1; }
        features.top_level_programs.push(p);
    }

    for Ix { path, v } in &order {
        let height = if path.contains('.') { v["stackHeight"].as_u64().unwrap_or(0) } else { 1 };
        // Anything at this height replaces what was there and closes deeper frames.
        last_at.retain(|h, _| *h < height);
        last_at.insert(height, None);
        let pid = v["programId"].as_str().unwrap_or("");
        if MEMO_PROGRAMS.contains(&pid) {
            let m = v["parsed"].as_str().map(String::from).unwrap_or_default();
            features.memo_lengths.push(m.len());
            memos.push(m);
            continue;
        }
        if let Some(p) = v["parsed"].as_object() {
            collect_transfer(path, v, p, &acct_owner, &acct_mint, &mut transfers);
            continue;
        }
        let Some(prog) = reg.get(pid) else { continue };
        let Some(data) = v["data"].as_str().and_then(|d| bs58::decode(d).into_vec().ok()) else { continue };
        if data.len() >= 16 && data[..8] == EVENT_IX_TAG {
            let Some((ev_name, ev)) = prog.decode_event(&data[8..]) else { continue };
            if !matches!(ev_name.as_str(), "TradeEvent" | "BuyEvent" | "SellEvent") {
                continue;
            }
            if !has_heights {
                cpi_events.entry(prog.label.clone()).or_default().push((ev_name, ev));
                continue;
            }
            let parent = height.checked_sub(1).and_then(|h| last_at.get(&h)).and_then(|p| p.as_ref());
            match parent {
                Some(p) if p.label == prog.label => {
                    note_layout(&mut warnings, &prog.label, &ev_name, &p.path, &ev, &reg.idl_commit);
                    // Credit every instruction on the call stack: an outer
                    // multi_hop_swap that only routes through bonding curves
                    // emits nothing itself, but its nested call does.
                    for d in last_at.values().flatten() {
                        emitted_from.insert(d.path.clone());
                    }
                    pending_trades.push((p.clone(), ev_name, ev));
                }
                _ => warnings.push(format!("{}: {ev_name} at {path} has no emitting {} instruction above it; skipped", prog.label, prog.label)),
            }
            continue;
        }
        if let Some((def, _args)) = prog.decode_ix(&data) {
            let accts: Vec<(String, String)> = def.accounts.iter().cloned()
                .zip(v["accounts"].as_array().into_iter().flatten().map(|a| a.as_str().unwrap_or("").to_string()))
                .collect();
            let d = DecodedIx { label: prog.label.clone(), name: def.name.clone(), accts, path: path.clone() };
            let n = def.name.as_str();
            if n.starts_with("buy") || n.starts_with("sell") || n.starts_with("multi_hop") {
                trade_named.push((path.clone(), prog.label.clone(), def.name.clone()));
                trade_ix_accounts.extend(d.accts.iter().map(|(_, a)| a.clone()));
                legacy_ixs.entry(prog.label.clone()).or_default().push(d.clone());
            }
            last_at.insert(height, Some(d));
        }
    }

    for (p, ev_name, ev) in &pending_trades {
        trades.push(build_trade(&p.label, &p.path, &p.name, &p.accts, ev_name, ev, &acct_mint, &transfers));
    }
    if !has_heights && !failed {
        legacy_pairing(meta, reg, &acct_mint, &transfers, &legacy_ixs, &cpi_events, &mut trades, &mut warnings);
        trades.sort_by(|a, b| path_key(&a.ix_path).cmp(&path_key(&b.ix_path)));
    } else if !failed {
        for (path, label, name) in &trade_named {
            if !emitted_from.contains(path) {
                warnings.push(format!("{label}: {name} at {path} emitted no trade event in a successful transaction"));
            }
        }
    }

    // --- Per-wallet balance flows.
    let mut flows: BTreeMap<String, WalletFlow> = BTreeMap::new();
    let pre = meta["preBalances"].as_array().cloned().unwrap_or_default();
    let post = meta["postBalances"].as_array().cloned().unwrap_or_default();
    let mut lamport_delta: HashMap<String, i128> = HashMap::new();
    for (i, k) in keys.iter().enumerate() {
        let d = u(&post.get(i).cloned().unwrap_or(Value::Null)) as i128 - u(&pre.get(i).cloned().unwrap_or(Value::Null)) as i128;
        if d != 0 { lamport_delta.insert(k.clone(), d); }
    }
    for (k, d) in &lamport_delta {
        match tok.get(k) {
            // Token account: lamport change is rent, plus wrapped SOL if it holds wSOL.
            Some((owner, mint)) => {
                let f = flows.entry(owner.clone()).or_default();
                if mint == WSOL {
                    // wSOL accounts hold rent + wrapped balance; the wrapped part
                    // is counted under tokens[WSOL], so keep only the rent part here.
                    let wrapped = token_delta(meta, &keys, k);
                    f.token_account_rent_lamports += d - wrapped;
                } else {
                    f.token_account_rent_lamports += d;
                }
            }
            None => { flows.entry(k.clone()).or_default().sol_lamports += d; }
        }
    }
    for k in tok.keys() {
        let d = token_delta(meta, &keys, k);
        if d != 0 {
            let (owner, mint) = &tok[k];
            *flows.entry(owner.clone()).or_default().tokens.entry(mint.clone()).or_default() += d;
        }
    }
    // A token account created and closed in the same tx shows in neither
    // snapshot; its rent shows as lamport movements between system accounts,
    // which are already in sol_lamports above.

    // Tag transfers. Protocol accounts are those of trade instructions, minus
    // the traders, signers and anything they own.
    let people: std::collections::HashSet<&str> = trades.iter().map(|t| t.user.as_str())
        .chain(signers.iter().map(String::as_str)).collect();
    let protocol: std::collections::HashSet<&str> = trade_ix_accounts.iter().map(String::as_str)
        .filter(|a| !people.contains(a) && !acct_owner.get(*a).map(|o| people.contains(o.as_str())).unwrap_or(false))
        .collect();
    for x in transfers.iter_mut() {
        x.kind = if x.from_owner == x.to_owner {
            "self"
        } else if protocol.contains(x.to_owner.as_str()) || protocol.contains(x.to_account.as_str()) || protocol.contains(x.from_owner.as_str()) {
            "swap_leg"
        } else {
            "other"
        }.to_string();
    }

    Ok(DecodedTx {
        signature,
        slot: tx["slot"].as_u64().unwrap_or(0),
        tx_index: tx["transactionIndex"].as_u64().map(|i| i as u32),
        block_time: tx["blockTime"].as_i64(),
        failed: !meta["err"].is_null(),
        fee_lamports: u(&meta["fee"]),
        fee_payer,
        signers,
        idl_commit: reg.idl_commit.clone(),
        trades,
        wallet_flows: flows,
        transfers,
        memos,
        features,
        warnings,
    })
}

fn path_key(p: &str) -> (u64, i64) {
    let mut it = p.split('.');
    let a = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    let b = it.next().and_then(|x| x.parse().ok()).unwrap_or(-1);
    (a, b)
}

/// Raw token amount change for one token account, from the balance snapshots.
fn token_delta(meta: &Value, keys: &[String], account: &str) -> i128 {
    let amt = |side: &str| -> i128 {
        meta[side].as_array().into_iter().flatten()
            .find(|b| keys.get(b["accountIndex"].as_u64().unwrap_or(u64::MAX) as usize).map(|k| k == account).unwrap_or(false))
            .map(|b| u(&b["uiTokenAmount"]["amount"]) as i128).unwrap_or(0)
    };
    amt("postTokenBalances") - amt("preTokenBalances")
}

fn collect_transfer(path: &str, v: &Value, p: &serde_json::Map<String, Value>, owners: &HashMap<String, String>,
                    mints: &HashMap<String, String>, out: &mut Vec<Transfer>) {
    let ty = p.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let info = &p["info"];
    let prog = v["program"].as_str().unwrap_or("");
    let owner_of = |acct: &str| owners.get(acct).cloned().unwrap_or_else(|| acct.to_string());
    match (prog, ty) {
        ("system", "transfer") => out.push(Transfer {
            ix_path: path.into(), mint: SOL.into(),
            from_owner: info["source"].as_str().unwrap_or("").into(),
            to_owner: info["destination"].as_str().unwrap_or("").into(),
            to_account: info["destination"].as_str().unwrap_or("").into(),
            amount: u(&info["lamports"]),
            kind: String::new(),
        }),
        ("spl-token" | "spl-token-2022", "transfer" | "transferChecked") => {
            let src = info["source"].as_str().unwrap_or("");
            let dst = info["destination"].as_str().unwrap_or("");
            let mint = info["mint"].as_str().map(String::from)
                .or_else(|| mints.get(src).cloned()).unwrap_or_default();
            let amount = if info["tokenAmount"].is_object() { u(&info["tokenAmount"]["amount"]) } else { u(&info["amount"]) };
            out.push(Transfer { ix_path: path.into(), mint, from_owner: owner_of(src), to_owner: owner_of(dst), to_account: dst.into(), amount, kind: String::new() });
        }
        _ => {}
    }
}

fn build_trade(label: &str, path: &str, ix_name: &str, accts: &[(String, String)], ev_name: &str, ev: &Value,
               acct_mint: &HashMap<String, String>, transfers: &[Transfer]) -> Trade {
    // PumpSwap events name the user's token accounts; their mints are the
    // only reliable source when one instruction swaps through several pools.
    let mint_of = |field: &str| ev[field].as_str().and_then(|a| acct_mint.get(a)).cloned().filter(|m| !m.is_empty());
    // Multi-hop legs route pool to pool and write the all-zero key as the
    // user's account. The leg's coin is then the transfer leaving (or
    // entering) this pool for exactly the event's amount.
    let pool = ev["pool"].as_str().unwrap_or("");
    let pool_transfer_mint = |outgoing: bool, amounts: &[u64]| -> Option<String> {
        transfers.iter().find(|x| {
            let side = if outgoing { x.from_owner == pool } else { x.to_owner == pool };
            side && x.mint != SOL && amounts.contains(&x.amount)
        }).map(|x| x.mint.clone())
    };
    const ZERO_KEY: &str = "11111111111111111111111111111111";
    let user_base_acct = ev["user_base_token_account"].as_str().unwrap_or("");
    let acct = |names: &[&str]| -> String {
        names.iter().find_map(|n| accts.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone())).unwrap_or_default()
    };
    let fee_fields: &[&str] = &["fee", "creator_fee", "lp_fee", "protocol_fee", "coin_creator_fee", "buyback_fee", "cashback", "holder_rewards"];
    let fees = fee_fields.iter().filter_map(|f| ev.get(*f).and_then(|v| v.as_u64()).map(|v| (f.to_string(), v))).collect();
    // User-side quote amounts. Each rule was checked against the token and SOL
    // transfers of a real transaction (see tests/fixtures.rs); the IDL field
    // names alone are misleading (PumpSwap BuyEvent.user_quote_amount_in is
    // the amount *net* of fees, not what the user paid).
    let (is_buy, base_amount, pool_quote_amount, user_quote_amount) = match ev_name {
        "TradeEvent" => {
            let sol = u(&ev["sol_amount"]);
            let q = ev["quote_amount"].as_u64().filter(|q| *q > 0).unwrap_or(sol);
            let is_buy = ev["is_buy"].as_bool().unwrap_or(false);
            let extra_fees = u(&ev["buyback_fee"]) + u(&ev["cashback"]);
            // Buy: curve amount + protocol fee + creator fee (2EGN8M4i, 63a68W3c).
            // Sells and trades with buyback/cashback have no verified rule yet.
            let user = (is_buy && extra_fees == 0).then(|| q + u(&ev["fee"]) + u(&ev["creator_fee"]));
            (is_buy, u(&ev["token_amount"]), q, user)
        }
        // Buy: the user pays quote_amount_in (3J5U6DEf: 7,044,421 to pool + 1,741 buyback).
        "BuyEvent" => (true, u(&ev["base_amount_out"]), u(&ev["quote_amount_in"]), ev["quote_amount_in"].as_u64()),
        // Sell: the user receives user_quote_amount_out (E6rSqt83).
        _ => (false, u(&ev["base_amount_in"]), u(&ev["quote_amount_out"]), ev["user_quote_amount_out"].as_u64()),
    };
    // Old bonding curves write the all-zero key here, meaning SOL.
    // Bonding-curve events: an all-zero quote_mint means SOL (older curves).
    // PumpSwap events: the user's quote token account, then the instruction's
    // quote_mint account. Unresolved stays empty rather than guessing SOL.
    let quote_mint = match ev_name {
        "TradeEvent" => ev["quote_mint"].as_str().map(String::from)
            .filter(|s| !s.is_empty() && s != "11111111111111111111111111111111")
            .or_else(|| Some(acct(&["quote_mint"])).filter(|s| !s.is_empty()))
            .unwrap_or_else(|| WSOL.to_string()),
        "BuyEvent" => mint_of("user_quote_token_account")
            .or_else(|| pool_transfer_mint(false, &[u(&ev["quote_amount_in"]), u(&ev["quote_amount_in_with_lp_fee"])]))
            .unwrap_or_else(|| acct(&["quote_mint"])),
        _ => mint_of("user_quote_token_account")
            .or_else(|| pool_transfer_mint(true, &[u(&ev["user_quote_amount_out"]), u(&ev["quote_amount_out"]), u(&ev["quote_amount_out_without_lp_fee"])]))
            .unwrap_or_else(|| acct(&["quote_mint"])),
    };
    let base_from_pool = match ev_name {
        "BuyEvent" => pool_transfer_mint(true, &[u(&ev["base_amount_out"])]),
        "SellEvent" => pool_transfer_mint(false, &[u(&ev["base_amount_in"])]),
        _ => None,
    };
    Trade {
        ix_path: path.into(),
        program: label.into(),
        ix_name: ix_name.into(),
        event_name: ev_name.into(),
        user: ev["user"].as_str().map(String::from).unwrap_or_else(|| acct(&["user"])),
        mint: ev["mint"].as_str().map(String::from)
            .or_else(|| mint_of("user_base_token_account"))
            .or(base_from_pool)
            .unwrap_or_else(|| acct(&["base_mint", "mint"])),
        quote_mint,
        is_buy,
        routing_hop: ev_name != "TradeEvent" && user_base_acct == ZERO_KEY,
        base_amount,
        pool_quote_amount,
        user_quote_amount,
        fees,
        event: ev.clone(),
    }
}
