-- Chain data. Column names and types match the row structs in src/rows.rs,
-- so the backfill's files load as-is:
--
--   zstd -dc slots-A-B.trades.jsonl.zst | clickhouse-client \
--     --query "INSERT INTO trades FORMAT JSONEachRow"
--
-- (same for txs, flows, transfers). Amounts are raw integer units.
-- Rows are keyed by block position (slot, tx_index); only `txs` holds the
-- signature. Child rows carry a signature only when the index is unknown
-- (data from RPC getTransaction); backfill and live-stream rows never do.
-- ReplacingMergeTree drops exact re-loads of the same rows.
-- Not yet run against a server (no ClickHouse on the build machine).

CREATE TABLE txs (
    slot                UInt64,
    tx_index            Nullable(UInt32),
    block_time          Nullable(Int64),            -- unix seconds
    signature           String,
    failed              Bool,
    fee_lamports        UInt64,
    fee_payer           String,
    signers             Array(String),
    top_level_programs  Array(LowCardinality(String)),
    has_jitodontfront   Bool,
    compute_budget_ixs  UInt32,
    memos               Array(String),              -- app buys carry an opaque 72-byte memo
    idl_commit          LowCardinality(String),
    warnings            Array(String),
    ts                  DateTime MATERIALIZED toDateTime(ifNull(block_time, 0))
) ENGINE = ReplacingMergeTree
ORDER BY (slot, ifNull(tx_index, 0), signature)
SETTINGS allow_nullable_key = 1;

CREATE TABLE trades (
    slot                UInt64,
    tx_index            Nullable(UInt32),
    signature           Nullable(String),
    block_time          Nullable(Int64),
    ix_path             String,
    program             LowCardinality(String),     -- pump | pump_amm
    ix_name             LowCardinality(String),
    event_name          LowCardinality(String),
    user                String,
    mint                String,
    quote_mint          LowCardinality(String),
    is_buy              Bool,
    routing_hop         Bool,                       -- multi-hop leg the user never held
    base_amount         UInt64,
    pool_quote_amount   UInt64,
    user_quote_amount   Nullable(UInt64),           -- null where no rule is verified yet
    fees                Map(LowCardinality(String), UInt64),
    fee_bps             Map(LowCardinality(String), UInt64),
    reserves            Map(LowCardinality(String), String),  -- some are u128
    trailing_bytes      UInt32                      -- > 0: program ahead of the IDL
) ENGINE = ReplacingMergeTree
ORDER BY (mint, slot, ifNull(tx_index, 0), ix_path)
SETTINGS allow_nullable_key = 1;

-- All-in balance changes, only for wallets that signed or traded.
CREATE TABLE flows (
    slot                UInt64,
    tx_index            Nullable(UInt32),
    signature           Nullable(String),
    block_time          Nullable(Int64),
    wallet              String,
    sol_lamports        Int128,
    tokens              Map(String, Int128),
    token_account_rent_lamports Int128
) ENGINE = ReplacingMergeTree
ORDER BY (wallet, slot, ifNull(tx_index, 0))
SETTINGS allow_nullable_key = 1;

-- Only transfers the decoder tags "other": tips, router fees, gas paid back
-- to a sponsor, refunds. Swap legs and self-wraps are in trades and flows.
CREATE TABLE transfers (
    slot                UInt64,
    tx_index            Nullable(UInt32),
    signature           Nullable(String),
    ix_path             String,
    mint                LowCardinality(String),     -- 'SOL' for system transfers
    from_owner          String,
    to_owner            String,
    to_account          String,
    amount              UInt64,
    kind                LowCardinality(String)
) ENGINE = ReplacingMergeTree
ORDER BY (from_owner, slot, ifNull(tx_index, 0), ix_path)
SETTINGS allow_nullable_key = 1;

-- Callout reward payouts (src/payouts.rs PayoutRow). Written by the payout
-- ingester (`payouts ingest`, over RPC) and by the backfill (`payouts` file).
-- kind = 'payout' (the payout wallet sent it) or 'funding' (it received it).
-- Payout wallets are listed in src/payouts.rs with the evidence for each.
CREATE TABLE payouts (
    slot                 UInt64,
    tx_index             Nullable(UInt32),
    block_time           Nullable(Int64),
    signature            String,
    ix_path              String,
    payout_wallet        LowCardinality(String),
    kind                 LowCardinality(String),
    counterparty         String,
    counterparty_account String,
    mint                 LowCardinality(String),
    amount               UInt64,
    ts                   DateTime MATERIALIZED toDateTime(ifNull(block_time, 0))
) ENGINE = ReplacingMergeTree
ORDER BY (counterparty, slot, signature, ix_path);
