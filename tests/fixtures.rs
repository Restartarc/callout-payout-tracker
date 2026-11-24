//! Every expected number here was read off the raw transaction (transfers and
//! balance snapshots), not from the decoder, so these tests check the decoder
//! against the chain.

use callout_board::amm;
use callout_board::decode::{decode_tx, DecodedTx, WSOL};
use callout_board::idl::Registry;
use callout_board::idl_dir;
use callout_board::ledger::{build_position, USDC};

const CALLER: &str = "8itpCZ2haKvGpgBmTp1dzHndkJjxK7Ak2L7QboNuoT2F";
const COIN: &str = "2kWx47CUrcXtzptFNnbSXKq3vo1ebqyLBhRytKRGpump";
const SPONSOR: &str = "FHpcNSe6tb2n15bAdq4BkeYWGyZKFD7yLYrH92ng7wCT";
const APP_BUY: &str = "3J5U6DEfdv36M5mEWJc1KbxFE1dK4eACuPcKWz9PCvERc2i1FZ4HmhD4H3JxWBMtpNbvGYccxo5hoeGQ2m64QDE6";
const APP_SELL: &str = "E6rSqt839HD9DZsxgjpy8SNgY4e4wpdzZTLGnU4WKAAGbYWagSQD1Hdch26SsNaD4ZbJmXJSf1NQ12oAg19pZsZ";
const EX1: &str = "2EGN8M4iLEqJGTWEkEGnb98UF9gMGpUYJGNFzLP37kXreTwsaNoLYTxQhJ2qqaT2Pj5JXUCdxeKrezHoyVrC5PmG";
const EX2: &str = "63a68W3cgvVWoNrkH48QXRejidH6XpjdGSnwsr3hZbyx661yhJM139c1rAzrCrv6ZJXPgKQGJLLHnEssuHiuCZ57";
const EX3: &str = "3RKPnihcwi4QSvVzJYJE3VyKjx9c79cfML95HtYmtQKFJ7V6mDcm7mvtAKLHkxUYVrHRw87dN5f4FzfTinxGJoNV";

fn load(sig: &str) -> DecodedTx {
    let reg = Registry::load(&idl_dir()).unwrap();
    let path = format!("{}/fixtures/tx/{sig}.json", env!("CARGO_MANIFEST_DIR"));
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    decode_tx(&v, &reg).unwrap()
}

#[test]
fn every_fixture_has_exactly_one_trade_and_no_referrer_field() {
    for sig in [EX1, EX2, EX3, APP_BUY, APP_SELL] {
        let t = load(sig);
        assert_eq!(t.trades.len(), 1, "{sig}");
        let ev = t.trades[0].event.as_object().unwrap();
        assert!(!ev.keys().any(|k| k.contains("referr") || k.contains("callout")), "{sig}");
    }
}

#[test]
fn bonding_curve_buy_user_cost_matches_wallet_balance() {
    // EX1: wallet paid 68,683,901 lamports = trade + 3 router/tip transfers + 9,000 fee.
    let t = load(EX1);
    let tr = &t.trades[0];
    assert_eq!((tr.ix_name.as_str(), tr.is_buy, tr.quote_mint.as_str()), ("buy_exact_quote_in_v2", true, WSOL));
    assert_eq!(tr.user_quote_amount, Some(67_545_917 + 641_687 + 202_638));
    let extras = 102_998 + 171_661 + 10_000;
    assert_eq!(t.wallet_flows[&tr.user].sol_lamports, -((tr.user_quote_amount.unwrap() + extras + t.fee_lamports) as i128));

    // EX2: 2,525,000 = 2,475,000 trade + 18,655 + 25,000 + 6,345 fee.
    let t = load(EX2);
    let tr = &t.trades[0];
    assert_eq!(tr.user_quote_amount, Some(2_475_000));
    assert_eq!(t.wallet_flows[&tr.user].sol_lamports, -(2_475_000 + 18_655 + 25_000 + 6_345));
}

#[test]
fn pumpswap_user_amounts_match_transfers() {
    // Buy: user's wSOL went 7,044,421 to the pool + 1,741 buyback.
    let b = load(APP_BUY);
    let tr = &b.trades[0];
    assert_eq!((tr.ix_name.as_str(), tr.user.as_str(), tr.mint.as_str()), ("buy_exact_quote_in_v2", CALLER, COIN));
    assert_eq!(tr.user_quote_amount, Some(7_044_421 + 1_741));
    // Sell: pool quote account lost 6,880,796, of which 1,741 went to buyback.
    let s = load(APP_SELL);
    let tr = &s.trades[0];
    assert_eq!(tr.user_quote_amount, Some(6_880_796 - 1_741));
    // Fees the user bears on a PumpSwap sell are lp + protocol + coin creator;
    // buyback_fee and holder_rewards are allocations inside those, not extra.
    let f = &tr.fees;
    assert_eq!(tr.pool_quote_amount - tr.user_quote_amount.unwrap(), f["lp_fee"] + f["protocol_fee"] + f["coin_creator_fee"]);
}

#[test]
fn deployed_events_are_ahead_of_the_published_idl() {
    for sig in [EX1, APP_SELL] {
        assert!(load(sig).warnings.iter().any(|w| w.contains("past the last IDL field")), "{sig}");
    }
}

#[test]
fn app_fingerprint_features() {
    let b = load(APP_BUY);
    assert_eq!((b.fee_payer.as_str(), b.features.signer_count), (SPONSOR, 2));
    assert!(b.features.has_jitodontfront);
    assert_eq!(b.features.memo_lengths, vec![96]);
    let s = load(APP_SELL);
    assert!(s.features.memo_lengths.is_empty(), "sell carries no memo");
}

#[test]
fn caller_round_trip_pnl() {
    let p = build_position(CALLER, COIN, &[load(APP_SELL), load(APP_BUY)]);
    assert_eq!(p.qty, 0);
    assert_eq!(p.realized["USDC"], -339_690); // 0.715894 in, 0.376204 out
    assert_eq!(p.realized["SOL"], 2_976_880); // two wSOL rent refunds, = wallet's SOL balance
    assert_eq!(p.rent_locked, 1_513_840); // still in the empty coin token account
    assert!(p.cost.is_empty());
    assert_eq!(p.fills[0].signature, APP_BUY, "fills are ordered by slot");
    assert_eq!(p.fills[0].sponsor_reimbursement["USDC"], 215_894);
    assert_eq!(p.fills[1].sponsor_reimbursement["USDC"], 107_822);
    let _ = USDC;
}

fn eff_quote(e: &serde_json::Value) -> u128 {
    e["pool_quote_token_reserves"].as_u64().unwrap() as u128
        + e["virtual_quote_reserves"].as_str().unwrap().parse::<u128>().unwrap()
}

#[test]
fn pumpswap_math_matches_the_real_sell() {
    let s = load(APP_SELL);
    let e = &s.trades[0].event;
    let g = |k: &str| e[k].as_u64().unwrap();
    let fees = [g("lp_fee_basis_points"), g("protocol_fee_basis_points"), g("coin_creator_fee_basis_points")];
    // Pool balance alone gives 5,959,488; virtual reserves are required.
    assert_eq!(amm::sell_quote_out(g("base_amount_in"), g("pool_base_token_reserves"), g("pool_quote_token_reserves") as u128), 5_959_488);
    assert_eq!(amm::sell_quote_out(g("base_amount_in"), g("pool_base_token_reserves"), eff_quote(e)), g("quote_amount_out"));
    assert_eq!(amm::sell_user_out(g("base_amount_in"), g("pool_base_token_reserves"), eff_quote(e), &fees), g("user_quote_amount_out"));
}

#[test]
fn pumpswap_math_matches_the_real_buy() {
    let b = load(APP_BUY);
    let e = &b.trades[0].event;
    let g = |k: &str| e[k].as_u64().unwrap();
    let fees = [g("lp_fee_basis_points"), g("protocol_fee_basis_points"), g("coin_creator_fee_basis_points")];
    assert_eq!(amm::buy_quote_net(g("base_amount_out"), g("pool_base_token_reserves"), eff_quote(e)), g("user_quote_amount_in"));
    assert_eq!(amm::buy_user_in(g("base_amount_out"), g("pool_base_token_reserves"), eff_quote(e), &fees), g("quote_amount_in"));
}

#[test]
fn bot_buy_with_unnamed_remaining_accounts_still_decodes() {
    let t = load(EX3);
    assert_eq!(t.trades[0].ix_name, "buy");
    assert_ne!(t.fee_payer, t.trades[0].user, "bot pays the fee for the user");
}

// Multi-hop swaps found by the Old Faithful volume run. PumpSwap routes the
// intermediate coin pool to pool, so the user never holds it.
const MULTI_HOP_AMM: &str = "2YR17J57T1FaT3BQ9HyKiMDr7csh4bR7vdENx4S3aEb1c845STTz3QhvnvzZonSsEqnhRastRQbgDZnaQnHsS9r1";
const MULTI_HOP_CURVE: &str = "4X5agXnQbV1VAM17uyTPxavptucDEkcKPVKfk2NziewLzTpHMfLzhZ1zeKTM76JRBDVkYMG22JEadiN7TtqGm5NY";

#[test]
fn multi_hop_swaps_yield_one_trade_per_leg_with_the_right_coins() {
    // SOL -> 4Cxj (routing) -> E4XU. Raw transfers: pool 1 sent 347,347,414 of
    // 4Cxj straight to pool 2; pool 2 sent 116,003,205,346 E4XU to the user.
    let t = load(MULTI_HOP_AMM);
    assert!(t.warnings.iter().all(|w| w.contains("past the last IDL field")), "{:?}", t.warnings);
    assert_eq!(t.trades.len(), 2);
    let (a, b) = (&t.trades[0], &t.trades[1]);
    assert_eq!((a.mint.as_str(), a.quote_mint.as_str(), a.base_amount, a.routing_hop),
               ("4CxjZyMNoF1p6eXHXHv1TWpS2X47CuUmLwtGe18spump", WSOL, 347_347_414, true));
    assert_eq!((b.mint.as_str(), b.quote_mint.as_str(), b.base_amount, b.routing_hop),
               ("E4XUKh5vPSvkV6MyWYkjXqgzan5xj9CtprMwEEBhpump", "4CxjZyMNoF1p6eXHXHv1TWpS2X47CuUmLwtGe18spump", 116_003_205_346, false));
    let user = "1krnKHh9AqrLsA9jRusuD1AUJhzTu3cmD7TvwtroKAJ";
    assert_eq!(t.wallet_flows[user].tokens.get(&b.mint).copied(), Some(116_003_205_346));
    assert!(t.wallet_flows[user].tokens.get(&a.mint).is_none(), "user never held the routing coin");

    // PumpSwap leg into a bonding-curve leg: 3NZ9 -> 2G5b (routing) -> Cige.
    let t = load(MULTI_HOP_CURVE);
    assert_eq!(t.trades.len(), 2);
    let (a, b) = (&t.trades[0], &t.trades[1]);
    assert!(a.routing_hop && !b.routing_hop);
    assert_eq!(a.mint, b.quote_mint, "each leg's coin is the next leg's quote");
    assert_eq!(b.mint, "CigeTu4MqHk74zAgqvnsBZUXxgaNzQc74zDbBihtpump");
    assert_eq!(t.wallet_flows["8JZxnTpiGDPLZ9uscLkiiCdg6hoy6fLxxyDesmWU1P8U"].tokens.get(&b.mint).copied(), Some(b.base_amount as i128));
}
