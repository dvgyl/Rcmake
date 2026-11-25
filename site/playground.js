/* Rcmake Browser Playground Controller.
 * Pure ES module, zero external dependencies. MIT OR Apache-2.0.
 */

import { escapeHtml, statusBadgeHtml } from "./app.js";
import { PLAYGROUND_EXAMPLES, RcmakeWasmAdapter } from "./wasm-adapter.js";

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

const els = {
  modeBadge: document.getElementById("pg-mode-badge"),
  runtimeMsg: document.getElementById("pg-runtime-msg"),
  exampleDesc: document.getElementById("pg-example-desc"),
  exampleSelect: document.getElementById("example-select"),
  configSelect: document.getElementById("config-select"),
  generatorSelect: document.getElementById("generator-select"),
  resetBtn: document.getElementById("reset-btn"),
  runBtn: document.getElementById("run-btn"),
  fileTabs: document.getElementById("file-tabs"),
  dirtyIndicator: document.getElementById("dirty-indicator"),
  gutter: document.getElementById("editor-gutter"),
  textarea: document.getElementById("editor-textarea"),
  cursor: document.getElementById("editor-cursor"),
  inspectorTabs: Array.from(document.querySelectorAll("[data-inspector-tab]")),
  inspectorBody: document.getElementById("inspector-body"),
  copyNinjaBtn: document.getElementById("copy-ninja-btn"),
  diagCount: document.getElementById("tab-count-diag"),
  targetCount: document.getElementById("tab-count-targets"),
  runStatus: document.getElementById("run-status"),
  runTiming: document.getElementById("run-timing")
};

function populateExampleSelect() {
  els.exampleSelect.innerHTML = Object.values(PLAYGROUND_EXAMPLES)
    .map((ex) => `<option value="${escapeHtml(ex.id)}">${escapeHtml(ex.title)}</option>`)
    .join("");
}

function updateGutter() {
  const lines = els.textarea.value.split("\n").length;
  const nums = [];
  for (let i = 1; i <= lines; i += 1) {
    nums.push(String(i));
  }
  els.gutter.textContent = nums.join("\n");
}

function updateCursor() {
  const value = els.textarea.value;
  const pos = els.textarea.selectionStart || 0;
  const before = value.slice(0, pos);
  const lines = before.split("\n");
  const line = lines.length;
  const col = lines[lines.length - 1].length + 1;
  els.cursor.textContent = `Ln ${line}, Col ${col} | ${value.length} chars`;
}

function isDirty() {
  const baseline = PLAYGROUND_EXAMPLES[state.exampleId]?.files || {};
  return Object.entries(baseline).some(([path, content]) => (state.files[path] ?? "") !== content);
}

function updateDirtyIndicator() {
  els.dirtyIndicator.textContent = isDirty() ? "Modified" : "Unmodified";
}

function renderFileTabs() {
  const filenames = Object.keys(state.files);
  els.fileTabs.innerHTML = filenames
    .map(
      (name) =>
        `<button class="tab-btn" type="button" role="tab" aria-selected="${String(name === state.activeFile)}" data-file-tab="${escapeHtml(name)}">${escapeHtml(name)}</button>`
    )
    .join("");

  for (const btn of els.fileTabs.querySelectorAll("[data-file-tab]")) {
    btn.addEventListener("click", () => {
      state.files[state.activeFile] = els.textarea.value;
      state.activeFile = btn.dataset.fileTab;
      els.textarea.value = state.files[state.activeFile] ?? "";
      renderFileTabs();
      updateGutter();
      updateCursor();
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
  els.exampleDesc.textContent = ex.description;
  els.textarea.value = state.files[state.activeFile] ?? "";
  renderFileTabs();
  updateGutter();
  updateCursor();
  updateDirtyIndicator();
}

function renderDiagnostics(result) {
  if (!result || !result.diagnostics || result.diagnostics.length === 0) {
    return `<p style="color:var(--ink-muted);margin:0;">No diagnostics emitted.</p>`;
  }
  return `<div class="diag-list">${result.diagnostics
    .map((d) => {
      const badgeStatus = d.severity === "error" ? "fail" : d.severity === "warning" ? "partial" : "pass";
      return `
        <article class="diag-item">
          <div class="diag-head">
            <div style="display:flex;align-items:center;gap:6px;flex-wrap:wrap;">
              ${statusBadgeHtml(badgeStatus, d.severity.toUpperCase())}
              <span class="badge mono">${escapeHtml(d.category || "DIAGNOSTIC")}</span>
            </div>
            <span class="diag-loc">${escapeHtml(d.file || "CMakeLists.txt")}:${escapeHtml(d.line ?? 1)}:${escapeHtml(d.column ?? 1)}</span>
          </div>
          <p class="diag-msg">${escapeHtml(d.message)}</p>
        </article>
      `;
    })
    .join("")}</div>`;
}

function renderTargets(result) {
  if (!result || !result.targets || result.targets.length === 0) {
    return `<p style="color:var(--ink-muted);margin:0;">No targets resolved.</p>`;
  }
  return result.targets
    .map((t) => {
      const rows = [
        ["Defined at", t.definedAt],
        ["Sources", t.sources?.join(", ") || "(none)"],
        ["Features", t.compileFeatures?.join(", ") || "(none)"],
        ["Definitions", t.compileDefinitions?.join("; ") || "(none)"],
        ["Options", t.compileOptions?.join(" ") || "(none)"],
        ["Includes", t.includeDirectories?.join("; ") || "(none)"],
        ["Link libs", t.linkLibraries?.join(", ") || "(none)"]
      ];
      return `
        <article class="target-card">
          <div class="target-card-head">
            <span class="target-name">${escapeHtml(t.name)}</span>
            <span class="badge mono">${escapeHtml(t.kind)}</span>
          </div>
          <div class="kv-grid">
            ${rows
              .map(
                ([k, v]) =>
                  `<span class="kv-key">${escapeHtml(k)}</span><span class="kv-val">${escapeHtml(v)}</span>`
              )
              .join("")}
          </div>
        </article>
      `;
    })
    .join("");
}

function renderNinja(result) {
  const ninja = result?.files?.["build.ninja"] || "# No build.ninja generated.";
  return `<pre class="code-block"><code>${escapeHtml(ninja)}</code></pre>`;
}

function renderExplainDetails(report) {
  if (!report) {
    return `<p style="color:var(--ink-muted);margin:0;">Select a target or variable symbol to inspect its provenance.</p>`;
  }
  if (state.showRawExplainJson) {
    return `<pre class="code-block"><code>${escapeHtml(JSON.stringify(report, null, 2))}</code></pre>`;
  }

  if (report.kind === "not_found") {
    const suggestions = report.details?.suggestions || [];
    return `
      <div class="diag-item">
        <div class="diag-head">
          ${statusBadgeHtml("partial", "NOT FOUND")}
          <span class="mono" style="font-size:0.78rem;">${escapeHtml(report.schema)}</span>
        </div>
        <p class="diag-msg">Symbol <code>${escapeHtml(report.name)}</code> was not found in the active configure model.</p>
        ${
          suggestions.length > 0
            ? `<p style="margin:8px 0 0;font-size:0.82rem;color:var(--ink-muted);">Available: ${suggestions
                .map((s) => `<code>${escapeHtml(s.name)}</code> (${escapeHtml(s.kind)})`)
                .join(", ")}</p>`
            : ""
        }
      </div>
    `;
  }

  if (report.kind === "variable") {
    const d = report.details;
    const writes = d.writes || [];
    return `
      <div class="target-card">
        <div class="target-card-head">
          <span class="target-name">${escapeHtml(report.name)}</span>
          <div style="display:flex;gap:6px;">
            <span class="badge mono">VARIABLE</span>
            <span class="badge mono">${escapeHtml(d.binding || "directory")}</span>
          </div>
        </div>
        <div class="kv-grid">
          <span class="kv-key">Value</span><span class="kv-val">${escapeHtml(d.value)}</span>
          <span class="kv-key">Defined</span><span class="kv-val">${escapeHtml(`${d.location?.path || ""}:${d.location?.line || 0} (${d.location?.command || ""})`)}</span>
          <span class="kv-key">History</span><span class="kv-val">${escapeHtml(d.history || "complete")}</span>
          <span class="kv-key">Schema</span><span class="kv-val">${escapeHtml(report.schema)}</span>
        </div>
        <ul class="hop-list">
          ${writes
            .map(
              (w) =>
                `<li class="hop-item">#${escapeHtml(w.sequence)} [${escapeHtml(w.op)}] = ${escapeHtml(JSON.stringify(w.value))} @ ${escapeHtml(w.location)}</li>`
            )
            .join("")}
        </ul>
      </div>
    `;
  }

  const d = report.details;
  const cfg = d.configurations?.[0];
  const entries = cfg?.entries || [];
  return `
    <div class="target-card">
      <div class="target-card-head">
        <span class="target-name">${escapeHtml(d.target || report.name)}</span>
        <div style="display:flex;gap:6px;">
          <span class="badge mono">${escapeHtml(d.target_kind || "TARGET")}</span>
          <span class="badge mono">${escapeHtml(cfg?.name || state.buildType)}</span>
        </div>
      </div>
      <div class="kv-grid">
        <span class="kv-key">Definition</span><span class="kv-val">${escapeHtml(`${d.definition?.path || ""}:${d.definition?.line || 0} (${d.definition?.command || ""})`)}</span>
        <span class="kv-key">Schema</span><span class="kv-val">${escapeHtml(report.schema)}</span>
      </div>
      <ul class="hop-list">
        ${entries
          .map((e) => {
            const hops = (e.chain || [])
              .map((h) => `${h.from} -[${h.edge}]-> ${h.via || h.to} (${h.location})`)
              .join(" -> ");
            return `
              <li class="hop-item">
                <div><strong>${escapeHtml(e.property)}</strong> (${escapeHtml(e.scope)}): ${escapeHtml(e.value)}</div>
                <div style="color:var(--ink-muted);margin-top:3px;">Origin: ${escapeHtml(e.origin)}${hops ? ` | Link chain: ${escapeHtml(hops)}` : " | Direct"}</div>
              </li>
            `;
          })
          .join("")}
      </ul>
    </div>
  `;
}

function renderExplainTab(result) {
  const symbols = result?.explainSymbols || [];
  const chips = symbols
    .map(
      (sym) =>
        `<button class="chip-btn${sym === state.explainSymbol ? " active" : ""}" type="button" data-explain-sym="${escapeHtml(sym)}">${escapeHtml(sym)}</button>`
    )
    .join("");

  return `
    <div class="explain-controls">
      <form id="explain-form" class="explain-search">
        <input id="explain-input" type="text" value="${escapeHtml(state.explainSymbol)}" placeholder="Enter target or variable name..." aria-label="Explain symbol">
        <button class="btn btn-secondary btn-sm" type="submit">Explain</button>
        <button id="toggle-explain-json" class="btn btn-secondary btn-sm" type="button">${state.showRawExplainJson ? "Structured View" : "Raw JSON"}</button>
      </form>
      <div class="chip-row">${chips}</div>
    </div>
    <div id="explain-output">${renderExplainDetails(state.explainReport)}</div>
  `;
}

function wireExplainControls() {
  const form = document.getElementById("explain-form");
  const input = document.getElementById("explain-input");
  const toggleJson = document.getElementById("toggle-explain-json");

  if (form && input) {
    form.addEventListener("submit", async (e) => {
      e.preventDefault();
      state.explainSymbol = input.value.trim();
      state.explainReport = await adapter.explain(state.explainSymbol, { config: state.buildType });
      renderInspector();
    });
  }
  if (toggleJson) {
    toggleJson.addEventListener("click", () => {
      state.showRawExplainJson = !state.showRawExplainJson;
      renderInspector();
    });
  }
  for (const chip of els.inspectorBody.querySelectorAll("[data-explain-sym]")) {
    chip.addEventListener("click", async () => {
      state.explainSymbol = chip.dataset.explainSym;
      state.explainReport = await adapter.explain(state.explainSymbol, { config: state.buildType });
      renderInspector();
    });
  }
}

function renderInspector() {
  const result = state.lastResult;
  for (const tab of els.inspectorTabs) {
    tab.setAttribute("aria-selected", String(tab.dataset.inspectorTab === state.activeTab));
  }
  els.copyNinjaBtn.style.display = state.activeTab === "ninja" ? "inline-flex" : "none";

  els.diagCount.textContent = `(${result?.diagnostics?.length ?? 0})`;
  els.targetCount.textContent = `(${result?.targets?.length ?? 0})`;

  if (state.activeTab === "diagnostics") {
    els.inspectorBody.innerHTML = renderDiagnostics(result);
  } else if (state.activeTab === "targets") {
    els.inspectorBody.innerHTML = renderTargets(result);
  } else if (state.activeTab === "ninja") {
    els.inspectorBody.innerHTML = renderNinja(result);
  } else {
    els.inspectorBody.innerHTML = renderExplainTab(result);
    wireExplainControls();
  }
}

async function runConfigure() {
  state.files[state.activeFile] = els.textarea.value;
  updateDirtyIndicator();
  els.runStatus.textContent = "Configuring...";

  const result = await adapter.configure(state.files, {
    exampleId: state.exampleId,
    generator: state.generator,
    buildType: state.buildType
  });
  state.lastResult = result;

  if (!state.explainSymbol && result.explainSymbols?.length) {
    state.explainSymbol = result.explainSymbols[0];
  }
  if (state.explainSymbol) {
    state.explainReport = await adapter.explain(state.explainSymbol, { config: state.buildType });
  }

  els.runStatus.textContent = result.mode === "wasm"
    ? (result.ok ? "Configure succeeded (WASM)" : "Configure reported errors (WASM)")
    : (result.editedInPreview ? "Preview baseline shown (edited buffer)" : "Precomputed example ready (Preview mode)");
  els.runTiming.textContent = `${result.durationMs} ms | ${state.generator} | ${state.buildType}`;
  renderInspector();
}

function syncRuntimeModeUi(loadInfo) {
  if (loadInfo.mode === "wasm") {
    els.modeBadge.innerHTML = statusBadgeHtml("pass", "WASM Runtime Active");
    els.runtimeMsg.innerHTML = `Running compiled <code>rcmake_wasm.js</code> directly in browser via <code>PureHost</code>.`;
  } else {
    els.modeBadge.innerHTML = statusBadgeHtml("pending", "Preview mode (WASM bundle pending)");
    els.runtimeMsg.innerHTML = `Preview mode active with precomputed CMake 4.5.1 outputs. Drop a compiled bundle at <code>./wasm/rcmake_wasm.js</code> for live browser execution.`;
  }
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

  els.runBtn.addEventListener("click", () => {
    runConfigure();
  });

  els.textarea.addEventListener("input", () => {
    state.files[state.activeFile] = els.textarea.value;
    updateGutter();
    updateCursor();
    updateDirtyIndicator();
  });

  els.textarea.addEventListener("scroll", () => {
    els.gutter.scrollTop = els.textarea.scrollTop;
  });

  els.textarea.addEventListener("click", updateCursor);
  els.textarea.addEventListener("keyup", updateCursor);

  els.textarea.addEventListener("keydown", (event) => {
    if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
      event.preventDefault();
      runConfigure();
      return;
    }
    if (event.key === "Tab") {
      event.preventDefault();
      const start = els.textarea.selectionStart;
      const end = els.textarea.selectionEnd;
      const val = els.textarea.value;
      els.textarea.value = `${val.slice(0, start)}  ${val.slice(end)}`;
      els.textarea.selectionStart = start + 2;
      els.textarea.selectionEnd = start + 2;
      state.files[state.activeFile] = els.textarea.value;
      updateGutter();
      updateCursor();
      updateDirtyIndicator();
    }
  });

  for (const tab of els.inspectorTabs) {
    tab.addEventListener("click", () => {
      state.activeTab = tab.dataset.inspectorTab;
      renderInspector();
    });
  }

  els.copyNinjaBtn.addEventListener("click", async () => {
    const ninja = state.lastResult?.files?.["build.ninja"] || "";
    try {
      await navigator.clipboard.writeText(ninja);
      els.copyNinjaBtn.textContent = "Copied";
      window.setTimeout(() => {
        els.copyNinjaBtn.textContent = "Copy Ninja";
      }, 1500);
    } catch (_) {
      els.copyNinjaBtn.textContent = "Copy failed";
    }
  });
}

async function init() {
  populateExampleSelect();
  loadExample("hello-cpp20");
  wireEvents();

  const loadInfo = await adapter.load("./wasm/rcmake_wasm.js");
  syncRuntimeModeUi(loadInfo);
  await runConfigure();
}

init();
