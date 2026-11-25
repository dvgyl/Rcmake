/* rcmake compatibility report controller. MIT OR Apache-2.0. */

import { cleanNote, escapeHtml, formatNumber, formatRatio, initRovingGroup, loadLatestData, normalizeStatus, overviewRows, snapshotMeta, statusHtml } from "./app.js";
import { fuzzGaugeCard, horizontalBar, inlineBar, isMeasured, ringGaugeCard, segmentRing, shardHeatmap, speedupBars, statRow, statusIcon, waffleRowsHtml } from "./charts.js";
import { animateCountUp, createMotionController } from "./motion.js";

export function renderSummary(data) {
  const body = overviewRows(data).map((r) => `<tr data-status="${r.status}"><td><a href="#${escapeHtml(r.id)}">${escapeHtml(r.check)}</a></td><td>${r.result ? `<span data-value>${escapeHtml(r.result)}</span>` : `<span class="dim"><span data-value></span>${escapeHtml(r.scope)}</span>`}</td><td class="inline-bar-cell">${inlineBar(r.ratio, r.status)}</td><td>${statusHtml(r.status)}</td></tr>`).join("");
  return `<section id="summary" class="report-section" data-animate data-test="section-summary"><h2>Summary</h2><div class="table-panel"><table class="runner-table"><thead><tr><th>Check</th><th>Result</th><th></th><th>Status</th></tr></thead><tbody>${body}</tbody></table></div></section>`;
}

export function renderRunCmake(track) {
  const plats = track?.platforms || [];
  const bars = plats.map((p) => horizontalBar({ label: p.name, passed: p.suitesPassed, total: p.suitesTotal, rate: p.passRate, status: p.status, testId: `runcmake-bar-${p.id}` })).join("");
  const waffles = waffleRowsHtml(plats, { cell: 10, gap: 2, testPrefix: "dash-waffle" });
  return `<section id="runcmake" class="report-section" data-animate data-test="section-runcmake"><h2>Tests/RunCMake by OS</h2><figure class="panel rate-bars" aria-label="Tests/RunCMake pass rate by operating system">${bars}</figure><div class="panel waffle-panel">${waffles}</div></section>`;
}

export function renderCore(core, policies) {
  const bars = (core?.highlights || []).map((h) => horizontalBar({ label: h.suite, passed: h.passed, total: h.total, status: h.status, testId: `core-bar-${h.suite}` })).join("");
  const ok = isMeasured(policies?.suitesPassed);
  const st = ok ? normalizeStatus(policies.status) : "pending";
  const aria = ok ? `Policies ${policies.activeRange}. ${policies.suitesPassed} of ${policies.suitesTotal} suites pass.` : `Policies ${policies?.activeRange || ""}. ${policies?.suitesTotal ?? 56} suites, pending.`;
  return `<section id="core" class="report-section" data-animate data-test="section-core"><h2>Core suites and policies</h2><div class="core-grid"><figure class="panel rate-bars" aria-label="Selected core suites" style="margin-bottom:0">${bars}</figure><figure class="panel policy-card" aria-label="${escapeHtml(aria)}" data-status="${st}" data-test="policy-ring">${segmentRing({ passed: policies?.suitesPassed, total: policies?.suitesTotal ?? 56, status: st, size: 112, stroke: 7 })}<div class="gauge-label">Policies ${escapeHtml(policies?.activeRange || "")}</div><div class="policy-meta">Always NEW: ${escapeHtml(policies?.baselineAlwaysNew || "CMP0000-CMP0065")}</div></figure></div></section>`;
}

export function projectRowHtml(p) {
  const [l1, l2] = [normalizeStatus(p.l1), normalizeStatus(p.l2)];
  const targets = Array.isArray(p.targets) ? p.targets : [];
  const chips = targets.map((t) => `<span class="chip is-ink">${escapeHtml(t)}</span>`).join("");
  return `<tr data-name="${escapeHtml(`${p.name} ${p.repository}`.toLowerCase())}" data-tier="${escapeHtml(p.tier)}" data-l1="${l1}" data-l2="${l2}"><td><span>${escapeHtml(p.name)}</span><span class="proj-repo">${escapeHtml(p.repository)}</span></td><td class="mono nowrap">${escapeHtml(p.version)}</td><td><button type="button" class="targets-toggle" aria-expanded="false" data-targets-toggle><span>${targets.length} ${targets.length === 1 ? "target" : "targets"}</span></button><div class="targets-list" hidden>${chips}</div></td><td>${statusHtml(l1)}</td><td>${statusHtml(l2)}</td></tr>`;
}

export function renderProjects(track) {
  const rows = (track?.projects || []).map(projectRowHtml).join("");
  return `<section id="projects" class="report-section" data-test="section-projects"><h2>Large projects</h2><div class="filter-bar"><div class="seg" role="tablist" aria-label="Filter by tier" id="proj-tier" data-test="proj-tier"><button type="button" role="tab" aria-selected="true" data-tier-val="all">All</button><button type="button" role="tab" aria-selected="false" data-tier-val="standard">Standard</button><button type="button" role="tab" aria-selected="false" data-tier-val="large">Large</button></div><div class="seg" role="tablist" aria-label="Filter by status" id="proj-status" data-test="proj-status"><button type="button" role="tab" aria-selected="true" data-status-val="all">All</button><button type="button" role="tab" aria-selected="false" data-status-val="l1">L1 pass</button><button type="button" role="tab" aria-selected="false" data-status-val="l2">L2 pass</button><button type="button" role="tab" aria-selected="false" data-status-val="pending">Pending</button></div><input id="proj-search" class="search-input" type="search" placeholder="Filter projects" aria-label="Filter projects" data-test="proj-search"></div><div class="table-panel"><table class="bench-table" id="proj-table" data-test="proj-table"><thead><tr><th>Project</th><th>Version</th><th>Targets</th><th>L1</th><th>L2</th></tr></thead><tbody>${rows}<tr id="proj-empty" hidden><td colspan="5" class="dim">No projects match.</td></tr></tbody></table></div></section>`;
}

export function renderHoldout(track) {
  const total = formatNumber(track?.reposTotal) || "240";
  const bars = (track?.categories || []).map((c, i) => horizontalBar({ label: c.category, passed: c.reposPassed, total: c.reposTotal, rate: c.passRate, status: c.status, testId: `holdout-cat-${i}` })).join("");
  return `<section id="holdout" class="report-section" data-animate data-test="section-holdout"><h2>Blind holdout</h2><figure class="panel" aria-label="Blind holdout category results"><div class="holdout-banner"><svg class="i" aria-hidden="true"><use href="#i-lock"/></svg><span>${escapeHtml(total)} sealed repositories. Only category totals are published.</span></div><div class="rate-bars" style="margin-bottom:0">${bars}</div></figure></section>`;
}

export function shardDetail(shard) {
  const st = normalizeStatus(shard.status);
  if (!isMeasured(shard.portsPassed) || !isMeasured(shard.portsTotal)) return `shard ${shard.shard} \u00b7 ${st}`;
  const dur = isMeasured(shard.durationSeconds) ? ` \u00b7 ${Math.round(shard.durationSeconds)} s` : "";
  const fail = isMeasured(shard.portsFailed) ? ` \u00b7 ${formatNumber(shard.portsFailed)} failed` : "";
  return `shard ${shard.shard} \u00b7 ${formatRatio(shard.portsPassed, shard.portsTotal)} ports${fail}${dur}`;
}

export function renderVcpkg(track) {
  const shards = track?.shards || [];
  const counts = { pass: 0, partial: 0, fail: 0, pending: 0 };
  shards.forEach((s) => { counts[normalizeStatus(s.status)] += 1; });
  const legend = `pass ${counts.pass} \u00b7 partial ${counts.partial} \u00b7 fail ${counts.fail} \u00b7 pending ${counts.pending}`;
  return `<section id="vcpkg" class="report-section" data-animate data-test="section-vcpkg"><h2>vcpkg ports</h2><figure class="panel vcpkg-wrap" aria-label="${escapeHtml(`vcpkg ports across ${shards.length} shards. ${legend}.`)}"><div class="heatmap-area"><div class="heatmap-grid" id="vcpkg-grid" role="group" aria-label="vcpkg shards" data-test="vcpkg-heatmap">${shardHeatmap(shards, { cols: 8 })}</div><div class="hm-detail" id="shard-detail" data-test="shard-detail" aria-live="polite">Select a shard to inspect its counts.</div></div><div class="hm-legend" data-test="vcpkg-legend">${escapeHtml(legend)}</div></figure></section>`;
}

export function renderSpeed(benchTrack) {
  const b = benchTrack?.benchmarks || {};
  const [w, c, l, r] = [b.warmProbeSpeedup || {}, b.coldConfigureSpeedup || {}, b.incrementalLeafP95Ms || {}, b.peakRssMiB || {}];
  return `<section id="speed" class="report-section" data-animate data-test="section-speed"><h2>Speed and memory</h2><div class="panel speed-panel">${speedupBars({ label: "Warm configure", value: w.value, unit: w.unit, status: w.status, testId: "dash-speed-warm" })}${speedupBars({ label: "Cold configure, 500 targets", value: c.value, unit: c.unit, status: c.status, testId: "dash-speed-cold" })}<div class="speed-stats">${statRow({ label: "Leaf edit reconfigure, p95", value: l.value, unit: l.unit, baseline: l.baseline, status: l.status, testId: "dash-speed-leaf" })}${statRow({ label: "Peak memory, 500 targets", value: r.value, unit: r.unit, baseline: r.baseline, status: r.status, testId: "dash-speed-rss" })}</div></div></section>`;
}

export function renderBenchmarks(benchTrack) {
  const cards = (benchTrack?.gates || []).map((g) => {
    const summary = cleanNote(g.summary);
    if (g.id === "fuzz") return fuzzGaugeCard({ label: g.label, passed: g.passed, failures: g.failures, status: g.status, summary, testId: "dash-gate-fuzz" });
    return ringGaugeCard({
      label: g.label,
      passed: g.id === "blackbox" ? g.scenariosPassed : g.passed,
      total: g.id === "blackbox" ? g.scenariosTotal : g.total,
      status: g.status,
      summary,
      testId: `dash-gate-${g.id}`
    });
  }).join("");
  return `<section id="gates" class="report-section" data-animate data-test="section-gates"><h2>Soundness gates</h2><div class="panel gates-shell"><div class="gates-grid">${cards}</div></div></section>`;
}

export function renderReport(data) {
  const t = data.tracks;
  return [renderSummary(data), renderRunCmake(t.runCmakeOs), renderCore(t.coreConformance, t.policies), renderProjects(t.bossFight), renderHoldout(t.unseenHoldout), renderVcpkg(t.vcpkgMatrix), renderSpeed(t.benchmarksAndGates), renderBenchmarks(t.benchmarksAndGates)].join("");
}

function buildRail(doc, data) {
  const t = data.tracks;
  const gList = t.benchmarksAndGates?.gates || [];
  const bList = Object.values(t.benchmarksAndGates?.benchmarks || {});
  const fold = (arr) => arr.every((x) => x.status === "pass") ? "pass" : arr.some((x) => x.status === "fail") ? "fail" : arr.some((x) => x.status === "pass") ? "partial" : "pending";
  const items = [
    { id: "summary", label: "Summary", status: "pending" },
    { id: "runcmake", label: "Tests/RunCMake", status: t.runCmakeOs.status },
    { id: "core", label: "Core and policies", status: t.coreConformance.status },
    { id: "projects", label: "Large projects", status: t.bossFight.status },
    { id: "holdout", label: "Blind holdout", status: t.unseenHoldout.status },
    { id: "vcpkg", label: "vcpkg ports", status: t.vcpkgMatrix.status },
    { id: "speed", label: "Speed and memory", status: fold(bList) },
    { id: "gates", label: "Soundness gates", status: fold(gList) }
  ];
  items[0].status = fold(items.slice(1));
  const links = items.map((it, i) => `<a href="#${it.id}" data-rail-link="${it.id}"${i === 0 ? ' class="is-current"' : ""}>${statusIcon(it.status)}<span>${escapeHtml(it.label)}</span></a>`).join("");
  ["report-rail", "report-rail-mobile"].forEach((id) => { const el = doc.getElementById(id); if (el) el.innerHTML = links; });
  if (typeof IntersectionObserver === "undefined") return;
  const io = new IntersectionObserver((entries) => {
    const hit = entries.find((e) => e.isIntersecting);
    if (hit) doc.querySelectorAll("[data-rail-link]").forEach((a) => a.classList.toggle("is-current", a.dataset.railLink === hit.target.id));
  }, { rootMargin: "-20% 0px -65% 0px", threshold: 0.01 });
  items.forEach((it) => { const sec = doc.getElementById(it.id); if (sec) io.observe(sec); });
}

function wireProjectFilter(doc) {
  const [tierEl, statEl, search, empty, table] = ["proj-tier", "proj-status", "proj-search", "proj-empty", "proj-table"].map((id) => doc.getElementById(id));
  if (!table) return;
  const rows = Array.from(table.querySelectorAll("tbody tr[data-tier]"));
  const qs = String(window.location?.hash || "").startsWith("#projects?") ? new URLSearchParams(window.location.hash.slice(10)) : new URLSearchParams();
  const state = {
    tier: ["all", "standard", "large"].includes(qs.get("tier")) ? qs.get("tier") : "all",
    status: ["all", "l1", "l2", "pending"].includes(qs.get("status")) ? qs.get("status") : "all",
    q: qs.get("q") || ""
  };
  if (search) search.value = state.q;

  const apply = (writeHash = false) => {
    tierEl.querySelectorAll("[data-tier-val]").forEach((b) => b.setAttribute("aria-selected", String(b.dataset.tierVal === state.tier)));
    statEl.querySelectorAll("[data-status-val]").forEach((b) => b.setAttribute("aria-selected", String(b.dataset.statusVal === state.status)));
    const q = state.q.trim().toLowerCase();
    let shown = 0;
    rows.forEach((row) => {
      const ok = (state.tier === "all" || row.dataset.tier === state.tier)
        && (state.status === "all" || (state.status === "l1" && row.dataset.l1 === "pass") || (state.status === "l2" && row.dataset.l2 === "pass") || (state.status === "pending" && (row.dataset.l1 === "pending" || row.dataset.l2 === "pending")))
        && (!q || row.dataset.name.includes(q));
      row.classList.toggle("is-hidden", !ok);
      if (ok) shown += 1;
    });
    if (empty) empty.hidden = shown > 0;
    if (writeHash && typeof history !== "undefined") {
      const p = new URLSearchParams();
      if (state.tier !== "all") p.set("tier", state.tier);
      if (state.status !== "all") p.set("status", state.status);
      if (state.q) p.set("q", state.q);
      try { history.replaceState(null, "", p.toString() ? `#projects?${p}` : "#projects"); } catch (_) {}
    }
  };

  initRovingGroup(tierEl, "[data-tier-val]", (b) => { state.tier = b.dataset.tierVal; apply(true); });
  initRovingGroup(statEl, "[data-status-val]", (b) => { state.status = b.dataset.statusVal; apply(true); });
  search?.addEventListener("input", () => { state.q = search.value; apply(true); });
  table.querySelectorAll("[data-targets-toggle]").forEach((btn) => btn.addEventListener("click", () => {
    const open = btn.getAttribute("aria-expanded") === "true";
    btn.setAttribute("aria-expanded", String(!open));
    if (btn.nextElementSibling) btn.nextElementSibling.hidden = open;
  }));
  apply(false);
}

function wireShards(doc, shards) {
  const grid = doc.getElementById("vcpkg-grid");
  const detail = doc.getElementById("shard-detail");
  if (!grid || !detail) return;
  const btns = Array.from(grid.querySelectorAll("[data-shard]"));
  const show = (btn) => {
    const s = shards.find((x) => x.shard === Number(btn.dataset.shard));
    if (s) detail.textContent = shardDetail(s);
  };
  const select = (btn) => {
    btns.forEach((b) => { b.setAttribute("aria-pressed", String(b === btn)); b.tabIndex = b === btn ? 0 : -1; });
    show(btn);
  };
  btns.forEach((b) => {
    b.addEventListener("click", () => select(b));
    b.addEventListener("mouseenter", () => show(b));
    b.addEventListener("focus", () => show(b));
  });
  grid.addEventListener("keydown", (e) => {
    const idx = btns.indexOf(e.target.closest("[data-shard]"));
    if (idx < 0) return;
    const moves = { ArrowRight: Math.min(btns.length - 1, idx + 1), ArrowLeft: Math.max(0, idx - 1), ArrowDown: Math.min(btns.length - 1, idx + 8), ArrowUp: Math.max(0, idx - 8), Home: 0, End: btns.length - 1 };
    if (!(e.key in moves)) return;
    e.preventDefault();
    btns[moves[e.key]].focus();
    select(btns[moves[e.key]]);
  });
}

async function init(doc) {
  const root = doc.getElementById("report");
  if (!root) return;
  const motion = createMotionController(doc);
  try {
    const data = await loadLatestData();
    doc.getElementById("report-meta").textContent = snapshotMeta(data);
    doc.getElementById("report-note").textContent = cleanNote(data.statusNote);
    const pendingEl = doc.getElementById("report-pending");
    if (pendingEl) pendingEl.hidden = !overviewRows(data).some((r) => r.status === "pending");
    root.innerHTML = renderReport(data);
    buildRail(doc, data);
    wireProjectFilter(doc);
    wireShards(doc, data.tracks.vcpkgMatrix.shards);
    motion.scan(root);
    const fEl = root.querySelector("[data-fuzz-target]");
    if (fEl) motion.observeReveal(doc.getElementById("gates"), () => animateCountUp(fEl, Number(fEl.dataset.fuzzTarget), { duration: 800 }));
  } catch (_) {
    root.innerHTML = '<p class="load-error">Could not load data/latest.json. Serve the site over HTTP.</p>';
  }
}

if (typeof document !== "undefined") init(document);
