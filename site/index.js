/* rcmake home page controller. MIT OR Apache-2.0. */

import { copyWithFeedback, escapeHtml, formatNumber, initRovingGroup, isPreFreezeGate, loadLatestData, normalizeStatus, revealScan, statusHtml } from "./app.js";
import { formatSpeed, isMeasured, osStripsHtml, speedupBars, statRow, statTile } from "./charts.js";
import { createMotionController, prefersReducedMotion } from "./motion.js";
import { initPipeline } from "./pipeline.js";

const RACE_MODES = {
  warm: {
    cmakeCmd: '<span class="term-prompt">$ </span>cmake -S . -B build-2 -G Ninja',
    rcmakeCmd: '<span class="term-prompt">$ </span>rcmake -S . -B build-2 -G Ninja',
    lines: [
      "-- The CXX compiler identification is AppleClang",
      "-- Detecting CXX compiler ABI info - done",
      "-- Configuring done",
      "-- Generating done",
      "-- Build files have been written to: <project>/build-2"
    ],
    caption: "CMake has no cross-tree cache.",
    key: "warmProbeSpeedup",
    kind: "ratio"
  },
  leaf: {
    cmakeCmd: '<span class="term-out"># edit src/net/CMakeLists.txt</span>\n<span class="term-prompt">$ </span>cmake -S . -B build',
    rcmakeCmd: '<span class="term-out"># edit src/net/CMakeLists.txt</span>\n<span class="term-prompt">$ </span>rcmake -S . -B build',
    lines: [
      "-- Configuring done",
      "-- Generating done",
      "-- Build files have been written to: <project>/build"
    ],
    caption: "",
    key: "incrementalLeafP95Ms",
    kind: "leaf"
  },
  cold: {
    cmakeCmd: '<span class="term-prompt">$ </span>rm -rf build &amp;&amp; cmake -S . -B build -G Ninja',
    rcmakeCmd: '<span class="term-prompt">$ </span>rm -rf build &amp;&amp; rcmake -S . -B build -G Ninja',
    lines: [
      "-- The C compiler identification is AppleClang",
      "-- Detecting C compiler ABI info - done",
      "-- Configuring done",
      "-- Generating done",
      "-- Build files have been written to: <project>/build"
    ],
    caption: "",
    key: "coldConfigureSpeedup",
    kind: "ratio"
  }
};

const DEFAULT_PLATFORMS = [
  { id: "macos-arm64", name: "macOS arm64", toolchain: "AppleClang + Ninja, Unix Makefiles, Xcode", suitesTotal: 371, suitesPassed: null, status: "pending" },
  { id: "linux-x86_64", name: "Linux x86_64", toolchain: "GCC + Clang + Ninja, Unix Makefiles", suitesTotal: 362, suitesPassed: null, status: "pending" },
  { id: "windows-x86_64", name: "Windows x86_64", toolchain: "MSVC + Ninja, Visual Studio 17 & 18", suitesTotal: 367, suitesPassed: null, status: "pending" }
];

function initTerminal(doc) {
  const tabs = doc.getElementById("install-tabs");
  const copyBtn = doc.getElementById("term-copy-btn");
  if (!tabs) return;
  let active = "install";
  initRovingGroup(tabs, "[data-term-tab]", (btn) => {
    active = btn.dataset.termTab;
    tabs.querySelectorAll("[data-term-tab]").forEach((b) => {
      const on = b.dataset.termTab === active;
      b.setAttribute("aria-selected", String(on));
      const p = doc.getElementById(`panel-${b.dataset.termTab}`);
      if (!p) return;
      p.hidden = !on;
      if (on) { p.classList.add("is-fading"); requestAnimationFrame(() => p.classList.remove("is-fading")); }
    });
  });
  copyBtn?.addEventListener("click", () => {
    const panel = doc.getElementById(`panel-${active}`);
    if (panel) copyWithFeedback(copyBtn, Array.from(panel.querySelectorAll("[data-line]"), (el) => el.textContent).join("\n"), "Copy");
  });
}

function initRace(doc, motion) {
  const $ = (id) => doc.getElementById(id);
  const [widget, modeTabs, cmCmdEl, rcCmdEl, cmLines, rcLines, cmBar, rcBar, caption, replayBtn] = [
    $("race-widget"), $("race-modes"), $("race-cmd-cmake"), $("race-cmd-rcmake"),
    $("race-lines-cmake"), $("race-lines-rcmake"),
    $("race-bar-cmake"), $("race-bar-rcmake"), $("race-caption"), $("race-replay")
  ];
  if (!widget) return { setData() {} };
  let mode = "warm";
  let bench = null;
  let timer = null;

  const drawLines = (win) => {
    const lines = RACE_MODES[mode].lines;
    const lastIdx = lines.length - 1;
    const mk = (w) => lines.map((l, i) => `<li style="--line-idx:${i}">${escapeHtml(l)}${w && i === lastIdx ? '<span class="race-cursor" aria-hidden="true"></span>' : ""}</li>`).join("");
    cmLines.innerHTML = mk(win === "cmake");
    rcLines.innerHTML = mk(win === "rcmake");
  };

  const render = () => {
    const cfg = RACE_MODES[mode];
    const m = bench?.[cfg.key];
    const val = m?.value;
    if (cmCmdEl) cmCmdEl.innerHTML = cfg.cmakeCmd;
    if (rcCmdEl) rcCmdEl.innerHTML = cfg.rcmakeCmd;
    if (!isMeasured(val) || val <= 0) {
      drawLines(null);
      cmBar.dataset.status = rcBar.dataset.status = "pending";
      cmBar.innerHTML = '<div class="bar-track is-pending" data-loop></div><div class="race-bar-meta"><span class="dim">cmake 4.5.1</span><span class="dim"><span data-value></span>pending</span></div>';
      rcBar.innerHTML = '<div class="bar-track is-pending" data-loop></div><div class="race-bar-meta"><span class="muted">rcmake</span><span class="dim"><span data-value></span>pending</span></div>';
      caption.textContent = cfg.caption;
      motion.scan(widget);
      return;
    }
    cmBar.dataset.status = rcBar.dataset.status = "pass";
    if (cfg.kind === "leaf") {
      drawLines("rcmake");
      cmBar.innerHTML = '<div class="race-bar-meta"><span class="dim">cmake 4.5.1</span><span class="dim">full reconfigure</span></div>';
      rcBar.innerHTML = `<div class="bar-track"><div class="bar-fill is-rcmake" style="--v:1;--bar-dur:320ms" data-scale="1"></div></div><div class="race-bar-meta"><span class="muted">rcmake</span><span data-value>${escapeHtml(formatNumber(val))} ms p95</span></div>`;
      caption.textContent = cfg.caption;
      return;
    }
    const scale = String(Number(Math.min(1, Math.max(0.02, 1 / val)).toFixed(4)));
    drawLines(val >= 1 ? "rcmake" : "cmake");
    cmBar.innerHTML = '<div class="bar-track"><div class="bar-fill is-cmake" style="--v:1;--bar-dur:800ms" data-scale="1"></div></div><div class="race-bar-meta"><span class="dim">cmake 4.5.1</span><span class="dim" data-value>1.00\u00d7</span></div>';
    rcBar.innerHTML = `<div class="bar-track"><div class="bar-fill is-rcmake" style="--v:${scale};--bar-dur:${Math.max(160, Math.round(800 / val))}ms" data-scale="${scale}" data-test="race-fill-rcmake"></div></div><div class="race-bar-meta"><span class="muted">rcmake</span><span data-value>${formatSpeed(val)}\u00d7 faster</span></div>`;
    caption.textContent = cfg.caption;
  };

  const play = () => {
    clearTimeout(timer);
    render();
    if (prefersReducedMotion()) { widget.classList.add("is-playing", "is-bars"); return; }
    widget.classList.remove("is-playing", "is-bars");
    void widget.offsetWidth;
    widget.classList.add("is-playing");
    timer = setTimeout(() => widget.classList.add("is-bars"), 460);
  };

  initRovingGroup(modeTabs, "[data-race-mode]", (b) => {
    mode = b.dataset.raceMode;
    modeTabs.querySelectorAll("[data-race-mode]").forEach((x) => x.setAttribute("aria-pressed", String(x.dataset.raceMode === mode)));
    play();
  });
  replayBtn?.addEventListener("click", play);
  render();
  motion.observeReveal(widget, play);
  return { setData(d) { bench = d; widget.classList.contains("is-in") ? play() : render(); } };
}

function renderSnapshot(el, data) {
  const gates = data?.tracks?.benchmarksAndGates?.gates || [];
  const g = (id, def) => gates.find((x) => x.id === id) || def;
  const fuzz = g("fuzz", { passed: null, failures: null, status: "pending" });
  const wasi = g("wasi", { passed: null, total: 6, status: "pending" });
  const core = data?.tracks?.coreConformance || { suitesAt100: null, suitesTotal: 62, status: "pending" };
  const hold = data?.tracks?.unseenHoldout || { reposPassed: null, reposTotal: 240, status: "pending" };
  const fuzzPre = isPreFreezeGate(fuzz, data);
  const wasiPre = isPreFreezeGate(wasi, data);
  const fails = fuzz.failures ?? 0;

  el.innerHTML = [
    statTile({ label: "Fuzz inputs", value: fuzz.passed, detail: fails === 0 ? "0 panics" : `${formatNumber(fails)} failures`, status: fuzzPre ? "partial" : fuzz.status, tag: fuzzPre ? "pre-freeze" : "", testId: "gauge-fuzz" }),
    statTile({ label: "WASI checks", value: wasi.passed, total: wasi.total ?? 6, status: wasiPre ? "partial" : wasi.status, tag: wasiPre ? "pre-freeze" : "", testId: "gauge-wasi" }),
    statTile({ label: "Core suites", value: core.suitesAt100, total: core.suitesTotal ?? 62, unit: isMeasured(core.suitesAt100) ? "" : "suites", detail: "fully passing", status: core.status, testId: "gauge-core" }),
    statTile({ label: "Blind holdout", value: hold.reposPassed, total: hold.reposTotal ?? 240, unit: isMeasured(hold.reposPassed) ? "" : "repos", detail: "L2 parity", status: hold.status, testId: "gauge-holdout" })
  ].join("");
}

function renderGiantsTable(giants) {
  if (!giants || !Array.isArray(giants.projects) || giants.projects.length === 0) return "";
  if (!giants.projects.some((p) => normalizeStatus(p?.status) !== "pending")) return "";
  const fmtRatio = (v) => (formatSpeed(v) ? `${formatSpeed(v)}\u00d7` : "-");
  const fmtRss = (p) => {
    const rc = [p?.cold?.rcmakePeakRssMiB, p?.warmProbe?.rcmakePeakRssMiB, p?.incrementalLeaf?.rcmakePeakRssMiB].filter(isMeasured);
    const cm = [p?.cold?.cmakePeakRssMiB, p?.warmProbe?.cmakePeakRssMiB, p?.incrementalLeaf?.cmakePeakRssMiB].filter(isMeasured);
    if (rc.length && cm.length) return `${formatNumber(Math.max(...rc))} / ${formatNumber(Math.max(...cm))} MiB`;
    if (rc.length) return `${formatNumber(Math.max(...rc))} MiB`;
    return "-";
  };
  const rows = giants.projects.map((p) => {
    const st = normalizeStatus(p?.status);
    const leafP95 = isMeasured(p?.incrementalLeaf?.rcmakeP95Ms) ? ` (${formatNumber(p.incrementalLeaf.rcmakeP95Ms)} ms)` : "";
    const leaf = formatSpeed(p?.incrementalLeaf?.speedup) ? `${formatSpeed(p.incrementalLeaf.speedup)}\u00d7${leafP95}` : (leafP95 ? leafP95.slice(2, -1) : "-");
    return `<tr data-status="${st}"><td>${escapeHtml(p?.name || "")}</td><td>${escapeHtml(fmtRatio(p?.cold?.speedup))}</td><td>${escapeHtml(fmtRatio(p?.warmProbe?.speedup))}</td><td>${escapeHtml(leaf)}</td><td>${escapeHtml(fmtRss(p))}</td><td>${statusHtml(st)}</td></tr>`;
  }).join("");
  return `<div style="border-top:1px solid var(--line);overflow-x:auto" data-test="home-giants-bench"><table class="runner-table"><thead><tr><th>Project</th><th>Cold</th><th>New tree (probe cache)</th><th>Leaf edit</th><th>Peak RSS</th><th>Status</th></tr></thead><tbody>${rows}</tbody></table></div>`;
}

function renderSpeed(el, bench, motion, giants = null) {
  const b = bench || {};
  const w = b.warmProbeSpeedup || {};
  const c = b.coldConfigureSpeedup || {};
  const l = b.incrementalLeafP95Ms || {};
  const r = b.peakRssMiB || {};
  el.innerHTML = `${speedupBars({ label: "New tree, shared probe cache", value: w.value, unit: w.unit, status: w.status, testId: "speed-warm" })}${speedupBars({ label: "Cold configure, 500 targets", value: c.value, unit: c.unit, status: c.status, testId: "speed-cold" })}<div class="speed-stats">${statRow({ label: "Leaf edit reconfigure, p95", value: l.value, unit: l.unit, status: l.status, testId: "speed-leaf" })}${statRow({ label: "Peak memory, 500 targets", value: r.value, unit: r.unit, status: r.status, testId: "speed-rss" })}</div>${renderGiantsTable(giants)}`;
  motion.scan(el);
}

function initCompare(doc) {
  const filter = doc.getElementById("cmp-filter");
  const panel = doc.getElementById("cmp-panel");
  if (!filter || !panel) return;
  initRovingGroup(filter, "[data-cmp-group]", (btn) => {
    const grp = btn.dataset.cmpGroup;
    filter.querySelectorAll("[data-cmp-group]").forEach((b) => b.setAttribute("aria-pressed", String(b.dataset.cmpGroup === grp)));
    if (panel.dataset.filter === grp) return;
    panel.dataset.filter = grp;
    if (prefersReducedMotion()) return;
    panel.querySelectorAll("tbody tr").forEach((row, i) => {
      row.classList.remove("row-in");
      row.style.setProperty("--row-i", String(Math.min(i, 12)));
    });
    void panel.offsetWidth;
    panel.querySelectorAll("tbody tr").forEach((row) => row.classList.add("row-in"));
  });
}

function initFaq(doc) {
  const items = Array.from(doc.querySelectorAll("#faq details.faq-item"));
  const reduced = prefersReducedMotion();
  items.forEach((item) => {
    const summary = item.querySelector("summary");
    const answer = item.querySelector(".faq-answer");
    if (!summary || !answer) return;
    if (item.open) item.classList.add("is-open");
    summary.addEventListener("click", (e) => {
      e.preventDefault();
      if (!item.open) {
        item.open = true;
        requestAnimationFrame(() => requestAnimationFrame(() => item.classList.add("is-open")));
        return;
      }
      item.classList.remove("is-open");
      if (reduced) { item.open = false; return; }
      const done = () => { if (!item.classList.contains("is-open")) item.open = false; };
      answer.addEventListener("transitionend", done, { once: true });
      setTimeout(done, 420);
    });
    item.addEventListener("toggle", () => { if (item.open) item.classList.add("is-open"); else item.classList.remove("is-open"); });
  });
}

function initHeroDepth(doc) {
  const glow = doc.querySelector(".hero-glow");
  if (!glow || typeof window === "undefined" || prefersReducedMotion()) return;
  let queued = false;
  const apply = () => {
    queued = false;
    const y = Math.min(window.scrollY, 900);
    glow.style.transform = `translate3d(0, ${(y * 0.18).toFixed(1)}px, 0)`;
    glow.style.opacity = String(Math.max(0, 1 - y / 900));
  };
  window.addEventListener("scroll", () => { if (!queued) { queued = true; requestAnimationFrame(apply); } }, { passive: true });
}

async function init() {
  const motion = createMotionController(document);
  initTerminal(document);
  const race = initRace(document, motion);
  initCompare(document);
  initFaq(document);
  initHeroDepth(document);
  initPipeline(document.getElementById("pipeline-widget"), motion);

  const [waffles, gauges, speed, errEl] = ["waffle-panel", "gauge-grid", "speed-panel", "home-load-error"].map((id) => document.getElementById(id));
  waffles.innerHTML = osStripsHtml(DEFAULT_PLATFORMS);
  renderSnapshot(gauges, null);
  revealScan(gauges);
  renderSpeed(speed, null, motion);

  try {
    const data = await loadLatestData();
    const range = data?.tracks?.policies?.activeRange;
    if (range) document.querySelectorAll("[data-policy-range]").forEach((el) => { el.textContent = range; });
    const syncCmpGate = (gateKey, trackStatus) => {
      const cell = document.querySelector(`[data-cmp-gate="${gateKey}"]`);
      if (!cell || normalizeStatus(trackStatus) !== "pass") return;
      cell.classList.add("st-pass");
      cell.innerHTML = '<svg class="i" aria-hidden="true"><use href="#i-check"/></svg><span class="visually-hidden">yes</span>';
    };
    syncCmpGate("policies", data?.tracks?.policies?.status);
    syncCmpGate("generators", data?.tracks?.runCmakeOs?.status);
    waffles.innerHTML = osStripsHtml(data?.tracks?.runCmakeOs?.platforms || DEFAULT_PLATFORMS);
    renderSnapshot(gauges, data);
    revealScan(gauges);
    renderSpeed(speed, data?.tracks?.benchmarksAndGates?.benchmarks, motion, data?.tracks?.benchmarksAndGates?.giantsBenchmark || null);
    race.setData(data?.tracks?.benchmarksAndGates?.benchmarks || null);
    motion.scan(document);
  } catch (_) {
    if (errEl) { errEl.textContent = "Could not load data/latest.json. Serve the site over HTTP."; errEl.hidden = false; }
  }
}

if (typeof document !== "undefined") init();
