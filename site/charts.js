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
  const scale = String(Number(Math.min(1, Math.max(0.02, 1 / value)).toFixed(4)));
  const isRatio = unit === "x";
  const u = isRatio ? "\u00d7" : safeText(unit) ? ` ${safeText(unit)}` : "";
  const spd = isRatio ? `${formatSpeed(value)}\u00d7 faster` : `${formatSpeed(value)}${u}`;
  return `<figure class="speed-group" aria-label="${lbl}. CMake 4.5.1 is 1.00 times, rcmake is ${formatSpeed(value)} times faster." data-status="${st}"${attrs}>${nameBlock}<div class="speed-lanes"><div class="speed-lane"><span class="speed-bin">cmake 4.5.1</span><div class="bar-track"><div class="bar-fill is-cmake" style="--v:1" data-scale="1"></div></div><span class="speed-val dim" data-value>1.00${u}</span></div><div class="speed-lane"><span class="speed-bin is-rcmake">rcmake</span><div class="bar-track"><div class="bar-fill is-rcmake" style="--v:${scale}" data-scale="${scale}" data-test="speed-bar-rcmake"></div></div><span class="speed-val" data-value>${spd}</span></div></div></figure>`;
}

export function statRow({ label, value, unit = "", baseline = "", status, testId } = {}) {
  const lbl = safeText(label);
  const base = safeText(baseline);
  const attrs = testId ? ` data-test="${safeText(testId)}"` : "";
  if (!isMeasured(value) || value < 0) {
    return `<figure class="stat-card" aria-label="${lbl}. Pending." data-status="pending"${attrs}><div class="stat-top"><span class="speed-name">${lbl}</span><span class="speed-bin is-rcmake">rcmake</span></div><div class="stat-num is-pending"><span data-value></span>pending</div>${base ? `<div class="stat-base">${base}</div>` : ""}</figure>`;
  }
  const st = normalizeStatus(status === "pending" ? "pass" : status);
  const u = safeText(unit);
  const fmt = `${formatNumber(value)}${u ? ` ${u}` : ""}`;
  return `<figure class="stat-card" aria-label="${lbl}. rcmake ${fmt}." data-status="${st}"${attrs}><div class="stat-top"><span class="speed-name">${lbl}</span><span class="speed-bin is-rcmake">rcmake</span></div><div class="stat-num" data-value>${fmt}</div>${base ? `<div class="stat-base">${base}</div>` : ""}</figure>`;
}

export function horizontalBar({ label, passed, total, rate, status, testId } = {}) {
  const lbl = safeText(label);
  const hasCounts = isMeasured(passed) && isMeasured(total) && total > 0 && passed >= 0;
  const pct = isMeasured(rate) && rate >= 0 ? Math.min(100, rate) : hasCounts ? Math.min(100, (passed / total) * 100) : null;
  const attrs = testId ? ` data-test="${safeText(testId)}"` : "";
  if (pct === null) {
    const scope = isMeasured(total) && total > 0 ? `/ ${formatNumber(total)}` : "pending";
    return `<div class="hbar-row" data-status="pending"${attrs}><span class="hbar-label">${lbl}</span><div class="bar-track is-pending" data-loop></div><span class="hbar-val dim"><span data-value></span>${escapeHtml(scope)}</span><span class="hbar-status">${statusIcon("pending")}</span></div>`;
  }
  const st = normalizeStatus(status);
  const v = Number(Math.max(0.01, Math.min(1, pct / 100)).toFixed(4));
  const val = hasCounts ? `${formatNumber(passed)} / ${formatNumber(total)}` : formatPercent(pct);
  const cls = st === "fail" ? "is-fail" : st === "partial" ? "is-partial" : "is-pass";
  return `<div class="hbar-row" data-status="${st}"${attrs}><span class="hbar-label">${lbl}</span><div class="bar-track"><div class="bar-fill ${cls}" style="--v:${v}"></div></div><span class="hbar-val" data-value>${escapeHtml(val)}</span><span class="hbar-status">${statusIcon(st)}</span></div>`;
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
  const n = isMeasured(total) && total > 0 ? Math.min(64, Math.floor(total)) : 0;
  const c = sz / 2;
  const r = Math.max(4, (sz - sw) / 2);
  const ok = n > 0 && isMeasured(passed) && passed >= 0;
  const hit = ok ? Math.min(n, Math.floor(passed)) : 0;
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
      const segSt = preFreeze ? "partial" : i < hit ? "pass" : st === "partial" ? "partial" : "fail";
      paths += `<path class="ring-seg is-${segSt}" data-seg-status="${segSt}" d="${d}" stroke-width="${sw}" stroke-dasharray="${len}" style="--len:${len};--seg-i:${i}"/>`;
    }
  }
  const center = ok
    ? `<text class="ring-val" x="${c}" y="${(c + 6).toFixed(1)}" text-anchor="middle" data-value>${hit}/${n}</text>`
    : `<text class="ring-val is-pending" x="${c}" y="${(c + 2).toFixed(1)}" text-anchor="middle">${n}</text><text class="ring-sub" x="${c}" y="${(c + 16).toFixed(1)}" text-anchor="middle">pending</text>`;
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
    rects += `<rect class="wf-cell ${cls}" x="${col * (sz + g)}" y="${row * (sz + g)}" width="${sz}" height="${sz}" rx="1" style="--c:${col}"/>`;
  }
  return `<svg class="waffle-svg" viewBox="0 0 ${c * (sz + g) - g} ${r * (sz + g) - g}" data-status="${st}" data-cells="${n}" aria-hidden="true">${rects}</svg>`;
}

export function waffleRowsHtml(platforms, { cell = 7, gap = 2, testPrefix = "waffle" } = {}) {
  return (platforms || []).map((p) => {
    const ok = isMeasured(p.suitesPassed);
    const st = ok ? normalizeStatus(p.status) : "pending";
    const aria = ok ? `${p.name}. ${p.suitesPassed} of ${p.suitesTotal} Tests/RunCMake suites pass.` : `${p.name}. ${p.suitesTotal} Tests/RunCMake suites, pending.`;
    const visual = ok
      ? waffle({ passed: p.suitesPassed, total: p.suitesTotal, cols: 53, rows: 7, status: st, cell, gap })
      : '<div class="bar-track waffle-pending-bar is-pending" data-loop data-status="pending" aria-hidden="true"></div>';
    const footLeft = ok ? `${formatNumber(p.suitesPassed)} of ${formatNumber(p.suitesTotal)} suites pass` : `${formatNumber(p.suitesTotal)} RunCMake suites`;
    return `<figure class="waffle-row${ok ? "" : " is-pending-row"}" aria-label="${escapeHtml(aria)}" data-status="${st}" data-test="${escapeHtml(testPrefix)}-${escapeHtml(p.id)}"><div class="waffle-head"><div class="waffle-os"><span class="waffle-name">${escapeHtml(p.name)}</span><span class="waffle-tc">${escapeHtml(p.toolchain)}</span></div>${statusHtml(st)}</div>${visual}<div class="waffle-foot"><span>${escapeHtml(footLeft)}</span></div></figure>`;
  }).join("");
}

export function fuzzGaugeCard({ label, passed, failures, status, summary, testId, preFreeze = false } = {}) {
  const ok = isMeasured(passed);
  const st = ok ? (preFreeze ? "partial" : normalizeStatus(status)) : "pending";
  const lbl = safeText(label);
  const failCount = failures ?? 0;
  const subText = failCount === 0 ? "0 panics" : `${failCount} failures`;
  const aria = ok ? `${lbl}. ${formatNumber(passed)} inputs, ${subText}.${preFreeze ? " Pre-freeze baseline." : ""}` : `${lbl}. Pending.`;
  return `<div class="gauge-card" data-status="${st}"${testId ? ` data-test="${safeText(testId)}"` : ""}><figure class="gauge-fig" aria-label="${aria}"><div class="fuzz-box">${ok ? `<span class="fuzz-num" data-fuzz-target="${passed}" data-value>${escapeHtml(formatNumber(passed))}</span><div class="fuzz-bar" aria-hidden="true"><div class="fuzz-bar-fill"></div></div><span class="gauge-sub">${escapeHtml(subText)}</span>` : '<span class="fuzz-num dim"><span data-value></span>pending</span>'}</div><figcaption class="gauge-label">${lbl}</figcaption></figure>${summary ? `<div class="gate-summary">${safeText(summary)}</div>` : ""}</div>`;
}

export function ringGaugeCard({ label, passed, total, status, summary, testId, preFreeze = false } = {}) {
  const ok = isMeasured(passed) && isMeasured(total) && total > 0;
  const st = ok ? (preFreeze ? "partial" : normalizeStatus(status)) : "pending";
  const lbl = safeText(label);
  const aria = ok ? `${lbl}. ${passed} of ${total} pass.${preFreeze ? " Pre-freeze baseline." : ""}` : `${lbl}. Pending.`;
  return `<div class="gauge-card" data-status="${st}"${testId ? ` data-test="${safeText(testId)}"` : ""}><figure class="gauge-fig" aria-label="${aria}">${segmentRing({ passed, total, status: st, preFreeze })}<figcaption class="gauge-label">${lbl}</figcaption></figure>${summary ? `<div class="gate-summary">${safeText(summary)}</div>` : ""}</div>`;
}

export function shardHeatmap(shards, { cols = 8 } = {}) {
  const c = isMeasured(cols) && cols > 0 ? Math.floor(cols) : 8;
  return (Array.isArray(shards) ? shards : []).map((s, idx) => {
    const id = isMeasured(s?.shard) ? Math.floor(s.shard) : idx;
    const st = normalizeStatus(s?.status);
    const ok = isMeasured(s?.portsPassed) && isMeasured(s?.portsTotal) && s.portsTotal > 0 && s.portsPassed >= 0;
    const pct = ok ? (s.portsPassed / s.portsTotal) * 100 : -1;
    const bucket = !ok ? "is-pending" : pct >= 100 ? "hm-4" : pct >= 99 ? "hm-3" : pct >= 95 ? "hm-2" : pct >= 80 ? "hm-1" : "hm-0";
    const cls = `hm-cell ${bucket}${st === "fail" ? " is-fail" : ""}`;
    const tip = ok ? `shard ${id} \u00b7 ${formatNumber(s.portsPassed)} / ${formatNumber(s.portsTotal)} ports${isMeasured(s.durationSeconds) ? ` \u00b7 ${Math.round(s.durationSeconds)} s` : ""}` : `shard ${id} \u00b7 pending`;
    return `<button type="button" class="${cls}" data-shard="${id}" data-status="${st}" style="--diag:${Math.floor(idx / c) + (idx % c)}" tabindex="${idx === 0 ? 0 : -1}" aria-pressed="false" title="${escapeHtml(tip)}" aria-label="${escapeHtml(tip)}">${String(id).padStart(2, "0")}</button>`;
  }).join("");
}
