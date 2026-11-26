/* rcmake shared site helpers. Plain ES module, importable in Node. MIT OR Apache-2.0. */

const STATUSES = ["pass", "partial", "fail", "pending"];
const ICON_BY_STATUS = { pass: "i-check", partial: "i-differs", fail: "i-cross", pending: "i-pending" };

export const TRACK_LABELS = Object.freeze({
  coreConformance: "Core suites",
  policies: "Policies",
  runCmakeOs: "Tests/RunCMake by operating system",
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

export function formatMiB(bytes) {
  if (typeof bytes !== "number" || !Number.isFinite(bytes) || bytes <= 0) return null;
  return `${(bytes / (1024 * 1024)).toFixed(2)} MiB`;
}

export function formatBytes(bytes) {
  const mib = formatMiB(bytes);
  if (!mib) return null;
  return `${mib} (${formatNumber(bytes)} bytes)`;
}

export function cleanNote(text) {
  return String(text ?? "").trim();
}

export function isPreFreezeGate(gate, data) {
  if (!gate?.preFreeze) return false;
  if (!data?.commit) return true;
  if (gate.commit) {
    const gc = String(gate.commit).trim().toLowerCase();
    const dc = String(data.commit).trim().toLowerCase();
    if (gc && dc && !dc.startsWith(gc) && !gc.startsWith(dc)) return true;
  }
  return false;
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
  if (root.dataset.theme !== "light" && root.dataset.theme !== "dark") {
    const prefersLight = typeof window !== "undefined" && typeof window.matchMedia === "function" && window.matchMedia("(prefers-color-scheme: light)").matches;
    root.dataset.theme = prefersLight ? "light" : "dark";
  }
  const sync = () => {
    if (!btn) return;
    const dark = root.dataset.theme !== "light";
    btn.setAttribute("aria-label", dark ? "Switch to light theme" : "Switch to dark theme");
    btn.removeAttribute("aria-pressed");
  };
  sync();
  btn?.addEventListener("click", () => {
    const flip = () => {
      root.dataset.theme = root.dataset.theme === "light" ? "dark" : "light";
      try { localStorage.setItem("rcmake-theme", root.dataset.theme); } catch (_) {}
      sync();
    };
    if (typeof doc.startViewTransition === "function" && !reducedMotion()) doc.startViewTransition(flip);
    else flip();
  });
}

function reducedMotion() {
  return typeof window !== "undefined" && typeof window.matchMedia === "function" && window.matchMedia("(prefers-reduced-motion: reduce)").matches;
}

export function initTopbarScroll(doc = document) {
  const bar = doc.querySelector(".topbar");
  const progress = doc.querySelector(".scroll-progress");
  if (typeof window === "undefined" || (!bar && !progress)) return;
  let queued = false;
  const sync = () => {
    queued = false;
    const y = window.scrollY;
    bar?.classList.toggle("is-scrolled", y > 8);
    if (progress) {
      const max = Math.max(1, doc.documentElement.scrollHeight - window.innerHeight);
      progress.style.setProperty("--p", String(Math.min(1, Math.max(0, y / max))));
    }
  };
  sync();
  window.addEventListener("scroll", () => { if (!queued) { queued = true; requestAnimationFrame(sync); } }, { passive: true });
  window.addEventListener("resize", sync, { passive: true });
}

/* Scroll reveal. Elements with [data-reveal] glide in once. Children of [data-reveal-stagger] get a stagger index.
   The hidden state applies only under html.reveal-on, which this function sets after the observer exists. */
let revealIO = null;

export function revealScan(scope = typeof document !== "undefined" ? document : null) {
  if (!scope?.querySelectorAll) return;
  const groups = Array.from(scope.querySelectorAll("[data-reveal-stagger]"));
  if (scope.matches?.("[data-reveal-stagger]")) groups.push(scope);
  groups.forEach((group) => {
    Array.from(group.children).forEach((child, i) => {
      if (!child.hasAttribute("data-reveal")) child.setAttribute("data-reveal", "");
      child.style?.setProperty("--reveal-i", String(i));
    });
  });
  scope.querySelectorAll("[data-reveal]:not(.is-revealed)").forEach((el) => {
    if (revealIO) revealIO.observe(el);
    else el.classList?.add("is-revealed");
  });
  revealVisible(scope.ownerDocument || scope);
}

export function revealVisible(doc = typeof document !== "undefined" ? document : null) {
  if (!doc?.querySelectorAll || typeof window === "undefined") return;
  const vh = window.innerHeight || doc.documentElement?.clientHeight || 0;
  if (!vh) return;
  const cutoff = vh + 80;
  doc.querySelectorAll("[data-reveal]:not(.is-revealed)").forEach((el) => {
    if (typeof el.getBoundingClientRect !== "function") return;
    const rect = el.getBoundingClientRect();
    if (rect.top <= cutoff) {
      el.classList?.add("is-revealed");
      revealIO?.unobserve(el);
    }
  });
}

export function initReveal(doc = document) {
  if (typeof window === "undefined" || typeof IntersectionObserver === "undefined" || reducedMotion()) {
    revealScan(doc);
    return;
  }
  revealIO = new IntersectionObserver((entries) => {
    const vh = window.innerHeight || doc.documentElement?.clientHeight || 0;
    const cutoff = vh + 80;
    for (const e of entries) {
      if (!e.isIntersecting && !(vh && e.boundingClientRect.top <= cutoff)) continue;
      e.target.classList.add("is-revealed");
      revealIO.unobserve(e.target);
    }
  }, { rootMargin: "0px 0px 80px 0px", threshold: 0.01 });
  revealScan(doc);
  doc.documentElement.classList.add("reveal-on");
  revealVisible(doc);
  if (typeof requestAnimationFrame === "function") requestAnimationFrame(() => revealVisible(doc));
  let revealQueued = false;
  const syncReveal = () => {
    revealVisible(doc);
    if (!revealQueued && typeof requestAnimationFrame === "function") {
      revealQueued = true;
      requestAnimationFrame(() => {
        revealQueued = false;
        revealVisible(doc);
      });
    }
  };
  window.addEventListener("scroll", syncReveal, { passive: true });
  window.addEventListener("resize", syncReveal, { passive: true });
  window.addEventListener("hashchange", syncReveal);
  const showAll = () => doc.querySelectorAll("[data-reveal]").forEach((el) => el.classList.add("is-revealed"));
  window.addEventListener("beforeprint", showAll);
}

/* Sliding pill behind the active option of every segmented control and tab bar. */
const ACTIVE_ATTRS = ["aria-selected", "aria-checked", "aria-pressed"];

export function initSegPill(group) {
  if (!group || group.dataset.pill === "on" || typeof window === "undefined") return;
  group.dataset.pill = "on";
  const pill = group.ownerDocument.createElement("span");
  pill.className = "seg-pill";
  pill.setAttribute("aria-hidden", "true");
  group.prepend(pill);
  const place = () => {
    if (pill.parentNode !== group) group.prepend(pill);
    const active = Array.from(group.querySelectorAll("button")).find((b) => ACTIVE_ATTRS.some((a) => b.getAttribute(a) === "true"));
    if (!active || !active.offsetWidth) { pill.style.opacity = "0"; return; }
    pill.style.opacity = "1";
    pill.style.width = `${active.offsetWidth}px`;
    pill.style.height = `${active.offsetHeight}px`;
    pill.style.transform = `translate(${active.offsetLeft}px, ${active.offsetTop}px)`;
  };
  place();
  requestAnimationFrame(() => { place(); group.classList.add("pill-ready"); });
  if (typeof MutationObserver !== "undefined") {
    new MutationObserver(place).observe(group, { subtree: true, childList: true, attributes: true, attributeFilter: ACTIVE_ATTRS });
  }
  if (typeof ResizeObserver !== "undefined") new ResizeObserver(place).observe(group);
  group.addEventListener("scroll", place, { passive: true });
}

export function initSegPills(doc = document) {
  doc.querySelectorAll(".seg, .pane-tabs").forEach(initSegPill);
}

function execCopyFallback(text) {
  if (typeof document === "undefined" || typeof document.execCommand !== "function") return false;
  const ta = document.createElement("textarea");
  ta.value = text;
  ta.setAttribute("readonly", "");
  ta.style.position = "fixed";
  ta.style.opacity = "0";
  ta.style.pointerEvents = "none";
  document.body.appendChild(ta);
  ta.select();
  let ok = false;
  try { ok = document.execCommand("copy"); } catch (_) {}
  ta.remove();
  return ok;
}

export async function copyWithFeedback(button, text, label = "Copy", fallbackEl = null) {
  const node = button.querySelector("[data-copy-label]");
  const set = (v) => { if (node) node.textContent = v; else button.textContent = v; };
  let copied = false;
  try {
    if (navigator.clipboard && typeof navigator.clipboard.writeText === "function") {
      await navigator.clipboard.writeText(text);
      copied = true;
    }
  } catch (_) {}
  if (!copied) copied = execCopyFallback(text);
  if (copied) {
    set("Copied");
    button.classList.add("is-copied");
  } else {
    if (fallbackEl && typeof window !== "undefined" && typeof window.getSelection === "function") {
      try { window.getSelection().selectAllChildren(fallbackEl); } catch (_) {}
    }
    set("Select and copy");
  }
  clearTimeout(button._copyTimer);
  button._copyTimer = setTimeout(() => { set(label); button.classList.remove("is-copied"); }, 1600);
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

function formatSpeedSummary(metric) {
  const v = metric?.value;
  if (typeof v !== "number" || !Number.isFinite(v) || v <= 0) return null;
  if (metric.unit === "x") {
    if (v > 1) return `${v.toFixed(2)}\u00d7 faster`;
    if (v < 1) return `${(1 / v).toFixed(2)}\u00d7 slower`;
    return "1.00\u00d7";
  }
  return `${v.toFixed(2)}${metric.unit ? ` ${metric.unit}` : ""}`;
}

function speedRowRatio(metric) {
  const v = metric?.value;
  if (typeof v !== "number" || !Number.isFinite(v) || v <= 0) return null;
  const st = normalizeStatus(metric.status);
  if (st === "pass") return 1;
  if (st === "fail") return Math.max(0.15, Math.min(0.85, v < 1 ? v : 0.35));
  return 0.5;
}

export function overviewRows(data) {
  const t = data?.tracks || {};
  const gates = t.benchmarksAndGates?.gates || [];
  const findGate = (id) => gates.find((g) => g.id === id);
  const gateOrPending = (id) => findGate(id) || { status: "pending" };
  const [bb, fuzz, wasi, probe, mutation] = [
    gateOrPending("blackbox"),
    gateOrPending("fuzz"),
    gateOrPending("wasi"),
    gateOrPending("probe-cache"),
    gateOrPending("mutation-replay")
  ];
  const conan = findGate("conan-ecosystem");
  const pathSwap = findGate("path-swap");
  const determinism = findGate("determinism");
  const diff = t.benchmarksAndGates?.differentialFuzz;
  const diffTotal = typeof diff?.casesTotal === "number" ? diff.casesTotal : diff?.total;
  const diffPassed = typeof diff?.casesPassed === "number" ? diff.casesPassed : diff?.passed;

  const { coreConformance: core = {}, policies: pol = {}, runCmakeOs: rc = {}, bossFight: boss = {}, unseenHoldout: hold = {}, vcpkgMatrix: vc = {} } = t;
  const plats = rc.platforms || [];
  const measuredPlats = plats.filter((p) => typeof p.passRate === "number");
  const osAny = measuredPlats.length > 0;
  const warm = t.benchmarksAndGates?.benchmarks?.warmProbeSpeedup || {};
  const cold = t.benchmarksAndGates?.benchmarks?.coldConfigureSpeedup || {};

  const fuzzPre = isPreFreezeGate(fuzz, data);
  const wasiPre = isPreFreezeGate(wasi, data);
  const fuzzTag = fuzzPre && fuzz.commit ? ` (${fuzz.commit})` : "";
  const wasiTag = wasiPre && wasi.commit ? ` (${wasi.commit})` : "";
  const fuzzFails = fuzz.failures ?? 0;
  const fuzzUnit = fuzzFails === 0 ? "panics" : fuzzFails === 1 ? "failure" : "failures";

  const hasL3 = typeof boss.l3Passed === "number";
  const hasL4 = typeof boss.l4Passed === "number";
  const bossCheck = hasL4 ? "Large projects, L1 to L4" : hasL3 ? "Large projects, L1 to L3" : "Large projects, L1 and L2";
  let bossResult = null;
  if (typeof boss.l1Passed === "number" && typeof boss.l2Passed === "number") {
    const parts = [boss.l1Passed, boss.l2Passed];
    if (hasL3) parts.push(boss.l3Passed);
    if (hasL4) parts.push(boss.l4Passed);
    bossResult = `${parts.join(" / ")} of ${boss.projectsTotal}`;
  }
  const bossCounts = [boss.l1Passed, boss.l2Passed, ...(hasL3 ? [boss.l3Passed] : []), ...(hasL4 ? [boss.l4Passed] : [])]
    .filter((n) => typeof n === "number" && Number.isFinite(n));
  const bossRatio = bossCounts.length ? ratioOf(Math.min(...bossCounts), boss.projectsTotal) : null;

  const rows = [
    { id: "core", check: "Core suites", result: formatRatio(core.suitesAt100, core.suitesTotal), scope: `- / ${formatNumber(core.suitesTotal) ?? "?"}`, ratio: ratioOf(core.suitesAt100, core.suitesTotal), status: normalizeStatus(core.status) },
    { id: "core", check: `Policies ${pol.activeRange || ""}`.trim(), result: formatRatio(pol.suitesPassed, pol.suitesTotal), scope: `- / ${formatNumber(pol.suitesTotal) ?? "?"}`, ratio: ratioOf(pol.suitesPassed, pol.suitesTotal), status: normalizeStatus(pol.status) },
    { id: "runcmake", check: "Tests/RunCMake across 3 operating systems", result: osAny ? plats.map((p) => formatPercent(p.passRate) ?? "-").join(" / ") : null, scope: `${plats.map((p) => p.suitesTotal).join(" / ")} suites`, ratio: osAny ? measuredPlats.reduce((a, p) => a + p.passRate, 0) / (plats.length * 100) : null, status: normalizeStatus(rc.status) },
    { id: "gates", check: "Black-box scenarios", result: formatRatio(bb.scenariosPassed, bb.scenariosTotal), scope: `- / ${formatNumber(bb.scenariosTotal) ?? "?"}`, ratio: ratioOf(bb.scenariosPassed, bb.scenariosTotal), status: normalizeStatus(bb.status) },
    { id: "projects", check: bossCheck, result: bossResult, scope: `- / ${formatNumber(boss.projectsTotal) ?? "?"}`, ratio: bossRatio, status: normalizeStatus(boss.status) },
    { id: "holdout", check: "Blind holdout", result: formatRatio(hold.reposPassed, hold.reposTotal), scope: `- / ${formatNumber(hold.reposTotal) ?? "?"}`, ratio: ratioOf(hold.reposPassed, hold.reposTotal), status: normalizeStatus(hold.status) },
    { id: "vcpkg", check: "vcpkg ports", result: typeof vc.portsPassed === "number" && typeof vc.portsTotal === "number" ? `${formatRatio(vc.portsPassed, vc.portsTotal)} ports` : null, scope: `${formatNumber(vc.shardsCompleted) ?? "0"} / ${formatNumber(vc.shardsTotal) ?? "64"} shards`, ratio: typeof vc.passRate === "number" ? Math.max(0, Math.min(1, vc.passRate / 100)) : null, status: normalizeStatus(vc.status) }
  ];

  if (conan) {
    rows.push({ id: "gates", check: "Conan ecosystem", result: formatRatio(conan.passed, conan.total), scope: `- / ${formatNumber(conan.total) ?? "?"}`, ratio: ratioOf(conan.passed, conan.total), status: normalizeStatus(conan.status) });
  }
  rows.push(
    { id: "gates", check: "Probe cache", result: formatRatio(probe.passed, probe.total), scope: `- / ${formatNumber(probe.total) ?? "?"}`, ratio: ratioOf(probe.passed, probe.total), status: normalizeStatus(probe.status) },
    { id: "gates", check: "Mutation replay", result: formatRatio(mutation.passed, mutation.total), scope: `- / ${formatNumber(mutation.total) ?? "?"}`, ratio: ratioOf(mutation.passed, mutation.total), status: normalizeStatus(mutation.status) }
  );
  if (pathSwap) {
    rows.push({ id: "gates", check: "Silent PATH swap", result: formatRatio(pathSwap.passed, pathSwap.total), scope: `- / ${formatNumber(pathSwap.total) ?? "1"}`, ratio: ratioOf(pathSwap.passed, pathSwap.total), status: normalizeStatus(pathSwap.status) });
  }
  if (determinism) {
    rows.push({ id: "gates", check: "Path-swap determinism", result: formatRatio(determinism.passed, determinism.total), scope: `- / ${formatNumber(determinism.total) ?? "1"}`, ratio: ratioOf(determinism.passed, determinism.total), status: normalizeStatus(determinism.status) });
  }
  rows.push(
    { id: "gates", check: "Seeded generative fuzzing", result: typeof fuzz.passed === "number" ? `${formatNumber(fuzz.passed)} inputs, ${formatNumber(fuzzFails)} ${fuzzUnit}${fuzzTag}` : null, scope: "pending", ratio: ratioOf(fuzz.passed, fuzz.total), status: fuzzPre ? "partial" : normalizeStatus(fuzz.status), statusText: fuzzPre ? "pre-freeze" : undefined }
  );
  if (diff && typeof diffTotal === "number" && diffTotal > 0) {
    rows.push({ id: "gates", check: "Oracle differential fuzzing", result: formatRatio(diffPassed, diffTotal), scope: `- / ${formatNumber(diffTotal)}`, ratio: ratioOf(diffPassed, diffTotal), status: normalizeStatus(diff.status) });
  }
  rows.push(
    { id: "gates", check: "WASI and wasm32", result: formatRatio(wasi.passed, wasi.total) ? `${formatRatio(wasi.passed, wasi.total)}${wasiTag}` : null, scope: `- / ${formatNumber(wasi.total) ?? "?"}`, ratio: ratioOf(wasi.passed, wasi.total), status: wasiPre ? "partial" : normalizeStatus(wasi.status), statusText: wasiPre ? "pre-freeze" : undefined },
    { id: "speed", check: "Cold configure on 500 targets", result: formatSpeedSummary(cold), scope: "pending", ratio: speedRowRatio(cold), status: normalizeStatus(cold.status) },
    { id: "speed", check: "New tree with shared probe cache", result: formatSpeedSummary(warm), scope: "pending", ratio: speedRowRatio(warm), status: normalizeStatus(warm.status) }
  );
  return rows;
}

export function overviewRowsHtml(rows) {
  return rows.map((r) => `<tr><td>${escapeHtml(r.check)}</td><td class="mono">${r.result ? escapeHtml(r.result) : `<span class="dim">${escapeHtml(r.scope)}</span>`}</td><td>${statusHtml(r.status, r.statusText)}</td></tr>`).join("");
}

export function snapshotMeta(data) {
  const parts = [];
  if (data?.rcmakeVersion) parts.push(`rcmake ${String(data.rcmakeVersion)}`);
  parts.push(data?.commit ? `commit ${String(data.commit).slice(0, 12)}` : "commit pending");
  parts.push(`CMake ${data?.targetCmakeVersion || "4.5.1"}`);
  const size = formatMiB(data?.receipt?.binarySizeBytes);
  if (size) parts.push(`binary ${size}`);
  const d = data?.generatedAt ? String(data.generatedAt).slice(0, 10) : null;
  parts.push(d ? `generated ${d}` : "not yet generated");
  return parts.join(" \u00b7 ");
}

if (typeof document !== "undefined") {
  initThemeToggle(document);
  initTopbarScroll(document);
  initReveal(document);
  initSegPills(document);
}
