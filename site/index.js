/* rcmake home page controller. MIT OR Apache-2.0. */

import { copyWithFeedback, escapeHtml, formatNumber, initRovingGroup, loadLatestData, normalizeStatus, statusHtml } from "./app.js";
import { formatSpeed, fuzzGaugeCard, isMeasured, ringGaugeCard, speedupBars, statRow, waffleRowsHtml } from "./charts.js";
import { animateCountUp, createMotionController, prefersReducedMotion } from "./motion.js";
import { initPipeline } from "./pipeline.js";

const RACE_MODES = {
  warm: { cmd: '<span class="term-prompt">$ </span>cmake -S . -B build -G Ninja', key: "warmProbeSpeedup", kind: "ratio" },
  leaf: { cmd: '<span class="term-out"># edit src/net/CMakeLists.txt</span>\n<span class="term-prompt">$ </span>cmake -S . -B build', key: "incrementalLeafP95Ms", kind: "leaf" },
  cold: { cmd: '<span class="term-prompt">$ </span>rm -rf build &amp;&amp; cmake -S . -B build -G Ninja', key: "coldConfigureSpeedup", kind: "ratio" }
};

const TRANSCRIPT = [
  "-- The CXX compiler identification is AppleClang",
  "-- Detecting CXX compiler ABI info - done",
  "-- Configuring done",
  "-- Generating done",
  "-- Build files have been written to: build"
];

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
  const [widget, modeTabs, cmdEl, cmLines, rcLines, cmBar, rcBar, caption, replayBtn] = [
    $("race-widget"), $("race-modes"), $("race-cmd"), $("race-lines-cmake"), $("race-lines-rcmake"),
    $("race-bar-cmake"), $("race-bar-rcmake"), $("race-caption"), $("race-replay")
  ];
  if (!widget) return { setData() {} };
  let mode = "warm";
  let bench = null;
  let timer = null;

  const drawLines = (win) => {
    const mk = (w) => TRANSCRIPT.map((l, i) => `<li style="--line-idx:${i}">${escapeHtml(l)}${w && i === 4 ? '<span class="race-cursor" aria-hidden="true"></span>' : ""}</li>`).join("");
    cmLines.innerHTML = mk(win === "cmake");
    rcLines.innerHTML = mk(win === "rcmake");
  };

  const render = () => {
    const cfg = RACE_MODES[mode];
    const m = bench?.[cfg.key];
    const val = m?.value;
    cmdEl.innerHTML = cfg.cmd;
    if (!isMeasured(val) || val <= 0) {
      drawLines(null);
      cmBar.dataset.status = rcBar.dataset.status = "pending";
      cmBar.innerHTML = '<div class="bar-track is-pending" data-loop></div><div class="race-bar-meta"><span class="dim">cmake 4.5.1</span><span class="dim"><span data-value></span>pending</span></div>';
      rcBar.innerHTML = '<div class="bar-track is-pending" data-loop></div><div class="race-bar-meta"><span class="muted">rcmake</span><span class="dim"><span data-value></span>pending</span></div>';
      caption.textContent = "Timings appear after the frozen release run.";
      motion.scan(widget);
      return;
    }
    cmBar.dataset.status = rcBar.dataset.status = "pass";
    if (cfg.kind === "leaf") {
      drawLines("rcmake");
      cmBar.innerHTML = '<div class="race-bar-meta"><span class="dim">cmake 4.5.1</span><span class="dim">full reconfigure</span></div>';
      rcBar.innerHTML = `<div class="bar-track"><div class="bar-fill is-rcmake" style="--v:1;--bar-dur:320ms" data-scale="1"></div></div><div class="race-bar-meta"><span class="muted">rcmake</span><span data-value>${escapeHtml(formatNumber(val))} ms p95</span></div>`;
      caption.textContent = m.baseline ? `Single-file leaf edit (${m.baseline}).` : "Single-file leaf edit.";
      return;
    }
    const scale = String(Number(Math.min(1, Math.max(0.02, 1 / val)).toFixed(4)));
    drawLines(val >= 1 ? "rcmake" : "cmake");
    cmBar.innerHTML = '<div class="bar-track"><div class="bar-fill is-cmake" style="--v:1;--bar-dur:800ms" data-scale="1"></div></div><div class="race-bar-meta"><span class="dim">cmake 4.5.1</span><span class="dim" data-value>1.00\u00d7</span></div>';
    rcBar.innerHTML = `<div class="bar-track"><div class="bar-fill is-rcmake" style="--v:${scale};--bar-dur:${Math.max(160, Math.round(800 / val))}ms" data-scale="${scale}" data-test="race-fill-rcmake"></div></div><div class="race-bar-meta"><span class="muted">rcmake</span><span data-value>${formatSpeed(val)}\u00d7 faster</span></div>`;
    caption.textContent = m.baseline ? `Measured ${m.baseline}.` : "Measured against CMake 4.5.1.";
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
    modeTabs.querySelectorAll("[data-race-mode]").forEach((x) => x.setAttribute("aria-selected", String(x.dataset.raceMode === mode)));
    play();
  });
  replayBtn?.addEventListener("click", play);
  render();
  motion.observeReveal(widget, play);
  return { setData(d) { bench = d; widget.classList.contains("is-in") ? play() : render(); } };
}

function renderGauges(el, data, motion) {
  const gates = data?.tracks?.benchmarksAndGates?.gates || [];
  const g = (id, def) => gates.find((x) => x.id === id) || def;
  const fuzz = g("fuzz", { passed: null, failures: null, status: "pending" });
  const wasi = g("wasi", { passed: null, total: 6, status: "pending" });
  const bb = g("blackbox", { scenariosPassed: null, scenariosTotal: 20, status: "pending" });
  const core = data?.tracks?.coreConformance || { suitesAt100: null, suitesTotal: 62, status: "pending" };
  const probe = g("probe-cache", { passed: null, total: 16, status: "pending" });
  const mut = g("mutation-replay", { passed: null, total: 20, status: "pending" });

  el.innerHTML = [
    fuzzGaugeCard({ label: "Fuzz inputs", passed: fuzz.passed, failures: fuzz.failures, status: fuzz.status, testId: "gauge-fuzz" }),
    ringGaugeCard({ label: "WASI checks", passed: wasi.passed, total: wasi.total ?? 6, status: wasi.status, testId: "gauge-wasi" }),
    ringGaugeCard({ label: "Black-box scenarios", passed: bb.scenariosPassed, total: bb.scenariosTotal ?? 20, status: bb.status, testId: "gauge-blackbox" }),
    ringGaugeCard({ label: "Core suites", passed: core.suitesAt100, total: core.suitesTotal ?? 62, status: core.status, testId: "gauge-core" }),
    ringGaugeCard({ label: "Probe cache checks", passed: probe.passed, total: probe.total ?? 16, status: probe.status, testId: "gauge-probe-cache" }),
    ringGaugeCard({ label: "Mutation replay checks", passed: mut.passed, total: mut.total ?? 20, status: mut.status, testId: "gauge-mutation-replay" })
  ].join("");

  const fEl = el.querySelector("[data-fuzz-target]");
  if (fEl && isMeasured(fuzz.passed)) {
    const go = () => animateCountUp(fEl, fuzz.passed, { duration: 800 });
    el.classList.contains("is-in") ? go() : motion.observeReveal(el, go);
  }
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
  return `<div style="border-top:1px solid var(--line);overflow-x:auto" data-test="home-giants-bench"><table class="runner-table"><thead><tr><th>Project</th><th>Cold</th><th>Warm probe</th><th>Leaf edit</th><th>Peak RSS</th><th>Status</th></tr></thead><tbody>${rows}</tbody></table></div>`;
}

function renderSpeed(el, bench, motion, giants = null) {
  const b = bench || {};
  const w = b.warmProbeSpeedup || {};
  const c = b.coldConfigureSpeedup || {};
  const l = b.incrementalLeafP95Ms || { baseline: "single-file change replay" };
  const r = b.peakRssMiB || { baseline: "native release binary" };
  el.innerHTML = `${speedupBars({ label: "Warm configure", value: w.value, unit: w.unit, status: w.status, testId: "speed-warm" })}${speedupBars({ label: "Cold configure, 500 targets", value: c.value, unit: c.unit, status: c.status, testId: "speed-cold" })}<div class="speed-stats">${statRow({ label: "Leaf edit reconfigure, p95", value: l.value, unit: l.unit, baseline: l.baseline, status: l.status, testId: "speed-leaf" })}${statRow({ label: "Peak memory, 500 targets", value: r.value, unit: r.unit, baseline: r.baseline, status: r.status, testId: "speed-rss" })}</div>${renderGiantsTable(giants)}`;
  motion.scan(el);
}

function initCompareAndLangs(doc) {
  const filter = doc.getElementById("cmp-filter");
  const rows = Array.from(doc.querySelectorAll("#cmp-body tr[data-group]"));
  initRovingGroup(filter, "[data-cmp-group]", (btn) => {
    const grp = btn.dataset.cmpGroup;
    filter.querySelectorAll("[data-cmp-group]").forEach((b) => b.setAttribute("aria-selected", String(b.dataset.cmpGroup === grp)));
    rows.forEach((row) => {
      const show = grp === "all" || row.dataset.group === grp;
      row.classList.toggle("is-hidden", !show);
      if (show) { row.classList.add("is-fading"); requestAnimationFrame(() => row.classList.remove("is-fading")); }
    });
  });

  const detail = doc.getElementById("lang-detail");
  const chips = Array.from(doc.querySelectorAll("#lang-grid .lang-chip"));
  const show = (chip) => {
    chips.forEach((c) => c.setAttribute("aria-pressed", String(c === chip)));
    const text = chip.dataset.langDetail || "";
    if (chip.dataset.langPartial === "true") {
      detail.innerHTML = `<span class="st st-partial"><svg class="i" aria-hidden="true"><use href="#i-differs"/></svg><span class="visually-hidden">partial</span></span><span>${escapeHtml(text)}</span>`;
    } else detail.textContent = text;
  };
  chips.forEach((c) => ["click", "mouseenter", "focus"].forEach((ev) => c.addEventListener(ev, () => show(c))));
}

async function init() {
  const motion = createMotionController(document);
  initTerminal(document);
  const race = initRace(document, motion);
  initCompareAndLangs(document);
  initPipeline(document.getElementById("pipeline-widget"), motion);

  const [waffles, gauges, speed, errEl] = ["waffle-panel", "gauge-grid", "speed-panel", "home-load-error"].map((id) => document.getElementById(id));
  waffles.innerHTML = waffleRowsHtml(DEFAULT_PLATFORMS);
  renderGauges(gauges, null, motion);
  renderSpeed(speed, null, motion);

  try {
    const data = await loadLatestData();
    const range = data?.tracks?.policies?.activeRange;
    if (range) document.querySelectorAll("[data-policy-range]").forEach((el) => { el.textContent = range; });
    waffles.innerHTML = waffleRowsHtml(data?.tracks?.runCmakeOs?.platforms || DEFAULT_PLATFORMS);
    renderGauges(gauges, data, motion);
    renderSpeed(speed, data?.tracks?.benchmarksAndGates?.benchmarks, motion, data?.tracks?.benchmarksAndGates?.giantsBenchmark || null);
    race.setData(data?.tracks?.benchmarksAndGates?.benchmarks || null);
    motion.scan(document);
  } catch (_) {
    if (errEl) { errEl.textContent = "Could not load data/latest.json. Serve the site over HTTP."; errEl.hidden = false; }
  }
}

if (typeof document !== "undefined") init();
