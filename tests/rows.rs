//! Compact rows must keep everything the ledger needs: building a position
//! from rows (after a trip through JSON) must equal building it from the
//! full decoded transactions.

use callout_board::decode::{decode_tx, DecodedTx};
use callout_board::idl::Registry;
use callout_board::idl_dir;
use callout_board::ledger::build_position;
use callout_board::rows::{from_rows, to_rows, Rows};

fn load_all() -> Vec<DecodedTx> {
    let reg = Registry::load(&idl_dir()).unwrap();
    let dir = format!("{}/fixtures/tx", env!("CARGO_MANIFEST_DIR"));
    let mut out = vec![];
    for e in std::fs::read_dir(dir).unwrap() {
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(e.unwrap().path()).unwrap()).unwrap();
        out.push(decode_tx(&v, &reg).unwrap());
    }
    out
}

fn through_json(rows: &Rows) -> Rows {
    // Same path as storage: one JSON object per row.
    let mut back = Rows::default();
    for r in &rows.txs { back.txs.push(serde_json::from_str(&serde_json::to_string(r).unwrap()).unwrap()); }
    for r in &rows.trades { back.trades.push(serde_json::from_str(&serde_json::to_string(r).unwrap()).unwrap()); }
    for r in &rows.flows { back.flows.push(serde_json::from_str(&serde_json::to_string(r).unwrap()).unwrap()); }
    for r in &rows.transfers { back.transfers.push(serde_json::from_str(&serde_json::to_string(r).unwrap()).unwrap()); }
    back
}

#[test]
fn rows_survive_json_unchanged() {
    let mut rows = Rows::default();
    for d in load_all() { rows.extend(to_rows(&d)); }
    assert_eq!(through_json(&rows), rows);
}

#[test]
fn ledger_gives_the_same_answer_from_rows() {
    let full = load_all();
    let mut rows = Rows::default();
    for d in &full { rows.extend(to_rows(d)); }
    let compact = from_rows(through_json(&rows));
    assert_eq!(compact.len(), full.len());
    // Every trader in every fixture, in every coin they touched.
    for d in &full {
        for t in &d.trades {
            let a = serde_json::to_value(build_position(&t.user, &t.mint, &full)).unwrap();
            let b = serde_json::to_value(build_position(&t.user, &t.mint, &compact)).unwrap();
            assert_eq!(a, b, "{} {}", t.user, t.mint);
        }
    }
}

#[test]
fn trades_keep_reserves_and_fee_rates() {
    let full = load_all();
    let sell = full.iter().find(|d| d.signature.starts_with("E6rSqt83")).unwrap();
    let r = &to_rows(sell).trades[0];
    assert_eq!(r.reserves["virtual_quote_reserves"], "17580976722");
    assert_eq!(r.reserves["pool_base_token_reserves"], "240837572087601");
    assert_eq!(r.fee_bps["coin_creator_fee_basis_points"], 95);
    assert_eq!(r.trailing_bytes, 8);
    assert!(!r.reserves.keys().any(|k| k.starts_with("user_")));
}

#[test]
fn ledger_gives_the_same_answer_when_rows_join_by_block_position() {
    // Backfill and live data carry the in-block index, so child rows join on
    // (slot, index) and drop the signature. Give each fixture a distinct index.
    let mut full = load_all();
    for (i, d) in full.iter_mut().enumerate() {
        d.tx_index = Some(i as u32);
    }
    let mut rows = Rows::default();
    for d in &full { rows.extend(to_rows(d)); }
    assert!(rows.trades.iter().all(|r| r.signature.is_none()));
    assert!(rows.flows.iter().all(|r| r.signature.is_none()));
    let compact = from_rows(through_json(&rows));
    for d in &full {
        for t in &d.trades {
            let a = serde_json::to_value(build_position(&t.user, &t.mint, &full)).unwrap();
            let b = serde_json::to_value(build_position(&t.user, &t.mint, &compact)).unwrap();
            assert_eq!(a, b, "{} {}", t.user, t.mint);
        }
    }
}

#[test]
fn transfer_tags_on_the_app_buy() {
    let full = load_all();
    let buy = full.iter().find(|d| d.signature.starts_with("3J5U6DEf")).unwrap();
    let kind_of = |to_owner: &str, amount: u64| {
        buy.transfers.iter().find(|t| t.to_owner == to_owner && t.amount == amount).map(|t| t.kind.as_str())
    };
    // Gas paid back to the sponsor, and the router fee: kept.
    assert_eq!(kind_of("FHpcNSe6tb2n15bAdq4BkeYWGyZKFD7yLYrH92ng7wCT", 215_894), Some("other"));
    assert_eq!(kind_of("KfHbT5Jrr5SV3CDkQVCLiSNWyVv5DmuVaCzbAHrzMFQ", 2_250), Some("other"));
    // The user's wSOL into the PumpSwap pool and the buyback: swap legs.
    assert_eq!(kind_of("6Lk5AZYQ9TkvqMzAc6YQ2RwWSg46RzuN3enqu6g6xQAE", 7_044_421), Some("swap_leg"));
    let kept: Vec<_> = to_rows(buy).transfers.iter().map(|t| t.amount).collect();
    assert!(kept.contains(&215_894) && kept.contains(&2_250) && !kept.contains(&7_044_421), "{kept:?}");
}
