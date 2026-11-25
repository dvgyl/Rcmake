/* Rcmake compatibility report controller.
 * Reads ./data/latest.json (see data/dashboard.schema.json). MIT OR Apache-2.0.
 */

import {
  cleanNote,
  escapeHtml,
  formatNumber,
  formatPercent,
  formatRatio,
  loadLatestData,
  normalizeStatus,
  snapshotMeta,
  statusHtml
} from "./app.js";

const DASH = '<span class="muted">-</span>';

function cell(value) {
  return value === null || value === undefined || value === "" ? DASH : escapeHtml(value);
}

function num(value) {
  return cell(formatNumber(value));
}

function rate(passed, total) {
  return typeof passed === "number" && typeof total === "number" && total > 0
    ? formatPercent((passed / total) * 100)
    : null;
}

function noteHtml(text) {
  const clean = cleanNote(text);
  return clean ? `<p class="note">${escapeHtml(clean)}</p>` : "";
}

function table(head, rows, attrs = "") {
  const th = head
    .map(([label, cls]) => `<th${cls ? ` class="${cls}"` : ""}>${escapeHtml(label)}</th>`)
    .join("");
  return `<div class="table-scroll"><table${attrs}><thead><tr>${th}</tr></thead><tbody>${rows.join("")}</tbody></table></div>`;
}

function summary(pairs) {
  const rows = pairs.map(([k, v]) => `<tr><th>${escapeHtml(k)}</th><td class="mono">${v}</td></tr>`);
  return `<table class="kv"><tbody>${rows.join("")}</tbody></table>`;
}

function section(id, n, title, body) {
  return `<section id="${id}"><h2>${n}. ${escapeHtml(title)}</h2>${body}</section>`;
}

/** Track 1: Core-62 suites and policies. */
export function renderCore(core, policies) {
  const rows = (core.highlights || []).map(
    (h) => `<tr>
      <td class="mono">${escapeHtml(h.suite)}</td>
      <td class="r mono">${num(h.passed)}</td>
      <td class="r mono">${num(h.total)}</td>
      <td class="r mono">${cell(rate(h.passed, h.total))}</td>
      <td class="r">${statusHtml(h.status)}</td>
    </tr>`
  );
  return section(
    "core",
    1,
    "Core-62 and policies",
    `${summary([
      ["Suites", `${num(core.suitesTotal)} (${num(core.suitesAt100)} at 100%)`],
      ["Cases", cell(formatRatio(core.casesPassed, core.casesTotal))],
      ["Pass rate", cell(formatPercent(core.passRate))],
      ["Status", statusHtml(core.status)]
    ])}${noteHtml(core.note)}
    <h3>Selected suites</h3>
    ${table([["Suite"], ["Passed", "r"], ["Total", "r"], ["Rate", "r"], ["Status", "r"]], rows)}
    <h3>${escapeHtml(policies.label)}</h3>
    ${summary([
      ["Always NEW", escapeHtml(policies.baselineAlwaysNew)],
      ["Active range", escapeHtml(policies.activeRange)],
      ["Suites", `${num(policies.suitesPassed)} / ${num(policies.suitesTotal)}`],
      ["Cases", cell(formatRatio(policies.casesPassed, policies.casesTotal))],
      ["Pass rate", cell(formatPercent(policies.passRate))],
      ["Status", statusHtml(policies.status)]
    ])}${noteHtml(policies.note)}`
  );
}

/** Track 2: upstream Tests/RunCMake on three operating systems. */
export function renderRunCmake(track) {
  const rows = track.platforms.map(
    (p) => `<tr>
      <td>${escapeHtml(p.name)}</td>
      <td class="note">${escapeHtml(p.toolchain)}</td>
      <td class="r mono">${num(p.suitesPassed)} / ${num(p.suitesTotal)}</td>
      <td class="r mono">${cell(formatRatio(p.casesPassed, p.casesTotal))}</td>
      <td class="r mono">${cell(formatPercent(p.passRate))}</td>
      <td class="r">${statusHtml(p.status)}</td>
    </tr>`
  );
  return section(
    "runcmake",
    2,
    "Tests/RunCMake on 3 operating systems",
    `${noteHtml(track.note)}${table(
      [["Platform"], ["Toolchain"], ["Suites", "r"], ["Cases", "r"], ["Rate", "r"], ["Status", "r"]],
      rows
    )}`
  );
}

/** One row of the large-project table. */
export function projectRowHtml(p, withNote = true) {
  const l1 = normalizeStatus(p.l1);
  const l2 = normalizeStatus(p.l2);
  return `<tr data-name="${escapeHtml(`${p.name} ${p.repository}`.toLowerCase())}" data-tier="${escapeHtml(p.tier)}" data-l1="${l1}" data-l2="${l2}">
    <td>${escapeHtml(p.name)}</td>
    <td class="mono">${escapeHtml(p.repository)}</td>
    <td class="mono nowrap">${escapeHtml(p.version)}</td>
    <td>${escapeHtml(p.tier)}</td>
    <td class="mono">${escapeHtml((p.targets || []).join(" "))}</td>
    <td>${statusHtml(l1)}</td>
    <td>${statusHtml(l2)}</td>
    ${withNote ? `<td class="note">${cell(cleanNote(p.note))}</td>` : ""}
  </tr>`;
}

/** Track 3: large real-world projects with a filter. */
export function renderProjects(track) {
  const withNote = track.projects.some((p) => cleanNote(p.note));
  const rows = track.projects.map((p) => projectRowHtml(p, withNote));
  const head = [["Project"], ["Repository"], ["Version"], ["Tier"], ["Targets"], ["L1"], ["L2"]];
  return section(
    "projects",
    3,
    `${track.projectsTotal} large projects`,
    `${summary([
      ["L1 configure", `${num(track.l1Passed)} / ${num(track.projectsTotal)}`],
      ["L2 Ninja dry run", `${num(track.l2Passed)} / ${num(track.projectsTotal)}`],
      ["Status", statusHtml(track.status)]
    ])}${noteHtml(track.note)}
    <div class="filters">
      <label>Find <input id="project-query" type="search" placeholder="name" aria-label="Filter projects by name"></label>
      <label>Tier <select id="project-tier"><option value="all">all</option><option value="standard">standard</option><option value="large">large</option></select></label>
      <label>Status <select id="project-status"><option value="all">all</option><option value="pass">pass</option><option value="partial">partial</option><option value="fail">fail</option><option value="pending">pending</option></select></label>
      <span id="project-count" class="muted"></span>
    </div>
    ${table(
      withNote ? [...head, ["Note"]] : head,
      rows,
      ' id="project-table"'
    )}`
  );
}

/** Track 4: sealed holdout. Aggregates and categories only. */
export function renderHoldout(track) {
  const rows = track.categories.map(
    (c) => `<tr>
      <td>${escapeHtml(c.category)}</td>
      <td class="r mono">${cell(formatRatio(c.reposPassed, c.reposTotal))}</td>
      <td class="r mono">${cell(formatPercent(c.passRate))}</td>
      <td class="r">${statusHtml(c.status)}</td>
    </tr>`
  );
  return section(
    "holdout",
    4,
    `Sealed ${track.reposTotal}-repository holdout`,
    `<p>${escapeHtml(track.policy)}</p>
    ${summary([
      ["Repositories", `${num(track.reposPassed)} / ${num(track.reposTotal)}`],
      ["Pass rate", cell(formatPercent(track.passRate))],
      ["Status", statusHtml(track.status)]
    ])}${noteHtml(track.note)}
    ${table([["Category"], ["Repositories", "r"], ["Rate", "r"], ["Status", "r"]], rows)}`
  );
}

/** Detail line for one vcpkg shard. */
export function shardDetail(shard) {
  const parts = [
    `shard ${shard.shard}`,
    normalizeStatus(shard.status),
    `ports ${formatRatio(shard.portsPassed, shard.portsTotal) ?? "-"}`,
    `failed ${formatNumber(shard.portsFailed) ?? "-"}`,
    `time ${typeof shard.durationSeconds === "number" ? `${shard.durationSeconds}s` : "-"}`
  ];
  const note = cleanNote(shard.note);
  return note ? `${parts.join("  ")}  ${note}` : parts.join("  ");
}

/** Track 5: vcpkg shard grid. */
export function renderVcpkg(track) {
  const cells = track.shards
    .map((s) => {
      const st = normalizeStatus(s.status);
      return `<button type="button" class="st-${st}" data-shard="${escapeHtml(s.shard)}" aria-pressed="false" title="shard ${escapeHtml(s.shard)}: ${st}">${String(s.shard).padStart(2, "0")}</button>`;
    })
    .join("");
  const counts = { pass: 0, partial: 0, fail: 0, pending: 0 };
  for (const s of track.shards) {
    counts[normalizeStatus(s.status)] += 1;
  }
  const legend = Object.entries(counts)
    .map(([k, v]) => `${statusHtml(k)} <span class="mono">${v}</span>`)
    .join(" &nbsp; ");
  return section(
    "vcpkg",
    5,
    `vcpkg ports, ${track.shardsTotal} shards`,
    `${summary([
      ["Shards", `${num(track.shardsCompleted)} of ${num(track.shardsTotal)} complete, ${num(track.shardsPassed)} passed`],
      ["Ports", cell(formatRatio(track.portsPassed, track.portsTotal))],
      ["Pass rate", cell(formatPercent(track.passRate))],
      ["Status", statusHtml(track.status)]
    ])}${noteHtml(track.note)}
    <div class="shards" role="group" aria-label="vcpkg shards">${cells}</div>
    <p class="note">${legend}</p>
    <pre id="shard-detail" class="code" aria-live="polite">Select a shard.</pre>`
  );
}

/** Track 6: benchmarks and soundness gates. */
export function renderBenchmarks(track) {
  const bench = Object.values(track.benchmarks).map(
    (b) => `<tr>
      <td>${escapeHtml(b.label)}</td>
      <td class="r mono">${typeof b.value === "number" ? escapeHtml(`${formatNumber(b.value)} ${b.unit}`) : DASH}</td>
      <td class="note">${escapeHtml(b.baseline)}</td>
      <td class="r">${statusHtml(b.status)}</td>
    </tr>`
  );
  const gates = track.gates.map(
    (g) => `<tr>
      <td>${escapeHtml(g.label)}</td>
      <td class="mono">${escapeHtml(cleanNote(g.summary))}</td>
      <td class="note">${escapeHtml(cleanNote(g.detail))}</td>
      <td class="r">${statusHtml(g.status)}</td>
    </tr>`
  );
  return section(
    "benchmarks",
    6,
    "Benchmarks and soundness",
    `${table([["Benchmark"], ["Value", "r"], ["Baseline"], ["Status", "r"]], bench)}
    <h3>Gates</h3>
    ${table([["Gate"], ["Result"], ["Detail"], ["Status", "r"]], gates)}`
  );
}

/** Render the full report body from a parsed `latest.json` object. */
export function renderReport(data) {
  const t = data.tracks;
  return [
    renderCore(t.coreConformance, t.policies),
    renderRunCmake(t.runCmakeOs),
    renderProjects(t.bossFight),
    renderHoldout(t.unseenHoldout),
    renderVcpkg(t.vcpkgMatrix),
    renderBenchmarks(t.benchmarksAndGates)
  ].join("");
}

function wireProjectFilter(doc) {
  const query = doc.getElementById("project-query");
  const tier = doc.getElementById("project-tier");
  const status = doc.getElementById("project-status");
  const count = doc.getElementById("project-count");
  const rows = Array.from(doc.querySelectorAll("#project-table tbody tr"));
  const apply = () => {
    const q = query.value.trim().toLowerCase();
    let shown = 0;
    for (const row of rows) {
      const ok = (!q || row.dataset.name.includes(q))
        && (tier.value === "all" || row.dataset.tier === tier.value)
        && (status.value === "all" || row.dataset.l1 === status.value || row.dataset.l2 === status.value);
      row.hidden = !ok;
      shown += ok ? 1 : 0;
    }
    count.textContent = `${shown} of ${rows.length}`;
  };
  for (const el of [query, tier, status]) {
    el.addEventListener("input", apply);
  }
  apply();
}

function wireShards(doc, shards) {
  const detail = doc.getElementById("shard-detail");
  const buttons = Array.from(doc.querySelectorAll("[data-shard]"));
  for (const btn of buttons) {
    btn.addEventListener("click", () => {
      const shard = shards.find((s) => String(s.shard) === btn.dataset.shard);
      for (const other of buttons) {
        other.setAttribute("aria-pressed", String(other === btn));
      }
      detail.textContent = shard ? shardDetail(shard) : "Unknown shard.";
    });
  }
}

async function init(doc) {
  const root = doc.getElementById("report");
  if (!root) {
    return;
  }
  const meta = doc.getElementById("report-meta");
  const note = doc.getElementById("report-note");
  try {
    const data = await loadLatestData();
    meta.textContent = snapshotMeta(data);
    note.textContent = cleanNote(data.statusNote);
    root.innerHTML = renderReport(data);
    wireProjectFilter(doc);
    wireShards(doc, data.tracks.vcpkgMatrix.shards);
  } catch (error) {
    root.innerHTML = `<p class="muted">Could not read data/latest.json (${escapeHtml(error.message)}). Serve the site over HTTP.</p>`;
  }
}

if (typeof document !== "undefined") {
  init(document);
}
