/* rcmake configure pipeline: four stages over Host, one detail card. MIT OR Apache-2.0. */

import { escapeHtml, initRovingGroup } from "./app.js";

export const PIPELINE_MODEL = {
  stages: [
    { id: "syntax", label: "Syntax", tone: "r", sentence: "The syntax stage lexes and parses CMakeLists.txt and .cmake files into a syntax tree with source spans.", crates: ["syntax"] },
    { id: "eval", label: "Eval", tone: "r", sentence: "The evaluator runs CMake commands and tracks variables, policies, presets, and generator expressions.", crates: ["eval", "commands", "expr", "genex", "model", "presets"] },
    { id: "resolve", label: "Resolve", tone: "g", sentence: "The resolver builds the target graph, propagates usage requirements, and computes the final link order.", crates: ["resolve", "build", "cxxmodules", "fortran", "cps"] },
    { id: "gen", label: "Generate", tone: "b", sentence: "The generators write build.ninja, Unix Makefiles, Xcode project.pbxproj files, and Visual Studio .vcxproj files.", crates: ["gen-common", "gen-ninja", "gen-make", "gen-xcode", "gen-vs"] }
  ],
  host: {
    id: "host",
    label: "Host",
    sentence: "Every stage accesses files, environment variables, processes, and the clock only through the Host trait.",
    impls: [
      { label: "NativeHost", sentence: "NativeHost uses the local filesystem, environment variables, child processes, and the system clock." },
      { label: "PureHost", sentence: "PureHost keeps all files and state in memory so tests, the language server, and the wasm32 build stay deterministic." }
    ],
    crates: ["host", "wasm"]
  }
};

export function renderDetail(model, id) {
  const isHost = id === "host";
  const info = isHost ? model.host : model.stages.find((s) => s.id === id) || model.stages[1];
  const crates = info.crates.map((c) => `<span class="chip">rcmake-${escapeHtml(c)}</span>`).join("");
  const impls = isHost
    ? `<div class="flow-impls">${info.impls.map((h) => `<div class="flow-impl"><span class="flow-impl-name">${escapeHtml(h.label)}</span><span>${escapeHtml(h.sentence)}</span></div>`).join("")}</div>`
    : "";
  return `<div class="flow-detail-inner" data-tone="${isHost ? "ink" : info.tone}"><div class="flow-detail-head"><span class="flow-dot" aria-hidden="true"></span><h3>${escapeHtml(info.label)}</h3></div><p>${escapeHtml(info.sentence)}</p>${impls}<div class="flow-crates">${crates}</div></div>`;
}

export function initPipeline(root, motion) {
  if (!root) return;
  const bar = root.querySelector("[data-pipe-stages]");
  const detail = root.querySelector("[data-pipe-inspector]");
  if (!bar || !detail) return;
  let selected = "eval";
  let timer = null;

  const select = (id, animate = true) => {
    if (!id) return;
    const changed = id !== selected;
    selected = id;
    bar.querySelectorAll("[data-stage-btn]").forEach((b) => {
      const on = b.dataset.stageBtn === id;
      b.setAttribute("aria-selected", String(on));
      b.tabIndex = on ? 0 : -1;
      if (on && b.id) detail.setAttribute("aria-labelledby", b.id);
    });
    if (!animate || !changed) { detail.innerHTML = renderDetail(PIPELINE_MODEL, id); return; }
    detail.classList.add("is-swapping");
    clearTimeout(timer);
    timer = setTimeout(() => {
      detail.innerHTML = renderDetail(PIPELINE_MODEL, selected);
      detail.classList.remove("is-swapping");
    }, 140);
  };

  initRovingGroup(bar, "[data-stage-btn]", (b) => select(b.dataset.stageBtn));
  select(selected, false);
  motion?.observeLoop(root);
}
