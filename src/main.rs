//! CLI:
//!   callout-board decode <tx.json>...                 decoded transactions as JSON
//!   callout-board position <wallet> <mint> <tx.json>... in-order PnL for one wallet and coin
//! Inputs are RPC getTransaction results (encoding=jsonParsed, maxSupportedTransactionVersion=1).

use anyhow::{bail, Context, Result};
use callout_board::{decode::decode_tx, idl::Registry, idl_dir, ledger::build_position};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let reg = Registry::load(&idl_dir())?;
    let load = |paths: &[String]| -> Result<Vec<_>> {
        paths.iter().map(|p| {
            let v: serde_json::Value = serde_json::from_slice(&std::fs::read(p).with_context(|| p.clone())?)?;
            decode_tx(&v, &reg).with_context(|| p.clone())
        }).collect()
    };
    match args.first().map(String::as_str) {
        Some("decode") => println!("{}", serde_json::to_string_pretty(&load(&args[1..])?)?),
        Some("position") if args.len() >= 4 => {
            let txs = load(&args[3..])?;
            println!("{}", serde_json::to_string_pretty(&build_position(&args[1], &args[2], &txs))?);
        }
        _ => bail!("usage: callout-board decode <tx.json>... | position <wallet> <mint> <tx.json>..."),
    }
    Ok(())
}
