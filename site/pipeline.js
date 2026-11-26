/* rcmake configure pipeline: four stages over Host, one detail card. MIT OR Apache-2.0. */

import { escapeHtml, initRovingGroup } from "./app.js";

export const PIPELINE_MODEL = {
  stages: [
    { id: "syntax", label: "Syntax", tone: "r", sentence: "Lexes and parses CMake language files.", crates: ["syntax"] },
    { id: "eval", label: "Eval", tone: "r", sentence: "Runs commands and tracks variables, policies, and generator expressions.", crates: ["eval", "commands", "expr", "genex", "model", "presets"] },
    { id: "resolve", label: "Resolve", tone: "g", sentence: "Builds the target graph, usage requirements, and link order.", crates: ["resolve", "build", "cxxmodules", "fortran", "cps"] },
    { id: "gen", label: "Generate", tone: "b", sentence: "Writes build.ninja, Unix Makefiles, project.pbxproj, and .vcxproj files.", crates: ["gen-common", "gen-ninja", "gen-make", "gen-xcode", "gen-vs"] }
  ],
  host: {
    id: "host",
    label: "Host",
    sentence: "Every stage reads files, environment, and processes only through the Host trait.",
    impls: [
      { label: "NativeHost", sentence: "Reads the real disk, environment, processes, and clock." },
      { label: "PureHost", sentence: "Keeps files in memory for deterministic tests, editors, and wasm32." }
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
  if (typeof window !== "undefined" && window.matchMedia?.("(hover: hover)").matches) {
    bar.querySelectorAll("[data-stage-btn]").forEach((b) => b.addEventListener("mouseenter", () => select(b.dataset.stageBtn)));
  }
  select(selected, false);
  motion?.observeLoop(root);
}
