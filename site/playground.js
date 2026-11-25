/* rcmake playground workbench controller. Preserves RcmakeWasmAdapter contract. MIT OR Apache-2.0. */

import { copyWithFeedback, escapeHtml, initRovingGroup } from "./app.js";
import { prefersReducedMotion } from "./motion.js";
import { PLAYGROUND_EXAMPLES, RcmakeWasmAdapter } from "./wasm-adapter.js";

const BUNDLE_URL = "./wasm/rcmake_wasm.js";
const SPLIT_KEY = "rcmake-split";
const LINE_H = 21;
const adapter = new RcmakeWasmAdapter();

const state = {
  exampleId: "hello-cpp20", activeFile: "CMakeLists.txt", files: {},
  generator: "Ninja", buildType: "Debug", activeTab: "diagnostics",
  lastResult: null, explainSymbol: "", explainReport: null, showRawJson: false
};

const $ = (id) => document.getElementById(id);
const els = {
  modeBadge: $("mode-badge"), exampleSelect: $("example-select"), genGroup: $("generator-group"),
  configSelect: $("config-select"), runBtn: $("run-btn"), runIcon: $("run-icon"), runKbd: $("run-kbd"),
  resetBtn: $("reset-btn"), panes: $("panes"), paneEditor: $("pane-editor"), paneOutput: $("pane-output"),
  splitHandle: $("split-handle"), fileTabs: $("file-tabs"), gutter: $("editor-gutter"), flash: $("editor-flash"),
  textarea: $("editor-textarea"), cursor: $("editor-cursor"), dirty: $("dirty-indicator"),
  outputTabs: $("output-tabs"), runLine: $("run-line"), body: $("inspector-body"), copyBtn: $("copy-file-btn"),
  fileTabName: $("tab-file-name"), diagCount: $("tab-count-diag"), targetCount: $("tab-count-targets"),
  runStatus: $("run-status"), runSummary: $("run-summary")
};

let flashTimer = null;

function parseLoc(loc) {
  if (!loc) return null;
  if (typeof loc === "object") {
    const path = loc.path || loc.file || "";
    const line = Number(loc.line || 0);
    return path && Number.isFinite(line) && line >= 1 ? { path, line, label: `${path}:${line}${loc.command ? ` ${loc.command}` : ""}` } : null;
  }
  const m = String(loc).match(/^([^:\s]+):(\d+)/);
  return m ? { path: m[1], line: Number(m[2]), label: String(loc) } : null;
}

const locBtn = (loc) => {
  const p = parseLoc(loc);
  return p ? `<button type="button" class="loc-btn" data-jump-file="${escapeHtml(p.path)}" data-jump-line="${p.line}">${escapeHtml(p.label)}</button>` : "";
};

function jumpTo(file, line) {
  const target = Math.max(1, Number(line) || 1);
  if (file && file in state.files && file !== state.activeFile) {
    state.files[state.activeFile] = els.textarea.value;
    state.activeFile = file;
    els.textarea.value = state.files[file] ?? "";
    renderFileTabs();
  }
  const lines = els.textarea.value.split("\n");
  const clamped = Math.min(target, Math.max(1, lines.length));
  let offset = 0;
  for (let i = 0; i < clamped - 1; i += 1) offset += lines[i].length + 1;
  els.textarea.focus();
  els.textarea.setSelectionRange(offset, offset);
  els.textarea.scrollTop = Math.max(0, (clamped - 3) * LINE_H);
  els.gutter.scrollTop = els.textarea.scrollTop;
  syncEditor();
  if (els.flash) {
    els.flash.style.top = `${12 + (clamped - 1) * LINE_H - els.textarea.scrollTop}px`;
    els.flash.classList.add("is-on");
    clearTimeout(flashTimer);
    requestAnimationFrame(() => { flashTimer = setTimeout(() => els.flash.classList.remove("is-on"), 40); });
  }
}

function syncEditor() {
  const v = els.textarea.value;
  const count = v.split("\n").length;
  els.gutter.textContent = Array.from({ length: count }, (_, i) => i + 1).join("\n");
  els.gutter.scrollTop = els.textarea.scrollTop;
  const before = v.slice(0, els.textarea.selectionStart || 0).split("\n");
  els.cursor.textContent = `Ln ${before.length}, Col ${before[before.length - 1].length + 1}`;
  const base = PLAYGROUND_EXAMPLES[state.exampleId]?.files || {};
  els.dirty.textContent = Object.entries(base).some(([p, t]) => (state.files[p] ?? "") !== t) ? "modified" : "";
}

function renderFileTabs() {
  els.fileTabs.innerHTML = Object.keys(state.files)
    .map((n) => `<button type="button" role="tab" aria-selected="${n === state.activeFile}" data-file-tab="${escapeHtml(n)}"><svg class="i" aria-hidden="true"><use href="#i-file"/></svg>${escapeHtml(n)}</button>`)
    .join("");
}

const syncGen = () => els.genGroup.querySelectorAll("[data-gen]").forEach((b) => b.setAttribute("aria-checked", String(b.dataset.gen === state.generator)));

function loadExample(id) {
  const ex = PLAYGROUND_EXAMPLES[id] || PLAYGROUND_EXAMPLES["hello-cpp20"];
  state.exampleId = ex.id;
  state.files = { ...ex.files };
  state.activeFile = Object.keys(state.files)[0];
  state.explainSymbol = ex.defaultSymbol || "";
  state.explainReport = null;
  if (ex.id === "presets-v13") { state.generator = "Ninja Multi-Config"; syncGen(); }
  els.exampleSelect.value = ex.id;
  els.exampleSelect.title = ex.description || "";
  els.textarea.value = state.files[state.activeFile] ?? "";
  renderFileTabs();
  syncEditor();
}

function primaryFile(res) {
  const f = res?.files || {};
  const name = "build.ninja" in f ? "build.ninja" : Object.keys(f)[0];
  return name ? { name, text: f[name] } : null;
}

const hlLine = (line) => line.trimStart().startsWith("#")
  ? `<span class="hl-cm">${escapeHtml(line)}</span>`
  : escapeHtml(line)
      .replace(/^(\s*)(rule|build|pool|default|subninja|include)\b/, '$1<span class="hl-kw">$2</span>')
      .replace(/(\$(?:in|out|[A-Z_][A-Z0-9_]*|\{[^}]+\}))/g, '<span class="hl-var">$1</span>');

const chips = (raw) => {
  const parts = String(raw ?? "").split(";").map((s) => s.trim()).filter(Boolean);
  return (parts.length ? parts : [String(raw ?? "")]).map((p) => `<span class="chip is-ink">${escapeHtml(p)}</span>`).join("");
};

function renderDiagnostics(res) {
  const list = res?.diagnostics || [];
  if (!list.length) return '<p class="dim">No diagnostics.</p>';
  return list.map((d) => {
    const sev = String(d.severity || "info");
    const [ic, cls] = sev === "error" ? ["i-cross", "st-fail"] : sev === "warning" ? ["i-differs", "st-partial"] : ["i-check", "st-pass"];
    return `<div class="diag-row"><span class="${cls}"><svg class="i" aria-hidden="true"><use href="#${ic}"/></svg><span class="visually-hidden">${escapeHtml(sev)}</span></span><div class="diag-msg">${escapeHtml(d.message)}${d.category ? `<span class="diag-cat">${escapeHtml(d.category)}</span>` : ""}</div><div>${d.file && d.line ? locBtn({ path: d.file, line: d.line }) : ""}</div></div>`;
  }).join("");
}

function renderTargets(res) {
  const list = res?.targets || [];
  if (!list.length) return '<p class="dim">No targets.</p>';
  const fields = [["defined at", (t) => t.definedAt], ["sources", (t) => t.sources?.join(" ")], ["features", (t) => t.compileFeatures?.join(" ")], ["definitions", (t) => t.compileDefinitions?.join(" ")], ["options", (t) => t.compileOptions?.join(" ")], ["includes", (t) => t.includeDirectories?.join(" ")], ["links", (t) => t.linkLibraries?.join(" ")]];
  return list.map((t) => {
    const n = t.sources?.length || 0;
    const rows = fields.filter(([, get]) => get(t)).map(([k, get]) => `<dt>${k}</dt><dd>${escapeHtml(get(t))}</dd>`).join("");
    return `<div class="target-card"><div class="target-head"><svg class="i" aria-hidden="true"><use href="#i-target"/></svg><span class="target-name">${escapeHtml(t.name)}</span><span class="chip is-ink">${escapeHtml(t.kind)}</span><span class="dim">${n} ${n === 1 ? "source" : "sources"}</span></div>${rows ? `<dl class="target-meta">${rows}</dl>` : ""}</div>`;
  }).join("");
}

function renderGenerated(res) {
  const f = primaryFile(res);
  if (!f) return '<p class="dim">No generated files.</p>';
  return `<div class="code-table" data-test="generated-code">${String(f.text || "").split("\n").map((l, i) => `<div class="code-line"><span class="code-ln">${i + 1}</span><span>${hlLine(l)}</span></div>`).join("")}</div>`;
}

function renderExplainReport(rep) {
  if (!rep) return '<p class="dim">Pick a target or variable.</p>';
  if (state.showRawJson) return `<pre class="code-table">${escapeHtml(JSON.stringify(rep, null, 2))}</pre>`;
  const d = rep.details || {};
  if (rep.kind === "not_found") {
    const sugs = (d.suggestions || []).map((s) => `<button type="button" class="ctrl-btn" data-explain-sym="${escapeHtml(s.name)}">${escapeHtml(s.name)} <span class="dim">${escapeHtml(s.kind)}</span></button>`).join("");
    return `<div class="explain-tree" data-test="explain-not-found"><div class="ex-root"><span class="st st-partial"><svg class="i" aria-hidden="true"><use href="#i-differs"/></svg><span>not found</span></span><span>${escapeHtml(rep.name)}</span></div>${sugs ? `<div class="explain-syms">${sugs}</div>` : ""}</div>`;
  }
  if (rep.kind === "variable") {
    const writes = (d.writes || []).map((w) => `<li class="var-step"><span class="muted">${escapeHtml(w.op || "set")}</span>${chips(w.value)}${locBtn(w.location)}</li>`).join("");
    return `<div class="explain-tree" data-test="explain-variable"><div class="ex-root"><span class="target-name">${escapeHtml(rep.name)}</span><span class="chip is-ink">${escapeHtml(d.binding || "directory")}</span>${chips(d.value)}${locBtn(d.location)}</div><ol class="var-timeline">${writes}</ol></div>`;
  }
  const cfg = d.configurations?.[0];
  const srcs = (cfg?.sources || []).map((s) => `<span class="chip is-ink">${escapeHtml(s)}</span>`).join("");
  const entries = (cfg?.entries || []).map((e) => {
    const chain = (e.chain || []).map((h) => `<div class="ex-chain"><span>${escapeHtml(`${h.from} -> ${h.via || h.to}`)}</span><span class="chip">${escapeHtml(h.edge)}</span>${locBtn(h.location)}</div>`).join("");
    return `<div class="ex-entry"><div class="ex-entry-row"><span class="ex-prop">${escapeHtml(e.property)}</span>${chips(e.value)}<span class="chip">${escapeHtml(e.scope)}</span>${locBtn(e.origin)}</div>${chain}</div>`;
  }).join("");
  return `<div class="explain-tree" data-test="explain-tree"><div class="ex-root"><svg class="i" aria-hidden="true"><use href="#i-target"/></svg><span class="target-name">${escapeHtml(d.target || rep.name)}</span><span class="chip is-ink">${escapeHtml(d.target_kind || "TARGET")}</span>${locBtn(d.definition)}</div><div class="ex-branch"><div class="ex-entry"><div class="ex-entry-row"><span class="ex-prop">${escapeHtml(cfg?.name || state.buildType)}</span><span class="dim">${escapeHtml(d.configuration_selection?.status || "matched")}</span></div></div><div class="ex-branch">${srcs ? `<div class="ex-entry"><div class="ex-entry-row"><span class="ex-prop">sources</span>${srcs}</div></div>` : ""}${entries}</div></div></div>`;
}

function renderExplainTab(res) {
  const syms = (res?.explainSymbols || []).map((s) => `<button type="button" class="ctrl-btn" data-explain-sym="${escapeHtml(s)}" aria-pressed="${s === state.explainSymbol}" style="${s === state.explainSymbol ? "background:var(--s3);color:var(--ink);border-color:var(--line-strong)" : ""}">${escapeHtml(s)}</button>`).join("");
  return `<form id="explain-form" class="explain-bar"><input id="explain-input" class="search-input" type="text" value="${escapeHtml(state.explainSymbol)}" placeholder="Target or variable" aria-label="Symbol to explain" data-test="explain-input"><button class="ctrl-btn" type="submit" data-test="explain-submit">Explain</button><button id="toggle-explain-json" class="ctrl-btn" type="button" data-test="toggle-explain-json">${state.showRawJson ? "Structured" : "Raw JSON"}</button></form><div class="explain-syms" data-test="explain-symbols">${syms}</div>${renderExplainReport(state.explainReport)}`;
}

async function explain(sym) {
  state.explainSymbol = sym;
  state.explainReport = await adapter.explain(sym, { config: state.buildType });
  renderInspector();
}

function renderInspector(fade = false) {
  const res = state.lastResult;
  els.outputTabs.querySelectorAll("[data-inspector-tab]").forEach((t) => t.setAttribute("aria-selected", String(t.dataset.inspectorTab === state.activeTab)));
  const f = primaryFile(res);
  els.fileTabName.textContent = f?.name || "build.ninja";
  els.copyBtn.hidden = state.activeTab !== "ninja" || !f;
  els.diagCount.textContent = res ? String(res.diagnostics?.length ?? 0) : "";
  els.targetCount.textContent = res ? String(res.targets?.length ?? 0) : "";
  const views = { diagnostics: renderDiagnostics, targets: renderTargets, ninja: renderGenerated, explain: renderExplainTab };
  if (fade && !prefersReducedMotion()) { els.body.classList.add("is-fading"); requestAnimationFrame(() => els.body.classList.remove("is-fading")); }
  els.body.innerHTML = (views[state.activeTab] || renderDiagnostics)(res);
  els.body.querySelectorAll("[data-jump-line]").forEach((b) => b.addEventListener("click", () => jumpTo(b.dataset.jumpFile, b.dataset.jumpLine)));
  if (state.activeTab === "explain") {
    $("explain-form")?.addEventListener("submit", (e) => { e.preventDefault(); explain($("explain-input").value.trim()); });
    $("toggle-explain-json")?.addEventListener("click", () => { state.showRawJson = !state.showRawJson; renderInspector(); });
    els.body.querySelectorAll("[data-explain-sym]").forEach((b) => b.addEventListener("click", () => explain(b.dataset.explainSym)));
  }
}

function setRunning(on) {
  const reduced = prefersReducedMotion();
  if (on) {
    els.runStatus.textContent = "running";
    if (!reduced) {
      els.runIcon.innerHTML = '<svg class="spin-ring" viewBox="0 0 16 16" aria-hidden="true"><circle cx="8" cy="8" r="6"/></svg>';
      els.runLine.classList.remove("is-done");
      els.runLine.classList.add("is-running");
    }
  } else {
    els.runIcon.innerHTML = '<svg class="i" aria-hidden="true"><use href="#i-play"/></svg>';
    if (!reduced) {
      els.runLine.classList.remove("is-running");
      els.runLine.classList.add("is-done");
      setTimeout(() => els.runLine.classList.remove("is-done"), 220);
    }
  }
}

async function runConfigure() {
  state.files[state.activeFile] = els.textarea.value;
  syncEditor();
  setRunning(true);
  try {
    const res = await adapter.configure(state.files, { exampleId: state.exampleId, generator: state.generator, buildType: state.buildType });
    state.lastResult = res;
    if (!state.explainSymbol && res.explainSymbols?.length) state.explainSymbol = res.explainSymbols[0];
    state.explainReport = state.explainSymbol ? await adapter.explain(state.explainSymbol, { config: state.buildType }) : null;
    const n = res.targets?.length ?? 0;
    els.runStatus.textContent = res.mode === "wasm" ? `configured in ${res.durationMs} ms` : "precomputed output";
    els.runSummary.textContent = `${n} ${n === 1 ? "target" : "targets"} \u00b7 ${state.generator} \u00b7 ${state.buildType}`;
  } catch (err) {
    els.runStatus.textContent = `error: ${err instanceof Error ? err.message : String(err)}`;
  } finally {
    setRunning(false);
  }
  renderInspector(true);
}

function initSplit() {
  const set = (pct) => {
    const p = Math.max(25, Math.min(75, Math.round(pct)));
    els.paneEditor.style.flex = `0 0 ${p}%`;
    els.paneOutput.style.flex = `1 1 ${100 - p}%`;
    els.splitHandle.setAttribute("aria-valuenow", String(p));
    try { localStorage.setItem(SPLIT_KEY, String(p)); } catch (_) {}
  };
  try { const s = Number(localStorage.getItem(SPLIT_KEY)); if (Number.isFinite(s) && s >= 25 && s <= 75) set(s); } catch (_) {}
  let drag = false;
  els.splitHandle.addEventListener("pointerdown", (e) => { drag = true; els.splitHandle.setPointerCapture(e.pointerId); });
  els.splitHandle.addEventListener("pointermove", (e) => {
    if (!drag) return;
    const r = els.panes.getBoundingClientRect();
    if (r.width > 0) set(((e.clientX - r.left) / r.width) * 100);
  });
  ["pointerup", "pointercancel"].forEach((ev) => els.splitHandle.addEventListener(ev, () => { drag = false; }));
  els.splitHandle.addEventListener("keydown", (e) => {
    const cur = Number(els.splitHandle.getAttribute("aria-valuenow")) || 50;
    if (e.key === "ArrowLeft") { e.preventDefault(); set(cur - 5); }
    else if (e.key === "ArrowRight") { e.preventDefault(); set(cur + 5); }
  });
}

async function init() {
  const isMac = typeof navigator !== "undefined" && /Mac|iPhone|iPad/i.test(navigator.platform || navigator.userAgent || "");
  if (els.runKbd) els.runKbd.textContent = isMac ? "\u2318\u21b5" : "Ctrl+Enter";
  els.runBtn.title = isMac ? "Run configure (Cmd+Enter)" : "Run configure (Ctrl+Enter)";
  els.exampleSelect.innerHTML = Object.values(PLAYGROUND_EXAMPLES).map((ex) => `<option value="${escapeHtml(ex.id)}">${escapeHtml(ex.title)}</option>`).join("");
  loadExample(state.exampleId);

  els.exampleSelect.addEventListener("change", () => { loadExample(els.exampleSelect.value); runConfigure(); });
  initRovingGroup(els.genGroup, "[data-gen]", (b) => { state.generator = b.dataset.gen; syncGen(); runConfigure(); });
  els.configSelect.addEventListener("change", () => { state.buildType = els.configSelect.value; runConfigure(); });
  els.resetBtn.addEventListener("click", () => { loadExample(state.exampleId); runConfigure(); });
  els.runBtn.addEventListener("click", runConfigure);
  initRovingGroup(els.fileTabs, "[data-file-tab]", (b) => {
    state.files[state.activeFile] = els.textarea.value;
    state.activeFile = b.dataset.fileTab;
    els.textarea.value = state.files[state.activeFile] ?? "";
    renderFileTabs();
    syncEditor();
  });
  initRovingGroup(els.outputTabs, "[data-inspector-tab]", (b) => { state.activeTab = b.dataset.inspectorTab; renderInspector(false); });
  els.textarea.addEventListener("input", () => { state.files[state.activeFile] = els.textarea.value; syncEditor(); });
  els.textarea.addEventListener("scroll", () => { els.gutter.scrollTop = els.textarea.scrollTop; });
  ["click", "keyup"].forEach((ev) => els.textarea.addEventListener(ev, syncEditor));
  els.textarea.addEventListener("keydown", (e) => {
    if ((e.metaKey || e.ctrlKey) && e.key === "Enter") { e.preventDefault(); runConfigure(); }
    else if (e.key === "Tab" && !e.shiftKey) {
      e.preventDefault();
      const { selectionStart: s, selectionEnd: end, value: v } = els.textarea;
      els.textarea.value = `${v.slice(0, s)}  ${v.slice(end)}`;
      els.textarea.selectionStart = els.textarea.selectionEnd = s + 2;
      state.files[state.activeFile] = els.textarea.value;
      syncEditor();
    }
  });
  els.copyBtn.addEventListener("click", () => copyWithFeedback(els.copyBtn, primaryFile(state.lastResult)?.text || "", "Copy"));
  initSplit();

  const info = await adapter.load(BUNDLE_URL);
  const wasm = info.mode === "wasm";
  els.modeBadge.textContent = wasm ? "wasm" : "preview";
  els.modeBadge.classList.toggle("is-wasm", wasm);
  els.modeBadge.title = adapter.loadReason || (wasm ? info.url : `Preview mode (${info.reason || "bundle pending"})`);
  await runConfigure();
}

init();
