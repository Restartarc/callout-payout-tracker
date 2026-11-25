# callout-board

Core of the pump.fun callout leaderboard: decoding from the official IDLs, and
all-in follower PnL.

## Built so far

- `src/idl.rs`: loads the official IDLs (pinned commit in `idl/`) and decodes instructions and events with a Borsh reader driven by the IDL types. Fields an older event lacks come back as null; bytes past the IDL are counted, not dropped.
- `src/decode.rs`: one RPC transaction (`jsonParsed`) becomes trades, per-wallet balance flows, transfers, memos and app-fingerprint features. Handles self-CPI events and log-only events, direct calls and router CPIs.
- `src/ledger.rs`: in-order average-cost PnL per wallet and coin, per currency. Cash comes from the wallet's real balance changes, so router fees, tips, gas paid back to a sponsor and rent are all counted.
- `src/amm.rs`: PumpSwap buy and sell math, including virtual quote reserves.
- `schema/`: ClickHouse (chain data) and Postgres (entities, attribution, scores, disputes).

```bash
cargo test
```

```bash
cargo run -q -- decode fixtures/tx/*.json
```

```bash
cargo run -q -- position <wallet> <mint> fixtures/tx/*.json
```

## Backfill from Old Faithful (`backfill/`)

This streams the free Old Faithful archive (`files.old-faithful.net`, one CAR file per epoch) through Anza's [Jetstreamer](https://github.com/anza-xyz/jetstreamer) firehose. It keeps every transaction that touches the pump, PumpSwap or pump-fees programs, including ones that only reach them through a lookup table. Each one is re-encoded with the official Solana crates into the same `jsonParsed` shape RPC returns, plus `blockTime` and `transactionIndex`. The core decoder decodes each one, and it is written as **compact rows**: one zstd JSON-lines file per table (`txs`, `trades`, `flows`, `transfers`), matching `schema/clickhouse.sql` column for column. They load with `INSERT ... FORMAT JSONEachRow`.

What the rows keep (`src/rows.rs`):
- Every transaction, failed ones included, since followers pay fees on failures.
- Trades with their fees, fee rates and pool reserves, which are needed for liquidation value and replay. The rest of each event can be looked up on Solscan.
- Balance flows only for wallets that signed or traded.
- Only "other" transfers: tips, router fees, gas paid back to a sponsor, refunds. Swap legs and self-wraps are already in trades and flows.
- The signature once, in `txs`. The other tables key on `(slot, tx_index)`.

`--raw` also keeps the full transactions. Anything the decoder rejects is always kept in full, in a `decode-failures` file.

```bash
. ./build-env.sh && cargo build --release -p backfill
```

```bash
backfill --epoch 1093 --threads 8 --out backfill-out
```

```bash
backfill --slots 472482504:472482505 --threads 1 --out backfill-out
```

```bash
backfill --slots 472482504:472482505 --threads 1 --out backfill-out --raw
```

```bash
backfill check backfill-out/slots-472482504-472482505.raw.jsonl.zst fixtures/tx
```

```bash
backfill compact <earlier-raw.jsonl.zst> --out backfill-out
```

```bash
backfill position <wallet> <mint> backfill-out
```

`position` rebuilds a ledger position straight from the row files. It's the end-to-end check: the caller's two app transactions, streamed fresh, reproduce the round trip exactly (−339,690 raw USDC, +2,976,880 lamports, 1,513,840 lamports of rent locked).

`check` decodes every backfilled transaction that also has an RPC fixture, then compares trades, balance flows, transfers, memos, fees and warnings. All 5 fixtures match, and they now carry their in-block index too.

Building on macOS needed three things, all set in `build-env.sh` or the lockfile:
- RocksDB's bindgen step loads `libclang.dylib` at runtime, so it is pointed at the Command Line Tools copy.
- `protobuf-src` fails when the path contains a space ("Application Support"), so builds go to a target directory without spaces.
- `Cargo.lock` is seeded from Jetstreamer's own lockfile, which pins Agave 4.2.2. Cargo's default pick, Agave 4.4.1, changed APIs that Jetstreamer 0.8.1 calls. Don't run a bare `cargo update` on the backfill crate.

The firehose retries a slot after a network timeout, so the writer drops repeated `(slot, index)` pairs and reports how many it dropped.

`backfill inspect <file>` re-decodes a backfill file and tallies decoder warnings, split into failed and successful transactions.

### Measured cost

Measured on 2,000 slots of epoch 1093 (472300000–472302000), on this Mac's home connection, with 8 threads:

| | Value |
|---|---|
| Wall time | 516 s (3.9 slots/s, about 2.7 MB/s, 22 Mbit/s, from the archive) |
| Transactions streamed | 890,053 |
| Touching pump programs | 556,237 (62%) |
| ...of which failed | 329,011 (59%) |
| Trades decoded | 101,984 from 227,226 successful transactions, with no unpaired events |
| Output, full JSON (`--raw`) | 418 MB zstd (8.5 GB of JSON) |
| Output, rows (default) | **73.5 MB** zstd: txs 46.1, transfers 9.5, trades 10.1, flows 7.9. Signatures, which can't be compressed, are most of `txs` |
| Firehose timeouts | 60, all retried; 0 duplicates; every transaction got its block time |

The callout window starts at **epoch 1016** (13 Aug 2026, 00:09 UTC) and runs to 1093, the latest published: **78 epochs, 50.5 TB** of archive. Every byte has to be streamed, since the archive has no server-side filter. Approximate wall time:
- this connection: about 215 days, which isn't practical
- 1 Gbit/s: about 5 days
- 10 Gbit/s: under a day, if CPU keeps up

The archive is hosted in Amsterdam, so run it on a server close to there.

Storage for the whole window as rows is about **1.24 TB** (16 GB per epoch), against about 7 TB as full JSON. 59% of `txs` rows are failed transactions. Dropping their signatures would save roughly another 0.5 TB, at the cost of not being able to link failed attempts to Solscan.

## The live site: Callout Payout Tracker (`site/`)

A static website showing every pump.fun callout payout:
- the latest batch's totals;
- paid per day;
- how big the payouts are;
- the most-paid wallets, for the latest batch and for all tracked days;
- a wallet lookup with a Solscan link for every payout. Lookups can be shared as `#lookup=<address>` links.

It also explains what the data can't show: whether pay is fair, which callout earned a payout, or who owns a wallet.

How it updates (`.github/workflows/payouts.yml`):
1. Every hour from 19:10 to 02:10 UTC, GitHub Actions builds `payouts`, so the site picks up the daily batch whenever it starts. A batch caught while it is still running is tagged "still arriving" until the next check completes it.
2. It restores the payout data from the `data` branch, then ingests all new payouts and fills in history with the time left (45 minutes per hourly check, 5 hours in a daily run at 03:30 UTC).
3. It commits the data back to the `data` branch, builds the site data and deploys `site/` to GitHub Pages.
4. History fills in over a few runs; the site says so until it's complete.

### Launch checklist (one-time)

1. **Create a GitHub repository** and push this folder to it. Make it public: Actions minutes are free for public repos, and the data is public anyway.
2. **Regenerate your Helius key** (the old one has been in chat and logs), then add the new endpoint as a repository secret: Settings > Secrets and variables > Actions > New secret. Name it `HELIUS_RPC_URL`, with a value like `https://mainnet.helius-rpc.com/?api-key=...`.
3. **Turn on Pages:** Settings > Pages > Source: **GitHub Actions**.
4. **Run it once now:** Actions > Update payouts > Run workflow. The site appears at `https://<user>.github.io/<repo>/` when the job finishes.
5. Optional: a custom domain, under Settings > Pages.

### Preview locally

```bash
payouts site --out payouts-out --site site
```

```bash
python3 -m http.server 8765 --directory site
```

## Callout payout ledger (`payouts/`)

pump.fun pays callout rewards on-chain as plain USDC transfers from `BNrYNyQZhdGRs8Rvi98WGTjhxab2wFTdGtVjW72RGyh4`. It sends one batch every evening, about 4,000 transfers in 5 to 8 minutes, around 19:15 to 20:50 UTC. This was confirmed by matching a caller's in-app reward to its on-chain transfer, to the cent. Known payout wallets, with their evidence, are in `src/payouts.rs`.

```bash
payouts ingest --rps 2 --out payouts-out
```

```bash
payouts report --out payouts-out
```

```bash
payouts wallet <ADDRESS> --out payouts-out
```

- `ingest` pages back through the payout wallet's history to `--since` (default 2026-08-13). It decodes each transaction with the core decoder and writes `payout` rows (sent by the payout wallet) and `funding` rows (received by it). Each page goes to its own file before progress is saved. A stopped run resumes, and a re-run picks up new payouts first. Set `RPC_URL` to use a faster endpoint.
- `report` gives per-batch totals (count, wallets, total, median, p90, max) and concentration: top 1% and top 10% share, top 10 wallets, and Gini. It also lists who funds the payout wallet.
- `wallet` shows one wallet's payouts, each with a Solscan link, plus its rank and percentile.
- The Old Faithful backfill also watches the payout wallet and writes a `payouts` file, so a full backfill collects payouts at no extra cost. The `payouts` table in `schema/clickhouse.sql` matches the row type.

**Speed:** the public RPC managed about 0.3 to 0.5 transactions/s on `getTransaction`; batching didn't help, because it throttles each request in a batch. One daily batch takes about 2.5 to 4 hours at that rate, and the history since 13 August (an estimated 400k payouts) over 10 days. A keyed endpoint at 10 req/s brings those to about 7 minutes and 11 hours.

**Sample so far:** 180 payouts from the 23 Nov batch. Not representative of a whole day.

**Still open:**
- the daily totals;
- when this wallet began paying, and whether earlier or sibling wallets exist;
- whether perps and prediction-market rewards come from the same wallet.

## Facts the fixtures pin down

Each one is a test in `tests/fixtures.rs`, checked against the raw transfers.

- PumpSwap buys: the user pays `quote_amount_in`. `user_quote_amount_in` is the amount net of fees, despite its name.
- PumpSwap prices against `pool quote + virtual_quote_reserves`. Pool balance alone is 14% off.
- Fees the user bears on PumpSwap: LP, protocol and coin creator. `buyback_fee` and `holder_rewards` are shares of those fees, not extra charges.
- Bonding-curve buys: the user pays `sol_amount + fee + creator_fee`.
- Each event is paired with the instruction that emitted it, by call-stack height. One instruction can emit several events (multi-hop swaps). PumpSwap multi-hop legs route the intermediate coin pool to pool, so those legs are flagged `routing_hop`, and their coins are resolved from the pool's matching transfer.
- Every event carries 8 bytes past the published IDL. The deployed programs are ahead of IDL commit `bc357961`.
- The caller's round trip: −339,690 raw USDC (−0.339690) and +2,976,880 lamports realized, with 1,513,840 lamports of rent still locked.

## Not yet verified or built

- No rule yet for the user-side amount on bonding-curve sells, or when buyback or cashback is nonzero. The field is left null.
- RPC data has no in-block transaction index, so order within a slot is a guess and flagged. Backfill and the live stream both provide it.
- Not built yet: stream ingest (Yellowstone gRPC needs a provider key), the full payout history (needs a faster RPC), loading backfill output into the database, callout capture, attribution, event study, clustering, scoring, caller pages.
- The schemas haven't been run against a server, because no database is installed here.
