/* Rcmake shared UI utilities and landing page controller.
 * Pure ES module, zero external dependencies. MIT OR Apache-2.0.
 */

const INSTALL_COMMANDS = {
  release: "curl -fsSL https://raw.githubusercontent.com/dvgyl/Rcmake/main/scripts/install.sh | bash",
  alias: "curl -fsSL https://raw.githubusercontent.com/dvgyl/Rcmake/main/scripts/install.sh | bash -s -- --alias-cmake",
  source: "git clone https://github.com/dvgyl/Rcmake.git && cd Rcmake && RCMAKE_STAGE_PROFILE=release ./scripts/stage.sh"
};

export function escapeHtml(value) {
  return String(value ?? "")
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}

export function formatNumber(value) {
  if (typeof value !== "number" || Number.isNaN(value)) {
    return null;
  }
  return value.toLocaleString("en-US");
}

export function formatPercent(value) {
  if (typeof value !== "number" || Number.isNaN(value)) {
    return null;
  }
  return `${value.toFixed(value % 1 === 0 ? 0 : 2)}%`;
}

export function statusLabel(status) {
  switch (status) {
    case "pass":
      return "Verified";
    case "partial":
      return "Partial";
    case "fail":
      return "Failing";
    default:
      return "Pending final run";
  }
}

export function statusBadgeHtml(status, customText) {
  const normalized = ["pass", "partial", "fail", "pending"].includes(status) ? status : "pending";
  const text = customText || statusLabel(normalized);
  return `<span class="badge badge-${normalized}"><span class="badge-dot"></span>${escapeHtml(text)}</span>`;
}

export function initThemeToggle() {
  const root = document.documentElement;
  const btn = document.getElementById("theme-toggle");
  const sun = document.getElementById("theme-icon-sun");
  const moon = document.getElementById("theme-icon-moon");

  function syncIcons(theme) {
    if (sun && moon) {
      sun.style.display = theme === "dark" ? "block" : "none";
      moon.style.display = theme === "dark" ? "none" : "block";
    }
    if (btn) {
      btn.setAttribute("aria-pressed", String(theme === "dark"));
      btn.setAttribute(
        "aria-label",
        theme === "dark" ? "Switch to light mode" : "Switch to dark mode"
      );
    }
  }

  const current = root.dataset.theme || (
    window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light"
  );
  root.dataset.theme = current;
  syncIcons(current);

  if (btn) {
    btn.addEventListener("click", () => {
      const next = root.dataset.theme === "dark" ? "light" : "dark";
      root.dataset.theme = next;
      try {
        localStorage.setItem("rcmake-theme", next);
      } catch (_) {}
      syncIcons(next);
    });
  }
}

export async function loadLatestData(url = "./data/latest.json") {
  const response = await fetch(url, { cache: "no-cache" });
  if (!response.ok) {
    throw new Error(`HTTP ${response.status} loading ${url}`);
  }
  return response.json();
}

function initInstallTabs() {
  const codeEl = document.getElementById("install-cmd-text");
  const copyBtn = document.getElementById("copy-cmd-btn");
  const tabs = Array.from(document.querySelectorAll("[data-cmd-tab]"));
  if (!codeEl || tabs.length === 0) {
    return;
  }

  let activeKey = "release";

  for (const tab of tabs) {
    tab.addEventListener("click", () => {
      activeKey = tab.dataset.cmdTab || "release";
      for (const item of tabs) {
        item.setAttribute("aria-selected", String(item === tab));
      }
      codeEl.textContent = INSTALL_COMMANDS[activeKey] || INSTALL_COMMANDS.release;
      if (copyBtn) {
        copyBtn.textContent = "Copy";
      }
    });
  }

  if (copyBtn) {
    copyBtn.addEventListener("click", async () => {
      const text = INSTALL_COMMANDS[activeKey] || codeEl.textContent || "";
      try {
        await navigator.clipboard.writeText(text);
        copyBtn.textContent = "Copied";
        window.setTimeout(() => {
          copyBtn.textContent = "Copy";
        }, 1600);
      } catch (_) {
        copyBtn.textContent = "Select & Copy";
      }
    });
  }
}

function renderProofCard({ label, status, badgeText, value, sub }) {
  return `
    <article class="proof-card">
      <div class="proof-card-top">
        <span class="proof-label">${escapeHtml(label)}</span>
        ${statusBadgeHtml(status, badgeText)}
      </div>
      <div class="proof-value tabular">${escapeHtml(value)}</div>
      <div class="proof-sub">${escapeHtml(sub)}</div>
    </article>
  `;
}

async function hydrateProofStrip() {
  const strip = document.getElementById("proof-strip");
  if (!strip) {
    return;
  }
  const metaCommit = document.getElementById("proof-meta-commit");
  const metaNote = document.getElementById("proof-meta-note");

  try {
    const data = await loadLatestData("./data/latest.json");
    const tracks = data.tracks;
    const core = tracks.coreConformance;
    const osTrack = tracks.runCmakeOs;
    const boss = tracks.bossFight;
    const unseen = tracks.unseenHoldout;
    const gates = tracks.benchmarksAndGates.gates;
    const fuzzGate = gates.find((g) => g.id === "fuzz");
    const wasiGate = gates.find((g) => g.id === "wasi");

    const osTotalSuites = osTrack.platforms.reduce((sum, p) => sum + p.suitesTotal, 0);
    const osMeasured = osTrack.platforms.filter((p) => typeof p.passRate === "number");
    const osHeadline = osMeasured.length === osTrack.platforms.length
      ? osTrack.platforms.map((p) => `${p.name.split(" ")[0]} ${formatPercent(p.passRate)}`).join(" / ")
      : `${osTotalSuites} OS-applicable suites`;

    const cards = [
      {
        label: "Core Conformance",
        status: core.status,
        value: core.passRate !== null ? formatPercent(core.passRate) : `${core.suitesTotal} Core Suites`,
        sub: core.casesPassed !== null && core.casesTotal !== null
          ? `${formatNumber(core.casesPassed)} / ${formatNumber(core.casesTotal)} cases`
          : "Core-62 differential replay queued for final binary"
      },
      {
        label: "Tests/RunCMake (3 OS)",
        status: osTrack.status,
        value: osHeadline,
        sub: "macOS arm64 (371), Linux (362), Windows (367)"
      },
      {
        label: "Adversarial Fuzzing",
        status: fuzzGate ? fuzzGate.status : "pending",
        value: fuzzGate && fuzzGate.passed !== null
          ? `${formatNumber(fuzzGate.passed)} inputs`
          : "Pending",
        sub: fuzzGate ? `${fuzzGate.failures ?? 0} failures across 9 parser surfaces` : "Pending final run"
      },
      {
        label: "WebAssembly / WASI",
        status: wasiGate ? wasiGate.status : "pending",
        value: wasiGate && wasiGate.passed !== null
          ? `${wasiGate.passed} / ${wasiGate.total} checks`
          : "Pending",
        sub: "wasm32-unknown-unknown + wasm32-wasip1 PureHost"
      },
      {
        label: "Boss-Fight Giants",
        status: boss.status,
        value: boss.l1Passed !== null && boss.l2Passed !== null
          ? `${boss.l1Passed}/${boss.projectsTotal} L1, ${boss.l2Passed}/${boss.projectsTotal} L2`
          : `${boss.projectsTotal} Real-World Projects`,
        sub: "DuckDB, OpenCV, VTK, Qt 6, LLVM, PyTorch, KDE, Boost, ROS 2, Blender"
      },
      {
        label: "Sealed Unseen Holdout",
        status: unseen.status,
        value: unseen.passRate !== null ? formatPercent(unseen.passRate) : `${unseen.reposTotal} Repos`,
        sub: unseen.reposPassed !== null
          ? `${formatNumber(unseen.reposPassed)} / ${formatNumber(unseen.reposTotal)} passed (aggregate only)`
          : "Blind aggregate holdout (no repository names published)"
      }
    ];

    strip.innerHTML = cards.map(renderProofCard).join("");

    if (metaCommit) {
      const commitPart = data.commit ? `commit ${data.commit}` : "commit pending final tag";
      const timePart = data.generatedAt ? `updated ${data.generatedAt}` : "snapshot awaiting final run";
      metaCommit.textContent = `Target CMake ${data.targetCmakeVersion} | ${commitPart} | ${timePart}`;
    }
    if (metaNote && data.statusNote) {
      metaNote.textContent = data.statusNote.replace("[FINAL_METRIC]", "").trim();
    }
  } catch (error) {
    strip.innerHTML = renderProofCard({
      label: "Snapshot Unavailable",
      status: "pending",
      badgeText: "Serve over HTTP",
      value: "data/latest.json",
      sub: `Open via static web server or GitHub Pages (${error.message})`
    });
  }
}

initThemeToggle();
initInstallTabs();
hydrateProofStrip();
