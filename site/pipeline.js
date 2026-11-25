/* rcmake configure pipeline diagram and 34-crate map. MIT OR Apache-2.0. */

import { escapeHtml, initRovingGroup } from "./app.js";
import { prefersReducedMotion } from "./motion.js";

export const PIPELINE_MODEL = {
  stages: [
    { id: "syntax", label: "syntax", colorVar: "var(--ch-r)", icon: "i-code", sentence: "Lexes and parses CMake language files.", crates: ["syntax"] },
    { id: "eval", label: "eval", colorVar: "var(--ch-r)", icon: "i-terminal", sentence: "Runs commands and tracks variables, policies, and generator expressions.", crates: ["eval", "commands", "expr", "genex", "model", "presets"] },
    { id: "resolve", label: "resolve", colorVar: "var(--ch-g)", icon: "i-target", sentence: "Builds the target graph, usage requirements, and link order.", crates: ["resolve", "build", "cxxmodules", "fortran"] },
    { id: "gen", label: "gen", colorVar: "var(--ch-b)", icon: "i-graph", sentence: "Writes build.ninja, Makefiles, project.pbxproj, and .vcxproj files.", crates: ["gen-common", "gen-ninja", "gen-make", "gen-xcode", "gen-vs"] }
  ],
  outputs: ["build.ninja", "Makefile", "project.pbxproj", ".sln .vcxproj"],
  hosts: {
    native: { label: "NativeHost", sentence: "NativeHost reads the real disk, environment, processes, and clock.", chips: ["disk", "env", "processes", "clock"], crates: ["host"] },
    pure: { label: "PureHost", sentence: "PureHost keeps files in memory and gives the same result every run. Tests, editors, and the wasm32 playground use it.", chips: ["in-memory files", "tests", "editors", "wasm32"], crates: ["host"] }
  },
  crateGroups: [
    { name: "Base", crates: ["base", "json", "diag", "host", "regex", "archive"] },
    { name: "Language", crates: ["syntax", "expr", "genex", "model", "eval", "commands", "presets"] },
    { name: "Build", crates: ["resolve", "build", "cxxmodules", "fortran", "gen-common", "gen-ninja", "gen-make", "gen-xcode", "gen-vs"] },
    { name: "Drivers", crates: ["cli", "api", "lsp", "ctest", "cpack", "tool-e", "store", "fetch", "instrument"] },
    { name: "Verification", crates: ["cps", "harness", "fuzz"] }
  ]
};

export function renderPipeline(model = PIPELINE_MODEL, orientation = "horizontal", state = {}) {
  const sel = state.selected || "eval";
  const isPure = state.hostMode === "pure";
  const host = isPure ? model.hosts.pure : model.hosts.native;
  const edge = (d, ch, delay, end) => `<path class="pipe-edge" d="${d}"/><path class="pipe-pulse" d="${d}" stroke="${ch}" style="--p-delay:${delay}ms;--p-end:${end}"/>`;
  const tap = (d, i) => `<path class="pipe-tap${isPure ? " is-pure" : ""}" d="${d}"/><path class="pipe-tap-pulse" d="${d}" style="--t-delay:${i * 180}ms"/>`;

  if (orientation === "vertical") {
    const ns = [
      { id: "syntax", y: 90, ch: "var(--ch-r)" }, { id: "eval", y: 170, ch: "var(--ch-r)" },
      { id: "resolve", y: 250, ch: "var(--ch-g)" }, { id: "gen", y: 330, ch: "var(--ch-b)" }
    ];
    const outs = model.outputs.map((label, i) => ({ label, x: i % 2 ? 186 : 20, y: i < 2 ? 418 : 462 }));
    const edges = [
      edge("M 130 56 L 130 90", "var(--ch-r)", 0, -120), edge("M 130 134 L 130 170", "var(--ch-r)", 150, -120),
      edge("M 130 214 L 130 250", "var(--ch-r)", 300, -120), edge("M 130 294 L 130 330", "var(--ch-g)", 450, -120),
      ...outs.map((o) => edge(`M 130 374 C 130 398, ${o.x + 76} 398, ${o.x + 76} ${o.y}`, "var(--ch-b)", 600, -120))
    ].join("");
    const taps = ns.map((n, i) => tap(`M 230 ${n.y + 22} L 272 ${n.y + 22}`, i)).join("");
    const boxes = ns.map((n) => `<g class="pipe-node-group${sel === n.id ? " is-selected" : ""}" data-pipe-stage="${n.id}" style="--node-ch:${n.ch}"><rect class="pipe-node-bg" x="30" y="${n.y}" width="200" height="44" rx="6"/><rect x="31" y="${n.y}" width="198" height="2.5" rx="1" fill="${n.ch}"/><text class="pipe-label" x="130" y="${n.y + 27}" text-anchor="middle">${escapeHtml(n.id)}</text></g>`).join("");
    const outBoxes = outs.map((o) => `<g><rect class="pipe-node-bg" x="${o.x}" y="${o.y}" width="154" height="32" rx="5"/><rect x="${o.x + 1}" y="${o.y}" width="152" height="2" rx="1" fill="var(--ch-b)"/><text class="pipe-label-muted" x="${o.x + 77}" y="${o.y + 20}" text-anchor="middle">${escapeHtml(o.label)}</text></g>`).join("");
    return `<svg class="pipe-svg" viewBox="0 0 360 512" role="img" aria-label="Configure pipeline from CMakeLists.txt through syntax, eval, resolve, and gen to build files over Host">${edges}${taps}<g><rect class="pipe-node-bg" x="30" y="12" width="200" height="44" rx="6"/><text class="pipe-label" x="130" y="39" text-anchor="middle">CMakeLists.txt</text></g>${boxes}${outBoxes}<g class="pipe-node-group${sel === "host" ? " is-selected" : ""}" data-pipe-stage="host" style="--node-ch:var(--ink)"><rect class="pipe-node-bg" x="272" y="90" width="68" height="284" rx="6"/><text class="pipe-label" x="306" y="220" text-anchor="middle" transform="rotate(-90 306 220)">${escapeHtml(host.label)}</text></g></svg>`;
  }

  const ns = [
    { id: "syntax", x: 230, w: 140, ch: "var(--ch-r)" }, { id: "eval", x: 410, w: 140, ch: "var(--ch-r)" },
    { id: "resolve", x: 590, w: 140, ch: "var(--ch-g)" }, { id: "gen", x: 770, w: 110, ch: "var(--ch-b)" }
  ];
  const outs = model.outputs.map((label, i) => ({ label, y: 40 + i * 60 }));
  const edges = [
    edge("M 170 142 L 230 142", "var(--ch-r)", 0, -100), edge("M 370 142 L 410 142", "var(--ch-r)", 140, -80),
    edge("M 550 142 L 590 142", "var(--ch-r)", 280, -80), edge("M 730 142 L 770 142", "var(--ch-g)", 440, -80),
    ...outs.map((o) => edge(`M 880 142 C 900 142, 900 ${o.y + 16}, 920 ${o.y + 16}`, "var(--ch-b)", 600, -160))
  ].join("");
  const taps = ns.map((n, i) => tap(`M ${n.x + n.w / 2} 164 L ${n.x + n.w / 2} 280`, i)).join("");
  const boxes = ns.map((n) => `<g class="pipe-node-group${sel === n.id ? " is-selected" : ""}" data-pipe-stage="${n.id}" style="--node-ch:${n.ch}"><rect class="pipe-node-bg" x="${n.x}" y="120" width="${n.w}" height="44" rx="6"/><rect x="${n.x + 1}" y="120" width="${n.w - 2}" height="2.5" rx="1" fill="${n.ch}"/><text class="pipe-label" x="${n.x + n.w / 2}" y="147" text-anchor="middle">${escapeHtml(n.id)}</text></g>`).join("");
  const outBoxes = outs.map((o) => `<g><rect class="pipe-node-bg" x="920" y="${o.y}" width="110" height="32" rx="5"/><rect x="921" y="${o.y}" width="108" height="2" rx="1" fill="var(--ch-b)"/><text class="pipe-label-muted" x="975" y="${o.y + 20}" text-anchor="middle">${escapeHtml(o.label)}</text></g>`).join("");
  return `<svg class="pipe-svg" viewBox="0 0 1040 344" role="img" aria-label="Configure pipeline from CMakeLists.txt through syntax, eval, resolve, and gen to build files over Host">${edges}${taps}<g><rect class="pipe-node-bg" x="20" y="120" width="150" height="44" rx="6"/><text class="pipe-label" x="95" y="147" text-anchor="middle">CMakeLists.txt</text></g>${boxes}${outBoxes}<g class="pipe-node-group${sel === "host" ? " is-selected" : ""}" data-pipe-stage="host" style="--node-ch:var(--ink)"><rect class="pipe-node-bg" x="20" y="280" width="1000" height="48" rx="6"/><text class="pipe-label" x="44" y="309">Host</text><text class="pipe-label-muted" x="108" y="309">${escapeHtml(host.label)}</text><text class="pipe-label-muted" x="996" y="309" text-anchor="end">${escapeHtml(host.chips.join("  \u00b7  "))}</text></g></svg>`;
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
    inspector.innerHTML = `<span class="pipe-insp-name" style="color:${ch}"><svg class="i" aria-hidden="true"><use href="#${isHost ? "i-host" : info.icon}"/></svg>${escapeHtml(isHost ? host.label : info.label)}</span><span class="pipe-insp-text">${escapeHtml(info.sentence)}</span><div class="pipe-insp-crates">${info.crates.map((c) => `<span class="chip is-lit" style="--chip-ch:${ch}">rcmake-${escapeHtml(c)}</span>`).join("")}</div>`;

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
