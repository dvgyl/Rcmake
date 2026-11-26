/* rcmake pure SVG and HTML chart builders. Zero DOM access. MIT OR Apache-2.0. */

import { escapeHtml, formatNumber, formatPercent, normalizeStatus, statusHtml } from "./app.js";

const STATUS_META = {
  pass: { id: "i-check", word: "pass" },
  partial: { id: "i-differs", word: "partial" },
  fail: { id: "i-cross", word: "fail" },
  pending: { id: "i-pending", word: "pending" }
};

export function isMeasured(v) {
  return typeof v === "number" && Number.isFinite(v);
}

const safeText = (v) => (typeof v === "string" ? escapeHtml(v) : isMeasured(v) ? escapeHtml(v) : "");

export function formatSpeed(v) {
  return isMeasured(v) && v > 0 ? v.toFixed(2) : null;
}

export function normalizeGiants(benchTrack) {
  const gb = benchTrack?.giantsBenchmark;
  if (gb && Array.isArray(gb.projects) && gb.projects.length > 0) return gb;
  const rg = benchTrack?.realGiants;
  if (!Array.isArray(rg) || rg.length === 0) return null;
  const projects = rg.map((item) => ({
    name: item?.project || item?.name || "",
    repository: item?.repository || "",
    version: item?.version || "",
    status: normalizeStatus(item?.status),
    note: item?.note || "",
    cold: {
      speedup: item?.coldConfigureSpeedup ?? null,
      rcmakeMedianMs: item?.rcmakeMedianMs ?? null,
      cmakeMedianMs: item?.oracleMedianMs ?? null,
      rcmakePeakRssMiB: item?.peakRssMiB ?? null
    },
    warmProbe: {
      speedup: item?.warmProbeSpeedup ?? null
    },
    incrementalLeaf: {
      rcmakeP95Ms: item?.incrementalLeafP95Ms ?? null
    }
  }));
  const statuses = projects.map((p) => p.status);
  const status = statuses.includes("fail") ? "fail" : statuses.every((s) => s === "pass") ? "pass" : statuses.some((s) => s !== "pending") ? "partial" : "pending";
  return { label: "Real-world large projects", status, projects };
}

export function statusIcon(status) {
  const s = normalizeStatus(status);
  const m = STATUS_META[s];
  return `<span class="st st-${s}" data-status="${s}"><svg class="i" aria-hidden="true"><use href="#${m.id}"/></svg><span class="visually-hidden">${m.word}</span></span>`;
}

export function inlineBar(ratio, status) {
  const s = normalizeStatus(status);
  if (!isMeasured(ratio) || ratio < 0) return '<div class="bar-track inline-bar is-pending" data-loop data-status="pending" aria-hidden="true"></div>';
  const v = Number(Math.max(0.03, Math.min(1, ratio)).toFixed(4));
  const cls = s === "fail" ? "is-fail" : s === "partial" ? "is-partial" : "is-pass";
  return `<div class="bar-track inline-bar" data-status="${s}" aria-hidden="true"><div class="bar-fill ${cls}" style="--v:${v}"></div></div>`;
}

export function speedupBars({ label, baseline = "", value, unit = "x", status, testId } = {}) {
  const lbl = safeText(label);
  const sub = safeText(baseline);
  const nameBlock = sub ? `<div class="speed-name"><span>${lbl}</span><span class="speed-sub">${sub}</span></div>` : `<div class="speed-name">${lbl}</div>`;
  const measured = isMeasured(value) && value > 0;
  const attrs = testId ? ` data-test="${safeText(testId)}"` : "";
  if (!measured) {
    return `<figure class="speed-group" aria-label="${lbl}. Pending." data-status="pending"${attrs}>${nameBlock}<div class="speed-lanes"><div class="speed-lane"><span class="speed-bin">cmake 4.5.1</span><div class="bar-track is-pending" data-loop></div><span class="speed-val dim"><span data-value></span>pending</span></div><div class="speed-lane"><span class="speed-bin is-rcmake">rcmake</span><div class="bar-track is-pending" data-loop></div><span class="speed-val dim"><span data-value></span>pending</span></div></div></figure>`;
  }
  const st = normalizeStatus(status === "pending" ? "pass" : status);
  const isRatio = unit === "x";
  const u = isRatio ? "\u00d7" : safeText(unit) ? ` ${safeText(unit)}` : "";
  let cmScale = "1";
  let rcScale = "1";
  let spd = `${formatSpeed(value)}${u}`;
  let rcCls = "is-rcmake";
  let ariaText = `${lbl}. CMake 4.5.1 is the 1.00 times baseline, and rcmake is ${formatSpeed(value)}${isRatio ? " times" : ` ${unit}`}.`;
  if (isRatio) {
    if (value > 1) {
      rcScale = String(Number(Math.min(1, Math.max(0.02, 1 / value)).toFixed(4)));
      spd = `${formatSpeed(value)}\u00d7 faster`;
      ariaText = `${lbl}. CMake 4.5.1 is the 1.00 times baseline, and rcmake is ${formatSpeed(value)} times faster.`;
    } else if (value < 1) {
      const slower = (1 / value).toFixed(2);
      cmScale = String(Number(Math.min(1, Math.max(0.02, value)).toFixed(4)));
      rcScale = "1";
      spd = `${slower}\u00d7 slower`;
      rcCls = st === "fail" ? "is-fail" : "is-partial";
      ariaText = `${lbl}. CMake 4.5.1 is the 1.00 times baseline, and rcmake is ${slower} times slower.`;
    } else {
      spd = "1.00\u00d7";
      ariaText = `${lbl}. rcmake matches CMake 4.5.1 at 1.00 times.`;
    }
  } else {
    rcScale = String(Number(Math.min(1, Math.max(0.02, 1 / value)).toFixed(4)));
  }
  return `<figure class="speed-group" aria-label="${escapeHtml(ariaText)}" data-status="${st}"${attrs}>${nameBlock}<div class="speed-lanes"><div class="speed-lane"><span class="speed-bin">cmake 4.5.1</span><div class="bar-track"><div class="bar-fill is-cmake" style="--v:${cmScale}" data-scale="${cmScale}"></div></div><span class="speed-val dim" data-value>1.00${u}</span></div><div class="speed-lane"><span class="speed-bin is-rcmake">rcmake</span><div class="bar-track"><div class="bar-fill ${rcCls}" style="--v:${rcScale}" data-scale="${rcScale}" data-test="speed-bar-rcmake"></div></div><span class="speed-val" data-value>${spd}</span></div></div></figure>`;
}

export function statRow({ label, value, unit = "", baseline = "", status, testId } = {}) {
  const lbl = safeText(label);
  const base = safeText(baseline);
  const attrs = testId ? ` data-test="${safeText(testId)}"` : "";
  if (!isMeasured(value) || value < 0) {
    return `<figure class="stat-card" aria-label="${lbl}. Pending." data-status="pending"${attrs}><div class="stat-num is-pending"><span data-value></span>pending</div><figcaption class="stat-label">${lbl}</figcaption>${base ? `<div class="stat-base">${base}</div>` : ""}</figure>`;
  }
  const st = normalizeStatus(status === "pending" ? "pass" : status);
  const u = safeText(unit);
  const fmt = `${formatNumber(value)}${u ? ` ${u}` : ""}`;
  return `<figure class="stat-card" aria-label="${lbl}. rcmake is ${fmt}." data-status="${st}"${attrs}><div class="stat-num" data-value>${fmt}</div><figcaption class="stat-label">${lbl}</figcaption>${base ? `<div class="stat-base">${base}</div>` : ""}</figure>`;
}

export function compactNumber(v) {
  if (!isMeasured(v)) return null;
  if (Math.abs(v) >= 1e6) return `${Number((v / 1e6).toFixed(v % 1e6 === 0 ? 0 : 1))}M`;
  if (Math.abs(v) >= 1e4) return `${Number((v / 1e3).toFixed(0))}K`;
  return formatNumber(v);
}

export function statTile({ label, value, total, unit = "", detail = "", status, tag = "", testId } = {}) {
  const lbl = safeText(label);
  const attrs = testId ? ` data-test="${safeText(testId)}"` : "";
  const ok = isMeasured(value);
  const st = ok ? normalizeStatus(status) : "pending";
  const main = ok
    ? `${escapeHtml(compactNumber(value))}${isMeasured(total) ? `<span class="tile-total">/${escapeHtml(compactNumber(total))}</span>` : ""}${unit ? `<span class="tile-unit">${safeText(unit)}</span>` : ""}`
    : `<span class="tile-pending">${isMeasured(total) ? `${escapeHtml(formatNumber(total))}${unit ? ` ${safeText(unit)}` : ""}` : "pending"}</span>`;
  const foot = ok ? safeText(detail) : "pending";
  const ariaParts = [
    `${lbl}.`,
    ok
      ? `${formatNumber(value)}${isMeasured(total) ? ` of ${formatNumber(total)}` : ""}${unit ? ` ${unit}` : ""}.`
      : "Pending."
  ];
  if (ok && detail) ariaParts.push(`${String(detail).replace(/\.$/, "")}.`);
  if (ok && tag) ariaParts.push(`${String(tag).replace(/\.$/, "")}.`);
  const aria = ariaParts.join(" ");
  return `<figure class="tile" data-status="${st}" aria-label="${escapeHtml(aria)}"${attrs}><div class="tile-num" data-value>${main}</div><figcaption class="tile-label">${lbl}</figcaption><div class="tile-foot">${statusIcon(st)}<span>${foot}</span>${tag ? `<span class="tile-tag">${safeText(tag)}</span>` : ""}</div></figure>`;
}

export function osStripsHtml(platforms, { testPrefix = "os-strip" } = {}) {
  return (platforms || []).map((p) => {
    const hasCount = isMeasured(p.suitesPassed) && isMeasured(p.suitesTotal) && p.suitesTotal > 0;
    const pct = isMeasured(p.passRate) ? Math.min(100, Math.max(0, p.passRate)) : hasCount ? (p.suitesPassed / p.suitesTotal) * 100 : null;
    const st = pct === null ? "pending" : normalizeStatus(p.status);
    const name = String(p.name || "").split(" ")[0];
    const arch = String(p.name || "").split(" ").slice(1).join(" ");
    const valLabel = hasCount
      ? (isMeasured(p.passRate) ? `${formatPercent(p.passRate)} \u00b7 ${formatNumber(p.suitesPassed)}/${formatNumber(p.suitesTotal)}` : `${formatNumber(p.suitesPassed)} / ${formatNumber(p.suitesTotal)}`)
      : formatPercent(pct);
    const value = pct === null
      ? `<span class="dim"><span data-value></span>${escapeHtml(formatNumber(p.suitesTotal) ?? "")} suites</span>`
      : `<span data-value>${escapeHtml(valLabel)}</span>`;
    const bar = pct === null
      ? '<div class="bar-track is-pending" data-loop aria-hidden="true"></div>'
      : `<div class="bar-track" aria-hidden="true"><div class="bar-fill ${st === "fail" ? "is-fail" : st === "partial" ? "is-partial" : "is-pass"}" style="--v:${Number(Math.max(0.01, pct / 100).toFixed(4))}"></div></div>`;
    const aria = pct === null ? `${p.name}. ${p.suitesTotal} Tests/RunCMake suites pending.` : `${p.name}. ${hasCount ? `${p.suitesPassed} of ${p.suitesTotal}` : formatPercent(pct)} Tests/RunCMake suites pass.`;
    return `<figure class="os-strip" data-status="${st}" aria-label="${escapeHtml(aria)}" data-test="${escapeHtml(testPrefix)}-${escapeHtml(p.id)}"><div class="os-head"><span class="os-name">${escapeHtml(name)}<span class="os-arch">${escapeHtml(arch)}</span></span><span class="os-val">${value}${statusIcon(st)}</span></div>${bar}</figure>`;
  }).join("");
}

export function horizontalBar({ label, passed, total, rate, status, testId, detail = "" } = {}) {
  const lbl = safeText(label);
  const hasCounts = isMeasured(passed) && isMeasured(total) && total > 0 && passed >= 0;
  const pct = isMeasured(rate) && rate >= 0 ? Math.min(100, rate) : hasCounts ? Math.min(100, (passed / total) * 100) : null;
  const attrs = testId ? ` data-test="${safeText(testId)}"` : "";
  const titleAttr = detail ? ` title="${safeText(detail)}"` : "";
  if (pct === null) {
    const scope = isMeasured(total) && total > 0 ? `/ ${formatNumber(total)}` : "pending";
    return `<div class="hbar-row" data-status="pending"${attrs}${titleAttr}><span class="hbar-label">${lbl}</span><div class="bar-track is-pending" data-loop></div><span class="hbar-val dim"><span data-value></span>${escapeHtml(scope)}</span><span class="hbar-status">${statusIcon("pending")}</span></div>`;
  }
  const st = normalizeStatus(status);
  const v = Number(Math.max(0.01, Math.min(1, pct / 100)).toFixed(4));
  const val = hasCounts ? `${formatNumber(passed)} / ${formatNumber(total)}` : formatPercent(pct);
  const cls = st === "fail" ? "is-fail" : st === "partial" ? "is-partial" : "is-pass";
  return `<div class="hbar-row" data-status="${st}"${attrs}${titleAttr}><span class="hbar-label">${lbl}</span><div class="bar-track"><div class="bar-fill ${cls}" style="--v:${v}"></div></div><span class="hbar-val" data-value>${escapeHtml(val)}</span><span class="hbar-status">${statusIcon(st)}</span></div>`;
}

export function arcPath(cx, cy, r, startDeg, endDeg) {
  const x = isMeasured(cx) ? cx : 44;
  const y = isMeasured(cy) ? cy : 44;
  const rad = isMeasured(r) && r > 0 ? r : 1;
  const s = isMeasured(startDeg) ? startDeg : 0;
  const span = Math.max(0.01, Math.min(359.99, (isMeasured(endDeg) ? endDeg : s) - s));
  const sR = ((s - 90) * Math.PI) / 180;
  const eR = ((s + span - 90) * Math.PI) / 180;
  return `M ${(x + rad * Math.cos(sR)).toFixed(2)} ${(y + rad * Math.sin(sR)).toFixed(2)} A ${rad.toFixed(2)} ${rad.toFixed(2)} 0 ${span > 180 ? 1 : 0} 1 ${(x + rad * Math.cos(eR)).toFixed(2)} ${(y + rad * Math.sin(eR)).toFixed(2)}`;
}

export function segmentRing({ passed, total, status, size = 88, stroke = 6, gapDeg = 1.25, preFreeze = false } = {}) {
  const sz = isMeasured(size) && size > 16 ? Math.round(size) : 88;
  const sw = isMeasured(stroke) && stroke > 0 ? stroke : 6;
  const gap = isMeasured(gapDeg) && gapDeg >= 0 ? gapDeg : 1.25;
  const rawTotal = isMeasured(total) && total > 0 ? Math.floor(total) : 0;
  const n = Math.min(64, rawTotal);
  const c = sz / 2;
  const r = Math.max(4, (sz - sw) / 2);
  const ok = rawTotal > 0 && isMeasured(passed) && passed >= 0;
  const rawPassed = ok ? Math.min(rawTotal, Math.floor(passed)) : 0;
  let segHit = 0;
  if (ok) {
    segHit = rawTotal === n ? rawPassed : Math.round((rawPassed / rawTotal) * n);
    if (rawPassed < rawTotal && n > 1 && segHit >= n) segHit = n - 1;
    if (rawPassed > 0 && segHit === 0) segHit = 1;
  }
  const step = n > 0 ? 360 / n : 0;
  const arc = n > 0 ? Math.max(0.8, step - gap) : 0;
  const len = Number((r * ((arc * Math.PI) / 180)).toFixed(2));
  const st = ok ? (preFreeze ? "partial" : normalizeStatus(status)) : "pending";
  let paths = "";
  for (let i = 0; i < n; i += 1) {
    const start = i * step + gap / 2;
    const d = arcPath(c, c, r, start, start + arc);
    if (!ok) paths += `<path class="ring-seg is-pending" data-seg-status="pending" d="${d}"/>`;
    else {
      const segSt = preFreeze ? "partial" : i < segHit ? "pass" : st === "partial" ? "partial" : "fail";
      paths += `<path class="ring-seg is-${segSt}" data-seg-status="${segSt}" d="${d}" stroke-width="${sw}" stroke-dasharray="${len}" style="--len:${len};--seg-i:${i}"/>`;
    }
  }
  const centerLabel = rawTotal > 0 ? `${compactNumber(rawPassed)}/${compactNumber(rawTotal)}` : "-";
  const pendingLabel = rawTotal > 0 ? compactNumber(rawTotal) : "-";
  const center = ok
    ? `<text class="ring-val" x="${c}" y="${(c + 6).toFixed(1)}" text-anchor="middle" data-value>${escapeHtml(centerLabel)}</text>`
    : `<text class="ring-val is-pending" x="${c}" y="${(c + 2).toFixed(1)}" text-anchor="middle">${escapeHtml(pendingLabel)}</text><text class="ring-sub" x="${c}" y="${(c + 16).toFixed(1)}" text-anchor="middle">pending</text>`;
  return `<svg class="ring-svg" width="${sz}" height="${sz}" viewBox="0 0 ${sz} ${sz}" data-status="${st}" aria-hidden="true">${paths}${center}</svg>`;
}

export function rateArc({ rate, status, size = 88, stroke = 6 } = {}) {
  const sz = isMeasured(size) && size > 16 ? Math.round(size) : 88;
  const sw = isMeasured(stroke) && stroke > 0 ? stroke : 6;
  const c = sz / 2;
  const r = Math.max(4, (sz - sw) / 2);
  if (!isMeasured(rate) || rate < 0) {
    return `<svg class="ring-svg" width="${sz}" height="${sz}" viewBox="0 0 ${sz} ${sz}" data-status="pending" aria-hidden="true"><path class="ring-seg is-pending" d="${arcPath(c, c, r, 0, 359.99)}"/><text class="ring-sub" x="${c}" y="${(c + 4).toFixed(1)}" text-anchor="middle">pending</text></svg>`;
  }
  const pct = Math.max(0, Math.min(100, rate));
  const deg = Math.max(0.5, (pct / 100) * 359.99);
  const len = Number((r * ((deg * Math.PI) / 180)).toFixed(2));
  const st = normalizeStatus(status);
  const cls = st === "fail" ? "is-fail" : st === "partial" ? "is-partial" : "is-pass";
  return `<svg class="ring-svg" width="${sz}" height="${sz}" viewBox="0 0 ${sz} ${sz}" data-status="${st}" aria-hidden="true"><path fill="none" stroke="var(--line)" stroke-width="${sw}" d="${arcPath(c, c, r, 0, 359.99)}"/><path class="ring-seg ${cls}" d="${arcPath(c, c, r, 0, deg)}" stroke-width="${sw}" stroke-dasharray="${len}" style="--len:${len}"/><text class="ring-val" x="${c}" y="${(c + 6).toFixed(1)}" text-anchor="middle" data-value>${escapeHtml(formatPercent(pct))}</text></svg>`;
}

export function waffle({ passed, total, cols = 53, rows = 7, status, cell = 7, gap = 2 } = {}) {
  const c = isMeasured(cols) && cols > 0 ? Math.min(100, Math.floor(cols)) : 53;
  const r = isMeasured(rows) && rows > 0 ? Math.min(40, Math.floor(rows)) : 7;
  const sz = isMeasured(cell) && cell > 0 ? cell : 7;
  const g = isMeasured(gap) && gap >= 0 ? gap : 2;
  const n = isMeasured(total) && total > 0 ? Math.min(c * r, Math.floor(total)) : 0;
  const ok = n > 0 && isMeasured(passed) && passed >= 0;
  const hit = ok ? Math.min(n, Math.floor(passed)) : 0;
  const st = ok ? normalizeStatus(status) : "pending";
  let rects = "";
  for (let i = 0; i < n; i += 1) {
    const col = i % c;
    const row = Math.floor(i / c);
    const cls = !ok ? "is-pending" : i < hit ? "is-pass" : "is-fail";
    rects += `<rect class="wf-cell ${cls}" x="${col * (sz + g)}" y="${row * (sz + g)}" width="${sz}" height="${sz}" rx="${(sz * 0.32).toFixed(1)}" style="--c:${col}"/>`;
  }
  return `<svg class="waffle-svg" viewBox="0 0 ${c * (sz + g) - g} ${r * (sz + g) - g}" data-status="${st}" data-cells="${n}" aria-hidden="true">${rects}</svg>`;
}

export function waffleRowsHtml(platforms, { cell = 7, gap = 2, testPrefix = "waffle" } = {}) {
  return (platforms || []).map((p) => {
    const ok = isMeasured(p.suitesPassed);
    const st = ok ? normalizeStatus(p.status) : "pending";
    const aria = ok ? `${p.name}. ${p.suitesPassed} of ${p.suitesTotal} Tests/RunCMake suites pass.` : `${p.name}. ${p.suitesTotal} Tests/RunCMake suites pending.`;
    const visual = ok
      ? waffle({ passed: p.suitesPassed, total: p.suitesTotal, cols: 53, rows: 7, status: st, cell, gap })
      : '<div class="bar-track waffle-pending-bar is-pending" data-loop data-status="pending" aria-hidden="true"></div>';
    const footParts = [];
    if (ok) {
      if (isMeasured(p.passRate)) footParts.push(formatPercent(p.passRate));
      footParts.push(`${formatNumber(p.suitesPassed)} of ${formatNumber(p.suitesTotal)} suites pass`);
      if (isMeasured(p.casesPassed) && isMeasured(p.casesTotal)) {
        footParts.push(`${formatNumber(p.casesPassed)} / ${formatNumber(p.casesTotal)} cases`);
      }
      if (p.partial && isMeasured(p.completedSuites) && isMeasured(p.expectedSuites)) {
        footParts.push(`partial run, ${formatNumber(p.completedSuites)} of ${formatNumber(p.expectedSuites)} suites completed`);
      }
    } else {
      footParts.push(`${formatNumber(p.suitesTotal)} RunCMake suites`);
    }
    return `<figure class="waffle-row${ok ? "" : " is-pending-row"}" aria-label="${escapeHtml(aria)}" data-status="${st}" data-test="${escapeHtml(testPrefix)}-${escapeHtml(p.id)}"><div class="waffle-head"><div class="waffle-os"><span class="waffle-name">${escapeHtml(p.name)}</span><span class="waffle-tc">${escapeHtml(p.toolchain)}</span></div>${statusHtml(st)}</div>${visual}<div class="waffle-foot"><span>${escapeHtml(footParts.join(" \u00b7 "))}</span></div></figure>`;
  }).join("");
}

export function fuzzGaugeCard({ label, passed, failures, status, summary, detail = "", testId, preFreeze = false } = {}) {
  const ok = isMeasured(passed);
  const st = ok ? (preFreeze ? "partial" : normalizeStatus(status)) : "pending";
  const lbl = safeText(label);
  const failCount = failures ?? 0;
  const subText = failCount === 0 ? "0 panics" : failCount === 1 ? "1 failure" : `${formatNumber(failCount)} failures`;
  const aria = ok ? `${lbl}. ${formatNumber(passed)} inputs, ${subText}.${preFreeze ? " Pre-freeze baseline." : ""}` : `${lbl}. Pending.`;
  const titleAttr = detail ? ` title="${safeText(detail)}"` : "";
  return `<div class="gauge-card" data-status="${st}"${testId ? ` data-test="${safeText(testId)}"` : ""}${titleAttr}><figure class="gauge-fig" aria-label="${escapeHtml(aria)}"><div class="fuzz-box">${ok ? `<span class="fuzz-num" data-fuzz-target="${passed}" data-value>${escapeHtml(formatNumber(passed))}</span><div class="fuzz-bar" aria-hidden="true"><div class="fuzz-bar-fill"></div></div><span class="gauge-sub">${escapeHtml(subText)}</span>` : '<span class="fuzz-num dim"><span data-value></span>pending</span>'}</div><figcaption class="gauge-label">${lbl}</figcaption></figure>${summary ? `<div class="gate-summary">${safeText(summary)}</div>` : ""}</div>`;
}

export function ringGaugeCard({ label, passed, total, status, summary, sub = "", detail = "", testId, preFreeze = false } = {}) {
  const ok = isMeasured(passed) && isMeasured(total) && total > 0;
  const st = ok ? (preFreeze ? "partial" : normalizeStatus(status)) : "pending";
  const lbl = safeText(label);
  const aria = ok ? `${lbl}. ${passed} of ${total} pass.${sub ? ` ${sub}.` : ""}${preFreeze ? " Pre-freeze baseline." : ""}` : `${lbl}. Pending.`;
  const titleAttr = detail ? ` title="${safeText(detail)}"` : "";
  return `<div class="gauge-card" data-status="${st}"${testId ? ` data-test="${safeText(testId)}"` : ""}${titleAttr}><figure class="gauge-fig" aria-label="${escapeHtml(aria)}">${segmentRing({ passed, total, status: st, preFreeze })}<figcaption class="gauge-label">${lbl}</figcaption></figure>${sub ? `<div class="gauge-sub">${safeText(sub)}</div>` : ""}${summary ? `<div class="gate-summary">${safeText(summary)}</div>` : ""}</div>`;
}

export function shardHeatmap(shards, { cols = 8 } = {}) {
  const c = isMeasured(cols) && cols > 0 ? Math.floor(cols) : 8;
  return (Array.isArray(shards) ? shards : []).map((s, idx) => {
    const id = isMeasured(s?.shard) ? Math.floor(s.shard) : idx;
    const st = normalizeStatus(s?.status);
    const hasScored = isMeasured(s?.portsPassed) && isMeasured(s?.portsTotal) && s.portsTotal > 0 && s.portsPassed >= 0;
    const measured = hasScored || st !== "pending";
    const pct = hasScored ? (s.portsPassed / s.portsTotal) * 100 : -1;
    const bucket = !measured ? "is-pending" : !hasScored ? "hm-0" : pct >= 100 ? "hm-4" : pct >= 99 ? "hm-3" : pct >= 95 ? "hm-2" : pct >= 80 ? "hm-1" : "hm-0";
    const cls = `hm-cell ${bucket}${st === "fail" ? " is-fail" : st === "partial" ? " is-partial" : ""}`;
    const tip = hasScored
      ? `shard ${id} \u00b7 ${formatNumber(s.portsPassed)} / ${formatNumber(s.portsTotal)} ports${isMeasured(s.durationSeconds) ? ` \u00b7 ${Math.round(s.durationSeconds)} s` : ""}`
      : measured
        ? `shard ${id} \u00b7 ${st}${isMeasured(s?.portsInspected) ? ` \u00b7 ${formatNumber(s.portsInspected)} inspected` : ""}`
        : `shard ${id} \u00b7 pending`;
    return `<button type="button" class="${cls}" data-shard="${id}" data-status="${st}" style="--diag:${Math.floor(idx / c) + (idx % c)}" tabindex="${idx === 0 ? 0 : -1}" aria-pressed="false" data-tip="${escapeHtml(tip)}" aria-label="${escapeHtml(tip)}">${String(id).padStart(2, "0")}</button>`;
  }).join("");
}
