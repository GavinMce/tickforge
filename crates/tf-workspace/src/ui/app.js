// The workspace overview. Reads /api/overview and draws it; it has no way to change anything.
// Text from the API goes in with textContent only, never as markup.
(function () {
  "use strict";
  var COLORS = ["#2C63B8", "#0E7C6B", "#8A4FB0", "#B26A0B", "#B33A5B", "#4A6B1E"];
  var app = document.getElementById("app");
  var data = null;      // /api/overview
  var selected = null;  // selected group on the overview
  var failed = false;
  var runView = null;   // {strategy, id, runs, detail} for the run screen
  var REASONS = {
    invalid: "the order itself was malformed", kill_switch: "the kill switch was on",
    max_notional: "the order was larger than the order limit", max_position: "it would exceed the position limit",
    daily_loss_limit: "the day's loss limit was reached", order_rate: "too many orders too quickly",
    not_shortable: "the stock cannot be shorted", short_sale_restricted: "short sales were restricted",
    halted: "the stock was halted", outside_luld_band: "the price was outside the limit-up/limit-down band",
    spread_too_wide: "the spread was too wide", run_up_too_large: "the run-up was too large to chase",
    broker: "the broker refused it", opposing_position: "it opposed an existing position",
    gap_risk: "a gap against a short would risk too much", nothing_to_close: "there was nothing to close",
    unknown_instrument: "the instrument is not known", no_budget: "the strategy has no budget",
    strategy_budget: "it would exceed the strategy's budget", group_budget: "it would exceed the group's budget",
    strategy_loss_limit: "the strategy had reached its loss limit", other: "another limit"
  };

  function h(tag, props) {
    var el = document.createElement(tag);
    var kids = Array.prototype.slice.call(arguments, 2);
    Object.keys(props || {}).forEach(function (k) {
      if (k === "class") el.className = props[k];
      else if (k === "on") el.addEventListener("click", props[k]);
      else el.setAttribute(k, props[k]);
    });
    kids.forEach(function (c) {
      if (c === null || c === undefined || c === false) return;
      el.appendChild(typeof c === "string" ? document.createTextNode(c) : c);
    });
    return el;
  }

  // "-1234.5" -> "-$1,234.50". The API sends money as text to the cent.
  function usd(s) {
    var neg = s.charAt(0) === "-";
    var parts = (neg ? s.slice(1) : s).split(".");
    var whole = parts[0].replace(/\B(?=(\d{3})+(?!\d))/g, ",");
    return (neg ? "-" : "") + "$" + whole + "." + (parts[1] || "00");
  }
  function num(s) { return parseFloat(s) || 0; }
  function pct(bp) { return (bp / 100).toFixed(bp % 100 === 0 ? 0 : 2).replace(/(\.\d)0$/, "$1") + "%"; }
  function ratio(a, b) { return b > 0 ? Math.min(1, Math.max(0, a / b)) : 0; }
  function pctOf(a, b) { return b > 0 ? Math.round((a / b) * 100) + "%" : "0%"; }
  function pnlClass(s) { var n = num(s); return n > 0 ? "mono pos" : n < 0 ? "mono neg" : "mono"; }
  function select(id) { selected = id; location.hash = "g=" + encodeURIComponent(id); draw(); }

  function totals(a) {
    var reserved = data.groups.reduce(function (t, g) { return t + num(g.budget); }, 0);
    var used = num(a.used);
    function box(label, value, cls, note) {
      return h("div", { "class": "card" },
        h("div", { "class": "small mute" }, label),
        h("div", { "class": "big " + (cls || "") }, value),
        note ? h("div", { "class": "small mute" }, note) : null);
    }
    return h("section", { "class": "totals", "aria-label": "Totals" },
      box("Total balance", usd(a.balance)),
      box("Day P&L", usd(a.day_pnl), num(a.day_pnl) > 0 ? "pos" : num(a.day_pnl) < 0 ? "neg" : ""),
      box("In use", usd(a.used), "", pctOf(used, reserved) + " of reserved"),
      box("Free inside budgets", usd(String(Math.max(0, reserved - used).toFixed(2)))));
  }

  function allocation() {
    var sum = data.groups.reduce(function (t, g) { return t + g.share_bp; }, 0);
    var segs = data.groups.map(function (g, i) {
      var b = h("button", {
        "class": "seg", "aria-pressed": String(g.id === selected),
        "aria-label": g.id + ", " + pct(g.share_bp) + " of the balance"
      }, h("b", null, g.id), " ", h("span", { "class": "mono" }, pct(g.share_bp)));
      b.style.flex = String(g.share_bp) + " 1 0";
      b.style.background = COLORS[i % COLORS.length];
      b.addEventListener("click", function () { select(g.id); });
      return b;
    });
    if (sum < 10000) {
      var f = h("div", { "class": "seg free", "aria-label": pct(10000 - sum) + " of the balance is not reserved" }, "Not reserved " + pct(10000 - sum));
      f.style.flex = String(10000 - sum) + " 1 0";
      segs.push(f);
    }
    return h("section", { "class": "card alloc", "aria-label": "Allocation" },
      h("div", { "class": "between" }, h("h2", null, "How the balance is split"),
        h("span", { "class": "small mute" }, pct(sum) + " reserved across " + data.groups.length + (data.groups.length === 1 ? " group" : " groups"))),
      h.apply(null, ["div", { "class": "bar" }].concat(segs)));
  }

  function groupCards() {
    return h.apply(null, ["section", { "class": "groups", "aria-label": "Groups" }].concat(data.groups.map(function (g, i) {
      var budget = num(g.budget), used = num(g.used);
      var fill = h("div", { "class": "fill" + (used > budget ? " hot" : "") });
      fill.style.width = (ratio(used, budget) * 100).toFixed(1) + "%";
      var card = h("button", { "class": "card gcard", "aria-pressed": String(g.id === selected) },
        h("div", { "class": "between" },
          h("span", { style: "font-weight:600;font-size:15px" }, h("span", { "class": "dot" }), g.id),
          h("span", { "class": "mono small" }, pct(g.share_bp))),
        h("div", { "class": "mono", style: "font-size:22px;font-weight:500" }, usd(g.budget)),
        h("div", { "class": "track" }, fill),
        h("div", { "class": "between small mute" },
          h("span", null, "In use " + usd(g.used) + " (" + pctOf(used, budget) + ")"),
          h("span", null, "Free " + usd(Math.max(0, budget - used).toFixed(2)))),
        h("div", { "class": "between", style: "border-top:1px solid var(--soft);padding-top:8px" },
          h("span", { "class": "mute" }, "Day P&L"), h("span", { "class": pnlClass(g.day_pnl) }, usd(g.day_pnl))));
      card.querySelector(".dot").style.background = COLORS[i % COLORS.length];
      card.addEventListener("click", function () { select(g.id); });
      return card;
    })));
  }

  function strategies(g, gi) {
    var mine = data.strategies.filter(function (s) { return s.group === g.id; });
    var assigned = mine.reduce(function (t, s) { return t + s.share_bp; }, 0);
    var rows = mine.map(function (s) {
      var budget = num(s.budget), used = num(s.used);
      var fill = h("div", { "class": "fill" + (used > budget ? " hot" : "") });
      fill.style.width = (ratio(used, budget) * 100).toFixed(1) + "%";
      var track = h("div", { "class": "track", style: "height:6px" }, fill);
      return h("a", { "class": "trow link", role: "row", href: "#/s/" + encodeURIComponent(s.name), "aria-label": "Open " + s.name + ", " + s.runs + (s.runs === 1 ? " run" : " runs") },
        h("span", { style: "font-weight:500" }, s.name),
        h("span", { "class": "mono" }, pct(s.share_bp)),
        h("span", { "class": "mono" }, usd(s.budget)),
        h("span", null, track, h("span", { "class": "small mute mono" }, usd(s.used) + " · " + pctOf(used, budget))),
        h("span", { "class": pnlClass(s.day_pnl) }, usd(s.day_pnl)),
        h("span", null, h("span", { "class": "state" + (s.state === "active" ? "" : " bad") }, s.state)),
        h("span", { "class": "small", style: "color:var(--link)" }, s.runs + (s.runs === 1 ? " run ›" : " runs ›")));
    });
    if (assigned < 10000) {
      var gb = num(g.budget) * (10000 - assigned) / 10000;
      rows.push(h("div", { "class": "trow mute" },
        h("span", { style: "font-style:italic" }, "Unassigned"),
        h("span", { "class": "mono" }, pct(10000 - assigned)),
        h("span", { "class": "mono" }, usd(gb.toFixed(2))),
        h("span", { "class": "small", style: "grid-column:span 4" }, "Held back inside the group until a person assigns it.")));
    }
    var dot = h("span", { "class": "dot" });
    dot.style.background = COLORS[gi % COLORS.length];
    var head = h("div", { "class": "thead", role: "row" },
      h("span", null, "Strategy"), h("span", null, "Share"), h("span", null, "Budget"), h("span", null, "In use"),
      h("span", null, "Day P&L"), h("span", null, "State"), h("span", null, "Runs"));
    return h("section", { "class": "card", "aria-label": "Strategies", style: "overflow:hidden" },
      h("div", { "class": "between", style: "padding:14px 16px;border-bottom:1px solid var(--soft);flex-wrap:wrap" },
        h("h2", { style: "font-size:15px" }, dot, g.id + " strategies"),
        h("span", { "class": "small mute" }, "Budget " + usd(g.budget) + " · shares of the group below")),
      h("div", { "class": "table" }, h.apply(null, ["div", { role: "table" }, head].concat(rows))));
  }

  function limits(g) {
    var mine = data.strategies.filter(function (s) { return s.group === g.id; });
    var lines = mine.map(function (s) {
      return h("div", { "class": "kv small" }, h("b", null, s.name),
        h("span", { "class": "mono" }, "stop " + usd(s.loss_soft) + " · flatten " + usd(s.loss_hard)));
    });
    return h("section", { "class": "card panel", "aria-label": "Loss limits" },
      h("h2", null, "Loss limits · " + g.id),
      h("div", { "class": "kv" }, h("span", null, "Stop opening at"), h("span", { "class": "mono" }, pct(g.loss_soft_bp) + " of budget")),
      h("div", { "class": "kv" }, h("span", null, "Flatten the strategy at"), h("span", { "class": "mono" }, pct(g.loss_hard_bp) + " of budget")),
      h.apply(null, ["div", { style: "display:flex;flex-direction:column;gap:6px;border-top:1px solid var(--soft);padding-top:8px" }].concat(lines)),
      h("div", { "class": "small mute" }, "Per strategy, from its own budget. Exits are never blocked."));
  }

  function waitingNote(a) {
    if (!a.requests_waiting) return null;
    return h("div", { "class": "banner", role: "status" }, a.requests_waiting + (a.requests_waiting === 1 ? " budget change request is" : " budget change requests are") +
      " waiting for the engine to record " + (a.requests_waiting === 1 ? "it" : "them") + ". ", h("a", { href: "#/edit", style: "color:inherit" }, "Review"));
  }

  function banner(a) {
    if (!a.scheduled || a.scheduled.length === 0) return null;
    var items = a.scheduled.map(function (t) { return h("li", null, t); });
    return h("div", { "class": "banner", role: "status" },
      h("b", null, "Budget changes are scheduled."),
      " They take effect at the next rebalance, after the session.",
      h.apply(null, ["ul", null].concat(items)));
  }

  function drawOverview() {
    var a = data.account;
    var kids = [];
    var head = h("header", { "class": "between", style: "align-items:flex-end;flex-wrap:wrap" },
      h("div", null,
        h("div", { "class": "row" }, h("span", { "class": "eyebrow" }, "Workspace"),
          a ? h("span", { "class": "chip" }, a.kind + " · " + a.records + " ledger records") : null,
          a && a.killed ? h("span", { "class": "state bad" }, "Kill switch on") : null,
          failed ? h("span", { "class": "state bad" }, "Could not refresh") : null),
        h("h1", null, "Overview")),
      h("div", { "class": "row", style: "justify-content:flex-end" },
        h("div", { "class": "small mute", style: "max-width:380px" }, "Budgets are reserved and rebalance after each session: gains and losses move into the strategy that made them."),
        a && a.budgets ? h("a", { "class": "btn dark", href: "#/edit" }, "Edit budgets") : null));
    kids.push(head);
    if (!a) {
      kids.push(h("div", { "class": "card empty" }, "No ledger is connected to this workspace."));
    } else if (!a.budgets) {
      kids.push(h("div", { "class": "card empty" }, "This account has no budgets set, so there is nothing to split yet."));
    } else {
      var ids = data.groups.map(function (g) { return g.id; });
      if (ids.indexOf(selected) < 0) selected = ids[0];
      var gi = ids.indexOf(selected), g = data.groups[gi];
      kids.push(banner(a), waitingNote(a), totals(a), allocation(),
        h("div", { "class": "split" },
          h("div", { "class": "mainc" }, groupCards(), strategies(g, gi)),
          h("aside", null, limits(g))));
    }
    app.replaceChildren.apply(app, kids.filter(Boolean));
  }

  // ---- the run view -------------------------------------------------------------------------

  function goRun(strategy, id, latest) {
    location.hash = "#/s/" + encodeURIComponent(strategy) + (latest ? "" : "/" + encodeURIComponent(id));
  }

  function chart(curve) {
    var NS = "http://www.w3.org/2000/svg";
    var vals = curve.map(num);
    var lo = Math.min.apply(null, vals.concat([0])), hi = Math.max.apply(null, vals.concat([0]));
    if (hi === lo) { hi += 1; lo -= 1; }
    function y(v) { return (150 - ((v - lo) / (hi - lo)) * 140).toFixed(1); }
    function x(i) { return vals.length === 1 ? 300 : (10 + (i / (vals.length - 1)) * 580).toFixed(1); }
    var svg = document.createElementNS(NS, "svg");
    svg.setAttribute("viewBox", "0 0 600 160");
    svg.setAttribute("preserveAspectRatio", "none");
    svg.setAttribute("role", "img");
    svg.setAttribute("class", "chart");
    svg.setAttribute("aria-label", "Realised profit after each fill, from " + usd(curve[0]) + " to " + usd(curve[curve.length - 1]));
    var zero = document.createElementNS(NS, "line");
    zero.setAttribute("x1", "0"); zero.setAttribute("x2", "600"); zero.setAttribute("y1", y(0)); zero.setAttribute("y2", y(0));
    zero.setAttribute("class", "zero");
    svg.appendChild(zero);
    if (vals.length > 1) {
      var line = document.createElementNS(NS, "polyline");
      line.setAttribute("points", vals.map(function (v, i) { return x(i) + "," + y(v); }).join(" "));
      line.setAttribute("class", "curve");
      svg.appendChild(line);
    }
    // A dot is a zero-length line with round caps: unlike a circle it stays round when the chart is
    // stretched to the width of the screen.
    vals.forEach(function (v, i) {
      var c = document.createElementNS(NS, "line");
      c.setAttribute("x1", x(i)); c.setAttribute("x2", x(i)); c.setAttribute("y1", y(v)); c.setAttribute("y2", y(v));
      c.setAttribute("class", "pt");
      svg.appendChild(c);
    });
    return svg;
  }

  function summaryBox(label, value, note, cls) {
    return h("div", { "class": "card", style: "padding:12px 14px" },
      h("div", { "class": "small mute" }, label),
      h("div", { "class": "mono " + (cls || ""), style: "font-size:20px;font-weight:500;margin-top:2px;overflow-wrap:anywhere" }, value),
      note ? h("div", { "class": "small mute" }, note) : null);
  }

  function noTradeText(d) {
    if (!d.refused || d.refused.length === 0) return "No entry: the strategy's rules found nothing to enter this session.";
    var parts = d.refused.map(function (r) { return (REASONS[r.reason] || REASONS.other) + (r.count > 1 ? " (" + r.count + " times)" : ""); });
    return "No entry. Every order the strategy proposed was refused: " + parts.join("; ") + ".";
  }

  function tradesTable(d) {
    var head = h("div", { "class": "tr6 thead", role: "row" },
      h("span", null, "Time"), h("span", null, "Action"), h("span", null, "Qty"), h("span", null, "Instrument"), h("span", null, "Price"), h("span", null, "P&L"));
    var rows = d.trades.map(function (t) {
      return h("div", { "class": "tr6 mono", role: "row", style: "font-size:13px;border-bottom:1px solid var(--soft)" },
        h("span", null, t.time), h("span", null, t.side + " (" + t.purpose + ")"), h("span", null, String(t.qty)),
        h("span", null, "#" + t.instrument), h("span", null, "$" + t.price),
        h("span", { "class": t.purpose === "close" ? pnlClass(t.pnl) : "mute" }, t.purpose === "close" ? usd(t.pnl) : "—"));
    });
    return h("div", { "class": "table" }, h.apply(null, ["div", { role: "table", style: "min-width:560px" }, head].concat(rows)));
  }

  function drawRun() {
    var rv = runView;
    var st = data && data.strategies ? data.strategies.filter(function (s) { return s.name === rv.strategy; })[0] : null;
    var runs = rv.runs;
    var back = h("a", { "class": "btn", href: "#" }, "‹ Overview");
    var crumbs = h("nav", { "class": "row small mute", "aria-label": "Breadcrumb" }, back,
      h("span", null, "Workspace"), h("span", { "aria-hidden": "true" }, "/"),
      st ? h("span", null, st.group) : null, st ? h("span", { "aria-hidden": "true" }, "/") : null,
      h("b", { style: "color:var(--ink)" }, rv.strategy));
    if (runs.length === 0) {
      app.replaceChildren(crumbs, h("h1", null, rv.strategy), h("div", { "class": "card empty" }, "This strategy has no runs yet."));
      return;
    }
    var idx = 0;
    runs.forEach(function (r, i) { if (r.id === rv.id) idx = i; });
    var run = runs[idx];
    var isLatest = idx === 0;
    function nav(label, to, off) {
      var b = h("button", { "class": "btn", "aria-label": label.label }, label.text);
      if (off) b.disabled = true; else b.addEventListener("click", function () { goRun(rv.strategy, runs[to].id, to === 0); });
      return b;
    }
    var meta = st ? "Budget " + usd(st.budget) + " (" + pct(st.share_bp) + " of " + st.group + ") · " + st.state + " · " + runs.length + (runs.length === 1 ? " run" : " runs")
      : runs.length + (runs.length === 1 ? " run" : " runs");
    var header = h("header", { "class": "between", style: "align-items:flex-end;flex-wrap:wrap" },
      h("div", null, h("h1", null, rv.strategy), h("div", { "class": "small mute" }, meta)),
      h("div", { "class": "row" },
        nav({ text: "‹ Older", label: "Older run" }, idx + 1, idx === runs.length - 1),
        h("span", { "class": "mono small", style: "min-width:96px;text-align:center" }, (idx + 1) + " of " + runs.length + (isLatest ? " · latest" : "")),
        nav({ text: "Newer ›", label: "Newer run" }, idx - 1, isLatest),
        nav({ text: "Latest", label: "Latest run" }, 0, isLatest)));
    var list = runs.map(function (r, i) {
      var b = h("button", { "class": "runitem", "aria-current": String(i === idx) },
        h("span", null, h("span", { style: "font-weight:500;display:block" }, r.started),
          h("span", { "class": "small mute" }, r.kind + " · " + (r.trades === null ? "?" : r.trades) + (r.trades === 1 ? " trade" : " trades"))),
        h("span", { style: "text-align:right" }, h("span", { "class": pnlClass(r.net_pnl || "0") }, r.net_pnl === null ? "—" : usd(r.net_pnl)),
          i === 0 ? h("span", { "class": "badge" }, "Latest") : null));
      b.addEventListener("click", function () { goRun(rv.strategy, r.id, i === 0); });
      return b;
    });
    var side = h("section", { "class": "card", "aria-label": "Runs", style: "flex:1 1 250px;max-width:360px;overflow:hidden" },
      h("h2", { style: "padding:12px 16px;border-bottom:1px solid var(--soft)" }, "Runs, newest first"),
      h.apply(null, ["div", { style: "max-height:560px;overflow-y:auto" }].concat(list)));
    var d = rv.detail;
    var main = [];
    main.push(h("section", { "class": "summary", "aria-label": "Run summary" },
      summaryBox(run.started, run.net_pnl === null ? "—" : usd(run.net_pnl), "net P&L", run.net_pnl === null ? "" : pnlClass(run.net_pnl).replace("mono ", "")),
      summaryBox("Trades", run.trades === null ? "—" : String(run.trades), run.kind + (run.explorable ? " · stored" : "")),
      summaryBox("Budget that session", run.budget ? usd(run.budget) : "—", null),
      summaryBox("Entry rules", run.rules ? run.rules.slice(0, 12) : "—", run.rules ? "" : "not recorded for this kind of run")));
    if (!d) {
      main.push(h("div", { "class": "card empty" }, "Loading the run…"));
    } else if (d.trades === null) {
      var back = "/#/s/" + encodeURIComponent(rv.strategy) + "/" + encodeURIComponent(run.id);
      main.push(h("section", { "class": "card panel", "aria-label": "Run" },
        h("h2", null, "Trades and decisions"),
        h("div", { "class": "mute" }, d.note),
        h("a", { "class": "btn", style: "align-self:flex-start", href: "/explorer/" + encodeURIComponent(run.id) + "?back=" + encodeURIComponent(back) }, "Open in trade explorer ›")));
    } else {
      main.push(h("section", { "class": "card panel", "aria-label": "Run chart" },
        h("h2", null, "Realised profit through the session"),
        d.curve.length ? chart(d.curve) : h("div", { "class": "mute" }, "Nothing to draw: there were no fills."),
        h("div", { "class": "small mute" }, d.curve.length ? "Each point is a fill; the line is realised profit after it. A price chart is not available for ledger sessions." : "")));
      main.push(h("section", { "class": "card", "aria-label": "Trades", style: "overflow:hidden" },
        h("div", { "class": "between", style: "padding:12px 16px;border-bottom:1px solid var(--soft)" }, h("h2", null, "Trades"),
          h("span", { "class": "small mute" }, d.trades.length + (d.trades.length === 1 ? " fill" : " fills"))),
        d.trades.length ? tradesTable(d) : h("div", { style: "padding:16px" }, noTradeText(d))));
      if (d.trades.length && d.refused.length) {
        main.push(h("div", { "class": "small mute" }, "Refused this session: " + d.refused.map(function (r) { return (REASONS[r.reason] || REASONS.other) + " ×" + r.count; }).join("; ") + "."));
      }
    }
    app.replaceChildren(crumbs, header,
      h("div", { "class": "split" }, side, h.apply(null, ["div", { "class": "mainc", style: "gap:16px;flex:999 1 560px" }].concat(main))));
  }


  // ---- the budget editor --------------------------------------------------------------------
  // The page keeps a draft and asks the server, on every edit, whether it is allowed, what each
  // share may now be and what the changes come to in dollars. The sliders are held to the server's
  // ranges, and the server checks the draft again when it is scheduled: the rules live there.

  var ed = null;       // {view, draft, nodes, seq, timer, notice, busy}

  function pctText(bp) { return (bp / 100).toFixed(2).replace(/\.?0+$/, "") + "%"; }
  function draftText() {
    var t = "budgets v1\n";
    ed.draft.forEach(function (g) {
      t += "group " + g.id + " " + g.share_bp + " " + g.loss_soft_bp + " " + g.loss_hard_bp + "\n";
      g.strategies.forEach(function (s) { t += "strategy " + g.id + " " + s.id + " " + s.share_bp + "\n"; });
    });
    return t;
  }
  function post(path, body) {
    return fetch(path, { method: "POST", credentials: "same-origin", cache: "no-store",
      headers: { "X-Requested-With": "workspace", "Content-Type": "text/plain" }, body: body }).then(function (r) {
      if (r.status === 401) { location.href = "/"; return null; }
      return r.json().then(function (j) { return { ok: r.ok, status: r.status, body: j }; });
    });
  }
  function startDraft(view) {
    ed.view = view;
    ed.draft = view.groups.map(function (g) {
      return { id: g.id, share_bp: g.share_bp, loss_soft_bp: g.loss_soft_bp, loss_hard_bp: g.loss_hard_bp,
        strategies: g.strategies.map(function (s) { return { id: s.id, share_bp: s.share_bp }; }) };
    });
  }
  function ask() {
    clearTimeout(ed.timer);
    var seq = ++ed.seq;
    ed.timer = setTimeout(function () {
      post("/api/budgets/preview", draftText()).then(function (r) {
        if (!r || seq !== ed.seq) return;
        if (r.ok) { ed.view = r.body; paintEditor(); }
        else { ed.view.valid = false; ed.view.error = r.body.error || "The server refused the draft."; paintEditor(); }
      }).catch(function () { ed.view.valid = false; ed.view.error = "Could not reach the server."; paintEditor(); });
    }, 150);
  }
  function rangeOf(id) {
    for (var i = 0; i < ed.view.groups.length; i++) {
      var g = ed.view.groups[i];
      if (g.id === id) return g.range;
      for (var j = 0; j < g.strategies.length; j++) if (g.strategies[j].id === id) return g.strategies[j].range;
    }
    return { min: 0, max: 10000 };
  }
  function nodeShare(id, bp) {
    var done = false;
    ed.draft.forEach(function (g) {
      if (g.id === id) { g.share_bp = bp; done = true; }
      g.strategies.forEach(function (s) { if (s.id === id) { s.share_bp = bp; done = true; } });
    });
    return done;
  }

  function shareControl(id, label, bp) {
    var slider = h("input", { type: "range", min: "0", max: "10000", step: "1", value: String(bp), "aria-label": label + " share" });
    var num = h("input", { type: "number", min: "0", max: "100", step: "0.01", value: (bp / 100).toFixed(2), "aria-label": label + " share in percent", "class": "pctin" });
    var limit = h("div", { "class": "small mute" });
    var dollars = h("span", { "class": "mono" });
    function set(bp2, typed) {
      var r = rangeOf(id), v = Math.max(r.min, Math.min(r.max, Math.round(bp2)));
      if (typed && v !== Math.round(bp2)) ed.notice = label + " was limited to " + pctText(v) + (v === r.max ? ": " + r.max_why : ": " + r.min_why) + ".";
      else if (typed) ed.notice = null;
      nodeShare(id, v);
      slider.value = String(v); num.value = (v / 100).toFixed(2);
      ask(); paintEditor();
    }
    slider.addEventListener("input", function () { set(+slider.value, false); });
    num.addEventListener("change", function () { set(Math.round((parseFloat(num.value) || 0) * 100), true); });
    ed.nodes[id] = { slider: slider, num: num, limit: limit, dollars: dollars };
    return h("div", { "class": "ctl" }, slider, h("span", { "class": "pctwrap" }, num, "%"), dollars, limit);
  }

  function lossControl(g) {
    function field(key, label) {
      var i = h("input", { type: "number", min: "0.01", max: "100", step: "0.01", "class": "pctin", value: (g[key] / 100).toFixed(2), "aria-label": g.id + " " + label });
      i.addEventListener("change", function () {
        var dg = ed.draft.filter(function (x) { return x.id === g.id; })[0];
        dg[key] = Math.round((parseFloat(i.value) || 0) * 100);
        ed.notice = null; ask(); paintEditor();
      });
      return h("label", { "class": "small" }, label + " ", h("span", { "class": "pctwrap" }, i, "%"));
    }
    return h("div", { "class": "row small" }, h("span", { "class": "mute" }, "Per strategy, of its own budget:"),
      field("loss_soft_bp", "stop opening at"), field("loss_hard_bp", "flatten at"));
  }

  function drawEditor() {
    var v = ed.view;
    ed.nodes = {};
    var crumbs = h("nav", { "class": "row small mute", "aria-label": "Breadcrumb" }, h("a", { "class": "btn", href: "#" }, "‹ Overview"),
      h("span", null, "Workspace"), h("span", { "aria-hidden": "true" }, "/"), h("b", { style: "color:var(--ink)" }, "Edit budgets"));
    var groups = ed.draft.map(function (g, gi) {
      var head = h("div", { "class": "edhead" }, h("h2", { style: "font-size:15px" }, h("span", { "class": "dot", style: "background:" + COLORS[gi % COLORS.length] }), g.id),
        shareControl(g.id, g.id, g.share_bp));
      var rows = g.strategies.map(function (s) {
        return h("div", { "class": "edrow" }, h("b", null, s.id), shareControl(s.id, s.id, s.share_bp));
      });
      return h.apply(null, ["section", { "class": "card", "aria-label": g.id }, head, h("div", { style: "padding:0 16px 8px" }, lossControl(g))].concat(rows));
    });
    var out = h("div", { "class": "stack" });
    ed.refs = {
      split: h("span", { "class": "small mute" }), error: h("div", { "class": "err", role: "alert" }), notice: h("div", { "class": "small", role: "status" }),
      changes: h("ul", { style: "margin:6px 0 0;padding-left:18px" }), pending: h("div", null),
      go: h("button", { "class": "btn dark" }, "Schedule for the next rebalance"),
      reset: h("button", { "class": "btn" }, "Discard changes"),
      withdraw: h("button", { "class": "btn" }, "Ask to cancel what is scheduled"),
      done: h("div", { "class": "banner", role: "status", hidden: "" })
    };
    ed.refs.go.addEventListener("click", function () {
      ed.refs.go.disabled = true;
      post("/api/budgets/schedule", draftText()).then(function (r) {
        if (!r) return;
        if (r.ok) { ed.done = "Requested. The engine records it as a scheduled change and it takes effect at the next rebalance, after the session. Until then the budgets stay as they are."; refreshEditor(); }
        else { ed.view.valid = false; ed.view.error = r.body.error; paintEditor(); }
      });
    });
    ed.refs.reset.addEventListener("click", function () { ed.notice = null; ed.done = null; refreshEditor(); });
    ed.refs.withdraw.addEventListener("click", function () {
      post("/api/budgets/withdraw", "").then(function (r) {
        if (r && r.ok) { ed.done = "Requested. The engine will cancel the scheduled change when it takes the request."; refreshEditor(); }
      });
    });
    app.replaceChildren(crumbs,
      h("header", null, h("h1", null, "Edit budgets"),
        h("div", { "class": "small mute" }, "Reserved shares of " + usd(v.balance) + ". A share can't go above what is not yet assigned in its parent, or below what is in use. Changes are scheduled and take effect at the next rebalance.")),
      ed.refs.done,
      h("section", { "class": "card panel", "aria-label": "Split" }, h("div", { "class": "between" }, h("h2", null, "Split of the balance"), ed.refs.split)),
      h.apply(null, ["div", { "class": "stack" }].concat(groups)),
      h("section", { "class": "card panel", "aria-label": "Changes" }, h("h2", null, "Changes"), ed.refs.error, ed.refs.notice, ed.refs.changes,
        h("div", { "class": "row" }, ed.refs.go, ed.refs.reset)),
      h("section", { "class": "card panel", "aria-label": "Waiting" }, h("h2", null, "Waiting for the engine"), ed.refs.pending, h("div", null, ed.refs.withdraw)));
    paintEditor();
  }

  // Bring everything the server decides (ranges, dollars, changes, errors) up to date without
  // rebuilding the controls the person is using.
  function paintEditor() {
    var v = ed.view, r = ed.refs;
    function node(id, range, budget, used, share) {
      var n = ed.nodes[id];
      if (!n) return;
      n.slider.min = String(range.min); n.slider.max = String(range.max);
      n.num.min = (range.min / 100).toFixed(2); n.num.max = (range.max / 100).toFixed(2);
      n.limit.textContent = "between " + pctText(range.min) + " (" + range.min_why + ") and " + pctText(range.max) + " (" + range.max_why + ")";
      n.dollars.textContent = budget + " · in use " + used;
    }
    v.groups.forEach(function (g) {
      node(g.id, g.range, g.budget, g.used);
      g.strategies.forEach(function (s) { node(s.id, s.range, s.budget, s.used); });
    });
    var topSum = ed.draft.reduce(function (t, g) { return t + g.share_bp; }, 0);
    r.split.textContent = pctText(topSum) + " assigned · " + pctText(10000 - topSum) + " not reserved";
    r.error.textContent = v.valid ? "" : v.error || "";
    r.notice.textContent = ed.notice || "";
    r.changes.replaceChildren.apply(r.changes, v.changes.length ? v.changes.map(function (c) { return h("li", null, c); }) : [h("li", { "class": "mute", style: "list-style:none;margin-left:-18px" }, "No changes yet.")]);
    r.go.disabled = !(v.valid && v.changes.length > 0);
    r.reset.disabled = v.changes.length === 0 && !ed.notice;
    var waiting = v.pending || [];
    r.pending.replaceChildren.apply(r.pending, waiting.length ? waiting.map(function (p) {
      return h("div", { "class": "small", style: "margin-bottom:6px" }, h("b", null, p.name), " · " + p.by, h.apply(null, ["ul", { style: "margin:2px 0 0;padding-left:18px" }].concat(p.changes.map(function (c) { return h("li", null, c); }))));
    }) : [h("div", { "class": "small mute" }, "No requests are waiting.")]);
    r.withdraw.hidden = !(data && data.account && data.account.scheduled && data.account.scheduled.length);
    if (ed.done) { r.done.hidden = false; r.done.textContent = ed.done; } else r.done.hidden = true;
  }

  function refreshEditor() {
    get("/api/budgets").then(function (view) {
      if (!view) return;
      ed = ed || { seq: 0 };
      var done = ed.done, notice = ed.notice;
      startDraft(view);
      ed.done = done; ed.notice = notice;
      drawEditor();
    });
  }

  // ---- routing and loading ------------------------------------------------------------------

  function route() {
    if (location.hash === "#/edit") return { edit: true };
    var m = /^#\/s\/([^/]+)(?:\/(.+))?$/.exec(location.hash);
    if (m) return { strategy: decodeURIComponent(m[1]), id: m[2] ? decodeURIComponent(m[2]) : null };
    return null;
  }

  function get(url) {
    return fetch(url, { credentials: "same-origin", cache: "no-store" }).then(function (r) {
      if (r.status === 401) { location.href = "/"; return null; }
      if (!r.ok) throw new Error(String(r.status));
      return r.json();
    });
  }

  function draw() {
    if (route() && runView) drawRun(); else if (data) drawOverview();
  }

  function load() {
    var want = route();
    if (want && want.edit) {
      // The editor is not redrawn by the timer: that would throw away a draft.
      return get("/api/overview").then(function (d) {
        if (!d) return;
        data = d;
        if (ed && ed.refs && ed.view && !ed.fresh) { paintEditor(); return; }
        ed = { seq: 0, fresh: false };
        refreshEditor();
      }).catch(function () { app.replaceChildren(h("p", { "class": "err" }, "Could not load the editor.")); });
    }
    ed = null;
    var jobs = [get("/api/overview")];
    if (want) jobs.push(get("/api/runs?strategy=" + encodeURIComponent(want.strategy)));
    Promise.all(jobs).then(function (res) {
      if (!res[0]) return null;
      data = res[0]; failed = false;
      if (!want) { runView = null; drawOverview(); return null; }
      var runs = res[1] ? res[1].runs : [];
      var id = want.id || (runs.length ? runs[0].id : null);
      if (id && !runs.some(function (r) { return r.id === id; })) id = runs.length ? runs[0].id : null;
      var keep = runView && runView.strategy === want.strategy && runView.id === id ? runView.detail : null;
      runView = { strategy: want.strategy, id: id, runs: runs, detail: keep };
      drawRun();
      if (!id) return null;
      return get("/api/run?strategy=" + encodeURIComponent(want.strategy) + "&id=" + encodeURIComponent(id)).then(function (d) {
        if (runView && runView.id === id) { runView.detail = d; drawRun(); }
      });
    }).catch(function () {
      failed = true;
      if (data) draw(); else app.replaceChildren(h("p", { "class": "err" }, "Could not load the overview."));
    });
  }

  var m = /g=([^&]+)/.exec(location.hash);
  if (m) selected = decodeURIComponent(m[1]);
  window.addEventListener("hashchange", function () {
    var g = /g=([^&]+)/.exec(location.hash);
    if (g) selected = decodeURIComponent(g[1]);
    load();
  });
  load();
  setInterval(load, 15000);
})();
