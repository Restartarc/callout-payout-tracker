// Callout Payout Tracker. Reads data/summary.json and per-wallet shards
// written by `payouts site`. Amounts arrive in raw USDC units (6 decimals).
// All text goes into the page via textContent, never as HTML.
(() => {
  "use strict";
  const $ = (s) => document.querySelector(s);
  const el = (tag, props = {}, ...kids) => {
    const n = document.createElement(tag);
    for (const [k, v] of Object.entries(props)) {
      if (k === "class") n.className = v;
      else if (k === "text") n.textContent = v;
      else n.setAttribute(k, v);
    }
    for (const c of kids) n.append(c);
    return n;
  };
  const usd = (raw, digits) => {
    const v = raw / 1e6;
    const d = digits ?? (v >= 1000 ? 0 : 2);
    return "$" + v.toLocaleString("en-US", { minimumFractionDigits: d, maximumFractionDigits: d });
  };
  const usdShort = (raw) => {
    const v = raw / 1e6;
    if (v >= 1e6) return "$" + (v / 1e6).toFixed(2) + "M";
    if (v >= 1e3) return "$" + (v / 1e3).toFixed(v >= 1e5 ? 0 : 1) + "k";
    return "$" + v.toFixed(2);
  };
  const pct = (x) => (x * 100).toFixed(1) + "%";
  const int = (n) => n.toLocaleString("en-US");
  const day = (t) => new Date(t * 1000).toISOString().slice(0, 10);
  const when = (t) => new Date(t * 1000).toISOString().replace("T", " ").slice(0, 16) + " UTC";
  const short = (w) => w.slice(0, 4) + "…" + w.slice(-4);
  const BASE58 = /^[1-9A-HJ-NP-Za-km-z]{32,44}$/;
  const shardOf = (w) => Array.from(w.slice(0, 2)).map((c) => c.charCodeAt(0).toString(16).padStart(2, "0")).join("");
  const solscanAcct = (w) => "https://solscan.io/account/" + encodeURIComponent(w);
  const solscanTx = (s) => "https://solscan.io/tx/" + encodeURIComponent(s);

  let summary = null;

  function card(k, v, n) {
    return el("div", { class: "card" }, el("div", { class: "k", text: k }), el("div", { class: "v", text: v }), el("div", { class: "n", text: n || "" }));
  }

  function renderCoverage(s) {
    const c = s.coverage;
    const p = $("#coverage");
    p.textContent = `Tracking ${int(c.payouts)} payouts to ${int(c.wallets)} wallets, ${day(c.from)} to ${day(c.to)}. `;
    if (!c.history_complete) {
      p.append(el("span", { class: "warn", text: "Earlier history is still loading, so all-time figures only cover the days shown." }));
    }
    $("#generated").textContent = "Updated " + when(s.generated_at);
    const ev = $("#evidence");
    for (const w of s.payout_wallets) {
      const a = el("a", { href: solscanAcct(w.wallet), rel: "noopener", target: "_blank" }, el("code", { text: w.wallet }));
      ev.append(el("li", {}, a, document.createTextNode(" — " + w.evidence)));
    }
  }

  function renderLatest(s) {
    const L = s.latest;
    if (!L) return;
    const b = L.batch, c = L.concentration;
    $("#latest-date").textContent = b.date;
    if (L.in_progress) $("#latest-date").append(el("span", { class: "tag", text: "still arriving" }));
    $("#cards").replaceChildren(
      card("Total paid", usdShort(b.total), usd(b.total, 2) + " USDC"),
      card("Wallets paid", int(b.wallets), int(b.payouts) + " transfers"),
      card("Median payout", usd(b.median, 2), "half got less than this"),
      card("Largest payout", usdShort(b.max), usd(b.max, 2)),
      card("Top 1% of wallets got", c ? pct(c.top_1pct_share) : "–", c ? "Gini " + c.gini.toFixed(2) + " (1 = all to one wallet)" : "")
    );
  }

  function renderChart(s) {
    const bs = s.batches;
    const W = 1000, H = 200, padL = 46, padB = 26, padT = 10;
    const max = Math.max(...bs.map((b) => b.total), 1);
    const ns = "http://www.w3.org/2000/svg";
    const svg = document.createElementNS(ns, "svg");
    svg.setAttribute("viewBox", `0 0 ${W} ${H}`);
    svg.setAttribute("preserveAspectRatio", "none");
    const mk = (tag, attrs, text) => {
      const n = document.createElementNS(ns, tag);
      for (const [k, v] of Object.entries(attrs)) n.setAttribute(k, v);
      if (text) n.textContent = text;
      return n;
    };
    for (const f of [0, 0.5, 1]) {
      const y = padT + (H - padT - padB) * (1 - f);
      svg.append(mk("text", { x: 0, y: y + 4 }, usdShort(max * f)));
    }
    const n = bs.length, slot = (W - padL) / Math.max(n, 1), bw = Math.max(Math.min(slot * 0.7, 60), 2);
    const arriving = !!(s.latest && s.latest.in_progress);
    const note = (i) => i === 0 && !s.coverage.history_complete ? "partial: history still loading"
      : i === n - 1 && arriving ? "still arriving: the batch was running at the last check" : "";
    bs.forEach((b, i) => {
      const h = (H - padT - padB) * (b.total / max);
      const x = padL + i * slot + (slot - bw) / 2;
      const why = note(i);
      const r = mk("rect", { class: "bar" + (why ? " partial" : ""), x, y: H - padB - h, width: bw, height: Math.max(h, 1), rx: 3 });
      r.append(mk("title", {}, `${b.date}: ${usd(b.total, 2)} to ${int(b.wallets)} wallets${why ? " (" + why + ")" : ""}`));
      svg.append(r);
      if (n <= 14 || i % Math.ceil(n / 14) === 0) svg.append(mk("text", { x: x + bw / 2, y: H - 8, "text-anchor": "middle" }, b.date.slice(5)));
    });
    $("#chart").replaceChildren(svg);

    const tb = $("#batches tbody");
    tb.replaceChildren(...bs.slice().reverse().map((b, i, arr) => {
      const d = el("td", { text: b.date });
      if (i === arr.length - 1 && !s.coverage.history_complete) d.append(el("span", { class: "tag", text: "partial" }));
      else if (i === 0 && arriving) d.append(el("span", { class: "tag", text: "still arriving" }));
      return el("tr", {}, d, el("td", { text: int(b.wallets) }), el("td", { text: usd(b.total, 0) }),
        el("td", { text: usd(b.median, 2) }), el("td", { text: usd(b.p90, 2) }), el("td", { text: usd(b.max, 0) }));
    }));
  }

  function renderBands(s) {
    const L = s.latest;
    if (!L) return;
    const total = L.bands.reduce((a, b) => a + b.total, 0) || 1;
    const maxCount = Math.max(...L.bands.map((b) => b.payouts), 1);
    const label = (b) => b.to === null ? `$${int(b.from)}+` : `$${int(b.from)}–${int(b.to)}`;
    const wrap = el("div", { class: "bands" });
    for (const b of L.bands) {
      const fill = el("div", { class: "fill" });
      fill.style.width = (100 * b.payouts / maxCount).toFixed(1) + "%";
      const val = el("div", { class: "val" }, el("b", { text: int(b.payouts) + " wallets" }), document.createTextNode(" · " + pct(b.total / total) + " of the money"));
      wrap.append(el("div", { class: "band" }, el("div", { class: "lbl", text: label(b) }), el("div", { class: "track" }, fill), val));
    }
    $("#bands").replaceChildren(wrap);
  }

  function renderTop(which) {
    const s = summary;
    const src = which === "all" ? s.all_time : s.latest;
    $("#tab-latest").setAttribute("aria-selected", String(which !== "all"));
    $("#tab-all").setAttribute("aria-selected", String(which === "all"));
    const c = src && src.concentration;
    $("#conc").textContent = c
      ? `${int(c.recipients)} wallets. Top 10 wallets got ${pct(c.top_10_share)}, top 1% got ${pct(c.top_1pct_share)}, top 10% got ${pct(c.top_10pct_share)}.`
      : "";
    const rows = (src ? src.top : []).map((r, i) => el("tr", {},
      el("td", { text: String(i + 1) }),
      el("td", {}, el("a", { class: "addr", href: solscanAcct(r.wallet), target: "_blank", rel: "noopener", title: r.wallet, text: short(r.wallet) })),
      el("td", { text: usd(r.total, 2) }),
      el("td", { text: int(r.payouts) })));
    $("#top-table tbody").replaceChildren(...rows);
  }

  async function lookup(addr) {
    const out = $("#lookup-result");
    if (!BASE58.test(addr)) {
      out.replaceChildren(el("div", { class: "notice", text: "That doesn't look like a Solana wallet address. Paste the full address (32–44 characters)." }));
      return;
    }
    out.replaceChildren(el("div", { class: "notice", text: "Checking…" }));
    let shard = {};
    try {
      const r = await fetch("data/w/" + shardOf(addr) + ".json", { cache: "no-cache" });
      if (r.ok) shard = await r.json();
    } catch (_) { /* treated as not found */ }
    const w = shard[addr];
    if (!w) {
      out.replaceChildren(el("div", { class: "notice", text: `No callout payouts to this wallet between ${day(summary.coverage.from)} and ${day(summary.coverage.to)}.` }));
      return;
    }
    const head = el("div", { class: "result-head" },
      card("Total received", usd(w.total, 2), "USDC"),
      card("Payouts", int(w.payouts), ""),
      card("Rank", "#" + int(w.rank), "of " + int(w.of) + " paid wallets"),
      card("Paid at least as much as", pct(w.percentile), "of paid wallets"));
    const rows = w.history.slice().reverse().map(([t, amt, sig]) => el("tr", {},
      el("td", { text: t ? when(t) : "–" }), el("td", { text: usd(amt, 2) }),
      el("td", {}, el("a", { href: solscanTx(sig), target: "_blank", rel: "noopener", text: "View transaction" }))));
    const table = el("table", { class: "num-table" },
      el("thead", {}, el("tr", {}, el("th", { scope: "col", text: "When" }), el("th", { scope: "col", text: "Amount" }), el("th", { scope: "col", text: "Proof" }))),
      el("tbody", {}, ...rows));
    out.replaceChildren(head, el("div", { class: "table-wrap" }, table));
  }

  async function main() {
    try {
      const r = await fetch("data/summary.json", { cache: "no-cache" });
      if (!r.ok) throw new Error("HTTP " + r.status);
      summary = await r.json();
    } catch (e) {
      $("#coverage").textContent = "Couldn't load payout data. Please try again shortly.";
      return;
    }
    renderCoverage(summary);
    renderLatest(summary);
    renderChart(summary);
    renderBands(summary);
    renderTop("latest");
    $("#tab-latest").addEventListener("click", () => renderTop("latest"));
    $("#tab-all").addEventListener("click", () => renderTop("all"));
    $("#lookup-form").addEventListener("submit", (e) => {
      e.preventDefault();
      const a = $("#addr").value.trim();
      history.replaceState(null, "", "#lookup=" + encodeURIComponent(a));
      lookup(a);
    });
    // Shareable links: #lookup=<address>, on load and when the hash changes.
    const fromHash = () => {
      const m = location.hash.match(/^#lookup=(.+)$/);
      if (!m) return;
      const a = decodeURIComponent(m[1]);
      $("#addr").value = a;
      lookup(a);
      $("#lookup").scrollIntoView();
    };
    window.addEventListener("hashchange", fromHash);
    fromHash();
  }
  main();
})();
