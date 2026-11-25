/* Rcmake 7-Track Nightly Compatibility Dashboard Controller.
 * Driven strictly by `./data/latest.json`. Pure ES module, zero external dependencies.
 * Licensed under MIT OR Apache-2.0.
 */

import {
  escapeHtml,
  formatNumber,
  formatPercent,
  loadLatestData,
  statusBadgeHtml
} from "./app.js";

const state = {
  data: null,
  bossFilter: "all",
  vcpkgFilter: "all",
  selectedShard: 0
};

function cleanNote(note) {
  return String(note || "").replaceAll("[FINAL_METRIC]", "").trim();
}

function metricValue(val, suffix = "") {
  if (val === null || val === undefined) {
    return "Pending";
  }
  const formatted = typeof val === "number" ? formatNumber(val) : String(val);
  return `${formatted}${suffix}`;
}

function renderProgressBar(rate) {
  if (typeof rate !== "number") {
    return `<div class="progress-track" aria-hidden="true"><div class="progress-fill pending"></div></div>`;
  }
  const clamped = Math.max(0, Math.min(100, rate));
  return `<div class="progress-track" role="progressbar" aria-valuenow="${clamped}" aria-valuemin="0" aria-valuemax="100"><div class="progress-fill" style="width:${clamped}%"></div></div>`;
}

function renderTrack1Core(core) {
  const rows = (core.highlights || [])
    .map((h) => {
      const rate = typeof h.passed === "number" && typeof h.total === "number" && h.total > 0
        ? formatPercent((h.passed / h.total) * 100)
        : "Pending";
      return `
        <tr>
          <td class="mono">${escapeHtml(h.suite)}</td>
          <td class="num mono">${metricValue(h.passed)}</td>
          <td class="num mono">${metricValue(h.total)}</td>
          <td class="num mono">${escapeHtml(rate)}</td>
          <td>${statusBadgeHtml(h.status)}</td>
        </tr>
      `;
    })
    .join("");

  return `
    <section id="track-core" class="track-section">
      <div class="container">
        <div class="track-header">
          <div>
            <span class="track-num">Track 1 of 7</span>
            <h2>${escapeHtml(core.label)}</h2>
            <p class="track-note">${escapeHtml(cleanNote(core.note))}</p>
          </div>
          ${statusBadgeHtml(core.status)}
        </div>

        <div class="stat-row">
          <div class="stat-box">
            <div class="stat-box-label">Core Suites</div>
            <div class="stat-box-val">${metricValue(core.suitesTotal)}</div>
          </div>
          <div class="stat-box">
            <div class="stat-box-label">Suites at 100%</div>
            <div class="stat-box-val">${metricValue(core.suitesAt100)}</div>
          </div>
          <div class="stat-box">
            <div class="stat-box-label">Cases Passed</div>
            <div class="stat-box-val">${core.casesPassed !== null && core.casesTotal !== null ? `${formatNumber(core.casesPassed)} / ${formatNumber(core.casesTotal)}` : "Pending"}</div>
          </div>
          <div class="stat-box">
            <div class="stat-box-label">Pass Rate</div>
            <div class="stat-box-val">${core.passRate !== null ? formatPercent(core.passRate) : "Pending"}</div>
            ${renderProgressBar(core.passRate)}
          </div>
        </div>

        <div class="data-table-wrap">
          <table class="data-table">
            <thead>
              <tr>
                <th>Key Suite</th>
                <th class="num">Passed</th>
                <th class="num">Total</th>
                <th class="num">Rate</th>
                <th>Status</th>
              </tr>
            </thead>
            <tbody>${rows}</tbody>
          </table>
        </div>
      </div>
    </section>
  `;
}

function renderTrack2Policies(policies) {
  return `
    <section id="track-policies" class="track-section">
      <div class="container">
        <div class="track-header">
          <div>
            <span class="track-num">Track 2 of 7</span>
            <h2>${escapeHtml(policies.label)}</h2>
            <p class="track-note">${escapeHtml(cleanNote(policies.note))}</p>
          </div>
          ${statusBadgeHtml(policies.status)}
        </div>

        <div class="stat-row">
          <div class="stat-box">
            <div class="stat-box-label">Active Policy Range</div>
            <div class="stat-box-val mono" style="font-size:1.05rem;">${escapeHtml(policies.activeRange)}</div>
          </div>
          <div class="stat-box">
            <div class="stat-box-label">CMake 4.0+ Always NEW</div>
            <div class="stat-box-val mono" style="font-size:1.05rem;">${escapeHtml(policies.baselineAlwaysNew)}</div>
          </div>
          <div class="stat-box">
            <div class="stat-box-label">Policy Suites</div>
            <div class="stat-box-val">${policies.suitesPassed !== null ? `${formatNumber(policies.suitesPassed)} / ${formatNumber(policies.suitesTotal)}` : `${formatNumber(policies.suitesTotal)} suites`}</div>
          </div>
          <div class="stat-box">
            <div class="stat-box-label">Case Pass Rate</div>
            <div class="stat-box-val">${policies.passRate !== null ? formatPercent(policies.passRate) : "Pending"}</div>
            ${renderProgressBar(policies.passRate)}
          </div>
        </div>
      </div>
    </section>
  `;
}

function renderTrack3RunCmake(runCmake) {
  const cards = runCmake.platforms
    .map((p) => `
      <article class="os-card">
        <div class="os-card-head">
          <h3>${escapeHtml(p.name)}</h3>
          ${statusBadgeHtml(p.status)}
        </div>
        <div class="os-toolchain">${escapeHtml(p.toolchain)}</div>
        <div class="kv-grid" style="grid-template-columns:110px 1fr;">
          <span class="kv-key">Suites</span>
          <span class="kv-val">${p.suitesPassed !== null ? `${formatNumber(p.suitesPassed)} / ${formatNumber(p.suitesTotal)}` : `${formatNumber(p.suitesTotal)} applicable`}</span>
          <span class="kv-key">Cases</span>
          <span class="kv-val">${p.casesPassed !== null && p.casesTotal !== null ? `${formatNumber(p.casesPassed)} / ${formatNumber(p.casesTotal)}` : "Pending"}</span>
          <span class="kv-key">Pass Rate</span>
          <span class="kv-val">${p.passRate !== null ? formatPercent(p.passRate) : "Pending"}</span>
        </div>
        ${renderProgressBar(p.passRate)}
      </article>
    `)
    .join("");

  return `
    <section id="track-runcmake" class="track-section">
      <div class="container">
        <div class="track-header">
          <div>
            <span class="track-num">Track 3 of 7</span>
            <h2>${escapeHtml(runCmake.label)}</h2>
            <p class="track-note">${escapeHtml(cleanNote(runCmake.note))}</p>
          </div>
          ${statusBadgeHtml(runCmake.status)}
        </div>
        <div class="os-grid">${cards}</div>
      </div>
    </section>
  `;
}

function renderBossCardsHtml(boss) {
  const filtered = boss.projects.filter((p) => {
    if (state.bossFilter === "all") return true;
    if (state.bossFilter === "standard" || state.bossFilter === "large") return p.tier === state.bossFilter;
    return p.l1 === state.bossFilter || p.l2 === state.bossFilter;
  });

  if (filtered.length === 0) {
    return `<p style="color:var(--ink-muted);margin:0;">No projects match the active filter.</p>`;
  }

  return filtered
    .map((p) => `
      <article class="boss-card">
        <div class="boss-top">
          <div>
            <div class="boss-name">${escapeHtml(p.name)}</div>
            <div class="boss-repo">${escapeHtml(p.repository)} &middot; ${escapeHtml(p.version)}</div>
          </div>
          <span class="badge mono">${escapeHtml(p.tier)}</span>
        </div>
        <div class="boss-statuses">
          ${statusBadgeHtml(p.l1, `L1 Configure: ${p.l1}`)}
          ${statusBadgeHtml(p.l2, `L2 Ninja Dry-Run: ${p.l2}`)}
        </div>
        <div class="boss-targets">Targets: ${escapeHtml((p.targets || []).join(", "))}</div>
      </article>
    `)
    .join("");
}

function renderTrack4Boss(boss) {
  const filters = [
    ["all", `All (${boss.projects.length})`],
    ["standard", "Standard Tier"],
    ["large", "Large Tier"],
    ["pass", "Verified"],
    ["pending", "Pending"]
  ];

  return `
    <section id="track-boss" class="track-section">
      <div class="container">
        <div class="track-header">
          <div>
            <span class="track-num">Track 4 of 7</span>
            <h2>${escapeHtml(boss.label)}</h2>
            <p class="track-note">${escapeHtml(cleanNote(boss.note))}</p>
          </div>
          ${statusBadgeHtml(boss.status)}
        </div>

        <div class="stat-row">
          <div class="stat-box">
            <div class="stat-box-label">Projects</div>
            <div class="stat-box-val">${metricValue(boss.projectsTotal)}</div>
          </div>
          <div class="stat-box">
            <div class="stat-box-label">L1 Configure Parity</div>
            <div class="stat-box-val">${boss.l1Passed !== null ? `${boss.l1Passed} / ${boss.projectsTotal}` : "Pending"}</div>
          </div>
          <div class="stat-box">
            <div class="stat-box-label">L2 Ninja Dry-Run Parity</div>
            <div class="stat-box-val">${boss.l2Passed !== null ? `${boss.l2Passed} / ${boss.projectsTotal}` : "Pending"}</div>
          </div>
          <div class="stat-box">
            <div class="stat-box-label">Coverage</div>
            <div class="stat-box-val" style="font-size:0.95rem;">Unmodified upstream trees</div>
          </div>
        </div>

        <div class="filter-bar" role="group" aria-label="Filter boss-fight projects">
          ${filters
            .map(
              ([id, label]) =>
                `<button class="chip-btn${state.bossFilter === id ? " active" : ""}" type="button" data-boss-filter="${id}">${escapeHtml(label)}</button>`
            )
            .join("")}
        </div>

        <div id="boss-grid" class="boss-grid">${renderBossCardsHtml(boss)}</div>
      </div>
    </section>
  `;
}

function renderTrack5Unseen(unseen) {
  const rows = unseen.categories
    .map((c) => `
      <tr>
        <td>${escapeHtml(c.category)}</td>
        <td class="num mono">${metricValue(c.reposPassed)}</td>
        <td class="num mono">${metricValue(c.reposTotal)}</td>
        <td class="num mono">${c.passRate !== null ? formatPercent(c.passRate) : "Pending"}</td>
        <td>${statusBadgeHtml(c.status)}</td>
      </tr>
    `)
    .join("");

  return `
    <section id="track-unseen" class="track-section">
      <div class="container">
        <div class="track-header">
          <div>
            <span class="track-num">Track 5 of 7</span>
            <h2>${escapeHtml(unseen.label)}</h2>
            <p class="track-note">${escapeHtml(cleanNote(unseen.note))}</p>
          </div>
          ${statusBadgeHtml(unseen.status)}
        </div>

        <div class="sealed-notice">
          <strong>Sealed Holdout Policy:</strong> ${escapeHtml(unseen.policy)}
        </div>

        <div class="stat-row">
          <div class="stat-box">
            <div class="stat-box-label">Holdout Corpus</div>
            <div class="stat-box-val">${metricValue(unseen.reposTotal)} repos</div>
          </div>
          <div class="stat-box">
            <div class="stat-box-label">Aggregate Passed</div>
            <div class="stat-box-val">${metricValue(unseen.reposPassed)}</div>
          </div>
          <div class="stat-box">
            <div class="stat-box-label">Aggregate Pass Rate</div>
            <div class="stat-box-val">${unseen.passRate !== null ? formatPercent(unseen.passRate) : "Pending"}</div>
            ${renderProgressBar(unseen.passRate)}
          </div>
          <div class="stat-box">
            <div class="stat-box-label">Publication Scope</div>
            <div class="stat-box-val" style="font-size:0.95rem;">Aggregate &amp; categories only</div>
          </div>
        </div>

        <div class="data-table-wrap">
          <table class="data-table">
            <thead>
              <tr>
                <th>Category</th>
                <th class="num">Passed</th>
                <th class="num">Repos</th>
                <th class="num">Pass Rate</th>
                <th>Status</th>
              </tr>
            </thead>
            <tbody>${rows}</tbody>
          </table>
        </div>
      </div>
    </section>
  `;
}

function renderShardInspectorHtml(vcpkg) {
  const s = vcpkg.shards[state.selectedShard] || vcpkg.shards[0];
  const label = `Shard ${String(s.shard).padStart(2, "0")}`;
  const rate = typeof s.portsPassed === "number" && typeof s.portsTotal === "number" && s.portsTotal > 0
    ? formatPercent((s.portsPassed / s.portsTotal) * 100)
    : "Pending";
  const portsText = s.portsTotal !== null
    ? `${metricValue(s.portsPassed)} / ${metricValue(s.portsTotal)} ports (${rate})`
    : "Port counts pending final matrix run";
  const durText = typeof s.durationSeconds === "number" ? `${s.durationSeconds.toFixed(1)}s` : "Pending";

  return `
    <div style="display:flex;align-items:center;gap:10px;flex-wrap:wrap;">
      <strong class="mono">${escapeHtml(label)}</strong>
      ${statusBadgeHtml(s.status)}
      <span class="mono" style="color:var(--ink-secondary);">${escapeHtml(portsText)}</span>
    </div>
    <div class="mono" style="color:var(--ink-muted);font-size:0.8rem;">
      Duration: ${escapeHtml(durText)}${s.note ? ` | ${escapeHtml(cleanNote(s.note) || "Awaiting shard execution")}` : ""}
    </div>
  `;
}

function renderTrack6Vcpkg(vcpkg) {
  const filters = [
    ["all", "All 64 Shards"],
    ["pass", "Pass"],
    ["partial", "Partial"],
    ["fail", "Fail"],
    ["pending", "Pending"]
  ];

  const cells = vcpkg.shards
    .map((s) => {
      const idx = String(s.shard).padStart(2, "0");
      const selected = s.shard === state.selectedShard ? " selected" : "";
      const dimmed = state.vcpkgFilter !== "all" && s.status !== state.vcpkgFilter ? " dimmed" : "";
      return `<button class="shard-cell status-${escapeHtml(s.status)}${selected}${dimmed}" type="button" data-shard-idx="${s.shard}" aria-label="Shard ${idx}, status ${escapeHtml(s.status)}" title="Shard ${idx}: ${escapeHtml(s.status)}">${idx}</button>`;
    })
    .join("");

  return `
    <section id="track-vcpkg" class="track-section">
      <div class="container">
        <div class="track-header">
          <div>
            <span class="track-num">Track 6 of 7</span>
            <h2>${escapeHtml(vcpkg.label)}</h2>
            <p class="track-note">${escapeHtml(cleanNote(vcpkg.note))}</p>
          </div>
          ${statusBadgeHtml(vcpkg.status)}
        </div>

        <div class="stat-row">
          <div class="stat-box">
            <div class="stat-box-label">Shards</div>
            <div class="stat-box-val">${formatNumber(vcpkg.shardsCompleted)} / ${formatNumber(vcpkg.shardsTotal)}</div>
          </div>
          <div class="stat-box">
            <div class="stat-box-label">Shards Passed</div>
            <div class="stat-box-val">${metricValue(vcpkg.shardsPassed)}</div>
          </div>
          <div class="stat-box">
            <div class="stat-box-label">Ports Verified</div>
            <div class="stat-box-val">${vcpkg.portsPassed !== null && vcpkg.portsTotal !== null ? `${formatNumber(vcpkg.portsPassed)} / ${formatNumber(vcpkg.portsTotal)}` : "Pending"}</div>
          </div>
          <div class="stat-box">
            <div class="stat-box-label">Registry Pass Rate</div>
            <div class="stat-box-val">${vcpkg.passRate !== null ? formatPercent(vcpkg.passRate) : "Pending"}</div>
            ${renderProgressBar(vcpkg.passRate)}
          </div>
        </div>

        <div class="filter-bar" role="group" aria-label="Filter vcpkg shards">
          ${filters
            .map(
              ([id, label]) =>
                `<button class="chip-btn${state.vcpkgFilter === id ? " active" : ""}" type="button" data-vcpkg-filter="${id}">${escapeHtml(label)}</button>`
            )
            .join("")}
        </div>

        <div id="shard-grid" class="shard-grid" role="grid" aria-label="64-shard vcpkg compatibility matrix">
          ${cells}
        </div>

        <div id="shard-inspector" class="shard-inspector">
          ${renderShardInspectorHtml(vcpkg)}
        </div>
      </div>
    </section>
  `;
}

function renderTrack7Benchmarks(section) {
  const benchCards = Object.values(section.benchmarks)
    .map((b) => `
      <article class="stat-box">
        <div style="display:flex;align-items:center;justify-content:space-between;gap:6px;margin-bottom:6px;">
          <span class="stat-box-label">${escapeHtml(b.label)}</span>
          ${statusBadgeHtml(b.status)}
        </div>
        <div class="stat-box-val">${b.value !== null ? `${formatNumber(b.value)} ${escapeHtml(b.unit)}` : "Pending"}</div>
        <div style="margin-top:4px;font-size:0.78rem;color:var(--ink-muted);">${escapeHtml(b.baseline)}</div>
      </article>
    `)
    .join("");

  const gateCards = section.gates
    .map((g) => `
      <article class="gate-card">
        <div class="gate-card-head">
          <strong style="font-size:0.95rem;">${escapeHtml(g.label)}</strong>
          ${statusBadgeHtml(g.status)}
        </div>
        <div class="gate-summary">${escapeHtml(g.summary)}</div>
        <p class="gate-detail">${escapeHtml(cleanNote(g.detail))}</p>
      </article>
    `)
    .join("");

  return `
    <section id="track-benchmarks" class="track-section">
      <div class="container">
        <div class="track-header">
          <div>
            <span class="track-num">Track 7 of 7</span>
            <h2>${escapeHtml(section.label)}</h2>
            <p class="track-note">Real-world configure latency, memory footprint, adversarial fuzzing, WASI, probe cache, mutation replay, and black-box scenarios.</p>
          </div>
        </div>

        <div class="bench-grid">${benchCards}</div>
        <div class="gates-grid">${gateCards}</div>
      </div>
    </section>
  `;
}

function bindDashboardInteractions() {
  const boss = state.data.tracks.bossFight;
  const vcpkg = state.data.tracks.vcpkgMatrix;

  for (const btn of document.querySelectorAll("[data-boss-filter]")) {
    btn.addEventListener("click", () => {
      state.bossFilter = btn.dataset.bossFilter;
      for (const b of document.querySelectorAll("[data-boss-filter]")) {
        b.classList.toggle("active", b.dataset.bossFilter === state.bossFilter);
      }
      const grid = document.getElementById("boss-grid");
      if (grid) {
        grid.innerHTML = renderBossCardsHtml(boss);
      }
    });
  }

  for (const btn of document.querySelectorAll("[data-vcpkg-filter]")) {
    btn.addEventListener("click", () => {
      state.vcpkgFilter = btn.dataset.vcpkgFilter;
      for (const b of document.querySelectorAll("[data-vcpkg-filter]")) {
        b.classList.toggle("active", b.dataset.vcpkgFilter === state.vcpkgFilter);
      }
      for (const cell of document.querySelectorAll("[data-shard-idx]")) {
        const idx = Number(cell.dataset.shardIdx);
        const shard = vcpkg.shards[idx];
        cell.classList.toggle("dimmed", state.vcpkgFilter !== "all" && shard.status !== state.vcpkgFilter);
      }
    });
  }

  for (const cell of document.querySelectorAll("[data-shard-idx]")) {
    cell.addEventListener("click", () => {
      state.selectedShard = Number(cell.dataset.shardIdx);
      for (const c of document.querySelectorAll("[data-shard-idx]")) {
        c.classList.toggle("selected", Number(c.dataset.shardIdx) === state.selectedShard);
      }
      const inspector = document.getElementById("shard-inspector");
      if (inspector) {
        inspector.innerHTML = renderShardInspectorHtml(vcpkg);
      }
    });
  }
}

async function init() {
  const root = document.getElementById("dash-root");
  const meta = document.getElementById("dash-meta");

  try {
    const data = await loadLatestData("./data/latest.json");
    state.data = data;

    meta.innerHTML = `
      <span>Target: CMake ${escapeHtml(data.targetCmakeVersion)}</span>
      <span>Rcmake: v${escapeHtml(data.rcmakeVersion)}</span>
      <span>Commit: ${escapeHtml(data.commit || "pending final tag")}</span>
      <span>Updated: ${escapeHtml(data.generatedAt || "pending final run")}</span>
    `;

    const t = data.tracks;
    root.innerHTML = [
      renderTrack1Core(t.coreConformance),
      renderTrack2Policies(t.policies),
      renderTrack3RunCmake(t.runCmakeOs),
      renderTrack4Boss(t.bossFight),
      renderTrack5Unseen(t.unseenHoldout),
      renderTrack6Vcpkg(t.vcpkgMatrix),
      renderTrack7Benchmarks(t.benchmarksAndGates)
    ].join("");

    bindDashboardInteractions();
  } catch (error) {
    meta.innerHTML = `<span>Failed to load data/latest.json</span>`;
    root.innerHTML = `
      <section class="track-section">
        <div class="container">
          <div class="sealed-notice">
            Could not load <code>./data/latest.json</code> (${escapeHtml(error.message)}). Serve <code>site/</code> via GitHub Pages or a local static HTTP server.
          </div>
        </div>
      </section>
    `;
  }
}

init();
