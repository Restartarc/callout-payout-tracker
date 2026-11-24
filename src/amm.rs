//! PumpSwap quote math, used for liquidation value of open positions and for
//! replaying a simulated follower trade against real reserves.
//!
//! PumpSwap prices against `pool quote reserves + virtual_quote_reserves`
//! (a field in Buy/SellEvent), not the pool balance alone. Both directions
//! below reproduce real trades to the unit (tests/fixtures.rs), but each is
//! checked against one trade so far; widen the test set before relying on the
//! rounding at the edges.

/// Fee for a basis-point rate, rounded up.
pub fn fee(amount: u64, bps: u64) -> u64 {
    ((amount as u128 * bps as u128 + 9_999) / 10_000) as u64
}

/// Gross quote out for selling `base_in` (floor). `quote_eff` = pool + virtual.
pub fn sell_quote_out(base_in: u64, pool_base: u64, quote_eff: u128) -> u64 {
    (quote_eff * base_in as u128 / (pool_base as u128 + base_in as u128)) as u64
}

/// What the seller receives: gross out minus each fee taken from the output.
pub fn sell_user_out(base_in: u64, pool_base: u64, quote_eff: u128, fee_bps: &[u64]) -> u64 {
    let gross = sell_quote_out(base_in, pool_base, quote_eff);
    gross - fee_bps.iter().map(|b| fee(gross, *b)).sum::<u64>()
}

/// Quote the pool takes, before fees, for exactly `base_out`: ceil(...) + 1,
/// as observed on-chain (one data point: 3J5U6DEf).
pub fn buy_quote_net(base_out: u64, pool_base: u64, quote_eff: u128) -> u64 {
    let num = quote_eff * base_out as u128;
    let den = pool_base as u128 - base_out as u128;
    (num.div_ceil(den) + 1) as u64
}

/// What the buyer pays in total: net quote plus each fee charged on top of it.
pub fn buy_user_in(base_out: u64, pool_base: u64, quote_eff: u128, fee_bps: &[u64]) -> u64 {
    let net = buy_quote_net(base_out, pool_base, quote_eff);
    net + fee_bps.iter().map(|b| fee(net, *b)).sum::<u64>()
}
