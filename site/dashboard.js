/* rcmake compatibility report controller. MIT OR Apache-2.0. */

import { cleanNote, escapeHtml, formatBytes, formatNumber, formatPercent, formatRatio, initRovingGroup, initSegPills, isPreFreezeGate, loadLatestData, normalizeStatus, overviewRows, revealScan, revealVisible, snapshotMeta, statusHtml } from "./app.js";
import { formatSpeed, fuzzGaugeCard, horizontalBar, inlineBar, isMeasured, normalizeGiants, ringGaugeCard, segmentRing, shardHeatmap, speedupBars, statRow, statusIcon, waffleRowsHtml } from "./charts.js";
import { animateCountUp, createMotionController } from "./motion.js";

const GATE_LABELS = Object.freeze({
  fuzz: "Seeded fuzzing",
  wasi: "WASI and wasm32",
  "probe-cache": "Probe cache",
  "mutation-replay": "Mutation replay",
  blackbox: "Black-box scenarios",
  "conan-ecosystem": "Conan ecosystem",
  "path-swap": "Silent PATH swap",
  determinism: "Path-swap determinism"
});

const LEVEL_LABELS = Object.freeze({
  L1: "L1 configure",
  L2: "L2 build graph",
  L3: "L3 build",
  L4: "L4 test and install"
});

function sectionNote(text) {
  const note = cleanNote(text);
  return note ? `<p class="section-note">${escapeHtml(note)}</p>` : "";
}

function diffTotals(diff) {
  if (!diff || typeof diff !== "object") return null;
  const total = isMeasured(diff.casesTotal) ? diff.casesTotal : isMeasured(diff.total) ? diff.total : null;
  if (!isMeasured(total) || total <= 0) return null;
  const passed = isMeasured(diff.casesPassed) ? diff.casesPassed : isMeasured(diff.passed) ? diff.passed : null;
  const mismatches = isMeasured(diff.mismatches) ? diff.mismatches : isMeasured(diff.failed) ? diff.failed : 0;
  const oracleErrors = isMeasured(diff.oracleErrors) ? diff.oracleErrors : 0;
  const passRate = isMeasured(diff.passRate) ? diff.passRate : (isMeasured(passed) ? (passed / total) * 100 : null);
  const seeds = Array.isArray(diff.seeds) && diff.seeds.length
    ? diff.seeds
    : isMeasured(diff.seed)
      ? [diff.seed]
      : [];
  return { total, passed, mismatches, oracleErrors, passRate, seeds };
}

export function renderProvenance(data) {
  if (!data || typeof data !== "object") return "";
  const note = cleanNote(data.statusNote);
  const receipt = data.receipt && typeof data.receipt === "object" ? data.receipt : {};
  const chips = [];
  const binSize = formatBytes(receipt.binarySizeBytes);
  if (binSize) chips.push(`<span class="chip is-ink">binary ${escapeHtml(binSize)}</span>`);
  const binSha = data.binarySha256 || receipt.binarySha256;
  if (typeof binSha === "string" && binSha.trim()) {
    const h = binSha.trim();
    chips.push(`<span class="chip" title="Binary SHA-256 ${escapeHtml(h)}">binary sha256 ${escapeHtml(h.slice(0, 12))}</span>`);
  }
  if (typeof receipt.wasmSha256 === "string" && receipt.wasmSha256.trim()) {
    const h = receipt.wasmSha256.trim();
    chips.push(`<span class="chip" title="WASM SHA-256 ${escapeHtml(h)}">wasm sha256 ${escapeHtml(h.slice(0, 12))}</span>`);
  }
  if (typeof receipt.tarballSha256 === "string" && receipt.tarballSha256.trim()) {
    const h = receipt.tarballSha256.trim();
    chips.push(`<span class="chip" title="Tarball SHA-256 ${escapeHtml(h)}">tarball sha256 ${escapeHtml(h.slice(0, 12))}</span>`);
  }
  if (typeof receipt.manifestSha256 === "string" && receipt.manifestSha256.trim()) {
    const h = receipt.manifestSha256.trim();
    chips.push(`<span class="chip" title="Manifest SHA-256 ${escapeHtml(h)}">manifest sha256 ${escapeHtml(h.slice(0, 12))}</span>`);
  }
  if (typeof receipt.runId === "string" && receipt.runId.trim()) {
    chips.push(`<span class="chip">run ${escapeHtml(receipt.runId.trim())}</span>`);
  }
  const artifactLinks = receipt.artifactUrls && typeof receipt.artifactUrls === "object"
    ? Object.entries(receipt.artifactUrls)
      .filter(([, url]) => typeof url === "string" && /^https?:\/\//i.test(url.trim()))
      .map(([k, url]) => `<a class="chip chip-link" href="${escapeHtml(url.trim())}" rel="noopener noreferrer">${escapeHtml(k)}</a>`)
    : [];
  chips.push(...artifactLinks);

  let gapsHtml = "";
  if ("knownParityGaps" in data && data.knownParityGaps !== null && data.knownParityGaps !== undefined) {
    if (Array.isArray(data.knownParityGaps)) {
      const cleaned = data.knownParityGaps.map((g) => String(g || "").trim()).filter(Boolean);
      if (cleaned.length > 1) {
        const items = cleaned.map((g) => `<li>${escapeHtml(g)}</li>`).join("");
        gapsHtml = `<div class="provenance-gaps"><strong>Known parity gaps.</strong><ul>${items}</ul></div>`;
      } else if (cleaned.length === 1) {
        gapsHtml = `<p class="provenance-gaps"><strong>Known parity gaps.</strong> ${escapeHtml(cleaned[0])}</p>`;
      } else {
        gapsHtml = '<p class="provenance-gaps"><strong>Known parity gaps.</strong> None recorded for this release.</p>';
      }
    } else if (typeof data.knownParityGaps === "string" && data.knownParityGaps.trim()) {
      gapsHtml = `<p class="provenance-gaps"><strong>Known parity gaps.</strong> ${escapeHtml(data.knownParityGaps.trim())}</p>`;
    }
  }

  if (!note && !chips.length && !gapsHtml) return "";
  return `<div class="panel provenance-bar" data-test="report-provenance">${note ? `<p class="provenance-note">${escapeHtml(note)}</p>` : ""}${gapsHtml}${chips.length ? `<div class="provenance-chips">${chips.join("")}</div>` : ""}</div>`;
}

export function renderSummary(data) {
  const body = overviewRows(data).map((r) => `<tr data-status="${r.status}"><td><a href="#${escapeHtml(r.id)}">${escapeHtml(r.check)}</a></td><td>${r.result ? `<span data-value>${escapeHtml(r.result)}</span>` : `<span class="dim"><span data-value></span>${escapeHtml(r.scope)}</span>`}</td><td class="inline-bar-cell">${inlineBar(r.ratio, r.status)}</td><td>${statusHtml(r.status, r.statusText)}</td></tr>`).join("");
  return `<section id="summary" class="report-section" data-animate data-reveal data-test="section-summary"><h2>Summary</h2>${renderProvenance(data)}<div class="table-panel"><table class="runner-table"><thead><tr><th>Check</th><th>Result</th><th class="inline-bar-cell" aria-hidden="true"></th><th>Status</th></tr></thead><tbody>${body}</tbody></table></div></section>`;
}

export function renderRunCmake(track) {
  const plats = track?.platforms || [];
  const bars = plats.map((p) => {
    const detailParts = [];
    if (isMeasured(p.passRate)) detailParts.push(`${formatPercent(p.passRate)} case pass rate`);
    if (isMeasured(p.casesPassed) && isMeasured(p.casesTotal)) detailParts.push(`${formatRatio(p.casesPassed, p.casesTotal)} cases`);
    if (p.note) detailParts.push(p.note);
    return horizontalBar({ label: p.name, passed: p.suitesPassed, total: p.suitesTotal, rate: p.passRate, status: p.status, detail: detailParts.join(" \u00b7 "), testId: `runcmake-bar-${p.id}` });
  }).join("");
  const measured = plats.some((p) => isMeasured(p.suitesPassed));
  const waffles = measured ? `<div class="panel waffle-panel">${waffleRowsHtml(plats, { cell: 10, gap: 2, testPrefix: "dash-waffle" })}</div>` : "";
  return `<section id="runcmake" class="report-section" data-animate data-reveal data-test="section-runcmake"><h2>Tests/RunCMake by operating system</h2><figure class="panel rate-bars" aria-label="Tests/RunCMake pass rate by operating system">${bars}</figure>${waffles}${sectionNote(track?.note)}</section>`;
}

export function renderCore(core, policies) {
  const bars = (core?.highlights || []).map((h) => horizontalBar({ label: h.suite, passed: h.passed, total: h.total, status: h.status, detail: h.note || "", testId: `core-bar-${h.suite}` })).join("");
  const coreSummaryParts = [];
  if (isMeasured(core?.suitesAt100) && isMeasured(core?.suitesTotal)) {
    coreSummaryParts.push(`${formatRatio(core.suitesAt100, core.suitesTotal)} core suites at 100%`);
  } else if (isMeasured(core?.suitesTotal)) {
    coreSummaryParts.push(`${formatNumber(core.suitesTotal)} core suites`);
  }
  if (isMeasured(core?.casesPassed) && isMeasured(core?.casesTotal)) {
    coreSummaryParts.push(`${formatRatio(core.casesPassed, core.casesTotal)} cases`);
  }
  if (isMeasured(core?.passRate)) coreSummaryParts.push(formatPercent(core.passRate));
  const coreHeader = coreSummaryParts.length
    ? `<div class="core-summary"><span>${escapeHtml(coreSummaryParts.join(" \u00b7 "))}</span>${statusHtml(core?.status)}</div>`
    : "";

  const polTotal = isMeasured(policies?.suitesTotal) ? policies.suitesTotal : 35;
  const ok = isMeasured(policies?.suitesPassed);
  const st = ok ? normalizeStatus(policies.status) : "pending";
  const aria = ok
    ? `Policies ${policies.activeRange}. ${policies.suitesPassed} of ${polTotal} suites pass.`
    : `Policies ${policies?.activeRange || ""}. ${polTotal} suites pending.`;
  const polCases = isMeasured(policies?.casesPassed) && isMeasured(policies?.casesTotal)
    ? `<div class="policy-meta">${escapeHtml(`${formatRatio(policies.casesPassed, policies.casesTotal)} cases${isMeasured(policies.passRate) ? ` (${formatPercent(policies.passRate)})` : ""}`)}</div>`
    : "";
  const note = [cleanNote(core?.note), cleanNote(policies?.note)].filter((v, i, arr) => v && arr.indexOf(v) === i).join(" ");
  return `<section id="core" class="report-section" data-animate data-reveal data-test="section-core"><h2>Core suites and policies</h2><div class="core-grid"><figure class="panel rate-bars" aria-label="Selected core suites" style="margin-bottom:0">${coreHeader}${bars}</figure><figure class="panel policy-card" aria-label="${escapeHtml(aria)}" data-status="${st}" data-test="policy-ring">${segmentRing({ passed: policies?.suitesPassed, total: polTotal, status: st, size: 112, stroke: 7 })}<div class="gauge-label">Policies ${escapeHtml(policies?.activeRange || "")}</div>${polCases}<div class="policy-meta">${escapeHtml(policies?.baselineAlwaysNew || "CMP0000-CMP0065")} always NEW</div></figure></div>${sectionNote(note)}</section>`;
}

export function projectRowHtml(p, { showL3 = false, showL4 = false } = {}) {
  const [l1, l2, l3, l4] = [normalizeStatus(p.l1), normalizeStatus(p.l2), normalizeStatus(p.l3), normalizeStatus(p.l4)];
  const targets = Array.isArray(p.targets) ? p.targets : [];
  const chips = targets.map((t) => `<span class="chip is-ink">${escapeHtml(t)}</span>`).join("");
  const note = cleanNote(p.note);
  const extraCells = `${showL3 ? `<td class="proj-cell-level" data-label="L3 build">${statusHtml(l3)}</td>` : ""}${showL4 ? `<td class="proj-cell-level" data-label="L4 test and install">${statusHtml(l4)}</td>` : ""}`;
  return `<tr data-name="${escapeHtml(`${p.name} ${p.repository}`.toLowerCase())}" data-tier="${escapeHtml(p.tier)}" data-l1="${l1}" data-l2="${l2}" data-l3="${l3}" data-l4="${l4}"><td class="proj-cell-name"><span>${escapeHtml(p.name)}</span><span class="proj-repo">${escapeHtml(p.repository)}</span>${note ? `<span class="proj-repo">${escapeHtml(note)}</span>` : ""}</td><td class="mono nowrap proj-cell-ver" data-label="Version">${escapeHtml(p.version)}</td><td class="proj-cell-targets" data-label="Targets"><button type="button" class="targets-toggle" aria-expanded="false" data-targets-toggle><span>${targets.length} ${targets.length === 1 ? "target" : "targets"}</span></button><div class="targets-list" hidden>${chips}</div></td><td class="proj-cell-level" data-label="L1 configure">${statusHtml(l1)}</td><td class="proj-cell-level" data-label="L2 build graph">${statusHtml(l2)}</td>${extraCells}</tr>`;
}

export function renderProjects(track) {
  const list = track?.projects || [];
  const showL3 = isMeasured(track?.l3Passed) || list.some((p) => p?.l3 && normalizeStatus(p.l3) !== "pending");
  const showL4 = isMeasured(track?.l4Passed) || list.some((p) => p?.l4 && normalizeStatus(p.l4) !== "pending");
  const colCount = 5 + (showL3 ? 1 : 0) + (showL4 ? 1 : 0);
  const rows = list.map((p) => projectRowHtml(p, { showL3, showL4 })).join("");
  const extraHead = `${showL3 ? '<th title="Listed targets compile and link without errors">L3 build</th>' : ""}${showL4 ? '<th title="Tests and install manifest checks pass">L4 test and install</th>' : ""}`;
  return `<section id="projects" class="report-section" data-reveal data-test="section-projects"><h2>Large projects</h2><div class="filter-bar"><div class="seg" role="group" aria-label="Filter by tier" id="proj-tier" data-test="proj-tier"><button type="button" aria-pressed="true" data-tier-val="all">All</button><button type="button" aria-pressed="false" data-tier-val="standard">Standard</button><button type="button" aria-pressed="false" data-tier-val="large">Large</button></div><div class="seg" role="group" aria-label="Filter by status" id="proj-status" data-test="proj-status"><button type="button" aria-pressed="true" data-status-val="all">All</button><button type="button" aria-pressed="false" data-status-val="l1">L1 pass</button><button type="button" aria-pressed="false" data-status-val="l2">L2 pass</button><button type="button" aria-pressed="false" data-status-val="fail">Fail</button><button type="button" aria-pressed="false" data-status-val="pending">Pending</button></div><input id="proj-search" class="search-input" type="search" placeholder="Filter projects" aria-label="Filter projects" data-test="proj-search"></div><div class="table-panel"><table class="bench-table" id="proj-table" data-test="proj-table" data-show-l3="${showL3}" data-show-l4="${showL4}"><thead><tr><th>Project</th><th>Version</th><th>Targets</th><th title="Configure output and stderr match CMake 4.5.1">L1 configure</th><th title="Cache, Ninja graph, and File API match CMake 4.5.1">L2 build graph</th>${extraHead}</tr></thead><tbody>${rows}<tr id="proj-empty" hidden><td colspan="${colCount}" class="dim">No projects match.</td></tr></tbody></table></div>${sectionNote(track?.note)}</section>`;
}

export function renderHoldout(track) {
  const sealedTotal = isMeasured(track?.reposTotal) && track.reposTotal < 240 ? 240 : (track?.reposTotal ?? 240);
  const measured = isMeasured(track?.reposPassed) && isMeasured(track?.reposTotal);
  const st = measured ? normalizeStatus(track.status) : "pending";
  const bannerSummary = measured
    ? `${formatNumber(sealedTotal)} sealed repositories (${formatNumber(track.reposTotal)} eligible) \u00b7 ${formatRatio(track.reposPassed, track.reposTotal)} passed${isMeasured(track.passRate) ? ` (${formatPercent(track.passRate)})` : ""}`
    : `${formatNumber(sealedTotal)} sealed repositories \u00b7 aggregate counts only`;

  const aggregateBar = measured
    ? horizontalBar({
      label: "Aggregate pass rate",
      passed: track.reposPassed,
      total: track.reposTotal,
      rate: track.passRate,
      status: st,
      detail: isMeasured(track.passRate)
        ? `${formatPercent(track.passRate)} (${formatRatio(track.reposPassed, track.reposTotal)} repositories)`
        : "",
      testId: "holdout-aggregate"
    })
    : "";

  const levelRates = track?.levelPassRates && typeof track.levelPassRates === "object" ? track.levelPassRates : null;
  const levelKeys = levelRates ? ["L1", "L2", "L3", "L4"].filter((k) => levelRates[k] && typeof levelRates[k] === "object") : [];
  const levelBars = levelKeys.map((k) => {
    const row = levelRates[k];
    const rowPct = isMeasured(row.passRate) ? row.passRate : (isMeasured(row.passed) && isMeasured(row.eligible) && row.eligible > 0 ? (row.passed / row.eligible) * 100 : null);
    const rowStatus = isMeasured(row.passed) && isMeasured(row.eligible) && row.eligible > 0
      ? (row.passed === row.eligible ? "pass" : rowPct >= 95 ? "partial" : "fail")
      : "pending";
    return horizontalBar({
      label: LEVEL_LABELS[k] || k,
      passed: row.passed,
      total: row.eligible,
      rate: row.passRate,
      status: rowStatus,
      detail: isMeasured(row.passRate) ? `${formatPercent(row.passRate)} (${formatRatio(row.passed, row.eligible)} eligible)` : "",
      testId: `holdout-level-${k}`
    });
  }).join("");

  const cats = Array.isArray(track?.categories) ? track.categories : [];
  const catsMeasured = cats.some((c) => isMeasured(c?.reposPassed));
  const showCats = catsMeasured || (!measured && !levelBars);
  const catBars = showCats
    ? cats.map((c, i) => horizontalBar({ label: c.category, passed: c.reposPassed, total: c.reposTotal, rate: c.passRate, status: c.status, testId: `holdout-cat-${i}` })).join("")
    : "";

  const rawFailures = track?.failureCategories && typeof track.failureCategories === "object" ? track.failureCategories : null;
  const failEntries = rawFailures ? Object.entries(rawFailures).filter(([, n]) => isMeasured(n) && n > 0) : [];
  const failHtml = failEntries.length
    ? `<div class="failure-chips" data-test="holdout-failure-categories"><span class="eyebrow">Failure categories</span>${failEntries.map(([k, n]) => `<span class="chip">${escapeHtml(k)} ${escapeHtml(formatNumber(n))}</span>`).join("")}</div>`
    : "";

  const note = [cleanNote(track?.policy), cleanNote(track?.note)].filter(Boolean).join(" ");
  return `<section id="holdout" class="report-section" data-animate data-reveal data-test="section-holdout"><h2>Blind holdout</h2><figure class="panel" aria-label="Blind holdout aggregate results"><div class="holdout-banner"><svg class="i" aria-hidden="true"><use href="#i-lock"/></svg><span>${escapeHtml(bannerSummary)}</span>${measured ? statusHtml(st) : ""}</div><div class="rate-bars" style="margin-bottom:0">${aggregateBar}${levelBars}${catBars}</div>${failHtml}</figure>${sectionNote(note)}</section>`;
}

export function shardDetail(shard) {
  const st = normalizeStatus(shard.status);
  const inspected = isMeasured(shard.portsInspected)
    ? (isMeasured(shard.portsExpected) ? ` \u00b7 ${formatRatio(shard.portsInspected, shard.portsExpected)} inspected` : ` \u00b7 ${formatNumber(shard.portsInspected)} inspected`)
    : "";
  const dur = isMeasured(shard.durationSeconds) ? ` \u00b7 ${Math.round(shard.durationSeconds)} s` : "";
  if (!isMeasured(shard.portsPassed) || !isMeasured(shard.portsTotal)) return `shard ${shard.shard} \u00b7 ${st}${inspected}${dur}`;
  if (shard.portsTotal === 0) return `shard ${shard.shard} \u00b7 0 scored ports \u00b7 ${st}${inspected}${dur}`;
  const fail = isMeasured(shard.portsFailed) && shard.portsFailed > 0 ? ` \u00b7 ${formatNumber(shard.portsFailed)} failed` : "";
  return `shard ${shard.shard} \u00b7 ${formatRatio(shard.portsPassed, shard.portsTotal)} scored ports${fail}${inspected}${dur}`;
}

export function renderVcpkg(track) {
  const shards = track?.shards || [];
  const counts = { pass: 0, partial: 0, fail: 0, pending: 0 };
  shards.forEach((s) => { counts[normalizeStatus(s.status)] += 1; });
  const legend = `pass ${counts.pass} \u00b7 partial ${counts.partial} \u00b7 fail ${counts.fail} \u00b7 pending ${counts.pending}`;
  const summaryParts = [];
  if (isMeasured(track?.portsPassed) && isMeasured(track?.portsTotal)) {
    summaryParts.push(`${formatRatio(track.portsPassed, track.portsTotal)} scored CMake ports`);
    if (isMeasured(track?.passRate)) summaryParts.push(formatPercent(track.passRate));
  }
  if (isMeasured(track?.portsInspected)) {
    summaryParts.push(isMeasured(track?.portsExpected) ? `${formatRatio(track.portsInspected, track.portsExpected)} inspected` : `${formatNumber(track.portsInspected)} inspected`);
  }
  const summaryHtml = summaryParts.length
    ? `<div class="vcpkg-summary"><span>${escapeHtml(summaryParts.join(" \u00b7 "))}</span>${statusHtml(track?.status)}</div>`
    : "";
  const initialDetail = shards.length ? escapeHtml(shardDetail(shards[0])) : "";
  return `<section id="vcpkg" class="report-section" data-animate data-reveal data-test="section-vcpkg"><h2>vcpkg ports</h2><figure class="panel vcpkg-wrap" aria-label="${escapeHtml(`vcpkg ports across ${shards.length} shards. ${legend}.`)}">${summaryHtml}<div class="heatmap-area"><div class="heatmap-grid" id="vcpkg-grid" role="group" aria-label="vcpkg shards" data-test="vcpkg-heatmap">${shardHeatmap(shards, { cols: 8 })}</div><div class="hm-detail" id="shard-detail" data-test="shard-detail" aria-live="polite">${initialDetail}</div></div><div class="hm-legend" data-test="vcpkg-legend">${escapeHtml(legend)}</div></figure>${sectionNote(track?.note)}</section>`;
}

function maxProjectRss(project, key) {
  const vals = [project?.cold?.[key], project?.warmProbe?.[key], project?.incrementalLeaf?.[key]].filter(isMeasured);
  return vals.length ? Math.max(...vals) : null;
}

function formatRatioLabel(speedup) {
  if (!formatSpeed(speedup)) return null;
  return speedup < 1 ? `${(1 / speedup).toFixed(2)}\u00d7 slower` : `${formatSpeed(speedup)}\u00d7`;
}

function formatGiantsPhaseHtml(phase, extra = null) {
  const sp = formatRatioLabel(phase?.speedup);
  const ms = isMeasured(phase?.rcmakeMedianMs) ? `${formatNumber(phase.rcmakeMedianMs)} ms` : null;
  const detail = extra || ms;
  if (sp && detail) return `<span class="giants-primary">${escapeHtml(sp)}</span><span class="giants-sub">${escapeHtml(detail)}</span>`;
  if (sp) return `<span class="giants-primary">${escapeHtml(sp)}</span>`;
  if (detail) return `<span class="giants-primary">${escapeHtml(detail)}</span>`;
  return '<span class="dim">-</span>';
}

function phaseTooltip(phase) {
  if (!phase || typeof phase !== "object") return "";
  const parts = [];
  if (isMeasured(phase.rcmakeMedianMs) && isMeasured(phase.cmakeMedianMs)) {
    parts.push(`rcmake ${formatNumber(phase.rcmakeMedianMs)} ms compared with CMake ${formatNumber(phase.cmakeMedianMs)} ms`);
  }
  if (isMeasured(phase.probeCacheHits)) {
    const misses = isMeasured(phase.probeCacheMisses) ? `, ${formatNumber(phase.probeCacheMisses)} misses` : "";
    parts.push(`probe cache ${formatNumber(phase.probeCacheHits)} hits${misses}`);
  }
  if (phase.leafFile) parts.push(`leaf file ${phase.leafFile}`);
  return parts.join(" \u00b7 ");
}

function formatRssPair(rcmakeMiB, cmakeMiB) {
  if (isMeasured(rcmakeMiB) && isMeasured(cmakeMiB)) return `${formatNumber(rcmakeMiB)} / ${formatNumber(cmakeMiB)} MiB`;
  if (isMeasured(rcmakeMiB)) return `${formatNumber(rcmakeMiB)} MiB`;
  return "-";
}

export function renderGiantsBench(giants) {
  if (!giants || !Array.isArray(giants.projects) || giants.projects.length === 0) return "";
  const hasMeasured = giants.projects.some((p) => normalizeStatus(p?.status) !== "pending");
  if (!hasMeasured && normalizeStatus(giants.status) === "pending") return "";
  const hasBuildVerify = giants.projects.some((p) => p?.buildVerify && typeof p.buildVerify === "object");
  const rows = giants.projects.map((p) => {
    const st = normalizeStatus(p?.status);
    const leafExtra = isMeasured(p?.incrementalLeaf?.rcmakeP95Ms) ? `${formatNumber(p.incrementalLeaf.rcmakeP95Ms)} ms p95` : null;
    const rcRss = maxProjectRss(p, "rcmakePeakRssMiB");
    const cmRss = maxProjectRss(p, "cmakePeakRssMiB");
    const sub = [p?.version || p?.repository || "", cleanNote(p?.note)].filter(Boolean).join(" \u00b7 ");
    const coldTip = phaseTooltip(p?.cold);
    const warmTip = phaseTooltip(p?.warmProbe);
    const leafTip = phaseTooltip(p?.incrementalLeaf);
    let bvCell = "";
    if (hasBuildVerify) {
      const bv = p?.buildVerify;
      if (bv && typeof bv === "object") {
        const bvTip = [bv.target || "", bv.dryRunStepCountMatch ? "dry-run step counts match" : ""].filter(Boolean).join(" \u00b7 ");
        bvCell = `<td${bvTip ? ` title="${escapeHtml(bvTip)}"` : ""}>${statusHtml(bv.status)}</td>`;
      } else {
        bvCell = '<td class="dim">-</td>';
      }
    }
    return `<tr data-status="${st}"><td class="giants-cell-name"><span>${escapeHtml(p?.name || "")}</span>${sub ? `<span class="proj-repo">${escapeHtml(sub)}</span>` : ""}</td><td class="mono" data-label="Cold"${coldTip ? ` title="${escapeHtml(coldTip)}"` : ""}>${formatGiantsPhaseHtml(p?.cold)}</td><td class="mono" data-label="Warm probe"${warmTip ? ` title="${escapeHtml(warmTip)}"` : ""}>${formatGiantsPhaseHtml(p?.warmProbe)}</td><td class="mono" data-label="Leaf edit"${leafTip ? ` title="${escapeHtml(leafTip)}"` : ""}>${formatGiantsPhaseHtml(p?.incrementalLeaf, leafExtra)}</td><td class="mono nowrap" data-label="Peak RSS" title="Peak RSS for rcmake compared with CMake">${escapeHtml(formatRssPair(rcRss, cmRss))}</td>${bvCell ? bvCell.replace("<td", '<td data-label="Build verify"') : ""}<td data-label="Status">${statusHtml(st)}</td></tr>`;
  }).join("");
  const s = giants.summary;
  let summaryRow = "";
  if (s && (isMeasured(s.coldSpeedupGeomean) || isMeasured(s.warmProbeSpeedupGeomean) || isMeasured(s.incrementalLeafSpeedupGeomean))) {
    const st = normalizeStatus(giants.status);
    const fmtG = (v) => {
      const lbl = formatRatioLabel(v);
      return lbl ? `<span class="giants-primary">${escapeHtml(lbl)}</span>` : '<span class="dim">-</span>';
    };
    const incRatio = formatRatioLabel(s.incrementalLeafSpeedupGeomean);
    const incP95 = isMeasured(s.incrementalLeafMaxP95Ms) ? `${formatNumber(s.incrementalLeafMaxP95Ms)} ms max p95` : null;
    const incHtml = incRatio && incP95
      ? `<span class="giants-primary">${escapeHtml(incRatio)}</span><span class="giants-sub">${escapeHtml(incP95)}</span>`
      : incRatio
        ? `<span class="giants-primary">${escapeHtml(incRatio)}</span>`
        : incP95
          ? `<span class="giants-primary">${escapeHtml(incP95)}</span>`
          : '<span class="dim">-</span>';
    const scope = isMeasured(s.projectsPassed) && isMeasured(s.projectsTotal) ? `${formatNumber(s.projectsPassed)} / ${formatNumber(s.projectsTotal)} passed` : "";
    const metaSub = scope ? `${scope} \u00b7 geometric mean and max` : "geometric mean and max";
    summaryRow = `<tr class="cmp-group-note" data-status="${st}" data-test="dash-giants-summary"><td class="giants-cell-name"><span>Summary</span><span class="proj-repo">${escapeHtml(metaSub)}</span></td><td class="mono" data-label="Cold">${fmtG(s.coldSpeedupGeomean)}</td><td class="mono" data-label="Warm probe">${fmtG(s.warmProbeSpeedupGeomean)}</td><td class="mono" data-label="Leaf edit">${incHtml}</td><td class="mono nowrap" data-label="Peak RSS" title="Maximum peak RSS for rcmake compared with CMake">${escapeHtml(formatRssPair(s.rcmakeMaxPeakRssMiB, s.cmakeMaxPeakRssMiB))}</td>${hasBuildVerify ? '<td class="dim" data-label="Build verify">-</td>' : ""}<td data-label="Status">${statusHtml(st)}</td></tr>`;
  }
  const footParts = [];
  if (isMeasured(giants.runs)) footParts.push(`Median of ${formatNumber(giants.runs)} runs per project.`);
  if (giants.reproducer) footParts.push(`Run ${giants.reproducer} to reproduce these results.`);
  return `<div class="table-panel" style="margin-top:16px" data-test="dash-giants-bench"><table class="bench-table"><thead><tr><th>Project</th><th>Cold</th><th title="New build tree with shared probe cache">Warm probe</th><th title="Incremental reconfigure after editing one leaf file">Leaf edit</th><th title="Peak RSS for rcmake compared with CMake">Peak RSS</th>${hasBuildVerify ? "<th>Build verify</th>" : ""}<th>Status</th></tr></thead><tbody>${rows}${summaryRow}</tbody></table></div>${sectionNote(footParts.join(" "))}`;
}

export function renderDifferentialFuzz(diff) {
  const stats = diffTotals(diff);
  if (!stats) return "";
  const st = normalizeStatus(diff.status);
  const ratio = formatRatio(stats.passed, stats.total) || "-";
  const pct = isMeasured(stats.passRate) ? ` (${formatPercent(stats.passRate)})` : "";
  const mismatches = formatNumber(stats.mismatches) ?? "0";
  const oracleErrors = formatNumber(stats.oracleErrors) ?? "0";
  const seeds = stats.seeds.length ? stats.seeds.join(", ") : "-";
  const subParts = [];
  if (isMeasured(diff.scriptCases)) subParts.push(`${formatNumber(diff.scriptCases)} script`);
  if (isMeasured(diff.configureCases)) subParts.push(`${formatNumber(diff.configureCases)} configure`);
  if (isMeasured(diff.families)) subParts.push(`${formatNumber(diff.families)} families`);
  const sub = subParts.length ? `<span class="proj-repo">${escapeHtml(subParts.join(" \u00b7 "))}</span>` : "";
  return `<div class="table-panel" style="margin-top:16px" data-test="dash-differential-fuzz"><table class="runner-table"><thead><tr><th>Differential fuzzing</th><th>Cases passed</th><th>Mismatches</th><th>Oracle errors</th><th>Seeds</th><th>Status</th></tr></thead><tbody><tr data-status="${st}"><td><span>${escapeHtml(diff.label || "Oracle differential fuzzing")}</span>${sub}</td><td><span data-value>${escapeHtml(`${ratio}${pct}`)}</span></td><td>${escapeHtml(mismatches)}</td><td>${escapeHtml(oracleErrors)}</td><td class="dim mono">${escapeHtml(seeds)}</td><td>${statusHtml(st)}</td></tr></tbody></table></div>${sectionNote(diff.note)}`;
}

export function renderSpeed(benchTrack) {
  const b = benchTrack?.benchmarks || {};
  const [w, c, l, r] = [b.warmProbeSpeedup || {}, b.coldConfigureSpeedup || {}, b.incrementalLeafP95Ms || {}, b.peakRssMiB || {}];
  const notes = [w.note, c.note, l.note, r.note].map(cleanNote).filter((v, i, arr) => v && !/^Pending /i.test(v) && arr.indexOf(v) === i);
  const giants = normalizeGiants(benchTrack);
  return `<section id="speed" class="report-section" data-animate data-reveal data-test="section-speed"><h2>Speed and memory</h2><div class="panel speed-panel">${speedupBars({ label: w.label || "New tree with shared probe cache", baseline: w.baseline || "", value: w.value, unit: w.unit, status: w.status, testId: "dash-speed-warm" })}${speedupBars({ label: c.label || "Cold configure on 500 targets", baseline: c.baseline || "", value: c.value, unit: c.unit, status: c.status, testId: "dash-speed-cold" })}<div class="speed-stats">${statRow({ label: l.label || "Leaf edit reconfigure p95", baseline: l.baseline || "", value: l.value, unit: l.unit, status: l.status, testId: "dash-speed-leaf" })}${statRow({ label: r.label || "Peak memory on 500 targets", baseline: r.baseline || "", value: r.value, unit: r.unit, status: r.status, testId: "dash-speed-rss" })}</div></div>${sectionNote(notes.join(" "))}${renderGiantsBench(giants)}</section>`;
}

export function renderBenchmarks(benchTrack, data = null) {
  const cards = (benchTrack?.gates || []).map((g) => {
    const preFreeze = isPreFreezeGate(g, data);
    const st = preFreeze ? "partial" : g.status;
    const label = GATE_LABELS[g.id] || g.label;
    const summary = preFreeze ? (g.commit ? `pre-freeze \u00b7 ${g.commit}` : "pre-freeze") : "";
    const detail = cleanNote(g.detail);
    if (g.id === "fuzz") return fuzzGaugeCard({ label, passed: g.passed, failures: g.failures, status: st, summary, detail, preFreeze, testId: "dash-gate-fuzz" });
    const sub = g.id === "blackbox" && isMeasured(g.passed) && isMeasured(g.total)
      ? `${formatRatio(g.passed, g.total)} checks`
      : (!preFreeze && g.id === "path-swap" && normalizeStatus(g.status) === "pass" ? "0 differences" : "");
    return ringGaugeCard({
      label,
      passed: g.id === "blackbox" ? g.scenariosPassed : g.passed,
      total: g.id === "blackbox" ? g.scenariosTotal : g.total,
      status: st,
      summary,
      sub,
      detail,
      preFreeze,
      testId: `dash-gate-${g.id}`
    });
  }).join("");
  return `<section id="gates" class="report-section" data-animate data-reveal data-test="section-gates"><h2>Soundness gates</h2><div class="panel gates-shell"><div class="gates-grid">${cards}</div></div>${renderDifferentialFuzz(benchTrack?.differentialFuzz)}</section>`;
}

export function renderReport(data) {
  const t = data?.tracks || {};
  return [renderSummary(data), renderRunCmake(t.runCmakeOs), renderCore(t.coreConformance, t.policies), renderProjects(t.bossFight), renderHoldout(t.unseenHoldout), renderVcpkg(t.vcpkgMatrix), renderSpeed(t.benchmarksAndGates), renderBenchmarks(t.benchmarksAndGates, data)].join("");
}

function buildRail(doc, data) {
  const t = data?.tracks || {};
  const diff = t.benchmarksAndGates?.differentialFuzz;
  const giants = normalizeGiants(t.benchmarksAndGates);
  const gList = [
    ...(t.benchmarksAndGates?.gates || []).map((g) => ({ status: isPreFreezeGate(g, data) ? "partial" : g.status })),
    ...(diffTotals(diff) ? [diff] : [])
  ];
  const bList = [
    ...Object.values(t.benchmarksAndGates?.benchmarks || {}),
    ...(giants && Array.isArray(giants.projects) && giants.projects.some((p) => normalizeStatus(p?.status) !== "pending") ? [giants] : [])
  ];
  const fold = (arr) => {
    if (!arr.length) return "pending";
    const norm = arr.map((x) => normalizeStatus(x?.status));
    if (norm.every((s) => s === "pass")) return "pass";
    if (norm.includes("fail")) return "fail";
    if (norm.some((s) => s === "pass" || s === "partial")) return "partial";
    return "pending";
  };
  const items = [
    { id: "summary", label: "Summary", status: "pending" },
    { id: "runcmake", label: "Tests/RunCMake", status: t.runCmakeOs?.status },
    { id: "core", label: "Core and policies", status: fold([t.coreConformance, t.policies]) },
    { id: "projects", label: "Large projects", status: t.bossFight?.status },
    { id: "holdout", label: "Blind holdout", status: t.unseenHoldout?.status },
    { id: "vcpkg", label: "vcpkg ports", status: t.vcpkgMatrix?.status },
    { id: "speed", label: "Speed and memory", status: fold(bList) },
    { id: "gates", label: "Soundness gates", status: fold(gList) }
  ];
  items[0].status = fold(items.slice(1));
  const links = items.map((it, i) => `<a href="#${it.id}" data-rail-link="${it.id}"${i === 0 ? ' class="is-current"' : ""}>${statusIcon(it.status)}<span>${escapeHtml(it.label)}</span></a>`).join("");
  ["report-rail", "report-rail-mobile"].forEach((id) => { const el = doc.getElementById(id); if (el) el.innerHTML = links; });
  if (typeof IntersectionObserver === "undefined") return;
  const mobileRail = doc.getElementById("report-rail-mobile");
  const io = new IntersectionObserver((entries) => {
    const hit = entries.find((e) => e.isIntersecting);
    if (!hit) return;
    doc.querySelectorAll("[data-rail-link]").forEach((a) => {
      const current = a.dataset.railLink === hit.target.id;
      a.classList.toggle("is-current", current);
      if (current && mobileRail && mobileRail.contains(a) && typeof a.scrollIntoView === "function") {
        a.scrollIntoView({ behavior: "auto", inline: "nearest", block: "nearest" });
      }
    });
  }, { rootMargin: "-20% 0px -65% 0px", threshold: 0.01 });
  items.forEach((it) => { const sec = doc.getElementById(it.id); if (sec) io.observe(sec); });
}

function wireProjectFilter(doc) {
  const [tierEl, statEl, search, empty, table] = ["proj-tier", "proj-status", "proj-search", "proj-empty", "proj-table"].map((id) => doc.getElementById(id));
  if (!table) return;
  const rows = Array.from(table.querySelectorAll("tbody tr[data-tier]"));
  const showL3 = table.dataset?.showL3 === "true";
  const showL4 = table.dataset?.showL4 === "true";
  const readHashState = () => {
    const rawHash = typeof window !== "undefined" ? String(window.location?.hash || "") : "";
    const qs = rawHash.startsWith("#projects?") ? new URLSearchParams(rawHash.slice(10)) : new URLSearchParams();
    return {
      tier: ["all", "standard", "large"].includes(qs.get("tier")) ? qs.get("tier") : "all",
      status: ["all", "l1", "l2", "fail", "pending"].includes(qs.get("status")) ? qs.get("status") : "all",
      q: qs.get("q") || ""
    };
  };
  const state = readHashState();
  if (search) search.value = state.q;

  const apply = (writeHash = false) => {
    tierEl.querySelectorAll("[data-tier-val]").forEach((b) => b.setAttribute("aria-pressed", String(b.dataset.tierVal === state.tier)));
    statEl.querySelectorAll("[data-status-val]").forEach((b) => b.setAttribute("aria-pressed", String(b.dataset.statusVal === state.status)));
    const q = state.q.trim().toLowerCase();
    let shown = 0;
    rows.forEach((row) => {
      const activeLevels = [row.dataset.l1, row.dataset.l2, ...(showL3 ? [row.dataset.l3] : []), ...(showL4 ? [row.dataset.l4] : [])];
      const hasFail = [row.dataset.l1, row.dataset.l2, row.dataset.l3, row.dataset.l4].includes("fail");
      const statusOk = state.status === "all"
        || (state.status === "l1" && row.dataset.l1 === "pass")
        || (state.status === "l2" && row.dataset.l2 === "pass")
        || (state.status === "fail" && hasFail)
        || (state.status === "pending" && activeLevels.includes("pending"));
      const ok = (state.tier === "all" || row.dataset.tier === state.tier) && statusOk && (!q || row.dataset.name.includes(q));
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
  if (typeof window !== "undefined") {
    window.addEventListener("hashchange", () => {
      if (!String(window.location?.hash || "").startsWith("#projects")) return;
      const next = readHashState();
      state.tier = next.tier;
      state.status = next.status;
      state.q = next.q;
      if (search) search.value = state.q;
      apply(false);
    });
  }
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

function scrollToHash(doc) {
  if (typeof window === "undefined" || typeof doc?.getElementById !== "function") return;
  const raw = String(window.location?.hash || "");
  if (!raw || raw.length < 2) return;
  const rawId = raw.slice(1).split("?")[0];
  if (!rawId) return;
  let id = rawId;
  try { id = decodeURIComponent(rawId); } catch (_) {}
  const target = doc.getElementById(id);
  if (!target) return;
  target.scrollIntoView?.({ behavior: "instant", block: "start" });
  if (target.hasAttribute?.("data-reveal")) target.classList?.add("is-revealed");
  revealVisible(doc);
}

async function init(doc) {
  const root = doc.getElementById("report");
  if (!root) return;
  const motion = createMotionController(doc);
  let data;
  try {
    data = await loadLatestData();
  } catch (err) {
    console.error("Failed to load data/latest.json:", err);
    root.classList?.remove("is-loading");
    root.innerHTML = '<p class="load-error">Could not load data/latest.json. Serve the site over HTTP.</p>';
    return;
  }
  try {
    doc.getElementById("report-meta").textContent = snapshotMeta(data);
    root.classList?.remove("is-loading");
    root.innerHTML = renderReport(data);
    buildRail(doc, data);
    wireProjectFilter(doc);
    wireShards(doc, data?.tracks?.vcpkgMatrix?.shards || []);
    initSegPills(root);
    revealScan(root);
    motion.scan(root);
    const fEl = root.querySelector("[data-fuzz-target]");
    if (fEl) motion.observeReveal(doc.getElementById("gates"), () => animateCountUp(fEl, Number(fEl.dataset.fuzzTarget), { duration: 800 }));
    scrollToHash(doc);
    if (typeof window !== "undefined") {
      window.addEventListener("hashchange", () => scrollToHash(doc));
    }
  } catch (err) {
    console.error("Failed to render compatibility report:", err);
    root.classList?.remove("is-loading");
    root.innerHTML = '<p class="load-error">Could not render the compatibility report from data/latest.json.</p>';
  }
}

if (typeof document !== "undefined") init(document);
