//! pump.fun callout reward payout ledger.
//!
//!   payouts ingest [--wallet W] [--since YYYY-MM-DD] [--rps N] [--workers N] [--batch N] [--max-pages N] [--max-minutes N] [--page-size N] [--out DIR]
//!   payouts report [--out DIR] [--wallet W]
//!   payouts wallet <ADDRESS> [--out DIR]
//!   payouts site [--out DIR] [--site DIR]     write the website's data files
//!
//! `ingest` pages back through the payout wallet's history over RPC
//! (RPC_URL from the environment or a git-ignored `.env`, default the public
//! endpoint; the key is never printed), decodes every transaction with the
//! core decoder and writes payout and funding rows. Each page is written to its
//! own file before progress is saved, so a stopped run resumes where it left
//! off and a re-run picks up new payouts first. `report` gives daily batch
//! totals and how concentrated payouts are; `wallet` looks one wallet up.

use anyhow::{anyhow, bail, Context, Result};
use callout_board::decode::decode_tx;
use callout_board::idl::Registry;
use callout_board::idl_dir;
use callout_board::ledger::USDC;
use callout_board::payouts::{batches, concentration, dedupe, payout_rows, percentile, totals_by_recipient, PayoutRow, PAYOUT_WALLETS};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

mod site;

const PUBLIC_RPC: &str = "https://api.mainnet-beta.solana.com";
const CALLOUT_REWARDS_START: &str = "2026-08-13";

fn arg(a: &[String], k: &str) -> Option<String> {
    a.iter().position(|x| x == k).and_then(|i| a.get(i + 1)).cloned()
}

fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let out = PathBuf::from(arg(&a, "--out").unwrap_or_else(|| "payouts-out".into()));
    let wallet = arg(&a, "--wallet").unwrap_or_else(|| PAYOUT_WALLETS[0].0.to_string());
    match a.first().map(String::as_str) {
        Some("ingest") => ingest(&a, &wallet, &out),
        Some("report") => report(&out, arg(&a, "--wallet").as_deref()),
        Some("wallet") if a.len() >= 2 => lookup(&a[1], &out),
        Some("site") => {
            let dir = PathBuf::from(arg(&a, "--site").unwrap_or_else(|| "site".into()));
            println!("{}", site::build(&out, &dir)?);
            Ok(())
        }
        _ => bail!("usage: payouts ingest|report|wallet <ADDRESS> (see the top of payouts/src/main.rs)"),
    }
}

// ---------------------------------------------------------------- RPC client

/// RPC endpoint: the RPC_URL environment variable, else RPC_URL in a `.env`
/// file in the current directory (git-ignored), else the public endpoint.
fn rpc_url() -> String {
    if let Ok(u) = std::env::var("RPC_URL") {
        if !u.trim().is_empty() {
            return u.trim().to_string();
        }
    }
    if let Ok(s) = std::fs::read_to_string(".env") {
        for line in s.lines() {
            if let Some(v) = line.trim().strip_prefix("RPC_URL=") {
                let v = v.trim().trim_matches('"').trim_matches('\'');
                if !v.is_empty() {
                    return v.to_string();
                }
            }
        }
    }
    PUBLIC_RPC.into()
}

/// The endpoint without its query string or path secrets, for logs:
/// provider keys usually sit in `?api-key=...` or in the path.
fn redact(url: &str) -> String {
    let base = url.split('?').next().unwrap_or(url);
    let mut parts: Vec<&str> = base.splitn(4, '/').collect(); // scheme, "", host, path
    if parts.len() == 4 && !parts[3].is_empty() {
        parts[3] = "…";
    }
    let shown = parts.join("/");
    if url.contains('?') || shown != base { format!("{shown} (key hidden)") } else { shown }
}

struct Rpc {
    url: String,
    min_gap: Duration,
    last: Instant,
    calls: u64,
    retries: u64,
}

impl Rpc {
    fn new(rps: f64) -> Rpc {
        Rpc {
            url: rpc_url(),
            min_gap: Duration::from_secs_f64(1.0 / rps.max(0.01)),
            last: Instant::now() - Duration::from_secs(10),
            calls: 0,
            retries: 0,
        }
    }

    /// Posts one JSON-RPC body (a request or an array of them), keeping to the
    /// rate limit and backing off on 429s and network errors.
    fn post(&mut self, body: &Value) -> Result<Value> {
        let mut wait = Duration::from_secs(2);
        for attempt in 0..10 {
            let since = self.last.elapsed();
            if since < self.min_gap {
                std::thread::sleep(self.min_gap - since);
            }
            self.last = Instant::now();
            self.calls += 1;
            match ureq::post(&self.url).set("Content-Type", "application/json").send_string(&body.to_string()) {
                Ok(resp) => return Ok(serde_json::from_str(&resp.into_string()?)?),
                Err(ureq::Error::Status(code, _)) if code == 429 || code >= 500 => {}
                Err(ureq::Error::Status(code, resp)) => bail!("RPC HTTP {code}: {}", resp.into_string().unwrap_or_default()),
                Err(e) if attempt < 9 => eprintln!("network error, retrying: {}", self.scrub(&e.to_string())),
                Err(e) => bail!("network error: {}", self.scrub(&e.to_string())),
            }
            self.retries += 1;
            std::thread::sleep(wait);
            wait = (wait * 2).min(Duration::from_secs(60));
        }
        bail!("RPC kept refusing after 10 attempts")
    }

    /// Error text from the HTTP library can include the full endpoint; swap
    /// it for the redacted form so a key never reaches the log.
    fn scrub(&self, s: &str) -> String {
        s.replace(&self.url, &redact(&self.url))
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let r = self.post(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))?;
        if let Some(e) = r.get("error") {
            bail!("{method}: {e}");
        }
        Ok(r["result"].clone())
    }

    /// Fetches transactions, `batch` per HTTP request. Items the server
    /// refuses inside a batch are retried on their own.
    fn transactions(&mut self, sigs: &[String], batch: usize) -> Result<Vec<Value>> {
        let cfg = json!({"encoding": "jsonParsed", "maxSupportedTransactionVersion": 1, "commitment": "confirmed"});
        let mut out = vec![];
        for chunk in sigs.chunks(batch.max(1)) {
            if chunk.len() == 1 {
                out.push(self.tx_single(&chunk[0], &cfg)?);
                continue;
            }
            let body: Vec<Value> = chunk.iter().enumerate()
                .map(|(i, s)| json!({"jsonrpc": "2.0", "id": i, "method": "getTransaction", "params": [s, cfg]}))
                .collect();
            let r = self.post(&Value::Array(body))?;
            let mut got: Vec<Option<Value>> = vec![None; chunk.len()];
            for item in r.as_array().cloned().unwrap_or_default() {
                let i = item["id"].as_u64().unwrap_or(u64::MAX) as usize;
                if i < got.len() && item.get("error").is_none() && !item["result"].is_null() {
                    got[i] = Some(item["result"].clone());
                }
            }
            for (i, g) in got.into_iter().enumerate() {
                out.push(match g { Some(v) => v, None => self.tx_single(&chunk[i], &cfg)? });
            }
        }
        Ok(out)
    }

    fn tx_single(&mut self, sig: &str, cfg: &Value) -> Result<Value> {
        let mut wait = Duration::from_secs(2);
        for _ in 0..8 {
            let r = self.post(&json!({"jsonrpc": "2.0", "id": 1, "method": "getTransaction", "params": [sig, cfg]}))?;
            if r.get("error").is_none() && !r["result"].is_null() {
                return Ok(r["result"].clone());
            }
            // Rate-limit errors also arrive as JSON-RPC errors with HTTP 200.
            self.retries += 1;
            std::thread::sleep(wait);
            wait = (wait * 2).min(Duration::from_secs(60));
        }
        bail!("could not fetch {sig}")
    }
}

// ------------------------------------------------------------------- ingest

#[derive(Default, Serialize, Deserialize)]
struct State {
    /// Newest signature already ingested; the next run's head pass stops here.
    newest: Option<String>,
    /// Oldest signature ingested so far; the backfill continues before it.
    cursor: Option<String>,
    backfill_done: bool,
    pages: u64,
    txs: u64,
    payout_rows: u64,
    funding_rows: u64,
}

fn date_to_unix(d: &str) -> Result<i64> {
    let p: Vec<i64> = d.split('-').map(|x| x.parse()).collect::<Result<_, _>>().context("date YYYY-MM-DD")?;
    let (y, m, day) = (p[0], p[1], p[2]);
    // Days from civil (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Ok((era * 146097 + doe - 719468) * 86400)
}

fn utc(t: i64) -> String {
    let days = t.div_euclid(86400);
    let secs = t.rem_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", secs / 3600, secs % 3600 / 60, secs % 60)
}

fn ingest(a: &[String], wallet: &str, out: &Path) -> Result<()> {
    let since = date_to_unix(&arg(a, "--since").unwrap_or_else(|| CALLOUT_REWARDS_START.into()))?;
    let rps: f64 = arg(a, "--rps").map(|x| x.parse()).transpose()?.unwrap_or(2.0);
    let batch: usize = arg(a, "--batch").map(|x| x.parse()).transpose()?.unwrap_or(1);
    // Requests wait ~0.5 s for a reply; workers run in parallel and split the
    // --rps budget between them, so throughput approaches --rps.
    let workers: usize = arg(a, "--workers").map(|x| x.parse()).transpose()?.unwrap_or(1).clamp(1, 32);
    let max_pages: u64 = arg(a, "--max-pages").map(|x| x.parse()).transpose()?.unwrap_or(u64::MAX);
    // Stop cleanly between pages after this long (CI jobs have time limits);
    // the next run resumes from the saved position.
    let max_minutes: f64 = arg(a, "--max-minutes").map(|x| x.parse()).transpose()?.unwrap_or(f64::INFINITY);
    let page_size: usize = arg(a, "--page-size").map(|x| x.parse()).transpose()?.unwrap_or(1000).clamp(1, 1000);
    let chunks = out.join("chunks");
    std::fs::create_dir_all(&chunks)?;
    let state_path = out.join(format!("state-{wallet}.json"));
    let mut st: State = std::fs::read(&state_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let reg = Registry::load(&idl_dir())?;
    let mut rpc = Rpc::new(rps);
    // Payouts happen on mainnet; a devnet/testnet endpoint would just find nothing.
    let host = redact(&rpc.url).to_lowercase();
    if host.contains("devnet") || host.contains("testnet") {
        bail!("RPC_URL points at {} - payouts are on mainnet. Use the mainnet endpoint (for Helius: https://mainnet.helius-rpc.com/?api-key=...).", redact(&rpc.url));
    }
    let started = Instant::now();
    eprintln!("RPC {} at up to {rps} req/s over {workers} workers, batch {batch}; wallet {wallet}; since {}", redact(&rpc.url), utc(since));
    let pool = Pool { rps, workers, batch };

    let mut pages_this_run = 0u64;
    // Head pass: new payouts since the last run (pages newest-first until the
    // newest signature we already have).
    if let Some(newest) = st.newest.clone() {
        let mut before: Option<String> = None;
        let mut first_new: Option<String> = None;
        loop {
            let mut p = json!({"limit": page_size, "until": newest});
            if let Some(b) = &before { p["before"] = json!(b); }
            let page = rpc.call("getSignaturesForAddress", json!([wallet, p]))?;
            let sigs = page.as_array().cloned().unwrap_or_default();
            if sigs.is_empty() { break; }
            if first_new.is_none() { first_new = sigs[0]["signature"].as_str().map(String::from); }
            before = sigs.last().and_then(|s| s["signature"].as_str()).map(String::from);
            process_page(&sigs, wallet, &reg, &pool, &chunks, &mut st)?;
            pages_this_run += 1;
            eprintln!("head: page {pages_this_run}, {} signatures", sigs.len());
            if sigs.len() < page_size || started.elapsed().as_secs_f64() / 60.0 > max_minutes { break; }
        }
        if let Some(f) = first_new { st.newest = Some(f); }
        save(&state_path, &st)?;
    }
    // Backfill pass: older history, down to --since.
    while !st.backfill_done && pages_this_run < max_pages && started.elapsed().as_secs_f64() / 60.0 < max_minutes {
        let mut p = json!({"limit": page_size});
        if let Some(c) = &st.cursor { p["before"] = json!(c); }
        let page = rpc.call("getSignaturesForAddress", json!([wallet, p]))?;
        let all = page.as_array().cloned().unwrap_or_default();
        if all.is_empty() {
            st.backfill_done = true;
            save(&state_path, &st)?;
            break;
        }
        let keep: Vec<Value> = all.iter().filter(|s| s["blockTime"].as_i64().map(|t| t >= since).unwrap_or(true)).cloned().collect();
        if st.newest.is_none() {
            st.newest = all[0]["signature"].as_str().map(String::from);
        }
        process_page(&keep, wallet, &reg, &pool, &chunks, &mut st)?;
        st.cursor = all.last().and_then(|s| s["signature"].as_str()).map(String::from);
        if keep.len() < all.len() || all.len() < page_size {
            st.backfill_done = true;
        }
        save(&state_path, &st)?;
        pages_this_run += 1;
        let oldest = all.last().and_then(|s| s["blockTime"].as_i64()).map(utc).unwrap_or_default();
        eprintln!("backfill: page {pages_this_run}, reached {oldest}, {} txs total, {:.1} min, {} calls, {} retries",
            st.txs, started.elapsed().as_secs_f64() / 60.0, rpc.calls, rpc.retries);
    }
    println!("{}", serde_json::to_string_pretty(&json!({
        "wallet": wallet, "backfill_done": st.backfill_done, "pages_total": st.pages, "txs_total": st.txs,
        "payout_rows_total": st.payout_rows, "funding_rows_total": st.funding_rows,
        "this_run": { "pages": pages_this_run, "minutes": started.elapsed().as_secs_f64() / 60.0, "rpc_calls": rpc.calls, "retries": rpc.retries },
    }))?);
    Ok(())
}

fn save(path: &Path, st: &State) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(st)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

/// Fetches and decodes one page of signatures and writes its rows to a new
/// chunk file (written to a temp name, then renamed, so it is never partial).
/// Parallel transaction fetching: each worker gets its own client and an
/// equal share of the request budget; results come back in page order.
struct Pool {
    rps: f64,
    workers: usize,
    batch: usize,
}

impl Pool {
    fn fetch(&self, sigs: &[String]) -> Result<Vec<Value>> {
        let per = sigs.len().div_ceil(self.workers).max(1);
        let parts: Vec<&[String]> = sigs.chunks(per).collect();
        let share = self.rps / parts.len().max(1) as f64;
        std::thread::scope(|s| {
            let handles: Vec<_> = parts.iter().map(|part| {
                let batch = self.batch;
                s.spawn(move || Rpc::new(share).transactions(part, batch))
            }).collect();
            let mut out = Vec::with_capacity(sigs.len());
            for h in handles {
                out.extend(h.join().map_err(|_| anyhow!("worker panicked"))??);
            }
            Ok(out)
        })
    }
}

fn process_page(sigs: &[Value], wallet: &str, reg: &Registry, pool: &Pool, dir: &Path, st: &mut State) -> Result<()> {
    let ok: Vec<String> = sigs.iter()
        .filter(|s| s["err"].is_null())
        .filter_map(|s| s["signature"].as_str().map(String::from))
        .collect();
    if ok.is_empty() {
        return Ok(());
    }
    let txs = pool.fetch(&ok)?;
    let mut rows: Vec<PayoutRow> = vec![];
    for t in &txs {
        let d = decode_tx(t, reg).with_context(|| format!("decoding {}", t["transaction"]["signatures"][0]))?;
        rows.extend(payout_rows(&d, &[wallet]));
    }
    let name = format!("{}-{}-{}.jsonl.zst", &wallet[..8], ok.first().unwrap(), ok.len());
    let path = dir.join(&name);
    let tmp = dir.join(format!("{name}.tmp"));
    let mut enc = zstd::stream::write::Encoder::new(std::fs::File::create(&tmp)?, 9)?;
    for r in &rows {
        enc.write_all(serde_json::to_string(r)?.as_bytes())?;
        enc.write_all(b"\n")?;
    }
    enc.finish()?;
    std::fs::rename(tmp, path)?;
    st.pages += 1;
    st.txs += txs.len() as u64;
    st.payout_rows += rows.iter().filter(|r| r.kind == "payout").count() as u64;
    st.funding_rows += rows.iter().filter(|r| r.kind == "funding").count() as u64;
    Ok(())
}

// ------------------------------------------------------------------- report

fn load_rows(out: &Path, wallet: Option<&str>) -> Result<Vec<PayoutRow>> {
    let mut rows = vec![];
    let dir = out.join("chunks");
    for e in std::fs::read_dir(&dir).with_context(|| format!("{dir:?}: run `payouts ingest` first"))? {
        let p = e?.path();
        if !p.to_string_lossy().ends_with(".jsonl.zst") {
            continue;
        }
        let rd = std::io::BufReader::new(zstd::stream::read::Decoder::new(std::fs::File::open(&p)?)?);
        for line in rd.lines() {
            let r: PayoutRow = serde_json::from_str(&line?)?;
            if wallet.map(|w| r.payout_wallet == w).unwrap_or(true) {
                rows.push(r);
            }
        }
    }
    dedupe(&mut rows);
    Ok(rows)
}

fn usdc(x: u64) -> String {
    format!("{:.2}", x as f64 / 1e6)
}

fn report(out: &Path, wallet: Option<&str>) -> Result<()> {
    let rows = load_rows(out, wallet)?;
    let pays: Vec<&PayoutRow> = rows.iter().filter(|r| r.kind == "payout").collect();
    if pays.is_empty() {
        bail!("no payout rows yet");
    }
    let other_mints: std::collections::BTreeMap<&str, usize> = pays.iter().filter(|r| r.mint != USDC)
        .fold(Default::default(), |mut m, r| { *m.entry(r.mint.as_str()).or_default() += 1; m });
    let b = batches(&rows, USDC, 1800);
    let totals = totals_by_recipient(&rows, USDC);
    let c = concentration(&totals).ok_or_else(|| anyhow!("no USDC payouts"))?;
    let first = pays.iter().filter_map(|r| r.block_time).min().unwrap_or(0);
    let last = pays.iter().filter_map(|r| r.block_time).max().unwrap_or(0);

    println!("Callout payout ledger: {} USDC payouts, {} to {} UTC", pays.len(), utc(first), utc(last));
    if !other_mints.is_empty() {
        println!("Non-USDC payouts (not in the totals below): {other_mints:?}");
    }
    println!("\nBatches (payouts less than 30 min apart):");
    println!("  {:<19}  {:>6}  {:>8}  {:>10}  {:>12}  {:>8}  {:>8}  {:>9}", "start (UTC)", "min", "payouts", "wallets", "total USDC", "median", "p90", "max");
    for x in &b {
        println!("  {:<19}  {:>6.1}  {:>8}  {:>10}  {:>12}  {:>8}  {:>8}  {:>9}",
            utc(x.start), (x.end - x.start) as f64 / 60.0, x.payouts, x.recipients, usdc(x.total), usdc(x.median), usdc(x.p90), usdc(x.max));
    }
    println!("\nConcentration across {} wallets, {} USDC in total:", c.recipients, usdc(c.total));
    println!("  top 1% of wallets got {:.1}%, top 10% got {:.1}%, top 10 wallets got {:.1}%, Gini {:.3}",
        c.top_1pct_share * 100.0, c.top_10pct_share * 100.0, c.top_10_share * 100.0, c.gini);
    let mut top: Vec<(&String, &u64)> = totals.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1));
    println!("\nTop 10 wallets:");
    for (w, t) in top.iter().take(10) {
        let n = pays.iter().filter(|r| &r.counterparty == *w).count();
        println!("  {w}  {:>12} USDC  {n:>4} payouts", usdc(**t));
    }
    let mut funders: std::collections::BTreeMap<&str, (u64, usize)> = Default::default();
    for r in rows.iter().filter(|r| r.kind == "funding" && r.mint == USDC) {
        let e = funders.entry(r.counterparty.as_str()).or_default();
        e.0 += r.amount;
        e.1 += 1;
    }
    if !funders.is_empty() {
        println!("\nUSDC funding into the payout wallet:");
        for (f, (amt, n)) in &funders {
            println!("  {f}  {:>12} USDC  {n} transfers", usdc(*amt));
        }
    }
    let rep = json!({ "first": first, "last": last, "payouts": pays.len(), "batches": b, "concentration": c,
        "top": top.iter().take(100).map(|(w, t)| json!({"wallet": w, "usdc_raw": t})).collect::<Vec<_>>() });
    std::fs::write(out.join("report.json"), serde_json::to_vec_pretty(&rep)?)?;
    println!("\nFull report: {}", out.join("report.json").display());
    println!("Note: covers only the payout wallets ingested, and only what they paid in this window.");
    Ok(())
}

fn lookup(addr: &str, out: &Path) -> Result<()> {
    let rows = load_rows(out, None)?;
    let totals = totals_by_recipient(&rows, USDC);
    let mine: Vec<&PayoutRow> = rows.iter().filter(|r| r.kind == "payout" && r.counterparty == addr).collect();
    if mine.is_empty() {
        println!("No callout payouts to {addr} in the ingested history.");
        return Ok(());
    }
    let total = totals.get(addr).copied().unwrap_or(0);
    let mut ranked: Vec<u64> = totals.values().copied().collect();
    ranked.sort_unstable_by(|a, b| b.cmp(a));
    let rank = ranked.iter().position(|v| *v == total).map(|i| i + 1).unwrap_or(0);
    println!("{addr}: {} payouts, {} USDC in total", mine.len(), usdc(total));
    println!("Rank {rank} of {} paid wallets; at or above {:.1}% of them", totals.len(), percentile(&totals, total) * 100.0);
    for r in &mine {
        println!("  {}  {:>10} {}  https://solscan.io/tx/{}",
            r.block_time.map(utc).unwrap_or_default(), usdc(r.amount), if r.mint == USDC { "USDC" } else { r.mint.as_str() }, r.signature);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::redact;
    #[test]
    fn keys_never_reach_the_log() {
        assert_eq!(redact("https://mainnet.helius-rpc.com/?api-key=SECRET"), "https://mainnet.helius-rpc.com/ (key hidden)");
        assert_eq!(redact("https://solana-mainnet.g.alchemy.com/v2/SECRET"), "https://solana-mainnet.g.alchemy.com/… (key hidden)");
        assert_eq!(redact("https://api.mainnet-beta.solana.com"), "https://api.mainnet-beta.solana.com");
        assert!(!redact("https://x.example/abc?token=SECRET").contains("SECRET"));
    }
}
