/* rcmake configure pipeline diagram and 35-crate map. MIT OR Apache-2.0. */

import { escapeHtml, initRovingGroup } from "./app.js";
import { prefersReducedMotion } from "./motion.js";

export const PIPELINE_MODEL = {
  stages: [
    { id: "syntax", label: "syntax", colorVar: "var(--ch-r)", icon: "i-code", sentence: "Lexes and parses CMake language files.", crates: ["syntax"] },
    { id: "eval", label: "eval", colorVar: "var(--ch-r)", icon: "i-terminal", sentence: "Runs commands and tracks variables, policies, and generator expressions.", crates: ["eval", "commands", "expr", "genex", "model", "presets"] },
    { id: "resolve", label: "resolve", colorVar: "var(--ch-g)", icon: "i-target", sentence: "Builds the target graph, usage requirements, and link order.", crates: ["resolve", "build", "cxxmodules", "fortran", "cps"] },
    { id: "gen", label: "gen", colorVar: "var(--ch-b)", icon: "i-graph", sentence: "Writes build.ninja, Unix Makefiles, project.pbxproj, and .vcxproj files.", crates: ["gen-common", "gen-ninja", "gen-make", "gen-xcode", "gen-vs"] }
  ],
  outputs: ["build.ninja", "Makefile", "project.pbxproj", ".sln .vcxproj"],
  hosts: {
    native: { label: "NativeHost", sentence: "NativeHost reads the real disk, environment, processes, and clock.", chips: ["disk", "env", "processes", "clock"], crates: ["host"] },
    pure: { label: "PureHost", sentence: "PureHost keeps files in memory and gives the same result every run. Tests, editors, and the wasm32 playground use it.", chips: ["in-memory files", "tests", "editors", "wasm32"], crates: ["host", "wasm"] }
  },
  crateGroups: [
    { name: "Base", crates: ["base", "json", "diag", "host", "regex", "archive"] },
    { name: "Language", crates: ["syntax", "expr", "genex", "model", "eval", "commands", "presets"] },
    { name: "Build", crates: ["resolve", "build", "cxxmodules", "fortran", "cps", "gen-common", "gen-ninja", "gen-make", "gen-xcode", "gen-vs"] },
    { name: "Drivers", crates: ["cli", "api", "lsp", "wasm", "ctest", "cpack", "tool-e", "store", "fetch", "instrument"] },
    { name: "Verification", crates: ["harness", "fuzz"] }
  ]
};

export function renderPipeline(model = PIPELINE_MODEL, orientation = "horizontal", state = {}) {
  const sel = state.selected || "eval";
  const isPure = state.hostMode === "pure";
  const host = isPure ? model.hosts.pure : model.hosts.native;
  const edge = (d, ch, delay, end, arrow = "") => `<path class="pipe-edge" d="${d}"/>${arrow}<path class="pipe-pulse" d="${d}" stroke="${ch}" style="--p-delay:${delay}ms;--p-end:${end}"/>`;
  const tap = (d, i) => `<path class="pipe-tap${isPure ? " is-pure" : ""}" d="${d}"/><path class="pipe-tap-pulse" d="${d}" style="--t-delay:${i * 180}ms"/>`;
  const arrowR = (x, y) => `<polygon class="pipe-arrow" points="${x},${y} ${x - 6},${y - 3.5} ${x - 6},${y + 3.5}"/>`;
  const arrowD = (x, y) => `<polygon class="pipe-arrow" points="${x},${y} ${x - 3.5},${y - 6} ${x + 3.5},${y - 6}"/>`;

  if (orientation === "vertical") {
    const ns = [
      { id: "syntax", y: 90, ch: "var(--ch-r)" }, { id: "eval", y: 170, ch: "var(--ch-r)" },
      { id: "resolve", y: 250, ch: "var(--ch-g)" }, { id: "gen", y: 330, ch: "var(--ch-b)" }
    ];
    const outs = model.outputs.map((label, i) => ({ label, y: 396 + i * 40 }));
    const edges = [
      edge("M 130 56 L 130 88", "var(--ch-r)", 0, -120, arrowD(130, 90)),
      edge("M 130 134 L 130 168", "var(--ch-r)", 150, -120, arrowD(130, 170)),
      edge("M 130 214 L 130 248", "var(--ch-r)", 300, -120, arrowD(130, 250)),
      edge("M 130 294 L 130 328", "var(--ch-g)", 450, -120, arrowD(130, 330)),
      ...outs.map((o) => edge(`M 54 374 L 54 ${o.y + 7} Q 54 ${o.y + 15} 62 ${o.y + 15} L 86 ${o.y + 15}`, "var(--ch-b)", 600, -240, arrowR(88, o.y + 15)))
    ].join("");
    const taps = ns.map((n, i) => tap(`M 230 ${n.y + 22} L 272 ${n.y + 22}`, i)).join("");
    const boxes = ns.map((n) => `<g class="pipe-node-group${sel === n.id ? " is-selected" : ""}" data-pipe-stage="${n.id}" style="--node-ch:${n.ch}"><rect class="pipe-node-bg" x="30" y="${n.y}" width="200" height="44" rx="6"/><rect x="31" y="${n.y}" width="198" height="2.5" rx="1" fill="${n.ch}"/><text class="pipe-label" x="130" y="${n.y + 27}" text-anchor="middle">${escapeHtml(n.id)}</text></g>`).join("");
    const outBoxes = outs.map((o) => `<g><rect class="pipe-node-bg" x="88" y="${o.y}" width="204" height="30" rx="5"/><rect x="89" y="${o.y}" width="202" height="2" rx="1" fill="var(--ch-b)"/><text class="pipe-label-muted" x="190" y="${o.y + 19}" text-anchor="middle">${escapeHtml(o.label)}</text></g>`).join("");
    return `<svg class="pipe-svg" viewBox="0 0 360 562" role="img" aria-label="Configure pipeline from CMakeLists.txt through syntax, eval, resolve, and gen to build files over Host">${edges}${taps}<g><rect class="pipe-node-bg" x="30" y="12" width="200" height="44" rx="6"/><text class="pipe-label" x="130" y="39" text-anchor="middle">CMakeLists.txt</text></g>${boxes}${outBoxes}<g class="pipe-node-group${sel === "host" ? " is-selected" : ""}" data-pipe-stage="host" style="--node-ch:var(--ink)"><rect class="pipe-node-bg" x="272" y="90" width="68" height="284" rx="6"/><text class="pipe-label" x="306" y="232" text-anchor="middle" transform="rotate(-90 306 232)">${escapeHtml(host.label)}</text></g></svg>`;
  }

  const ns = [
    { id: "syntax", x: 230, w: 140, ch: "var(--ch-r)" }, { id: "eval", x: 410, w: 140, ch: "var(--ch-r)" },
    { id: "resolve", x: 590, w: 140, ch: "var(--ch-g)" }, { id: "gen", x: 770, w: 108, ch: "var(--ch-b)" }
  ];
  const outs = model.outputs.map((label, i) => ({ label, y: 36 + i * 56 }));
  const edges = [
    edge("M 176 138 L 228 138", "var(--ch-r)", 0, -100, arrowR(230, 138)),
    edge("M 370 138 L 408 138", "var(--ch-r)", 140, -80, arrowR(410, 138)),
    edge("M 550 138 L 588 138", "var(--ch-r)", 280, -80, arrowR(590, 138)),
    edge("M 730 138 L 768 138", "var(--ch-g)", 440, -80, arrowR(770, 138)),
    ...outs.map((o) => edge(`M 878 138 C 898 138, 898 ${o.y + 16}, 916 ${o.y + 16}`, "var(--ch-b)", 600, -160, arrowR(918, o.y + 16)))
  ].join("");
  const taps = ns.map((n, i) => tap(`M ${n.x + n.w / 2} 160 L ${n.x + n.w / 2} 274`, i)).join("");
  const boxes = ns.map((n) => `<g class="pipe-node-group${sel === n.id ? " is-selected" : ""}" data-pipe-stage="${n.id}" style="--node-ch:${n.ch}"><rect class="pipe-node-bg" x="${n.x}" y="116" width="${n.w}" height="44" rx="6"/><rect x="${n.x + 1}" y="116" width="${n.w - 2}" height="2.5" rx="1" fill="${n.ch}"/><text class="pipe-label" x="${n.x + n.w / 2}" y="143" text-anchor="middle">${escapeHtml(n.id)}</text></g>`).join("");
  const outBoxes = outs.map((o) => `<g><rect class="pipe-node-bg" x="918" y="${o.y}" width="142" height="32" rx="5"/><rect x="919" y="${o.y}" width="140" height="2" rx="1" fill="var(--ch-b)"/><text class="pipe-label-muted" x="989" y="${o.y + 20}" text-anchor="middle">${escapeHtml(o.label)}</text></g>`).join("");
  return `<svg class="pipe-svg" viewBox="0 0 1076 336" role="img" aria-label="Configure pipeline from CMakeLists.txt through syntax, eval, resolve, and gen to build files over Host">${edges}${taps}<g><rect class="pipe-node-bg" x="16" y="116" width="160" height="44" rx="6"/><use href="#i-file" x="28" y="130" width="16" height="16" stroke="var(--ink-2)" fill="none" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/><text class="pipe-label" x="106" y="143" text-anchor="middle">CMakeLists.txt</text></g>${boxes}${outBoxes}<g class="pipe-node-group${sel === "host" ? " is-selected" : ""}" data-pipe-stage="host" style="--node-ch:var(--ink)"><rect class="pipe-node-bg" x="16" y="274" width="1044" height="46" rx="6"/><use href="#i-host" x="32" y="289" width="16" height="16" stroke="var(--ink-2)" fill="none" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/><text class="pipe-label" x="58" y="302">Host</text><text class="pipe-label-muted" x="114" y="302">${escapeHtml(host.label)}</text><text class="pipe-label-muted" x="1038" y="302" text-anchor="end">${escapeHtml(host.chips.join("  \u00b7  "))}</text></g></svg>`;
}

export function initPipeline(root, motion) {
  if (!root) return;
  const q = (s) => root.querySelector(s);
  const [canvas, stageBar, hostToggle, hostChips, inspector, crateMap] = [
    q("[data-pipe-canvas]"), q("[data-pipe-stages]"), q("[data-pipe-host]"),
    q("[data-pipe-host-chips]"), q("[data-pipe-inspector]"), q("[data-pipe-crates]")
  ];
  const state = {
    selected: "eval",
    hostMode: "native",
    orientation: typeof window !== "undefined" && window.matchMedia("(min-width: 720px)").matches ? "horizontal" : "vertical"
  };

  const renderAll = () => {
    stageBar.querySelectorAll("[data-stage-btn]").forEach((b) => b.setAttribute("aria-selected", String(b.dataset.stageBtn === state.selected)));
    hostToggle.querySelectorAll("[data-host-btn]").forEach((b) => b.setAttribute("aria-checked", String(b.dataset.hostBtn === state.hostMode)));
    canvas.innerHTML = renderPipeline(PIPELINE_MODEL, state.orientation, state);
    root.classList.toggle("is-pure-mode", state.hostMode === "pure");
    canvas.querySelectorAll("[data-pipe-stage]").forEach((g) => g.addEventListener("click", () => { state.selected = g.dataset.pipeStage; renderAll(); }));

    const host = PIPELINE_MODEL.hosts[state.hostMode];
    if (hostChips) hostChips.innerHTML = host.chips.map((c) => `<span class="chip is-ink">${escapeHtml(c)}</span>`).join("");
    const isHost = state.selected === "host";
    const info = isHost ? host : PIPELINE_MODEL.stages.find((s) => s.id === state.selected) || PIPELINE_MODEL.stages[1];
    const ch = isHost ? "var(--ink)" : info.colorVar;
    inspector.innerHTML = `<div class="pipe-insp-head"><span class="pipe-insp-name" style="color:${ch}"><svg class="i" aria-hidden="true"><use href="#${isHost ? "i-host" : info.icon}"/></svg>${escapeHtml(isHost ? host.label : info.label)}</span><span class="pipe-insp-text">${escapeHtml(info.sentence)}</span></div><div class="pipe-insp-crates">${info.crates.map((c) => `<span class="chip is-lit" style="--chip-ch:${ch}">rcmake-${escapeHtml(c)}</span>`).join("")}</div>`;

    const active = new Set(info.crates);
    crateMap.innerHTML = PIPELINE_MODEL.crateGroups.map((g) => `<div class="crate-group"><span class="crate-group-label">${escapeHtml(g.name)} <span class="dim">rcmake-</span></span><div class="crate-chips">${g.crates.map((c) => `<span class="chip${active.has(c) ? " is-lit" : ""}" style="--chip-ch:${ch}">${escapeHtml(c)}</span>`).join("")}</div></div>`).join("");
  };

  const pulse = (n = 1) => {
    if (prefersReducedMotion()) return;
    root.style.setProperty("--p-iters", String(n));
    root.classList.remove("is-pulsing");
    void root.offsetWidth;
    root.classList.add("is-pulsing");
  };

  initRovingGroup(stageBar, "[data-stage-btn]", (b) => { state.selected = b.dataset.stageBtn; renderAll(); });
  initRovingGroup(hostToggle, "[data-host-btn]", (b) => { state.hostMode = b.dataset.hostBtn === "pure" ? "pure" : "native"; renderAll(); });
  if (typeof window !== "undefined" && typeof window.matchMedia === "function") {
    window.matchMedia("(min-width: 720px)").addEventListener("change", (e) => {
      state.orientation = e.matches ? "horizontal" : "vertical";
      renderAll();
    });
  }
  root.addEventListener("mouseenter", () => pulse(1));
  root.addEventListener("focusin", () => pulse(1));
  renderAll();
  motion?.observeReveal(root, () => pulse(3));
  motion?.observeLoop(root);
}
