// The workspace overview. Reads /api/overview and draws it; it has no way to change anything.
// Text from the API goes in with textContent only, never as markup.
(function () {
  "use strict";
  var COLORS = ["#2C63B8", "#0E7C6B", "#8A4FB0", "#B26A0B", "#B33A5B", "#4A6B1E"];
  var app = document.getElementById("app");
  var data = null;
  var selected = null;
  var failed = false;

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
      return h("div", { "class": "trow", role: "row" },
        h("span", { style: "font-weight:500" }, s.name),
        h("span", { "class": "mono" }, pct(s.share_bp)),
        h("span", { "class": "mono" }, usd(s.budget)),
        h("span", null, track, h("span", { "class": "small mute mono" }, usd(s.used) + " · " + pctOf(used, budget))),
        h("span", { "class": pnlClass(s.day_pnl) }, usd(s.day_pnl)),
        h("span", null, h("span", { "class": "state" + (s.state === "active" ? "" : " bad") }, s.state)),
        h("span", { "class": "small" }, s.runs + (s.runs === 1 ? " run" : " runs")));
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

  function banner(a) {
    if (!a.scheduled || a.scheduled.length === 0) return null;
    var items = a.scheduled.map(function (t) { return h("li", null, t); });
    return h("div", { "class": "banner", role: "status" },
      h("b", null, "Budget changes are scheduled."),
      " They take effect at the next rebalance, after the session.",
      h.apply(null, ["ul", null].concat(items)));
  }

  function draw() {
    var a = data.account;
    var kids = [];
    var head = h("header", { "class": "between", style: "align-items:flex-end;flex-wrap:wrap" },
      h("div", null,
        h("div", { "class": "row" }, h("span", { "class": "eyebrow" }, "Workspace"),
          a ? h("span", { "class": "chip" }, a.kind + " · " + a.records + " ledger records") : null,
          a && a.killed ? h("span", { "class": "state bad" }, "Kill switch on") : null,
          failed ? h("span", { "class": "state bad" }, "Could not refresh") : null),
        h("h1", null, "Overview")),
      h("div", { "class": "small mute", style: "max-width:380px" }, "Read-only. Budgets are reserved and rebalance after each session: gains and losses move into the strategy that made them."));
    kids.push(head);
    if (!a) {
      kids.push(h("div", { "class": "card empty" }, "No ledger is connected to this workspace."));
    } else if (!a.budgets) {
      kids.push(h("div", { "class": "card empty" }, "This account has no budgets set, so there is nothing to split yet."));
    } else {
      var ids = data.groups.map(function (g) { return g.id; });
      if (ids.indexOf(selected) < 0) selected = ids[0];
      var gi = ids.indexOf(selected), g = data.groups[gi];
      kids.push(banner(a), totals(a), allocation(),
        h("div", { "class": "split" },
          h("div", { "class": "mainc" }, groupCards(), strategies(g, gi)),
          h("aside", null, limits(g))));
    }
    app.replaceChildren.apply(app, kids.filter(Boolean));
  }

  function load() {
    fetch("/api/overview", { credentials: "same-origin", cache: "no-store" }).then(function (r) {
      if (r.status === 401) { location.href = "/"; return null; }
      if (!r.ok) throw new Error(String(r.status));
      return r.json();
    }).then(function (d) {
      if (!d) return;
      data = d; failed = false; draw();
    }).catch(function () {
      failed = true;
      if (data) draw(); else app.replaceChildren(h("p", { "class": "err" }, "Could not load the overview."));
    });
  }

  var m = /g=([^&]+)/.exec(location.hash);
  if (m) selected = decodeURIComponent(m[1]);
  load();
  setInterval(load, 15000);
})();
