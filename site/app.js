/* Rcmake shared site helpers and the Overview page controller.
 * Plain ES module, no dependencies. MIT OR Apache-2.0.
 */

const STATUSES = ["pass", "partial", "fail", "pending"];
const PLACEHOLDER = /\[FINAL_METRIC\]\s*/g;

/** Escape a value for safe use in HTML text and attribute values. */
export function escapeHtml(value) {
  return String(value ?? "")
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}

/** Format an integer with thousands separators, or return null. */
export function formatNumber(value) {
  return typeof value === "number" && Number.isFinite(value) ? value.toLocaleString("en-US") : null;
}

/** Format a percentage with at most two decimals, or return null. */
export function formatPercent(value) {
  if (typeof value !== "number" || !Number.isFinite(value)) {
    return null;
  }
  return `${value.toFixed(Number.isInteger(value) ? 0 : 2)}%`;
}

/** Format "passed / total", or return null when either side is unknown. */
export function formatRatio(passed, total) {
  const p = formatNumber(passed);
  const t = formatNumber(total);
  return p !== null && t !== null ? `${p} / ${t}` : null;
}

/** Remove the internal placeholder token from a note. */
export function cleanNote(text) {
  return String(text ?? "").replace(PLACEHOLDER, "").trim();
}

/** Return a known status value. Unknown values become "pending". */
export function normalizeStatus(status) {
  return STATUSES.includes(status) ? status : "pending";
}

/** Render a status as a plain colored monospace word. */
export function statusHtml(status, text) {
  const s = normalizeStatus(status);
  return `<span class="st st-${s}">${escapeHtml(text || s)}</span>`;
}

/** Fetch and parse the compatibility snapshot. */
export async function loadLatestData(url = "./data/latest.json") {
  const response = await fetch(url, { cache: "no-cache" });
  if (!response.ok) {
    throw new Error(`HTTP ${response.status} for ${url}`);
  }
  return response.json();
}

/** Wire the plain-text "Dark" / "Light" toggle in the top bar. */
export function initThemeToggle(doc = document) {
  const root = doc.documentElement;
  const btn = doc.getElementById("theme-toggle");
  if (!root.dataset.theme) {
    const dark = typeof matchMedia === "function" && matchMedia("(prefers-color-scheme: dark)").matches;
    root.dataset.theme = dark ? "dark" : "light";
  }
  const sync = () => {
    if (btn) {
      const dark = root.dataset.theme === "dark";
      btn.textContent = dark ? "Light" : "Dark";
      btn.setAttribute("aria-label", dark ? "Use light theme" : "Use dark theme");
    }
  };
  sync();
  btn?.addEventListener("click", () => {
    root.dataset.theme = root.dataset.theme === "dark" ? "light" : "dark";
    try {
      localStorage.setItem("rcmake-theme", root.dataset.theme);
    } catch (_) {
      // Storage can be blocked. The theme still applies to this page view.
    }
    sync();
  });
}

/** Copy text to the clipboard and show the result on the button for a moment. */
export async function copyWithFeedback(button, text, label = "copy") {
  try {
    await navigator.clipboard.writeText(text);
    button.textContent = "copied";
  } catch (_) {
    button.textContent = "select and copy";
  }
  setTimeout(() => {
    button.textContent = label;
  }, 1500);
}

function initCopyButtons(doc) {
  for (const btn of doc.querySelectorAll("[data-copy]")) {
    const source = doc.getElementById(btn.dataset.copy);
    if (!source) {
      continue;
    }
    btn.addEventListener("click", () => {
      const lines = Array.from(source.querySelectorAll("[data-line]"), (el) => el.textContent);
      copyWithFeedback(btn, lines.length ? lines.join("\n") : source.textContent);
    });
  }
}

/** Build the Overview summary rows from a parsed `latest.json` object. */
export function overviewRows(data) {
  const t = data.tracks;
  const gates = t.benchmarksAndGates.gates;
  const gate = (id) => gates.find((g) => g.id === id) || { status: "pending" };
  const blackbox = gate("blackbox");
  const fuzz = gate("fuzz");
  const wasi = gate("wasi");
  const platforms = t.runCmakeOs.platforms;
  const measured = platforms.every((p) => typeof p.passRate === "number");
  const boss = t.bossFight;
  const unseen = t.unseenHoldout;
  const vcpkg = t.vcpkgMatrix;
  const warm = t.benchmarksAndGates.benchmarks.warmProbeSpeedup;

  return [
    {
      check: "Core-62 conformance",
      result: formatPercent(t.coreConformance.passRate),
      scope: `${t.coreConformance.suitesTotal} suites`,
      status: t.coreConformance.status
    },
    {
      check: `Policies ${t.policies.activeRange}`,
      result: formatPercent(t.policies.passRate),
      scope: `${t.policies.suitesTotal} suites`,
      status: t.policies.status
    },
    {
      check: "Tests/RunCMake, 3 OS",
      result: measured ? platforms.map((p) => formatPercent(p.passRate)).join(" / ") : null,
      scope: `${platforms.map((p) => p.suitesTotal).join(" / ")} suites`,
      status: t.runCmakeOs.status
    },
    {
      check: "Independent black-box suite",
      result: formatRatio(blackbox.passed, blackbox.total),
      scope: `${blackbox.scenariosTotal ?? "?"} scenarios, ${formatNumber(blackbox.total) ?? "?"} checks`,
      status: blackbox.status
    },
    {
      check: "Large projects, L1 / L2",
      result: boss.l1Passed !== null && boss.l2Passed !== null
        ? `${boss.l1Passed} / ${boss.l2Passed} of ${boss.projectsTotal}`
        : null,
      scope: `${boss.projectsTotal} projects`,
      status: boss.status
    },
    {
      check: "Sealed unseen holdout",
      result: formatPercent(unseen.passRate),
      scope: `${unseen.reposTotal} repositories`,
      status: unseen.status
    },
    {
      check: "vcpkg port matrix",
      result: formatPercent(vcpkg.passRate),
      scope: `${vcpkg.shardsTotal} shards`,
      status: vcpkg.status
    },
    {
      check: "Fuzzing",
      result: fuzz.passed !== null && fuzz.passed !== undefined
        ? `${formatNumber(fuzz.passed)} inputs, ${fuzz.failures ?? 0} failures`
        : null,
      scope: "9 parser surfaces",
      status: fuzz.status
    },
    {
      check: "WASI and wasm32",
      result: formatRatio(wasi.passed, wasi.total),
      scope: `${wasi.total ?? "?"} checks`,
      status: wasi.status
    },
    {
      check: "Warm configure vs CMake",
      result: typeof warm.value === "number" ? `${warm.value}${warm.unit}` : null,
      scope: "real-world corpus",
      status: warm.status
    }
  ];
}

/** Render Overview rows as table body HTML. */
export function overviewRowsHtml(rows) {
  return rows
    .map(
      (row) => `<tr>
        <td>${escapeHtml(row.check)}</td>
        <td class="mono">${row.result ? escapeHtml(row.result) : `<span class="muted">${escapeHtml(row.scope)}</span>`}</td>
        <td class="r">${statusHtml(row.status)}</td>
      </tr>`
    )
    .join("");
}

/** Describe the snapshot origin: commit, target, and date. */
export function snapshotMeta(data) {
  return [
    data.commit ? `commit ${String(data.commit).slice(0, 12)}` : "commit pending",
    `target CMake ${data.targetCmakeVersion}`,
    data.generatedAt ? `generated ${data.generatedAt}` : "not yet generated"
  ].join(" · ");
}

async function hydrateOverview(doc) {
  const body = doc.getElementById("spec-rows");
  if (!body) {
    return;
  }
  const meta = doc.getElementById("spec-meta");
  try {
    const data = await loadLatestData();
    body.innerHTML = overviewRowsHtml(overviewRows(data));
    if (meta) {
      meta.textContent = snapshotMeta(data);
    }
  } catch (error) {
    body.innerHTML = `<tr><td colspan="3" class="muted">Could not read data/latest.json (${escapeHtml(error.message)}). Serve the site over HTTP.</td></tr>`;
  }
}

if (typeof document !== "undefined") {
  initThemeToggle(document);
  initCopyButtons(document);
  hydrateOverview(document);
}
