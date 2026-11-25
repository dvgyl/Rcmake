/* Rcmake playground controller.
 * Plain ES module, no dependencies. MIT OR Apache-2.0.
 */

import { copyWithFeedback, escapeHtml } from "./app.js";
import { PLAYGROUND_EXAMPLES, RcmakeWasmAdapter } from "./wasm-adapter.js";

const BUNDLE_URL = "./wasm/rcmake_wasm.js";
const SEVERITY_STATUS = { error: "fail", warning: "partial", info: "pass", note: "pass" };

const adapter = new RcmakeWasmAdapter();

const state = {
  exampleId: "hello-cpp20",
  activeFile: "CMakeLists.txt",
  files: {},
  generator: "Ninja",
  buildType: "Debug",
  activeTab: "diagnostics",
  lastResult: null,
  explainSymbol: "",
  explainReport: null,
  showRawExplainJson: false
};

const $ = (id) => document.getElementById(id);

const els = {
  runtime: $("runtime"),
  exampleSelect: $("example-select"),
  configSelect: $("config-select"),
  generatorSelect: $("generator-select"),
  resetBtn: $("reset-btn"),
  runBtn: $("run-btn"),
  fileTabs: $("file-tabs"),
  dirty: $("dirty-indicator"),
  gutter: $("editor-gutter"),
  textarea: $("editor-textarea"),
  cursor: $("editor-cursor"),
  tabs: Array.from(document.querySelectorAll("[data-inspector-tab]")),
  body: $("inspector-body"),
  copyBtn: $("copy-file-btn"),
  fileTabName: $("tab-file-name"),
  diagCount: $("tab-count-diag"),
  targetCount: $("tab-count-targets"),
  runStatus: $("run-status"),
  runTiming: $("run-timing")
};

function muted(text) {
  return `<p class="muted">${escapeHtml(text)}</p>`;
}

function formatLoc(loc) {
  if (!loc) {
    return "";
  }
  return `${loc.path || ""}:${loc.line || 0}${loc.command ? ` ${loc.command}()` : ""}`;
}

/* Editor */

function syncEditorChrome() {
  const value = els.textarea.value;
  const count = value.split("\n").length;
  els.gutter.textContent = Array.from({ length: count }, (_, i) => i + 1).join("\n");
  els.gutter.scrollTop = els.textarea.scrollTop;
  const before = value.slice(0, els.textarea.selectionStart || 0).split("\n");
  els.cursor.textContent = `Ln ${before.length}, Col ${before[before.length - 1].length + 1}`;
  const baseline = PLAYGROUND_EXAMPLES[state.exampleId]?.files || {};
  const dirty = Object.entries(baseline).some(([path, text]) => (state.files[path] ?? "") !== text);
  els.dirty.textContent = dirty ? "modified" : "";
}

function renderFileTabs() {
  els.fileTabs.innerHTML = Object.keys(state.files)
    .map(
      (name) =>
        `<button type="button" role="tab" aria-selected="${name === state.activeFile}" data-file-tab="${escapeHtml(name)}">${escapeHtml(name)}</button>`
    )
    .join("");
  for (const btn of els.fileTabs.querySelectorAll("[data-file-tab]")) {
    btn.addEventListener("click", () => {
      state.files[state.activeFile] = els.textarea.value;
      state.activeFile = btn.dataset.fileTab;
      els.textarea.value = state.files[state.activeFile] ?? "";
      renderFileTabs();
      syncEditorChrome();
    });
  }
}

function loadExample(exampleId) {
  const ex = PLAYGROUND_EXAMPLES[exampleId] || PLAYGROUND_EXAMPLES["hello-cpp20"];
  state.exampleId = ex.id;
  state.files = { ...ex.files };
  state.activeFile = Object.keys(state.files)[0];
  state.explainSymbol = ex.defaultSymbol || "";
  state.explainReport = null;
  if (ex.id === "presets-v13") {
    state.generator = "Ninja Multi-Config";
    els.generatorSelect.value = state.generator;
  }
  els.exampleSelect.value = ex.id;
  els.exampleSelect.title = ex.description || "";
  els.textarea.value = state.files[state.activeFile] ?? "";
  renderFileTabs();
  syncEditorChrome();
}

/* Output panes */

function renderDiagnostics(result) {
  const list = result?.diagnostics || [];
  if (list.length === 0) {
    return muted("No diagnostics.");
  }
  return list
    .map((d) => {
      const sev = String(d.severity || "info");
      const st = SEVERITY_STATUS[sev] || "pending";
      return `<div class="block">
        <div><span class="st st-${st}">${escapeHtml(sev)}</span>&nbsp;&nbsp;${escapeHtml(d.category || "")}&nbsp;&nbsp;<span class="muted">${escapeHtml(d.file || "CMakeLists.txt")}:${escapeHtml(d.line ?? 1)}:${escapeHtml(d.column ?? 1)}</span></div>
        <div>${escapeHtml(d.message)}</div>
      </div>`;
    })
    .join("");
}

function renderTargets(result) {
  const list = result?.targets || [];
  if (list.length === 0) {
    return muted("No targets.");
  }
  const fields = [
    ["defined at", (t) => t.definedAt],
    ["sources", (t) => t.sources?.join(" ")],
    ["features", (t) => t.compileFeatures?.join(" ")],
    ["definitions", (t) => t.compileDefinitions?.join(" ")],
    ["options", (t) => t.compileOptions?.join(" ")],
    ["includes", (t) => t.includeDirectories?.join(" ")],
    ["links", (t) => t.linkLibraries?.join(" ")]
  ];
  return list
    .map(
      (t) => `<div class="block">
        <h3>${escapeHtml(t.name)} <span class="muted">${escapeHtml(t.kind)}</span></h3>
        <dl>${fields
          .map(([k, get]) => `<dt>${k}</dt><dd>${escapeHtml(get(t) || "-")}</dd>`)
          .join("")}</dl>
      </div>`
    )
    .join("");
}

function primaryFile(result) {
  const files = result?.files || {};
  const name = "build.ninja" in files ? "build.ninja" : Object.keys(files)[0];
  return name ? { name, text: files[name] } : null;
}

function renderGeneratedFile(result) {
  const file = primaryFile(result);
  return file ? `<pre>${escapeHtml(file.text)}</pre>` : muted("No generated files.");
}

function renderExplainReport(report) {
  if (!report) {
    return muted("Select a target or variable.");
  }
  if (state.showRawExplainJson) {
    return `<pre>${escapeHtml(JSON.stringify(report, null, 2))}</pre>`;
  }
  const d = report.details || {};
  if (report.kind === "not_found") {
    const names = (d.suggestions || []).map((s) => `${s.name} (${s.kind})`).join(", ");
    return `<div class="block">
      <div><span class="st st-partial">not found</span>&nbsp;&nbsp;${escapeHtml(report.name)}</div>
      ${names ? `<div class="muted">available: ${escapeHtml(names)}</div>` : ""}
    </div>`;
  }
  if (report.kind === "variable") {
    return `<div class="block">
      <h3>${escapeHtml(report.name)} <span class="muted">variable, ${escapeHtml(d.binding || "directory")} scope</span></h3>
      <dl>
        <dt>value</dt><dd>${escapeHtml(d.value)}</dd>
        <dt>defined at</dt><dd>${escapeHtml(formatLoc(d.location))}</dd>
        <dt>history</dt><dd>${escapeHtml(d.history || "complete")}</dd>
      </dl>
      <ol>${(d.writes || [])
        .map((w) => `<li>${escapeHtml(w.op)} = ${escapeHtml(JSON.stringify(w.value))} <span class="muted">${escapeHtml(w.location)}</span></li>`)
        .join("")}</ol>
    </div>`;
  }
  const cfg = d.configurations?.[0];
  return `<div class="block">
    <h3>${escapeHtml(d.target || report.name)} <span class="muted">${escapeHtml(d.target_kind || "target")}, ${escapeHtml(cfg?.name || state.buildType)}</span></h3>
    <dl><dt>defined at</dt><dd>${escapeHtml(formatLoc(d.definition))}</dd></dl>
    ${(cfg?.entries || [])
      .map((e) => {
        const chain = (e.chain || [])
          .map((h) => `<br>&nbsp;&nbsp;${escapeHtml(`${h.from} -[${h.edge}]-> ${h.via || h.to} (${h.location})`)}`)
          .join("");
        return `<div class="block">
          <div>${escapeHtml(e.property)} <span class="muted">${escapeHtml(e.scope)}</span></div>
          <div>${escapeHtml(e.value)}</div>
          <div class="muted">origin ${escapeHtml(e.origin)}${chain}</div>
        </div>`;
      })
      .join("")}
  </div>`;
}

function renderExplainTab(result) {
  const symbols = (result?.explainSymbols || [])
    .map(
      (sym) =>
        `<button type="button" data-explain-sym="${escapeHtml(sym)}" aria-pressed="${sym === state.explainSymbol}">${escapeHtml(sym)}</button>`
    )
    .join("");
  return `<form id="explain-form">
      <input id="explain-input" type="text" value="${escapeHtml(state.explainSymbol)}" placeholder="target or variable" aria-label="Symbol to explain">
      <button class="btn ghost" type="submit">Explain</button>
      <button id="toggle-explain-json" class="text" type="button">${state.showRawExplainJson ? "structured" : "raw json"}</button>
    </form>
    <div class="symbols">${symbols}</div>
    ${renderExplainReport(state.explainReport)}`;
}

async function explain(symbol) {
  state.explainSymbol = symbol;
  state.explainReport = await adapter.explain(symbol, { config: state.buildType });
  renderInspector();
}

function wireExplain() {
  const form = $("explain-form");
  form?.addEventListener("submit", (event) => {
    event.preventDefault();
    explain($("explain-input").value.trim());
  });
  $("toggle-explain-json")?.addEventListener("click", () => {
    state.showRawExplainJson = !state.showRawExplainJson;
    renderInspector();
  });
  for (const btn of els.body.querySelectorAll("[data-explain-sym]")) {
    btn.addEventListener("click", () => explain(btn.dataset.explainSym));
  }
}

function renderInspector() {
  const result = state.lastResult;
  for (const tab of els.tabs) {
    tab.setAttribute("aria-selected", String(tab.dataset.inspectorTab === state.activeTab));
  }
  const file = primaryFile(result);
  els.fileTabName.textContent = file?.name || "build.ninja";
  els.copyBtn.hidden = state.activeTab !== "ninja" || !file;
  els.diagCount.textContent = result ? String(result.diagnostics?.length ?? 0) : "";
  els.targetCount.textContent = result ? String(result.targets?.length ?? 0) : "";

  const views = {
    diagnostics: renderDiagnostics,
    targets: renderTargets,
    ninja: renderGeneratedFile,
    explain: renderExplainTab
  };
  els.body.innerHTML = (views[state.activeTab] || renderDiagnostics)(result);
  if (state.activeTab === "explain") {
    wireExplain();
  }
}

/* Configure */

async function runConfigure() {
  state.files[state.activeFile] = els.textarea.value;
  syncEditorChrome();
  els.runStatus.textContent = "configuring";
  try {
    const result = await adapter.configure(state.files, {
      exampleId: state.exampleId,
      generator: state.generator,
      buildType: state.buildType
    });
    state.lastResult = result;
    if (!state.explainSymbol && result.explainSymbols?.length) {
      state.explainSymbol = result.explainSymbols[0];
    }
    state.explainReport = state.explainSymbol
      ? await adapter.explain(state.explainSymbol, { config: state.buildType })
      : null;
    if (result.mode === "wasm") {
      els.runStatus.textContent = result.ok ? "configure done" : "configure failed";
    } else {
      els.runStatus.textContent = result.editedInPreview
        ? "edited buffer: showing the saved output for this example"
        : "saved output for this example";
    }
    els.runTiming.textContent = `${result.durationMs} ms · ${state.generator} · ${state.buildType}`;
  } catch (error) {
    els.runStatus.textContent = `error: ${error instanceof Error ? error.message : String(error)}`;
  }
  renderInspector();
}

function wireEvents() {
  els.exampleSelect.addEventListener("change", () => {
    loadExample(els.exampleSelect.value);
    runConfigure();
  });
  els.configSelect.addEventListener("change", () => {
    state.buildType = els.configSelect.value;
    runConfigure();
  });
  els.generatorSelect.addEventListener("change", () => {
    state.generator = els.generatorSelect.value;
    runConfigure();
  });
  els.resetBtn.addEventListener("click", () => {
    loadExample(state.exampleId);
    runConfigure();
  });
  els.runBtn.addEventListener("click", runConfigure);

  els.textarea.addEventListener("input", () => {
    state.files[state.activeFile] = els.textarea.value;
    syncEditorChrome();
  });
  els.textarea.addEventListener("scroll", () => {
    els.gutter.scrollTop = els.textarea.scrollTop;
  });
  els.textarea.addEventListener("click", syncEditorChrome);
  els.textarea.addEventListener("keyup", syncEditorChrome);
  els.textarea.addEventListener("keydown", (event) => {
    if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
      event.preventDefault();
      runConfigure();
    } else if (event.key === "Tab" && !event.shiftKey) {
      event.preventDefault();
      const { selectionStart: start, selectionEnd: end, value } = els.textarea;
      els.textarea.value = `${value.slice(0, start)}  ${value.slice(end)}`;
      els.textarea.selectionStart = els.textarea.selectionEnd = start + 2;
      state.files[state.activeFile] = els.textarea.value;
      syncEditorChrome();
    }
  });

  for (const tab of els.tabs) {
    tab.addEventListener("click", () => {
      state.activeTab = tab.dataset.inspectorTab;
      renderInspector();
    });
  }
  els.copyBtn.addEventListener("click", () => {
    copyWithFeedback(els.copyBtn, primaryFile(state.lastResult)?.text || "");
  });
}

async function init() {
  els.exampleSelect.innerHTML = Object.values(PLAYGROUND_EXAMPLES)
    .map((ex) => `<option value="${escapeHtml(ex.id)}">${escapeHtml(ex.title)}</option>`)
    .join("");
  loadExample(state.exampleId);
  wireEvents();
  const info = await adapter.load(BUNDLE_URL);
  els.runtime.textContent = info.mode === "wasm" ? "wasm32 live" : "preview mode, wasm bundle pending";
  els.runtime.title = info.mode === "wasm" ? info.url : `${info.url}: ${info.reason || "not found"}`;
  await runConfigure();
}

init();
