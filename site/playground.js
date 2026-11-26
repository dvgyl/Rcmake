/* rcmake playground workbench controller. Preserves RcmakeWasmAdapter contract. MIT OR Apache-2.0. */

import { copyWithFeedback, escapeHtml, initRovingGroup } from "./app.js";
import { prefersReducedMotion } from "./motion.js";
import { PLAYGROUND_EXAMPLES, RcmakeWasmAdapter } from "./wasm-adapter.js";

const BUNDLE_URL = "./wasm/rcmake_wasm.js";
const SPLIT_KEY = "rcmake-split";
const LINE_H = 21;
const adapter = new RcmakeWasmAdapter();

const BOILERPLATE_PREFIXES = [
  "CMAKE_CURRENT_LIST_",
  "CMAKE_CXX_FLAGS_",
  "CMAKE_C_FLAGS_",
  "CMAKE_INSTALL_FULL_",
  "_GNUInstallDirs_"
];
const BOILERPLATE_EXACT = new Set([
  "CMAKE_MAKE_PROGRAM",
  "CMAKE_MINIMUM_REQUIRED_VERSION",
  "CMAKE_INSTALL_BINDIR",
  "CMAKE_INSTALL_DATADIR",
  "CMAKE_INSTALL_DATAROOTDIR",
  "CMAKE_INSTALL_DOCDIR",
  "CMAKE_INSTALL_INFODIR",
  "CMAKE_INSTALL_LIBEXECDIR",
  "CMAKE_INSTALL_LOCALEDIR",
  "CMAKE_INSTALL_LOCALSTATEDIR",
  "CMAKE_INSTALL_MANDIR",
  "CMAKE_INSTALL_OLDINCLUDEDIR",
  "CMAKE_INSTALL_RUNSTATEDIR",
  "CMAKE_INSTALL_SBINDIR",
  "CMAKE_INSTALL_SHAREDSTATEDIR",
  "CMAKE_INSTALL_SYSCONFDIR"
]);

const state = {
  exampleId: "hello-cpp20",
  activeFile: "CMakeLists.txt",
  files: {},
  generator: "Ninja",
  buildType: "Debug",
  activeTab: "ninja",
  lastManualTab: "ninja",
  autoSwitchedToDiag: false,
  activeGenFile: "build.ninja",
  lastResult: null,
  runtimeError: null,
  explainSymbol: "",
  explainReport: null,
  showRawJson: false,
  showAllSymbols: false,
  wrapOutput: false
};

const $ = (id) => document.getElementById(id);
const els = {
  modeBadge: $("mode-badge"),
  modeBadgeText: $("mode-badge-text"),
  exampleSelect: $("example-select"),
  genGroup: $("generator-group"),
  configSelect: $("config-select"),
  runBtn: $("run-btn"),
  runIcon: $("run-icon"),
  runKbd: $("run-kbd"),
  resetBtn: $("reset-btn"),
  panes: $("panes"),
  paneEditor: $("pane-editor"),
  paneOutput: $("pane-output"),
  splitHandle: $("split-handle"),
  fileTabs: $("file-tabs"),
  gutter: $("editor-gutter"),
  flash: $("editor-flash"),
  textarea: $("editor-textarea"),
  cursor: $("editor-cursor"),
  dirty: $("dirty-indicator"),
  outputTabs: $("output-tabs"),
  outputActions: $("output-actions"),
  runLine: $("run-line"),
  body: $("inspector-body"),
  copyBtn: $("copy-file-btn"),
  fileTabName: $("tab-file-name"),
  fileCount: $("tab-count-files"),
  diagCount: $("tab-count-diag"),
  targetCount: $("tab-count-targets"),
  runStatus: $("run-status"),
  runSummary: $("run-summary")
};

let flashTimer = null;

function formatMs(ms) {
  if (typeof ms !== "number" || !Number.isFinite(ms)) return "";
  return ms < 10 ? `${ms.toFixed(1)} ms` : `${Math.round(ms)} ms`;
}

function updateModeBadge(mode, durationMs = null) {
  const wasm = mode === "wasm";
  els.modeBadge.classList.toggle("is-wasm", wasm);
  els.modeBadge.classList.toggle("is-error", mode === "error");
  const timing = wasm && typeof durationMs === "number" && Number.isFinite(durationMs)
    ? ` \u00b7 ${formatMs(durationMs)}`
    : "";
  const label = mode === "loading"
    ? "loading"
    : wasm
      ? `wasm${timing}`
      : "wasm error";
  if (els.modeBadgeText) els.modeBadgeText.textContent = label;
  else els.modeBadge.textContent = label;
  if (wasm) els.modeBadge.title = `rcmake_wasm.wasm, engine ${adapter.engineCommit}`;
}

function normalizePath(raw) {
  const str = String(raw || "");
  if (str.startsWith("/src/")) return str.slice(5);
  if (str.startsWith("/cmake/")) return str.slice(7);
  return str;
}

function stripSrc(raw) {
  const str = String(raw ?? "");
  return str.startsWith("/src/") ? str.slice(5) : str;
}

function parseLoc(loc) {
  if (!loc) return null;
  if (Array.isArray(loc)) {
    const withPath = loc.filter((f) => f && typeof f === "object" && (f.path || f.file));
    if (!withPath.length) return null;
    const preferred = withPath.find((f) => Number(f.line || 0) >= 1 && normalizePath(f.path || f.file) in state.files)
      || withPath.find((f) => Number(f.line || 0) >= 1)
      || withPath[0];
    return parseLoc(preferred);
  }
  if (typeof loc === "object") {
    const rawPath = String(loc.path || loc.file || "");
    if (!rawPath) return null;
    const path = normalizePath(rawPath);
    const line = Number(loc.line || 0);
    const col = Number(loc.column || loc.col || 0);
    const command = loc.command ? String(loc.command) : "";
    const editable = path in state.files;
    const coords = line >= 1 ? (col >= 1 && !command ? `${path}:${line}:${col}` : `${path}:${line}`) : path;
    const label = command ? `${coords} (${command})` : coords;
    return { path, rawPath, line: Number.isFinite(line) ? line : 0, col: Number.isFinite(col) ? col : 0, command, editable, label };
  }
  const m = String(loc).match(/^([^:\s]+):(\d+)(?::(\d+))?(?:\s+(.*))?$/);
  if (!m) return null;
  const path = normalizePath(m[1]);
  const line = Number(m[2]);
  const col = m[3] ? Number(m[3]) : 0;
  const extra = m[4] ? ` ${m[4]}` : "";
  const coords = col >= 1 ? `${path}:${line}:${col}` : `${path}:${line}`;
  return { path, rawPath: m[1], line, col, command: "", editable: path in state.files, label: `${coords}${extra}` };
}

function locBtn(loc) {
  const p = parseLoc(loc);
  if (!p) return "";
  if (p.editable && p.line >= 1) {
    return `<button type="button" class="loc-btn" data-jump-file="${escapeHtml(p.path)}" data-jump-line="${p.line}" data-jump-col="${p.col || 1}" title="Jump to ${escapeHtml(p.path)}:${p.line}">${escapeHtml(p.label)}</button>`;
  }
  return `<span class="loc-tag" title="${escapeHtml(p.rawPath || p.path)}">${escapeHtml(p.label)}</span>`;
}

function jumpTo(file, line, col = 1) {
  const targetLine = Math.max(1, Number(line) || 1);
  const targetCol = Math.max(1, Number(col) || 1);
  const norm = normalizePath(file);
  if (norm && norm in state.files && norm !== state.activeFile) {
    state.files[state.activeFile] = els.textarea.value;
    state.activeFile = norm;
    els.textarea.value = state.files[norm] ?? "";
    renderFileTabs();
  }
  const lines = els.textarea.value.split("\n");
  const clampedLine = Math.min(targetLine, Math.max(1, lines.length));
  let offset = 0;
  for (let i = 0; i < clampedLine - 1; i += 1) offset += lines[i].length + 1;
  offset += Math.min(targetCol - 1, lines[clampedLine - 1].length);
  els.textarea.focus();
  els.textarea.setSelectionRange(offset, offset);
  els.textarea.scrollTop = Math.max(0, (clampedLine - 3) * LINE_H);
  els.gutter.scrollTop = els.textarea.scrollTop;
  syncEditor();
  if (els.flash) {
    els.flash.style.top = `${12 + (clampedLine - 1) * LINE_H - els.textarea.scrollTop}px`;
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
  const dirty = Object.entries(base).some(([p, t]) => (state.files[p] ?? "") !== t);
  els.dirty.textContent = dirty ? "modified" : "";
  els.dirty.classList.toggle("is-dirty", dirty);
}

function renderFileTabs() {
  const base = PLAYGROUND_EXAMPLES[state.exampleId]?.files || {};
  els.fileTabs.innerHTML = Object.keys(state.files)
    .map((n) => {
      const mod = (state.files[n] ?? "") !== (base[n] ?? "");
      return `<button type="button" role="tab" aria-selected="${n === state.activeFile}" data-file-tab="${escapeHtml(n)}"><svg class="i" aria-hidden="true"><use href="#i-file"/></svg>${escapeHtml(n)}${mod ? '<span class="file-mod-dot" aria-label="modified"></span>' : ""}</button>`;
    })
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
  state.showAllSymbols = false;
  state.activeGenFile = "build.ninja";
  state.generator = ex.id === "presets-v13" ? "Ninja Multi-Config" : "Ninja";
  state.buildType = "Debug";
  els.configSelect.value = "Debug";
  syncGen();
  els.exampleSelect.value = ex.id;
  els.exampleSelect.title = ex.description || "";
  els.textarea.value = state.files[state.activeFile] ?? "";
  els.textarea.selectionStart = 0;
  els.textarea.selectionEnd = 0;
  els.textarea.scrollTop = 0;
  renderFileTabs();
  syncEditor();
}

function orderedGenFiles(res) {
  const names = Object.keys(res?.files || {});
  const rank = (name) => {
    if (name === "build.ninja") return 0;
    if (name === `build-${state.buildType}.ninja`) return 1;
    if (/^build-.*\.ninja$/.test(name)) return 2;
    if (name === "CMakeFiles/rules.ninja") return 3;
    if (name === "compile_commands.json") return 4;
    if (name === "cmake_install.cmake") return 5;
    if (name.startsWith("CMakeFiles/Export/")) return 6;
    if (name.startsWith("CMakeFiles/impl-")) return 7;
    return 8;
  };
  return names.sort((a, b) => rank(a) - rank(b) || a.localeCompare(b));
}

function currentGenFile(res) {
  const files = res?.files || {};
  const list = orderedGenFiles(res);
  if (!list.length) return null;
  const name = state.activeGenFile in files ? state.activeGenFile : list[0];
  return { name, text: files[name], all: list };
}

function hlLine(line, filename = "build.ninja") {
  if (filename.endsWith(".json")) return escapeHtml(line);
  if (line.trimStart().startsWith("#")) return `<span class="hl-cm">${escapeHtml(line)}</span>`;
  if (filename.endsWith(".cmake")) {
    return escapeHtml(line)
      .replace(/^(\s*)([A-Za-z_][A-Za-z0-9_]*)(\s*\()/, '$1<span class="hl-kw">$2</span>$3')
      .replace(/(\$\{[^}]+\})/g, '<span class="hl-var">$1</span>');
  }
  return escapeHtml(line)
    .replace(/^(\s*)(rule|build|pool|default|subninja|include)\b/, '$1<span class="hl-kw">$2</span>')
    .replace(/^(\s+)([A-Za-z_][A-Za-z0-9_]*)(\s*=)/, '$1<span class="hl-assign">$2</span>$3')
    .replace(/(\$(?:in|out|[A-Z_][A-Z0-9_]*|\{[^}]+\}))/g, '<span class="hl-var">$1</span>');
}

function previewText(val) {
  if (val === null || val === undefined) return "(unset)";
  if (typeof val === "object") {
    if (typeof val.prefix === "string") return val.truncated ? `${val.prefix}...` : val.prefix;
    if (typeof val.bytes_hex === "string") return `0x${val.bytes_hex}`;
    return JSON.stringify(val);
  }
  return String(val);
}

function chips(raw) {
  const str = previewText(raw);
  const parts = str.split(";").map((s) => s.trim()).filter(Boolean);
  return (parts.length ? parts : [str]).map((p) => `<span class="chip is-ink">${escapeHtml(stripSrc(p))}</span>`).join("");
}

function renderDiagnostics(res) {
  const list = res?.diagnostics || [];
  if (!list.length) {
    if (res?.ok) {
      const nTargets = res.targets?.length ?? 0;
      const nFiles = Object.keys(res.files || {}).length;
      const dur = typeof res.durationMs === "number" ? ` \u00b7 ${formatMs(res.durationMs)}` : "";
      return `<div class="diag-empty" data-test="diag-clean"><span class="st-pass"><svg class="i" aria-hidden="true"><use href="#i-check"/></svg></span><div><div class="diag-msg">Configure succeeded with 0 errors and 0 warnings.</div><span class="diag-cat">${nTargets} ${nTargets === 1 ? "target" : "targets"} \u00b7 ${nFiles} generated ${nFiles === 1 ? "file" : "files"}${dur}</span></div></div>`;
    }
    return '<p class="dim">No diagnostics.</p>';
  }
  return list.map((d) => {
    const sev = String(d.severity || "info");
    const [ic, cls] = sev === "error" ? ["i-cross", "st-fail"] : sev === "warning" ? ["i-differs", "st-partial"] : ["i-check", "st-pass"];
    const loc = d.file && d.line ? locBtn({ path: d.file, line: d.line, col: d.column }) : "";
    return `<div class="diag-row" data-diag-severity="${escapeHtml(sev)}"><span class="${cls}"><svg class="i" aria-hidden="true"><use href="#${ic}"/></svg><span class="visually-hidden">${escapeHtml(sev)}</span></span><div class="diag-msg">${escapeHtml(d.message)}${d.category ? `<span class="diag-cat">${escapeHtml(d.category)}</span>` : ""}</div><div>${loc}</div></div>`;
  }).join("");
}

function renderTargets(res) {
  const list = res?.targets || [];
  if (!list.length) return '<p class="dim">No targets.</p>';
  const fields = [
    ["defined at", (t) => t.definedAt, true],
    ["sources", (t) => t.sources?.join(" "), false],
    ["features", (t) => t.compileFeatures?.join(" "), false],
    ["definitions", (t) => t.compileDefinitions?.join(" "), false],
    ["options", (t) => t.compileOptions?.join(" "), false],
    ["includes", (t) => t.includeDirectories?.join(" "), false],
    ["links", (t) => t.linkLibraries?.join(" "), false]
  ];
  return list.map((t) => {
    const n = t.sources?.length || 0;
    const rows = fields
      .filter(([, get]) => get(t))
      .map(([k, get, isLoc]) => `<dt>${k}</dt><dd>${isLoc ? (locBtn(get(t)) || escapeHtml(get(t))) : escapeHtml(get(t))}</dd>`)
      .join("");
    return `<div class="target-card" data-target-name="${escapeHtml(t.name)}"><div class="target-head"><svg class="i" aria-hidden="true"><use href="#i-target"/></svg><span class="target-name">${escapeHtml(t.name)}</span><span class="chip is-ink">${escapeHtml(t.kind)}</span><span class="dim">${n} ${n === 1 ? "source" : "sources"}</span><button type="button" class="ctrl-btn target-explain-btn" data-target-explain="${escapeHtml(t.name)}"><svg class="i" aria-hidden="true"><use href="#i-tree"/></svg>Explain</button></div>${rows ? `<dl class="target-meta">${rows}</dl>` : ""}</div>`;
  }).join("");
}

function renderGenerated(res) {
  const f = currentGenFile(res);
  if (!f) return '<p class="dim">No generated files.</p>';
  const lines = String(f.text || "").split("\n");
  const filePicker = f.all.length > 1
    ? `<label class="gen-file-label">File <select id="gen-file-select" data-test="gen-file-select">${f.all.map((n) => `<option value="${escapeHtml(n)}"${n === f.name ? " selected" : ""}>${escapeHtml(n)}</option>`).join("")}</select></label>`
    : `<span class="gen-file-label">${escapeHtml(f.name)}</span>`;
  const countText = f.all.length > 1 ? `${lines.length} lines \u00b7 ${f.all.length} files` : `${lines.length} lines`;
  const bar = `<div class="gen-file-bar">${filePicker}<div class="gen-file-tools"><span class="dim">${escapeHtml(countText)}</span><button id="toggle-wrap-btn" type="button" class="ctrl-btn gen-wrap-btn" aria-pressed="${state.wrapOutput}" data-test="toggle-wrap-btn">Wrap</button></div></div>`;
  const code = `<div class="code-table${state.wrapOutput ? " is-wrapped" : ""}" data-test="generated-code">${lines.map((l, i) => `<div class="code-line"><span class="code-ln">${i + 1}</span><span>${hlLine(l, f.name)}</span></div>`).join("")}</div>`;
  return `${bar}${code}`;
}

function curateExplainSymbols(res) {
  const all = Array.isArray(res?.explainSymbols) ? res.explainSymbols : [];
  const targets = (res?.targets || []).map((t) => t.name);
  const targetSet = new Set(targets);
  const projectName = (() => {
    const m = String(state.files["CMakeLists.txt"] || "").match(/\bproject\s*\(\s*([A-Za-z0-9_.-]+)/i);
    return m ? m[1] : "";
  })();
  const exDefault = PLAYGROUND_EXAMPLES[state.exampleId]?.defaultSymbol || "";

  const isUsefulVar = (s) => {
    if (targetSet.has(s)) return false;
    if (s === exDefault || s === "PROJECT_VERSION" || s === "CMAKE_EXPORT_COMPILE_COMMANDS") return true;
    if (BOILERPLATE_EXACT.has(s)) return false;
    if (BOILERPLATE_PREFIXES.some((p) => s.startsWith(p))) return false;
    if (s.startsWith("PROJECT_")) return false;
    if (projectName && s.startsWith(`${projectName}_`)) return false;
    return true;
  };

  const quick = [];
  const push = (s) => { if (s && !quick.includes(s)) quick.push(s); };
  targets.forEach(push);
  if (exDefault && (all.includes(exDefault) || state.explainSymbol === exDefault)) push(exDefault);
  all.filter(isUsefulVar).forEach(push);
  if (state.explainSymbol && !quick.includes(state.explainSymbol)) push(state.explainSymbol);
  return { quick: quick.slice(0, 12), all: Array.from(new Set([...quick, ...all])) };
}

function renderExplainChain(chain) {
  if (!Array.isArray(chain) || !chain.length) return "";
  return chain.map((h) => {
    if (h.from !== undefined) {
      return `<div class="ex-chain"><span>${escapeHtml(`${h.from} -> ${h.via || h.to}`)}</span>${h.edge ? `<span class="chip">${escapeHtml(h.edge)}</span>` : ""}${locBtn(h.location)}</div>`;
    }
    const targetLabel = h.target ? `via ${h.target}` : `via target#${h.target_id}`;
    return `<div class="ex-chain"><span>${escapeHtml(targetLabel)}</span>${h.scope ? `<span class="chip">${escapeHtml(h.scope)}</span>` : ""}${locBtn(h.origin)}</div>`;
  }).join("");
}

function renderExplainSources(sources) {
  if (!Array.isArray(sources) || !sources.length) return "";
  return sources.map((s) => {
    if (typeof s === "string") return `<span class="chip is-ink">${escapeHtml(stripSrc(s))}</span>`;
    const name = stripSrc(s?.value || "");
    return `<span class="ex-src-item"><span class="chip is-ink">${escapeHtml(name)}</span>${locBtn(s?.origin)}</span>`;
  }).join("");
}

function renderExplainReport(rep) {
  if (!rep) return '<p class="dim">Pick a target or variable.</p>';
  if (state.showRawJson) return `<pre class="code-table" data-test="explain-raw-json">${escapeHtml(JSON.stringify(rep, null, 2))}</pre>`;
  const d = rep.details || {};
  if (rep.kind === "not_found") {
    const sugs = (d.suggestions || []).map((s) => `<button type="button" class="ctrl-btn" data-explain-sym="${escapeHtml(s.name)}">${escapeHtml(s.name)} <span class="dim">${escapeHtml(s.kind)}</span></button>`).join("");
    return `<div class="explain-tree" data-test="explain-not-found"><div class="ex-root"><span class="st st-partial"><svg class="i" aria-hidden="true"><use href="#i-differs"/></svg><span>not found</span></span><span class="target-name">${escapeHtml(rep.name)}</span></div>${sugs ? `<div class="explain-syms">${sugs}</div>` : '<p class="dim">Symbol not found in the current configure graph. Pick a target or variable above.</p>'}</div>`;
  }
  if (rep.kind === "ambiguous") {
    const kinds = (d.kinds || []).map((k) => `<span class="chip is-ink">${escapeHtml(k)}</span>`).join("");
    return `<div class="explain-tree" data-test="explain-ambiguous"><div class="ex-root"><span class="st st-partial"><svg class="i" aria-hidden="true"><use href="#i-differs"/></svg><span>ambiguous</span></span><span class="target-name">${escapeHtml(rep.name)}</span>${kinds}</div></div>`;
  }
  if (rep.kind === "variable") {
    const writesList = Array.isArray(d.writes) ? d.writes : [];
    const rootLoc = (d.location && Number(d.location.line || 0) >= 1)
      ? d.location
      : (writesList.length ? writesList[writesList.length - 1].origin || writesList[writesList.length - 1].location : d.location);
    const metaParts = d.cache
      ? [
          `cache ${d.cache.type || "STRING"}`,
          d.cache.help || "",
          d.history ? `history: ${d.history}` : ""
        ].filter(Boolean)
      : [];
    const cacheMeta = metaParts.length
      ? `<div class="ex-meta-row">${metaParts.map((part) => `<span class="dim">${escapeHtml(part)}</span>`).join('<span class="dim" aria-hidden="true">\u00b7</span>')}</div>`
      : "";
    const writes = writesList.map((w) => {
      const op = w.operation || w.op || w.command || "set";
      const val = chips(w.value);
      const scopeTag = w.in_queried_scope === false ? '<span class="chip">block scope</span>' : "";
      const loc = locBtn(w.origin?.length ? w.origin : w.location);
      return `<li class="var-step"><span class="muted">${escapeHtml(op)}</span>${val}${scopeTag}${loc}</li>`;
    }).join("");
    const timeline = writes
      ? `<ol class="var-timeline">${writes}</ol>`
      : '<p class="dim ex-note">Set via cache or preset before script evaluation (no listfile writes).</p>';
    return `<div class="explain-tree" data-test="explain-variable"><div class="ex-root"><span class="target-name">${escapeHtml(rep.name)}</span>${d.binding ? `<span class="chip is-ink">${escapeHtml(d.binding)}</span>` : ""}${chips(d.value)}${locBtn(rootLoc)}</div>${cacheMeta}${timeline}</div>`;
  }

  const cfg = d.configurations?.find((c) => c.name === state.buildType) || d.configurations?.[0];
  const srcs = renderExplainSources(cfg?.sources?.length ? cfg.sources : d.sources);
  const entries = (cfg?.entries || []).map((e) => {
    const prop = e.kind || e.property || "";
    const explicitScope = e.scope || e.link_role || "";
    const inputName = !explicitScope && e.input?.name ? String(e.input.name) : "";
    const scopeChip = explicitScope
      ? `<span class="chip">${escapeHtml(explicitScope)}</span>`
      : inputName
        ? `<button type="button" class="chip chip-btn" data-explain-sym="${escapeHtml(inputName)}" title="Explain ${escapeHtml(inputName)}">${escapeHtml(inputName)}</button>`
        : "";
    const originLoc = (Array.isArray(e.origin) ? e.origin.length > 0 : Boolean(e.origin)) ? e.origin : e.input?.location;
    const chain = renderExplainChain(e.chain);
    return `<div class="ex-entry"><div class="ex-entry-row"><span class="ex-prop">${escapeHtml(prop)}</span><div class="ex-vals">${chips(e.value)}${scopeChip}${locBtn(originLoc)}</div></div>${chain}</div>`;
  }).join("");
  const aliasLoc = d.alias_definition ? `<span class="chip">alias</span>${locBtn(d.alias_definition)}` : "";
  return `<div class="explain-tree" data-test="explain-tree"><div class="ex-root"><svg class="i" aria-hidden="true"><use href="#i-target"/></svg><span class="target-name">${escapeHtml(d.target || rep.name)}</span><span class="chip is-ink">${escapeHtml(d.target_kind || "TARGET")}</span>${locBtn(d.definition)}${aliasLoc}</div><div class="ex-branch"><div class="ex-entry"><div class="ex-entry-row"><span class="ex-prop">CONFIGURATION</span><div class="ex-vals"><span class="chip is-ink">${escapeHtml(cfg?.name || state.buildType)}</span><span class="dim">${escapeHtml(d.configuration_selection?.status || "matched")}</span></div></div></div><div class="ex-branch">${srcs ? `<div class="ex-entry"><div class="ex-entry-row"><span class="ex-prop">SOURCES</span><div class="ex-vals">${srcs}</div></div></div>` : ""}${entries || '<div class="ex-entry"><span class="dim">No resolved compile or link entries.</span></div>'}</div></div></div>`;
}

function renderExplainTab(res) {
  const { quick, all } = curateExplainSymbols(res);
  const visible = state.showAllSymbols ? all : quick;
  const hiddenCount = Math.max(0, all.length - quick.length);
  const syms = visible
    .map((s) => `<button type="button" class="ctrl-btn sym-btn${s === state.explainSymbol ? " is-active" : ""}" data-explain-sym="${escapeHtml(s)}" aria-pressed="${s === state.explainSymbol}">${escapeHtml(s)}</button>`)
    .join("");
  const moreBtn = hiddenCount > 0
    ? `<button id="toggle-all-syms" type="button" class="ctrl-btn sym-more-btn" data-test="toggle-all-syms">${state.showAllSymbols ? "Fewer symbols" : `+${hiddenCount} more symbols`}</button>`
    : "";
  const datalist = `<datalist id="explain-datalist">${all.map((s) => `<option value="${escapeHtml(s)}"></option>`).join("")}</datalist>`;
  return `<form id="explain-form" class="explain-bar"><input id="explain-input" class="search-input" type="text" list="explain-datalist" value="${escapeHtml(state.explainSymbol)}" placeholder="Target or variable" aria-label="Symbol to explain" data-test="explain-input">${datalist}<button class="ctrl-btn" type="submit" data-test="explain-submit">Explain</button><button id="toggle-explain-json" class="ctrl-btn" type="button" data-test="toggle-explain-json">${state.showRawJson ? "Structured" : "Raw JSON"}</button></form><div class="explain-syms" data-test="explain-symbols">${syms}${moreBtn}</div>${renderExplainReport(state.explainReport)}`;
}

async function explain(sym) {
  state.explainSymbol = sym;
  if (adapter.mode !== "wasm") {
    state.explainReport = null;
    renderInspector();
    return;
  }
  try {
    state.explainReport = await adapter.explain(sym, { config: state.buildType });
  } catch (err) {
    state.runtimeError = err instanceof Error ? err.message : String(err);
    state.explainReport = null;
  }
  renderInspector();
}

function renderRuntimeError(message) {
  return `<div class="diag-row" data-diag-severity="error" data-test="wasm-load-error"><span class="st-fail"><svg class="i" aria-hidden="true"><use href="#i-cross"/></svg><span class="visually-hidden">error</span></span><div class="diag-msg">${escapeHtml(message)}<span class="diag-cat">WebAssembly runtime unavailable. Serve the site over HTTP with WebAssembly enabled and retry.</span></div><div></div></div>`;
}

function renderInspector(fade = false) {
  const res = state.lastResult;
  els.outputTabs.querySelectorAll("[data-inspector-tab]").forEach((t) => t.setAttribute("aria-selected", String(t.dataset.inspectorTab === state.activeTab)));
  const f = currentGenFile(res);
  const genFiles = orderedGenFiles(res);
  els.fileTabName.textContent = genFiles.includes("build.ninja") ? "build.ninja" : (f?.name || "build.ninja");
  if (els.fileCount) els.fileCount.textContent = genFiles.length > 1 ? String(genFiles.length) : "";
  const showCopy = !state.runtimeError && ((state.activeTab === "ninja" && Boolean(f)) || (state.activeTab === "explain" && state.showRawJson && Boolean(state.explainReport)));
  els.copyBtn.hidden = !showCopy;
  if (els.outputActions) els.outputActions.hidden = !showCopy;
  const diags = res?.diagnostics || [];
  const errCount = diags.filter((d) => d.severity === "error").length;
  els.diagCount.textContent = state.runtimeError ? "1" : res ? String(diags.length) : "";
  els.diagCount.classList.toggle("is-error", Boolean(state.runtimeError) || errCount > 0);
  els.targetCount.textContent = res ? String(res.targets?.length ?? 0) : "";

  const views = { diagnostics: renderDiagnostics, targets: renderTargets, ninja: renderGenerated, explain: renderExplainTab };
  if (fade && !prefersReducedMotion()) {
    els.body.classList.add("is-fading");
    requestAnimationFrame(() => els.body.classList.remove("is-fading"));
  }
  if (state.runtimeError) {
    els.body.innerHTML = renderRuntimeError(state.runtimeError);
    return;
  }
  els.body.innerHTML = (views[state.activeTab] || renderGenerated)(res);
  els.body.querySelectorAll("[data-jump-line]").forEach((b) => b.addEventListener("click", () => jumpTo(b.dataset.jumpFile, b.dataset.jumpLine, b.dataset.jumpCol)));
  els.body.querySelectorAll("[data-target-explain]").forEach((b) => b.addEventListener("click", () => {
    state.activeTab = "explain";
    state.lastManualTab = "explain";
    state.autoSwitchedToDiag = false;
    explain(b.dataset.targetExplain);
  }));
  if (state.activeTab === "ninja") {
    $("gen-file-select")?.addEventListener("change", (e) => {
      state.activeGenFile = e.target.value;
      renderInspector(false);
    });
    $("toggle-wrap-btn")?.addEventListener("click", () => {
      state.wrapOutput = !state.wrapOutput;
      renderInspector(false);
    });
  }
  if (state.activeTab === "explain") {
    $("explain-form")?.addEventListener("submit", (e) => { e.preventDefault(); explain($("explain-input").value.trim()); });
    $("toggle-explain-json")?.addEventListener("click", () => { state.showRawJson = !state.showRawJson; renderInspector(); });
    $("toggle-all-syms")?.addEventListener("click", () => { state.showAllSymbols = !state.showAllSymbols; renderInspector(); });
    els.body.querySelectorAll("[data-explain-sym]").forEach((b) => b.addEventListener("click", () => explain(b.dataset.explainSym)));
  }
}

function setRunning(on) {
  const reduced = prefersReducedMotion();
  if (on) {
    els.runStatus.textContent = "running";
    els.runStatus.classList.remove("st-pass", "st-fail");
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
  renderFileTabs();
  syncEditor();
  setRunning(true);
  try {
    if (adapter.mode !== "wasm") {
      const retry = await adapter.load(BUNDLE_URL);
      updateModeBadge(retry.mode);
      els.modeBadge.title = adapter.loadReason;
      if (retry.mode !== "wasm") {
        throw new Error(adapter.loadReason);
      }
    }
    state.runtimeError = null;
    const res = await adapter.configure(state.files, { exampleId: state.exampleId, generator: state.generator, buildType: state.buildType });
    state.lastResult = res;
    updateModeBadge(res.mode, res.durationMs);

    const errCount = (res.diagnostics || []).filter((d) => d.severity === "error").length;
    if (!res.ok || errCount > 0) {
      state.explainReport = null;
      if (state.activeTab !== "diagnostics") {
        state.activeTab = "diagnostics";
        state.autoSwitchedToDiag = true;
      }
      els.runStatus.textContent = `configure failed (${errCount || 1} ${errCount === 1 ? "error" : "errors"}) \u00b7 ${formatMs(res.durationMs)}`;
      els.runStatus.classList.add("st-fail");
      els.runStatus.classList.remove("st-pass");
    } else {
      if (state.autoSwitchedToDiag) {
        state.activeTab = state.lastManualTab || "ninja";
        state.autoSwitchedToDiag = false;
      }
      if (!(state.activeGenFile in (res.files || {}))) {
        state.activeGenFile = orderedGenFiles(res)[0] || "build.ninja";
      }
      let report = state.explainSymbol ? await adapter.explain(state.explainSymbol, { config: state.buildType }) : null;
      if (!report || report.kind === "not_found") {
        const fallback = res.targets?.[0]?.name || curateExplainSymbols(res).quick[0] || res.explainSymbols?.[0] || "";
        if (fallback && fallback !== state.explainSymbol) {
          state.explainSymbol = fallback;
          report = await adapter.explain(fallback, { config: state.buildType });
        }
      }
      state.explainReport = report;
      els.runStatus.textContent = `configured in ${formatMs(res.durationMs)}`;
      els.runStatus.classList.add("st-pass");
      els.runStatus.classList.remove("st-fail");
    }
    const n = res.targets?.length ?? 0;
    els.runSummary.textContent = `${n} ${n === 1 ? "target" : "targets"} \u00b7 ${state.generator} \u00b7 ${state.buildType}`;
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    state.runtimeError = message;
    state.lastResult = null;
    state.explainReport = null;
    state.activeTab = "diagnostics";
    updateModeBadge("error");
    els.runStatus.textContent = message;
    els.runStatus.classList.add("st-fail");
    els.runStatus.classList.remove("st-pass");
    els.runSummary.textContent = "";
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
  const isMac = typeof navigator !== "undefined" && /Mac/i.test(navigator.platform || navigator.userAgent || "") && !/iPhone|iPad|iPod/i.test(navigator.userAgent || "");
  if (els.runKbd) els.runKbd.textContent = isMac ? "\u2318\u21b5" : "Ctrl+Enter";
  els.runBtn.title = isMac ? "Run configure (Cmd+Enter)" : "Run configure (Ctrl+Enter)";
  els.exampleSelect.innerHTML = Object.values(PLAYGROUND_EXAMPLES).map((ex) => `<option value="${escapeHtml(ex.id)}">${escapeHtml(ex.title)}</option>`).join("");

  const params = typeof window !== "undefined" ? new URLSearchParams(window.location.search) : new URLSearchParams();
  const reqExample = params.get("example");
  if (reqExample && reqExample in PLAYGROUND_EXAMPLES) state.exampleId = reqExample;
  loadExample(state.exampleId);
  const reqTab = params.get("tab");
  if (["ninja", "targets", "diagnostics", "explain"].includes(reqTab)) {
    state.activeTab = reqTab;
    state.lastManualTab = reqTab;
  }
  const reqSymbol = params.get("symbol");
  if (reqSymbol) state.explainSymbol = reqSymbol.trim();

  els.exampleSelect.addEventListener("change", () => { loadExample(els.exampleSelect.value); runConfigure(); });
  initRovingGroup(els.genGroup, "[data-gen]", (b) => { state.generator = b.dataset.gen; syncGen(); runConfigure(); });
  els.configSelect.addEventListener("change", () => {
    state.buildType = els.configSelect.value;
    if (state.generator === "Ninja Multi-Config" && (state.activeGenFile === "build.ninja" || /^build-.*\.ninja$/.test(state.activeGenFile))) {
      state.activeGenFile = `build-${state.buildType}.ninja`;
    }
    runConfigure();
  });
  els.resetBtn.addEventListener("click", () => { loadExample(state.exampleId); runConfigure(); });
  els.runBtn.addEventListener("click", runConfigure);
  initRovingGroup(els.fileTabs, "[data-file-tab]", (b) => {
    state.files[state.activeFile] = els.textarea.value;
    state.activeFile = b.dataset.fileTab;
    els.textarea.value = state.files[state.activeFile] ?? "";
    renderFileTabs();
    syncEditor();
  });
  initRovingGroup(els.outputTabs, "[data-inspector-tab]", (b) => {
    state.activeTab = b.dataset.inspectorTab;
    state.lastManualTab = state.activeTab;
    state.autoSwitchedToDiag = false;
    renderInspector(false);
  });
  document.addEventListener("keydown", (e) => {
    if ((e.metaKey || e.ctrlKey) && e.key === "Enter" && e.target !== $("explain-input")) {
      e.preventDefault();
      runConfigure();
    }
  });
  els.textarea.addEventListener("input", () => { state.files[state.activeFile] = els.textarea.value; renderFileTabs(); syncEditor(); });
  els.textarea.addEventListener("scroll", () => { els.gutter.scrollTop = els.textarea.scrollTop; });
  ["click", "keyup"].forEach((ev) => els.textarea.addEventListener(ev, syncEditor));
  els.textarea.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      e.preventDefault();
      els.textarea.blur();
      return;
    }
    if (e.key === "Tab") {
      e.preventDefault();
      const { selectionStart: s, selectionEnd: end, value: v } = els.textarea;
      if (!e.shiftKey) {
        let inserted = false;
        try {
          if (typeof document.execCommand === "function") inserted = document.execCommand("insertText", false, "  ");
        } catch (_) {}
        if (!inserted) {
          if (typeof els.textarea.setRangeText === "function") els.textarea.setRangeText("  ", s, end, "end");
          else {
            els.textarea.value = `${v.slice(0, s)}  ${v.slice(end)}`;
            els.textarea.selectionStart = els.textarea.selectionEnd = s + 2;
          }
        }
      } else {
        const lineStart = v.lastIndexOf("\n", Math.max(0, s - 1)) + 1;
        const prefix = v.slice(lineStart, lineStart + 2);
        const drop = prefix === "  " ? 2 : prefix.startsWith(" ") || prefix.startsWith("\t") ? 1 : 0;
        if (drop > 0) {
          els.textarea.setSelectionRange(lineStart, lineStart + drop);
          let removed = false;
          try {
            if (typeof document.execCommand === "function") removed = document.execCommand("delete");
          } catch (_) {}
          if (!removed) {
            if (typeof els.textarea.setRangeText === "function") els.textarea.setRangeText("", lineStart, lineStart + drop, "start");
            else els.textarea.value = `${v.slice(0, lineStart)}${v.slice(lineStart + drop)}`;
          }
          const nextStart = Math.max(lineStart, s - drop);
          els.textarea.setSelectionRange(nextStart, Math.max(nextStart, end - drop));
        }
      }
      state.files[state.activeFile] = els.textarea.value;
      renderFileTabs();
      syncEditor();
    }
  });
  els.copyBtn.addEventListener("click", () => {
    const text = state.activeTab === "explain" && state.showRawJson && state.explainReport
      ? JSON.stringify(state.explainReport, null, 2)
      : currentGenFile(state.lastResult)?.text || "";
    copyWithFeedback(els.copyBtn, text, "Copy");
  });
  initSplit();

  updateModeBadge("loading");
  const info = await adapter.load(BUNDLE_URL);
  updateModeBadge(info.mode);
  els.modeBadge.title = adapter.loadReason;
  if (info.mode !== "wasm") {
    state.runtimeError = adapter.loadReason;
    state.activeTab = "diagnostics";
    els.runStatus.textContent = adapter.loadReason;
    els.runStatus.classList.add("st-fail");
    els.runSummary.textContent = "";
    renderInspector(false);
    return;
  }
  await runConfigure();
}

init();
