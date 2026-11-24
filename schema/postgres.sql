-- Entities, attribution, scores and disputes. Not yet run against a server.

CREATE TABLE callers (
    caller_id        BIGSERIAL PRIMARY KEY,
    wallet           TEXT UNIQUE NOT NULL,   -- the `caller=` value in share links
    handle           TEXT,
    entity_id        BIGINT,                 -- set when clustered with other caller accounts
    first_seen_at    TIMESTAMPTZ NOT NULL
);

CREATE TABLE callouts (
    callout_id       TEXT PRIMARY KEY,       -- the app's id if exposed, else sha256 of the raw record
    caller_id        BIGINT NOT NULL REFERENCES callers,
    mint             TEXT NOT NULL,
    kind             TEXT NOT NULL CHECK (kind IN ('token', 'perp', 'prediction')),  -- only 'token' is scored
    ref_code         TEXT,                   -- `ref=` in share links; off-chain only
    app_created_at   TIMESTAMPTZ,            -- as the app reports it; never used for scoring
    first_seen_at    TIMESTAMPTZ NOT NULL,   -- our capture time; the event-study t0
    deleted_at       TIMESTAMPTZ,            -- kept as a tombstone, still scored
    raw_blob_sha256  TEXT NOT NULL,          -- verbatim captured response in object storage
    version          INT NOT NULL DEFAULT 1
);
CREATE INDEX ON callouts (mint, first_seen_at);

CREATE TABLE follows (
    follower_wallet  TEXT NOT NULL,
    caller_id        BIGINT NOT NULL REFERENCES callers,
    observed_at      TIMESTAMPTZ NOT NULL,
    source           TEXT NOT NULL,
    PRIMARY KEY (follower_wallet, caller_id, observed_at)
);

-- Ground truth from instrumented test accounts (method A).
CREATE TABLE test_trades (
    signature        TEXT PRIMARY KEY,
    test_account     TEXT NOT NULL,
    callout_id       TEXT REFERENCES callouts,   -- null for a control trade without a callout tap
    notified_at      TIMESTAMPTZ,
    tapped_at        TIMESTAMPTZ,
    landed_slot      BIGINT
);

CREATE TABLE app_wallets (
    wallet           TEXT PRIMARY KEY,
    method           TEXT NOT NULL,          -- e.g. 'sponsor_fee_payer'
    confidence       REAL NOT NULL,
    evidence_sigs    TEXT[] NOT NULL
);

CREATE TABLE attributions (
    callout_id       TEXT NOT NULL REFERENCES callouts,
    wallet           TEXT NOT NULL,
    first_buy_sig    TEXT NOT NULL,
    weight           REAL NOT NULL,          -- delay-kernel share when calls overlap
    method           TEXT NOT NULL,
    confidence       REAL NOT NULL,
    ambiguity_n      INT NOT NULL,           -- how many callouts competed for this buy
    PRIMARY KEY (callout_id, wallet)
);

CREATE TABLE clusters (
    entity_id        BIGINT NOT NULL,
    wallet           TEXT NOT NULL,
    signal           TEXT NOT NULL CHECK (signal IN ('declared', 'funding', 'fee_payer', 'same_block', 'sweep')),
    tier             TEXT NOT NULL CHECK (tier IN ('declared', 'strong', 'weak')),
    evidence_sigs    TEXT[] NOT NULL,
    PRIMARY KEY (entity_id, wallet, signal)
);

-- Hubs excluded from clustering: CEX hot wallets, bridges, and gas sponsors
-- such as the app's fee payer, which would otherwise join every app user.
CREATE TABLE cluster_exclusions (
    address          TEXT PRIMARY KEY,
    reason           TEXT NOT NULL,
    evidence_sigs    TEXT[] NOT NULL
);
INSERT INTO cluster_exclusions VALUES
  ('FHpcNSe6tb2n15bAdq4BkeYWGyZKFD7yLYrH92ng7wCT', 'app gas sponsor (fee payer)',
   ARRAY['3J5U6DEfdv36M5mEWJc1KbxFE1dK4eACuPcKWz9PCvERc2i1FZ4HmhD4H3JxWBMtpNbvGYccxo5hoeGQ2m64QDE6',
         'E6rSqt839HD9DZsxgjpy8SNgY4e4wpdzZTLGnU4WKAAGbYWagSQD1Hdch26SsNaD4ZbJmXJSf1NQ12oAg19pZsZ']);

CREATE TABLE methodology_versions (
    version          TEXT PRIMARY KEY,
    idl_commit       TEXT NOT NULL,
    published_at     TIMESTAMPTZ NOT NULL,
    notes            TEXT NOT NULL
);

CREATE TABLE callout_scores (
    callout_id       TEXT NOT NULL REFERENCES callouts,
    methodology      TEXT NOT NULL REFERENCES methodology_versions,
    excess_flow      REAL,
    outcome_gap      REAL,
    y_realized       REAL,
    y_sim_p50        REAL,
    y_sim_p90        REAL,
    exit_into_followers_ratio REAL,
    absorbed_share   REAL,
    frontrun_flag    BOOLEAN,
    PRIMARY KEY (callout_id, methodology)
);

CREATE TABLE caller_scores (
    caller_id        BIGINT NOT NULL REFERENCES callers,
    week             DATE NOT NULL,
    methodology      TEXT NOT NULL REFERENCES methodology_versions,
    n_calls          INT NOT NULL,
    post_mean        REAL,
    post_p10         REAL,                   -- ranking key
    absorbed_share_shrunk REAL,
    rank             INT,                    -- null = provisional (below eligibility)
    PRIMARY KEY (caller_id, week, methodology)
);

CREATE TABLE disputes (
    dispute_id       BIGSERIAL PRIMARY KEY,
    caller_id        BIGINT NOT NULL REFERENCES callers,
    callout_id       TEXT REFERENCES callouts,
    cited_sigs       TEXT[] NOT NULL CHECK (cardinality(cited_sigs) > 0),
    status           TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'accepted', 'rejected')),
    resolution       TEXT,
    resolved_by      TEXT,
    opened_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);
