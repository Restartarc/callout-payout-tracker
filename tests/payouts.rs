use callout_board::decode::decode_tx;
use callout_board::idl::Registry;
use callout_board::idl_dir;
use callout_board::ledger::USDC;
use callout_board::payouts::*;
use std::collections::BTreeMap;

const PAYOUT_WALLET: &str = "BNrYNyQZhdGRs8Rvi98WGTjhxab2wFTdGtVjW72RGyh4";
const CALLER: &str = "8itpCZ2haKvGpgBmTp1dzHndkJjxK7Ak2L7QboNuoT2F";

fn load(sig: &str) -> callout_board::decode::DecodedTx {
    let reg = Registry::load(&idl_dir()).unwrap();
    let p = format!("{}/fixtures/tx/{sig}.json", env!("CARGO_MANIFEST_DIR"));
    decode_tx(&serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap(), &reg).unwrap()
}

#[test]
fn the_confirmed_reward_is_read_as_one_payout() {
    // A single payout transfer from the payout wallet (22 Nov 2026 batch).
    let d = load("4E4cPsYyW6Qc9rGFPDKgegE1WnEsn5D7wtwoXWe7TxLNA9XtEYVeW1F5qejzn7iC6FMmDK4EuhsgW5QvurQRyY8u");
    let rows = payout_rows(&d, &[PAYOUT_WALLET]);
    assert_eq!(rows.len(), 1, "{rows:?}");
    let r = &rows[0];
    assert_eq!((r.kind.as_str(), r.counterparty.as_str(), r.mint.as_str(), r.amount), ("payout", CALLER, USDC, 682_179));
    // The recipient's token account was created in the same transaction; its
    // owner still resolves to the wallet, not the account.
    assert_eq!(r.counterparty_account, "3SmALaNnr6vFG8NfeWej9oYrkbbgk8QGWtw6rKkVzftJ");
}

#[test]
fn trades_are_not_payouts() {
    let d = load("3J5U6DEfdv36M5mEWJc1KbxFE1dK4eACuPcKWz9PCvERc2i1FZ4HmhD4H3JxWBMtpNbvGYccxo5hoeGQ2m64QDE6");
    assert!(payout_rows(&d, &[PAYOUT_WALLET]).is_empty());
}

fn row(t: i64, who: &str, amount: u64) -> PayoutRow {
    PayoutRow {
        slot: t as u64, tx_index: None, block_time: Some(t), signature: format!("s{t}{who}"), ix_path: "0".into(),
        payout_wallet: PAYOUT_WALLET.into(), kind: "payout".into(), counterparty: who.into(),
        counterparty_account: String::new(), mint: USDC.into(), amount,
    }
}

#[test]
fn batches_split_on_gaps_and_sum_correctly() {
    let rows = vec![row(0, "a", 5), row(60, "b", 1), row(120, "a", 4), row(100_000, "c", 7)];
    let b = batches(&rows, USDC, 1800);
    assert_eq!(b.len(), 2);
    assert_eq!((b[0].payouts, b[0].recipients, b[0].total, b[0].max), (3, 2, 10, 5));
    assert_eq!((b[1].payouts, b[1].total), (1, 7));
}

#[test]
fn concentration_and_percentile() {
    let mut even = BTreeMap::new();
    for i in 0..10 { even.insert(format!("w{i}"), 100u64); }
    let c = concentration(&even).unwrap();
    assert!(c.gini.abs() < 1e-9 && (c.top_10_share - 1.0).abs() < 1e-9);

    let mut skew = BTreeMap::new();
    skew.insert("whale".to_string(), 1_000u64);
    for i in 0..9 { skew.insert(format!("w{i}"), 0u64); }
    assert!((concentration(&skew).unwrap().gini - 0.9).abs() < 1e-9); // max for n=10 is (n-1)/n
    assert!((percentile(&skew, 0) - 0.9).abs() < 1e-9);
}

#[test]
fn dedupe_drops_repeated_pages() {
    let mut rows = vec![row(1, "a", 5), row(1, "a", 5), row(2, "b", 3)];
    dedupe(&mut rows);
    assert_eq!(rows.len(), 2);
}

#[test]
fn a_mid_batch_payout_to_another_caller() {
    // 21 Nov batch; RPC balances show the recipient's USDC rising by 53,677,359.
    // The same transaction streamed from Old Faithful gives the same row.
    let d = load("3j95jxZtha2RPHSXhZqwid7zDFmyrncpxboxd5Jb1rg8iLAPsnZA7ny9fFeyC4KSgR6FdwRHtkbgiANJ9dSoN2Q");
    let rows = payout_rows(&d, &[PAYOUT_WALLET]);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((rows[0].kind.as_str(), rows[0].counterparty.as_str(), rows[0].amount),
               ("payout", "CJTxUdpMo5gw5nqn3CZ5EXrkv5KwkdkrZP9ufyTgKtQs", 53_677_359));
}
