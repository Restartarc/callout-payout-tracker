//! Old Faithful backfill reader.
//!
//!   backfill --slots START:END [--threads N] [--out DIR] [--raw]   (END exclusive)
//!   backfill --epoch N         [--threads N] [--out DIR] [--raw]
//!   backfill compact <raw.jsonl.zst> [--out DIR]   convert an earlier raw file to rows
//!   backfill check <raw.jsonl.zst> <rpc-fixture-dir>
//!   backfill inspect <raw.jsonl.zst>
//!   backfill position <wallet> <mint> <rows-dir>   ledger position from row files
//!
//! Streams the epoch CAR archives over HTTP through Jetstreamer's firehose,
//! keeps every non-vote transaction that touches the pump, PumpSwap or pump
//! fees programs, re-encodes it with the official Solana crates into the same
//! `jsonParsed` shape RPC returns (plus `transactionIndex` and `blockTime`),
//! decodes it with the core decoder, and writes compact rows: one zstd
//! JSON-lines file per table (txs, trades, flows, transfers), ready for
//! ClickHouse `FORMAT JSONEachRow`. `--raw` also keeps the full transactions.
//! Transactions the decoder rejects are always kept in full, in their own file.
//! A summary with volume figures is written next to the data.

use anyhow::{anyhow, bail, Context, Result};
use callout_board::{decode::decode_tx, idl::Registry, idl_dir, payouts::{payout_rows, PAYOUT_WALLETS}, rows::to_rows};
use futures::FutureExt;
use jetstreamer_firehose::epochs::epoch_to_slot_range;
use jetstreamer_firehose::firehose::{
    firehose, BlockData, FirehoseErrorContext, OnEntryFn, OnRewardFn, OnStatsTrackingFn, TransactionData,
};
use serde_json::json;
use solana_transaction_status::{
    EncodedConfirmedTransactionWithStatusMeta, UiTransactionEncoding, VersionedTransactionWithStatusMeta,
};
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{mpsc, Arc};
use std::time::Instant;

const PROGRAMS: [&str; 3] = [
    "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P", // pump bonding curve
    "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA", // PumpSwap
    "pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ", // pump fees
];
/// Highest transaction version we ask the encoder to accept (v1 is in use).
const MAX_TX_VERSION: u8 = 1;

enum Msg {
    Tx(u64, Box<EncodedConfirmedTransactionWithStatusMeta>),
    Block(u64, Option<i64>),
    Done,
}

#[derive(Default)]
struct Counters {
    txs_seen: AtomicU64,
    votes: AtomicU64,
    matched: AtomicU64,
    encode_errors: AtomicU64,
    blocks: AtomicU64,
    skipped_slots: AtomicU64,
    firehose_errors: AtomicU64,
}

struct Shared {
    programs: Vec<[u8; 32]>,
    tx: mpsc::SyncSender<Msg>,
    c: Counters,
}

fn parse_args() -> Result<(u64, u64, u64, PathBuf, bool)> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let get = |k: &str| a.iter().position(|x| x == k).and_then(|i| a.get(i + 1)).cloned();
    let (start, end) = if let Some(s) = get("--slots") {
        let (x, y) = s.split_once(':').ok_or_else(|| anyhow!("--slots START:END"))?;
        (x.parse()?, y.parse()?)
    } else if let Some(e) = get("--epoch") {
        let (s, last) = epoch_to_slot_range(e.parse()?);
        (s, last + 1)
    } else {
        bail!("usage: backfill --slots START:END | --epoch N [--threads N] [--out DIR]");
    };
    if end <= start {
        bail!("empty slot range");
    }
    let threads = get("--threads").map(|t| t.parse()).transpose()?.unwrap_or(4);
    let out = PathBuf::from(get("--out").unwrap_or_else(|| "backfill-out".into()));
    Ok((start, end, threads, out, a.iter().any(|x| x == "--raw")))
}

/// `backfill check <data.jsonl.zst> <rpc-fixture-dir>`: for every backfilled
/// transaction that also has an RPC fixture, decode both and compare.
fn check(data: &str, fixtures: &str) -> Result<()> {
    use std::io::BufRead;
    let reg = Registry::load(&idl_dir())?;
    let rd = std::io::BufReader::new(zstd::stream::read::Decoder::new(std::fs::File::open(data)?)?);
    let (mut compared, mut mismatched) = (0, 0);
    for line in rd.lines() {
        let v: serde_json::Value = serde_json::from_str(&line?)?;
        let sig = v["transaction"]["signatures"][0].as_str().unwrap_or("").to_string();
        let fx = PathBuf::from(fixtures).join(format!("{sig}.json"));
        if !fx.exists() {
            continue;
        }
        let rpc: serde_json::Value = serde_json::from_slice(&std::fs::read(&fx)?)?;
        let (a, b) = (decode_tx(&v, &reg)?, decode_tx(&rpc, &reg)?);
        let mut diffs = vec![];
        let fields: [(&str, serde_json::Value, serde_json::Value); 9] = [
            ("trades", json!(a.trades), json!(b.trades)),
            ("wallet_flows", json!(a.wallet_flows), json!(b.wallet_flows)),
            ("transfers", json!(a.transfers), json!(b.transfers)),
            ("memos", json!(a.memos), json!(b.memos)),
            ("features", json!(a.features), json!(b.features)),
            ("fee", json!(a.fee_lamports), json!(b.fee_lamports)),
            ("fee_payer", json!(a.fee_payer), json!(b.fee_payer)),
            ("block_time", json!(a.block_time), json!(b.block_time)),
            ("warnings", json!(a.warnings), json!(b.warnings)),
        ];
        for (name, x, y) in fields {
            if x != y {
                diffs.push(name);
            }
        }
        compared += 1;
        if diffs.is_empty() {
            println!("MATCH    {sig}  (tx_index {:?})", a.tx_index);
        } else {
            mismatched += 1;
            println!("MISMATCH {sig}: {diffs:?}");
        }
    }
    println!("compared {compared}, mismatched {mismatched}");
    if compared == 0 || mismatched > 0 {
        bail!("check failed");
    }
    Ok(())
}

/// `backfill inspect <data.jsonl.zst>`: tallies decoder warnings by whether
/// the transaction failed, and prints a few succeeded examples of each kind.
fn inspect(data: &str) -> Result<()> {
    use std::io::BufRead;
    let reg = Registry::load(&idl_dir())?;
    let rd = std::io::BufReader::new(zstd::stream::read::Decoder::new(std::fs::File::open(data)?)?);
    let mut tally: BTreeMap<(String, bool), u64> = BTreeMap::new();
    let mut examples: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let (mut ok, mut failed, mut ok_trades) = (0u64, 0u64, 0u64);
    for line in rd.lines() {
        let v: serde_json::Value = serde_json::from_str(&line?)?;
        let d = decode_tx(&v, &reg)?;
        if d.failed { failed += 1 } else { ok += 1; ok_trades += d.trades.len() as u64 }
        for w in &d.warnings {
            let kind = w.split(':').nth(1).unwrap_or(w).trim().to_string();
            if kind.starts_with("8 bytes past") { continue; }
            *tally.entry((kind.clone(), d.failed)).or_default() += 1;
            if !d.failed {
                let e = examples.entry(kind).or_default();
                if e.len() < 3 { e.push(d.signature.clone()); }
            }
        }
    }
    println!("succeeded txs {ok} (trades decoded {ok_trades}), failed txs {failed}");
    for ((k, f), n) in &tally {
        println!("{:>8}  {}  {k}", n, if *f { "FAILED   " } else { "SUCCEEDED" });
    }
    for (k, sigs) in &examples {
        println!("examples [{k}]: {sigs:?}");
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("check") && args.len() == 4 {
        return check(&args[2], &args[3]);
    }
    if args.get(1).map(String::as_str) == Some("inspect") && args.len() == 3 {
        return inspect(&args[2]);
    }
    if args.get(1).map(String::as_str) == Some("position") && args.len() == 5 {
        return position(&args[2], &args[3], &args[4]);
    }
    if args.get(1).map(String::as_str) == Some("compact") && args.len() >= 3 {
        let out = args.iter().position(|x| x == "--out").and_then(|i| args.get(i + 1)).map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(&args[2]).parent().map(|p| p.to_path_buf()).unwrap_or_default());
        return compact(&args[2], &out);
    }
    let (start, end, threads, out_dir, raw) = parse_args()?;
    std::fs::create_dir_all(&out_dir)?;
    let stem = format!("slots-{start}-{end}");

    let (tx, rx) = mpsc::sync_channel::<Msg>(100_000);
    let shared = Arc::new(Shared {
        // Watched addresses: the three programs, plus the callout payout
        // wallets, whose plain USDC transfers touch no pump program.
        programs: PROGRAMS.iter().chain(PAYOUT_WALLETS.iter().map(|(w, _)| w))
            .map(|p| bs58::decode(p).into_vec().unwrap().try_into().unwrap()).collect(),
        tx,
        c: Counters::default(),
    });
    let writer = std::thread::spawn({
        let sink = Sink::create(&out_dir, &stem, raw)?;
        move || write_loop(rx, sink)
    });

    let s = shared.clone();
    let on_tx = move |_thread: usize, t: TransactionData| {
        let s = s.clone();
        async move {
            s.c.txs_seen.fetch_add(1, Relaxed);
            if t.is_vote {
                s.c.votes.fetch_add(1, Relaxed);
                return Ok(());
            }
            // Program ids can arrive through address lookup tables when a
            // router CPIs into pump, so check loaded addresses too.
            let m = &t.transaction_status_meta;
            let hit = t.transaction.message.static_account_keys().iter()
                .chain(m.loaded_addresses.writable.iter())
                .chain(m.loaded_addresses.readonly.iter())
                .any(|k| s.programs.iter().any(|p| k.as_ref() == p.as_slice()));
            if !hit {
                return Ok(());
            }
            s.c.matched.fetch_add(1, Relaxed);
            let slot = t.slot;
            let index = t.transaction_slot_index as u32;
            let v = VersionedTransactionWithStatusMeta { transaction: t.transaction, meta: t.transaction_status_meta };
            match v.encode(UiTransactionEncoding::JsonParsed, Some(MAX_TX_VERSION), false) {
                Ok(e) => {
                    let full = EncodedConfirmedTransactionWithStatusMeta {
                        slot,
                        transaction: e,
                        block_time: None, // filled in when the block arrives
                        transaction_index: Some(index),
                    };
                    let _ = s.tx.send(Msg::Tx(slot, Box::new(full)));
                }
                Err(_) => { s.c.encode_errors.fetch_add(1, Relaxed); }
            }
            Ok(())
        }
        .boxed()
    };
    let s = shared.clone();
    let on_block = move |_thread: usize, b: BlockData| {
        let s = s.clone();
        async move {
            match b {
                BlockData::Block { slot, block_time, .. } => {
                    s.c.blocks.fetch_add(1, Relaxed);
                    let _ = s.tx.send(Msg::Block(slot, block_time));
                }
                BlockData::PossibleLeaderSkipped { slot } => {
                    s.c.skipped_slots.fetch_add(1, Relaxed);
                    let _ = s.tx.send(Msg::Block(slot, None));
                }
            }
            Ok(())
        }
        .boxed()
    };
    let s = shared.clone();
    let on_error = move |_thread: usize, e: FirehoseErrorContext| {
        let s = s.clone();
        async move {
            s.c.firehose_errors.fetch_add(1, Relaxed);
            eprintln!("firehose error: thread {} slot {} epoch {}: {}", e.thread_id, e.slot, e.epoch, e.error_message);
            Ok(())
        }
        .boxed()
    };

    let started = Instant::now();
    eprintln!("streaming slots {start}..{end} ({} slots) with {threads} threads", end - start);
    let result = firehose(
        threads, false, false, None, start..end,
        Some(on_block), Some(on_tx), None::<OnEntryFn>, None::<OnRewardFn>, Some(on_error),
        None::<OnStatsTrackingFn>, None,
    )
    .await;
    let elapsed = started.elapsed().as_secs_f64();
    shared.tx.send(Msg::Done).ok();
    let w = writer.join().map_err(|_| anyhow!("writer thread panicked"))??;
    if let Err((e, slot)) = result {
        eprintln!("firehose stopped at slot {slot}: {e:?}");
    }

    let c = &shared.c;
    let slots = end - start;
    let summary = json!({
        "slot_range": [start, end],
        "slots": slots,
        "epochs_touched": (start / 432_000..=(end - 1) / 432_000).collect::<Vec<_>>(),
        "wall_seconds": elapsed,
        "txs_seen": c.txs_seen.load(Relaxed),
        "votes": c.votes.load(Relaxed),
        "matched_txs": c.matched.load(Relaxed),
        "encode_errors": c.encode_errors.load(Relaxed),
        "blocks": c.blocks.load(Relaxed),
        "skipped_slots": c.skipped_slots.load(Relaxed),
        "firehose_errors": c.firehose_errors.load(Relaxed),
        "written_txs": w.written,
        "written_without_block_time": w.no_block_time,
        "duplicates_dropped": w.duplicates_dropped,
        "decoded_trades": w.trades,
        "decode_failures": w.decode_failures,
        "decoder_warnings": w.warnings,
        "rows": w.rows,
        "files": w.files,
    });
    std::fs::write(out_dir.join(format!("{stem}.summary.json")), serde_json::to_vec_pretty(&summary)?)?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

#[derive(Default)]
struct WriteStats {
    written: u64,
    no_block_time: u64,
    duplicates_dropped: u64,
    trades: u64,
    decode_failures: u64,
    warnings: BTreeMap<String, u64>,
    rows: BTreeMap<&'static str, u64>,
    /// Per output file: uncompressed JSON bytes and bytes on disk.
    files: BTreeMap<String, serde_json::Value>,
}

type Zst = zstd::stream::write::Encoder<'static, std::fs::File>;

/// One zstd JSON-lines file per table, plus optional raw and the
/// decode-failure file.
struct Sink {
    reg: Registry,
    files: Vec<(&'static str, PathBuf, Zst, u64)>,
    raw: bool,
    st: WriteStats,
}

const TABLES: [&str; 5] = ["txs", "trades", "flows", "transfers", "payouts"];
const RAW: &str = "raw";
const FAILURES: &str = "decode-failures";

impl Sink {
    fn create(dir: &std::path::Path, stem: &str, raw: bool) -> Result<Sink> {
        let mut files = vec![];
        let mut names: Vec<&'static str> = TABLES.to_vec();
        names.push(FAILURES);
        if raw {
            names.push(RAW);
        }
        for name in names {
            let path = dir.join(format!("{stem}.{name}.jsonl.zst"));
            let f = std::fs::File::create(&path).with_context(|| format!("{path:?}"))?;
            files.push((name, path, zstd::stream::write::Encoder::new(f, 9)?, 0));
        }
        Ok(Sink { reg: Registry::load(&idl_dir())?, files, raw, st: WriteStats::default() })
    }

    fn line<T: serde::Serialize>(&mut self, table: &'static str, row: &T) -> Result<()> {
        let s = serde_json::to_string(row)?;
        let f = self.files.iter_mut().find(|f| f.0 == table).ok_or_else(|| anyhow!("no file {table}"))?;
        f.2.write_all(s.as_bytes())?;
        f.2.write_all(b"\n")?;
        f.3 += s.len() as u64 + 1;
        *self.st.rows.entry(table).or_default() += 1;
        Ok(())
    }

    /// Decodes one transaction (RPC jsonParsed shape) and writes its rows.
    fn write_tx(&mut self, v: &serde_json::Value) -> Result<()> {
        if v["blockTime"].is_null() {
            self.st.no_block_time += 1;
        }
        match decode_tx(v, &self.reg) {
            Ok(d) => {
                self.st.trades += d.trades.len() as u64;
                for w in &d.warnings {
                    // Group warnings by their kind, not their exact text.
                    let kind = w.split(':').nth(1).unwrap_or(w).split_whitespace().take(5).collect::<Vec<_>>().join(" ");
                    *self.st.warnings.entry(kind).or_default() += 1;
                }
                let rows = to_rows(&d);
                for r in &rows.txs { self.line("txs", r)?; }
                for r in &rows.trades { self.line("trades", r)?; }
                for r in &rows.flows { self.line("flows", r)?; }
                for r in &rows.transfers { self.line("transfers", r)?; }
                let wallets: Vec<&str> = PAYOUT_WALLETS.iter().map(|(w, _)| *w).collect();
                for r in payout_rows(&d, &wallets) { self.line("payouts", &r)?; }
            }
            Err(_) => {
                self.st.decode_failures += 1;
                self.line(FAILURES, v)?;
            }
        }
        if self.raw {
            self.line(RAW, v)?;
        }
        self.st.written += 1;
        Ok(())
    }

    fn finish(mut self) -> Result<WriteStats> {
        for (name, path, enc, json_bytes) in self.files {
            enc.finish()?;
            let on_disk = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            self.st.files.insert(name.to_string(), json!({ "path": path, "json_bytes": json_bytes, "file_bytes": on_disk }));
        }
        Ok(self.st)
    }
}

/// Holds each slot's transactions until its block (and block time) arrives,
/// then decodes them and writes their rows.
fn write_loop(rx: mpsc::Receiver<Msg>, mut sink: Sink) -> Result<WriteStats> {
    let mut pending: HashMap<u64, Vec<Box<EncodedConfirmedTransactionWithStatusMeta>>> = HashMap::new();
    let flush = |txs: Vec<Box<EncodedConfirmedTransactionWithStatusMeta>>, bt: Option<i64>, sink: &mut Sink| -> Result<()> {
        let mut txs = txs;
        txs.sort_by_key(|t| t.transaction_index);
        for mut t in txs {
            t.block_time = bt;
            sink.write_tx(&serde_json::to_value(&*t)?)?;
        }
        Ok(())
    };

    // The firehose retries a slot after a network timeout and can replay its
    // transactions, so drop repeats of (slot, index), including ones arriving
    // after the slot was already written.
    let mut flushed: std::collections::HashSet<u64> = std::collections::HashSet::new();
    for msg in rx {
        match msg {
            Msg::Tx(slot, t) => {
                let v = pending.entry(slot).or_default();
                if flushed.contains(&slot) || v.iter().any(|x| x.transaction_index == t.transaction_index) {
                    sink.st.duplicates_dropped += 1;
                } else {
                    v.push(t);
                }
                if flushed.contains(&slot) {
                    pending.remove(&slot);
                }
            }
            Msg::Block(slot, bt) => {
                if let Some(txs) = pending.remove(&slot) {
                    flush(txs, bt, &mut sink)?;
                }
                flushed.insert(slot);
            }
            Msg::Done => break,
        }
    }
    // Anything left never saw its block (e.g. the run stopped mid-slot).
    let mut rest: Vec<_> = pending.into_iter().collect();
    rest.sort_by_key(|(s, _)| *s);
    for (_, txs) in rest {
        flush(txs, None, &mut sink)?;
    }
    sink.finish()
}

/// `backfill compact <raw.jsonl.zst>`: turns an earlier raw file into rows.
fn compact(raw_path: &str, out_dir: &std::path::Path) -> Result<()> {
    use std::io::BufRead;
    std::fs::create_dir_all(out_dir)?;
    let name = std::path::Path::new(raw_path).file_name().and_then(|n| n.to_str()).unwrap_or("input");
    let stem = name.trim_end_matches(".jsonl.zst").trim_end_matches(".raw");
    let mut sink = Sink::create(out_dir, stem, false)?;
    let rd = std::io::BufReader::new(zstd::stream::read::Decoder::new(std::fs::File::open(raw_path)?)?);
    for line in rd.lines() {
        sink.write_tx(&serde_json::from_str(&line?)?)?;
    }
    let st = sink.finish()?;
    let summary = json!({
        "input": raw_path,
        "input_file_bytes": std::fs::metadata(raw_path).map(|m| m.len()).unwrap_or(0),
        "txs": st.written, "decoded_trades": st.trades, "decode_failures": st.decode_failures,
        "rows": st.rows, "files": st.files,
    });
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

/// `backfill position <wallet> <mint> <dir>`: reads every row file in `dir`
/// and prints the ledger position, exactly as the ledger would from storage.
fn position(wallet: &str, mint: &str, dir: &str) -> Result<()> {
    use callout_board::rows::{from_rows, Rows};
    use std::io::BufRead;
    fn read<T: serde::de::DeserializeOwned>(dir: &str, table: &str) -> Result<Vec<T>> {
        let mut out = vec![];
        for e in std::fs::read_dir(dir)? {
            let p = e?.path();
            if p.to_string_lossy().ends_with(&format!(".{table}.jsonl.zst")) {
                let rd = std::io::BufReader::new(zstd::stream::read::Decoder::new(std::fs::File::open(&p)?)?);
                for line in rd.lines() {
                    out.push(serde_json::from_str(&line?)?);
                }
            }
        }
        Ok(out)
    }
    let rows = Rows {
        txs: read(dir, "txs")?,
        trades: read(dir, "trades")?,
        flows: read(dir, "flows")?,
        transfers: read(dir, "transfers")?,
    };
    let txs = from_rows(rows);
    let p = callout_board::ledger::build_position(wallet, mint, &txs);
    println!("{}", serde_json::to_string_pretty(&p)?);
    Ok(())
}
