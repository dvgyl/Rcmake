/* rcmake shared site helpers. Plain ES module, importable in Node. MIT OR Apache-2.0. */

const STATUSES = ["pass", "partial", "fail", "pending"];
const ICON_BY_STATUS = { pass: "i-check", partial: "i-differs", fail: "i-cross", pending: "i-pending" };

export const TRACK_LABELS = Object.freeze({
  coreConformance: "Core suites",
  policies: "Policies",
  runCmakeOs: "Tests/RunCMake by OS",
  bossFight: "Large projects",
  unseenHoldout: "Blind holdout",
  vcpkgMatrix: "vcpkg ports",
  benchmarksAndGates: "Speed and soundness"
});

export function trackLabel(id, data) {
  if (id === "policies" && data?.tracks?.policies?.activeRange) return `Policies ${data.tracks.policies.activeRange}`;
  return TRACK_LABELS[id] || String(id || "");
}

export function escapeHtml(value) {
  return String(value ?? "")
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}

export function formatNumber(value) {
  return typeof value === "number" && Number.isFinite(value) ? value.toLocaleString("en-US") : null;
}

export function formatPercent(value) {
  if (typeof value !== "number" || !Number.isFinite(value)) return null;
  return `${value.toFixed(Number.isInteger(value) ? 0 : 2)}%`;
}

export function formatRatio(passed, total) {
  const p = formatNumber(passed);
  const t = formatNumber(total);
  return p !== null && t !== null ? `${p} / ${t}` : null;
}

export function cleanNote(text) {
  return String(text ?? "").trim();
}

export function isPreFreezeGate(gate, data) {
  return !data?.commit && Boolean(gate?.preFreeze);
}

export function normalizeStatus(status) {
  return STATUSES.includes(status) ? status : "pending";
}

export function statusHtml(status, text) {
  const s = normalizeStatus(status);
  return `<span class="st st-${s}" data-status="${s}"><svg class="i" aria-hidden="true"><use href="#${ICON_BY_STATUS[s]}"/></svg><span>${escapeHtml(text || s)}</span></span>`;
}

export async function loadLatestData(url = "./data/latest.json") {
  const res = await fetch(url, { cache: "no-cache" });
  if (!res.ok) throw new Error(`HTTP ${res.status} for ${url}`);
  return res.json();
}

export function initThemeToggle(doc = document) {
  const root = doc.documentElement;
  const btn = doc.getElementById("theme-toggle");
  if (!root.dataset.theme) root.dataset.theme = "dark";
  const sync = () => {
    if (!btn) return;
    const dark = root.dataset.theme !== "light";
    const lbl = btn.querySelector(".theme-label");
    if (lbl) lbl.textContent = dark ? "Light" : "Dark";
    else if (!btn.querySelector("svg")) btn.textContent = dark ? "Light" : "Dark";
    btn.setAttribute("aria-label", dark ? "Switch to light theme" : "Switch to dark theme");
  };
  sync();
  btn?.addEventListener("click", () => {
    root.dataset.theme = root.dataset.theme === "light" ? "dark" : "light";
    try { localStorage.setItem("rcmake-theme", root.dataset.theme); } catch (_) {}
    sync();
  });
}

export function initTopbarScroll(doc = document) {
  const bar = doc.querySelector(".topbar.is-hero-bleed");
  if (!bar || typeof window === "undefined") return;
  const sync = () => bar.classList.toggle("is-scrolled", window.scrollY > 10);
  sync();
  window.addEventListener("scroll", sync, { passive: true });
}

export async function copyWithFeedback(button, text, label = "Copy") {
  const node = button.querySelector("[data-copy-label]");
  const set = (v) => { if (node) node.textContent = v; else button.textContent = v; };
  try { await navigator.clipboard.writeText(text); set("Copied"); } catch (_) { set("Select and copy"); }
  setTimeout(() => set(label), 1500);
}

export function initRovingGroup(container, selector, onActivate) {
  if (!container) return;
  const items = () => Array.from(container.querySelectorAll(selector));
  const sync = (active) => items().forEach((el) => { el.tabIndex = el === active ? 0 : -1; });
  const initial = items().find((el) =>
    ["aria-selected", "aria-checked", "aria-pressed"].some((a) => el.getAttribute(a) === "true")
  ) || items()[0];
  if (initial) sync(initial);
  container.addEventListener("keydown", (e) => {
    const list = items();
    const idx = list.indexOf(e.target.closest(selector));
    if (idx < 0 || !list.length) return;
    let next = -1;
    if (e.key === "ArrowRight" || e.key === "ArrowDown") next = (idx + 1) % list.length;
    else if (e.key === "ArrowLeft" || e.key === "ArrowUp") next = (idx - 1 + list.length) % list.length;
    else if (e.key === "Home") next = 0;
    else if (e.key === "End") next = list.length - 1;
    if (next < 0) return;
    e.preventDefault();
    sync(list[next]);
    list[next].focus();
    onActivate?.(list[next]);
  });
  container.addEventListener("click", (e) => {
    const target = e.target.closest(selector);
    if (!target || !container.contains(target)) return;
    sync(target);
    onActivate?.(target);
  });
}

const ratioOf = (p, t) =>
  typeof p === "number" && Number.isFinite(p) && typeof t === "number" && t > 0 ? Math.max(0, Math.min(1, p / t)) : null;

export function overviewRows(data) {
  const t = data?.tracks || {};
  const gates = t.benchmarksAndGates?.gates || [];
  const gate = (id) => gates.find((g) => g.id === id) || { status: "pending" };
  const [bb, fuzz, wasi] = [gate("blackbox"), gate("fuzz"), gate("wasi")];
  const { coreConformance: core = {}, policies: pol = {}, runCmakeOs: rc = {}, bossFight: boss = {}, unseenHoldout: hold = {}, vcpkgMatrix: vc = {} } = t;
  const plats = rc.platforms || [];
  const osDone = plats.length > 0 && plats.every((p) => typeof p.passRate === "number");
  const warm = t.benchmarksAndGates?.benchmarks?.warmProbeSpeedup || {};

  const fuzzPre = isPreFreezeGate(fuzz, data);
  const wasiPre = isPreFreezeGate(wasi, data);
  const fuzzTag = fuzzPre && fuzz.commit ? ` (${fuzz.commit})` : "";
  const wasiTag = wasiPre && wasi.commit ? ` (${wasi.commit})` : "";
  const fuzzUnit = fuzz.failures ? "fail" : "panics";

  return [
    { id: "core", check: "Core suites", result: formatRatio(core.suitesAt100, core.suitesTotal), scope: `- / ${formatNumber(core.suitesTotal) ?? "?"}`, ratio: ratioOf(core.suitesAt100, core.suitesTotal), status: normalizeStatus(core.status) },
    { id: "core", check: `Policies ${pol.activeRange || ""}`.trim(), result: formatRatio(pol.suitesPassed, pol.suitesTotal), scope: `- / ${formatNumber(pol.suitesTotal) ?? "?"}`, ratio: ratioOf(pol.suitesPassed, pol.suitesTotal), status: normalizeStatus(pol.status) },
    { id: "runcmake", check: "Tests/RunCMake, 3 OS", result: osDone ? plats.map((p) => formatPercent(p.passRate)).join(" / ") : null, scope: `${plats.map((p) => p.suitesTotal).join(" / ")} suites`, ratio: osDone ? plats.reduce((a, p) => a + p.passRate, 0) / (plats.length * 100) : null, status: normalizeStatus(rc.status) },
    { id: "gates", check: "Black-box scenarios", result: formatRatio(bb.scenariosPassed, bb.scenariosTotal), scope: `- / ${formatNumber(bb.scenariosTotal) ?? "?"}`, ratio: ratioOf(bb.scenariosPassed, bb.scenariosTotal), status: normalizeStatus(bb.status) },
    { id: "projects", check: "Large projects, L1 / L2", result: typeof boss.l1Passed === "number" && typeof boss.l2Passed === "number" ? `${boss.l1Passed} / ${boss.l2Passed} of ${boss.projectsTotal}` : null, scope: `- / ${formatNumber(boss.projectsTotal) ?? "?"}`, ratio: ratioOf(boss.l2Passed, boss.projectsTotal), status: normalizeStatus(boss.status) },
    { id: "holdout", check: "Blind holdout", result: formatRatio(hold.reposPassed, hold.reposTotal), scope: `- / ${formatNumber(hold.reposTotal) ?? "?"}`, ratio: ratioOf(hold.reposPassed, hold.reposTotal), status: normalizeStatus(hold.status) },
    { id: "vcpkg", check: "vcpkg ports", result: typeof vc.portsPassed === "number" && typeof vc.portsTotal === "number" ? `${formatRatio(vc.portsPassed, vc.portsTotal)} ports` : null, scope: `${formatNumber(vc.shardsCompleted) ?? "0"} / ${formatNumber(vc.shardsTotal) ?? "64"} shards`, ratio: typeof vc.passRate === "number" ? Math.max(0, Math.min(1, vc.passRate / 100)) : null, status: normalizeStatus(vc.status) },
    { id: "gates", check: "Seeded generative fuzzing", result: typeof fuzz.passed === "number" ? `${formatNumber(fuzz.passed)} inputs, ${fuzz.failures ?? 0} ${fuzzUnit}${fuzzTag}` : null, scope: "pending", ratio: ratioOf(fuzz.passed, fuzz.total), status: fuzzPre ? "partial" : normalizeStatus(fuzz.status), statusText: fuzzPre ? "pre-freeze" : undefined },
    { id: "gates", check: "WASI and wasm32", result: formatRatio(wasi.passed, wasi.total) ? `${formatRatio(wasi.passed, wasi.total)}${wasiTag}` : null, scope: `- / ${formatNumber(wasi.total) ?? "?"}`, ratio: ratioOf(wasi.passed, wasi.total), status: wasiPre ? "partial" : normalizeStatus(wasi.status), statusText: wasiPre ? "pre-freeze" : undefined },
    { id: "speed", check: "New tree, shared probe cache", result: typeof warm.value === "number" && Number.isFinite(warm.value) ? `${warm.value}${warm.unit === "x" ? "\u00d7" : ` ${warm.unit || ""}`}` : null, scope: "pending", ratio: typeof warm.value === "number" && warm.value > 0 ? Math.min(1, 1 / warm.value) : null, status: normalizeStatus(warm.status) }
  ];
}

export function overviewRowsHtml(rows) {
  return rows.map((r) => `<tr><td>${escapeHtml(r.check)}</td><td class="mono">${r.result ? escapeHtml(r.result) : `<span class="dim">${escapeHtml(r.scope)}</span>`}</td><td>${statusHtml(r.status, r.statusText)}</td></tr>`).join("");
}

export function snapshotMeta(data) {
  const d = data?.generatedAt ? String(data.generatedAt).slice(0, 10) : null;
  return [data?.commit ? `commit ${String(data.commit).slice(0, 12)}` : "commit pending", `CMake ${data?.targetCmakeVersion || "4.5.1"}`, d ? `generated ${d}` : "not yet generated"].join(" \u00b7 ");
}

if (typeof document !== "undefined") {
  initThemeToggle(document);
  initTopbarScroll(document);
}
