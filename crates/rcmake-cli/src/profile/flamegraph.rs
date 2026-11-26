//! Aggregate call paths and write bounded folded stacks and interactive HTML.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

use super::{Coverage, ProfileKind, Site, Span};

const MAX_HTML_FRAMES: usize = 32_768;

/// Factual summary metadata embedded in `flamegraph.html`.
pub(super) struct HtmlSummary {
    pub(super) project: Option<String>,
    pub(super) wall: Duration,
    pub(super) commands: u64,
    pub(super) top_self: Vec<(String, Duration, u64)>,
}

/// Aggregated call paths with wall-clock coverage at the root.
pub(super) struct Graph<'a> {
    nodes: Vec<Node<'a>>,
    folded_time: BTreeMap<(usize, u32), Duration>,
    multiple_tracks: bool,
    overlapping_siblings: bool,
}

#[derive(Default)]
struct Node<'a> {
    children: BTreeMap<&'a Site, usize>,
    coverage: Coverage,
    children_coverage: Coverage,
    self_time: Duration,
    children_time: Duration,
    count: usize,
}

impl<'a> Graph<'a> {
    /// Aggregate spans ordered by start time, with parents before their children.
    pub(super) fn from_spans(spans: &'a [Span], coverage: Duration) -> Self {
        let mut graph = Self {
            nodes: vec![Node {
                coverage: Coverage {
                    elapsed: coverage,
                    ..Coverage::default()
                },
                ..Node::default()
            }],
            folded_time: BTreeMap::new(),
            multiple_tracks: spans
                .first()
                .is_some_and(|first| spans.iter().any(|span| span.tid != first.tid)),
            overlapping_siblings: false,
        };
        let mut path_for_span = Vec::with_capacity(spans.len());
        for span in spans {
            let parent = span
                .parent
                .and_then(|id| path_for_span.get(id).copied())
                .unwrap_or(0);
            let id = graph.child(parent, &span.site);
            path_for_span.push(id);
            let end = span.end();
            if let Some(parent) = graph.nodes.get_mut(parent) {
                parent.children_coverage.add(span.start, end);
            }
            let Some(node) = graph.nodes.get_mut(id) else {
                continue;
            };
            node.coverage.add(span.start, end);
            node.self_time = node.self_time.saturating_add(span.self_time);
            node.count += 1;
            if !span.self_time.is_zero() {
                let time = graph.folded_time.entry((id, span.tid)).or_default();
                *time = time.saturating_add(span.self_time);
            }
        }
        for id in 0..graph.nodes.len() {
            if let Some(node) = graph.nodes.get(id) {
                let sum = node.children.values().fold(Duration::ZERO, |sum, &child| {
                    let time = graph
                        .nodes
                        .get(child)
                        .map_or(Duration::ZERO, |n| n.coverage.elapsed);
                    sum.saturating_add(time)
                });
                if let Some(node) = graph.nodes.get_mut(id) {
                    graph.overlapping_siblings |= sum > node.children_coverage.elapsed;
                    node.children_time = sum;
                }
            }
        }
        graph
    }

    fn child(&mut self, parent: usize, site: &'a Site) -> usize {
        let id = self.nodes.len();
        let Some(parent) = self.nodes.get_mut(parent) else {
            return 0;
        };
        let child = *parent.children.entry(site).or_insert(id);
        if child == id {
            self.nodes.push(Node::default());
        }
        child
    }

    /// Write complete folded paths with self time in microseconds within the byte budget.
    ///
    /// Return the output and the number of paths omitted by the budget.
    pub(super) fn folded(&self, limit: usize, dropped_samples: usize) -> (String, usize) {
        let mut out = String::new();
        let mut omitted = 0;
        let mut stack: Vec<(String, usize)> = Vec::new();
        let mut pending = vec![(0, 0, None)];
        while let Some((id, depth, site)) = pending.pop() {
            let Some(node) = self.nodes.get(id) else {
                continue;
            };
            stack.truncate(depth);
            if let Some(site) = site {
                let label = folded_label(site);
                let prefix = stack.last().map_or(0, |(_, bytes)| *bytes);
                let bytes = prefix.saturating_add(label.len() + 1);
                stack.push((label, bytes));
            }
            let path_bytes = stack.last().map_or(0, |(_, bytes)| *bytes);
            for ((_, tid), time) in self.folded_time.range((id, 0)..=(id, u32::MAX)) {
                let micros = time.as_micros();
                if micros == 0 {
                    continue;
                }
                let count = micros.to_string();
                let track = if self.multiple_tracks {
                    format!("[track {tid}];")
                } else {
                    String::new()
                };
                if out
                    .len()
                    .saturating_add(path_bytes)
                    .saturating_add(track.len() + count.len() + 1)
                    > limit
                {
                    omitted += 1;
                    continue;
                }
                out.push_str(&track);
                for (i, (label, _)) in stack.iter().enumerate() {
                    if i > 0 {
                        out.push(';');
                    }
                    out.push_str(label);
                }
                let _ = writeln!(out, " {count}");
            }
            pending.extend(
                node.children
                    .iter()
                    .rev()
                    .map(|(&site, &id)| (id, stack.len(), Some(site))),
            );
        }
        if dropped_samples > 0 || omitted > 0 {
            loop {
                // Use a zero-weight row so folded-stack readers can parse the whole file.
                let note = format!(
                    "[rcmake omitted {dropped_samples} samples and {omitted} folded paths] 0\n"
                );
                if note.len() > limit {
                    break;
                }
                if out.len().saturating_add(note.len()) <= limit {
                    out.insert_str(0, &note);
                    break;
                }
                let before_last = out.trim_end_matches('\n').rfind('\n');
                out.truncate(before_last.map_or(0, |index| index + 1));
                omitted += 1;
            }
        }
        (out, omitted)
    }

    /// Write a self-contained HTML report within the byte and frame budgets.
    ///
    /// Return the output and the number of graph frames omitted by either budget.
    pub(super) fn html(
        &self,
        limit: usize,
        dropped_samples: usize,
        summary: Option<&HtmlSummary>,
    ) -> (String, usize) {
        let frames = self.nodes.len().saturating_sub(1);
        let footer =
            |omitted| format!("],\"omitted\":{omitted},\"dropped\":{dropped_samples}}}</script>\n");
        let suffix_bytes = footer(frames).len() + SCRIPT.len() + HTML_END.len();
        let mut out = String::from(HTML_START);
        let Some(root) = self.nodes.first() else {
            return (String::new(), frames);
        };
        let _ = write!(
            out,
            "{{\"coverage\":{},\"childrenTime\":{},\"overlap\":{},",
            root.coverage.elapsed.as_nanos(),
            root.children_time.as_nanos(),
            self.overlapping_siblings
        );
        if let Some(summary) = summary {
            out.push_str("\"summary\":{\"project\":");
            match &summary.project {
                Some(project) => write_script_string(&mut out, project),
                None => out.push_str("null"),
            }
            let _ = write!(
                out,
                ",\"wall\":{},\"commands\":{},\"topSelf\":[",
                summary.wall.as_nanos(),
                summary.commands,
            );
            for (idx, (label, self_time, calls)) in summary.top_self.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                out.push('[');
                write_script_string(&mut out, label);
                let _ = write!(out, ",{},{}]", self_time.as_nanos(), calls);
            }
            out.push_str("]},");
        }
        out.push_str("\"frames\":[");
        if out.len().saturating_add(suffix_bytes) > limit {
            let page = format!(
                "<!doctype html><meta charset=utf-8><title>rcmake profile</title>\n<p>The HTML size limit omitted {frames} graph frames. Recording omitted {dropped_samples} samples.</p>\n"
            );
            return (
                if page.len() <= limit {
                    page
                } else {
                    String::new()
                },
                frames,
            );
        }

        let mut pending: Vec<_> = root
            .children
            .iter()
            .rev()
            .map(|(&site, &id)| (id, 0, site))
            .collect();
        let mut written = 0;
        let mut frame = String::new();
        while let Some((id, parent, site)) = pending.pop() {
            let remaining = limit.saturating_sub(out.len()).saturating_sub(suffix_bytes);
            if written == MAX_HTML_FRAMES || remaining == 0 {
                break;
            }
            let Some(node) = self.nodes.get(id) else {
                continue;
            };
            // A skipped parent cannot contribute descendants to this report.
            if site.file.len().max(site.name.len()) > remaining {
                continue;
            }
            frame.clear();
            if written > 0 {
                frame.push(',');
            }
            let _ = write!(frame, "[{parent},\"{}\",", site.kind.category());
            write_script_string(&mut frame, &short_label(site));
            frame.push(',');
            write_script_string(&mut frame, &detailed_label(site));
            let _ = write!(
                frame,
                ",{},{},{},{}]",
                node.coverage.elapsed.as_nanos(),
                node.self_time.as_nanos(),
                node.count,
                node.children_time.as_nanos()
            );
            if frame.len() > remaining {
                continue;
            }
            out.push_str(&frame);
            written += 1;
            pending.extend(
                node.children
                    .iter()
                    .rev()
                    .map(|(&site, &id)| (id, written, site)),
            );
        }
        let omitted = frames.saturating_sub(written);
        out.push_str(&footer(omitted));
        out.push_str(SCRIPT);
        out.push_str(HTML_END);
        (out, omitted)
    }
}

fn detailed_label(site: &Site) -> String {
    if site.kind == ProfileKind::ListFile && !site.file.is_empty() {
        return String::from_utf8_lossy(&site.file).into_owned();
    }
    let name = String::from_utf8_lossy(&site.name);
    if site.kind.is_command() && !site.file.is_empty() {
        format!(
            "{name} ({}:{})",
            String::from_utf8_lossy(&site.file),
            site.line
        )
    } else {
        name.into_owned()
    }
}

fn short_label(site: &Site) -> String {
    let text = if site.kind == ProfileKind::ListFile && !site.file.is_empty() {
        &site.file
    } else {
        &site.name
    };
    let text = String::from_utf8_lossy(text);
    if site.kind == ProfileKind::ListFile {
        text.rsplit(['/', '\\'])
            .find(|part| !part.is_empty())
            .unwrap_or(&text)
            .to_owned()
    } else {
        text.into_owned()
    }
}

fn folded_label(site: &Site) -> String {
    let mut out = String::new();
    for ch in detailed_label(site).chars() {
        if ch.is_control() || matches!(ch, ';' | '%') {
            let _ = write!(out, "%{:02X}", u32::from(ch));
        } else {
            out.push(ch);
        }
    }
    out
}

// Escape JSON for a script element. In particular, source text cannot close the element.
fn write_script_string(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '<' | '>' | '&' | '\u{2028}' | '\u{2029}' => {
                let _ = write!(out, "\\u{:04x}", u32::from(ch));
            }
            ch if ch.is_control() => {
                let _ = write!(out, "\\u{:04x}", u32::from(ch));
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
}

const HTML_START: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>rcmake profile</title>
<style>
:root {
  color-scheme: light;
  --page: #fff; --panel: #f4f7fa; --text: #172635; --subtle: #43566a; --line: #c7d1dc;
  --accent: #0056a6; --bar: #bbd8ef; --dim: #e4e8ee; --dim-text: #5b6d80; --frame-text: #151e28;
  --bar-phase: #a4dcb9; --bar-listfile: #d2c8f0; --bar-macro: #f3c198; --bar-function: #efd797;
  font: 14px/1.4 system-ui, -apple-system, sans-serif;
}
@media (prefers-color-scheme: dark) {
  :root:not([data-theme]) {
    color-scheme: dark;
    --page: #111820; --panel: #1a2430; --text: #e7eef8; --subtle: #9fb1c4; --line: #344456;
    --accent: #8fd1ff; --bar: #2f4a63; --dim: #24303d; --dim-text: #8fa0b3; --frame-text: #e7eef8;
    --bar-phase: #2f5e45; --bar-listfile: #4a3f73; --bar-macro: #6e4a2c; --bar-function: #6b5a2a;
  }
}
:root[data-theme="dark"] {
  color-scheme: dark;
  --page: #111820; --panel: #1a2430; --text: #e7eef8; --subtle: #9fb1c4; --line: #344456;
  --accent: #8fd1ff; --bar: #2f4a63; --dim: #24303d; --dim-text: #8fa0b3; --frame-text: #e7eef8;
  --bar-phase: #2f5e45; --bar-listfile: #4a3f73; --bar-macro: #6e4a2c; --bar-function: #6b5a2a;
}
body { margin: 0; color: var(--text); background: var(--page); }
main { max-width: 1280px; margin: 0 auto; padding: clamp(12px, 2.5vw, 22px); }
header, .controls, #breadcrumbs, #legend { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; }
header { justify-content: space-between; padding-bottom: 10px; border-bottom: 1px solid var(--line); }
.title-row { display: flex; flex-wrap: wrap; align-items: baseline; gap: 6px 14px; }
h1 { font-size: 1.35rem; font-weight: 650; margin: 0; letter-spacing: -0.01em; }
h2 { font-size: 0.92rem; font-weight: 600; margin: 0 0 8px; }
#summary { margin: 0; color: var(--subtle); font-variant-numeric: tabular-nums; }
#legend { list-style: none; padding: 0; margin: 10px 0 8px; gap: 6px 14px; font-size: 0.85rem; color: var(--subtle); }
#legend li { display: inline-flex; align-items: center; gap: 6px; }
.swatch { display: inline-block; width: 12px; height: 12px; border-radius: 2px; border: 1px solid color-mix(in srgb, var(--line) 60%, transparent); }
.swatch.phase { background: var(--bar-phase); }
.swatch.listfile { background: var(--bar-listfile); }
.swatch.builtin { background: var(--bar); }
.swatch.function { background: var(--bar-function); }
.swatch.macro { background: var(--bar-macro); }
.controls { margin: 8px 0; }
input, button, select { font: inherit; color: inherit; background: var(--panel); border: 1px solid var(--line); border-radius: 5px; padding: 4px 8px; }
input { width: min(16rem, 50vw); }
button:disabled { opacity: .55; }
button:not(:disabled), select { cursor: pointer; }
:focus-visible { outline: 2px solid var(--accent); outline-offset: 2px; }
#breadcrumbs { margin: 0; font-size: 0.9rem; }
#breadcrumbs button { max-width: min(14rem, 70vw); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
#breadcrumbs span, #details-path { overflow-wrap: anywhere; }
.status, #matches, #render-status { color: var(--subtle); font-size: 0.88rem; }
.status { margin: 6px 0; }
.banner { padding: 8px 12px; border: 1px solid var(--line); border-radius: 5px; margin: 10px 0; }
#graph-wrap { overflow: auto; max-height: min(68vh, 640px); border: 1px solid var(--line); border-radius: 6px; background: var(--page); }
#graph { display: block; width: 100%; }
#graph:focus-visible { outline-offset: -2px; }
.frame { cursor: pointer; }
.frame rect { fill: var(--bar); }
.phase rect { fill: var(--bar-phase); }
.listfile rect { fill: var(--bar-listfile); }
.macro rect { fill: var(--bar-macro); }
.function rect { fill: var(--bar-function); }
.frame[data-search="dim"] rect { fill: var(--dim); }
.frame[data-search="match"] rect { fill: #ffd66e; }
.frame.active rect { stroke: var(--accent); stroke-width: 2px; }
.frame text { fill: var(--frame-text); font: 12px system-ui, -apple-system, sans-serif; pointer-events: none; }
.frame[data-search="dim"] text { fill: var(--dim-text); }
.frame[data-search="match"] text { fill: #151e28; }
.panels { display: grid; grid-template-columns: repeat(auto-fit, minmax(320px, 1fr)); gap: 12px; margin-top: 12px; }
#details, #top-costs { padding: 12px; background: var(--panel); border: 1px solid var(--line); border-radius: 6px; }
#details-path { margin: 0; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 0.86rem; }
dl { display: flex; flex-wrap: wrap; gap: 8px 20px; margin: 10px 0 0; }
dt { color: var(--subtle); font-size: 0.82rem; }
dd { margin: 1px 0 0; font-variant-numeric: tabular-nums; font-weight: 500; }
#top-self { width: 100%; border-collapse: collapse; font-size: 0.86rem; }
#top-self th, #top-self td { padding: 4px 6px; border-bottom: 1px solid color-mix(in srgb, var(--line) 55%, transparent); text-align: left; }
#top-self th:not(:first-child), #top-self td:not(:first-child) { text-align: right; font-variant-numeric: tabular-nums; white-space: nowrap; }
#top-self td:first-child { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; overflow-wrap: anywhere; }
#top-self tbody tr:last-child td { border-bottom: 0; }
</style></head><body><main>
<header>
<div class="title-row"><h1>rcmake profile</h1><p id="summary"></p></div>
<label>Theme <select id="theme">
<option value="system">System</option><option value="light">Light</option><option value="dark">Dark</option>
</select></label>
</header>
<noscript><p>Enable JavaScript to view the graph.</p></noscript>
<p id="omissions" class="banner" hidden></p>
<p id="empty" hidden></p>
<section id="profile" hidden>
<ul id="legend" aria-label="Frame types">
<li><span class="swatch phase"></span>Phase</li>
<li><span class="swatch listfile"></span>List file</li>
<li><span class="swatch builtin"></span>Built-in command</li>
<li><span class="swatch function"></span>Function</li>
<li><span class="swatch macro"></span>Macro</li>
</ul>
<div class="controls">
<label>Search <input type="search" id="search" aria-keyshortcuts="/"></label>
<button type="button" id="next" disabled>Next</button>
<output id="matches" aria-live="polite"></output>
<button type="button" id="up" disabled>Zoom out</button>
<button type="button" id="reset" disabled>Reset</button>
<nav id="breadcrumbs" aria-label="Zoom path"></nav>
</div>
<p class="status" id="overlap" hidden>Overlapping frames scaled to parent width.</p>
<div id="graph-wrap">
<svg id="graph" tabindex="0" role="tree" aria-label="Profile flamegraph"></svg>
</div>
<p id="render-status" class="status" hidden></p>
<div class="panels">
<section id="details" aria-labelledby="details-title">
<h2 id="details-title">Selected frame</h2><p id="details-path"></p>
<dl>
<div><dt>Inclusive</dt><dd id="inclusive"></dd></div>
<div><dt>Self</dt><dd id="self-time"></dd></div>
<div><dt>Calls</dt><dd id="calls"></dd></div>
<div><dt>% wall</dt><dd id="root-percent"></dd></div>
<div><dt>% view</dt><dd id="zoom-percent"></dd></div>
</dl>
</section>
<section id="top-costs" aria-labelledby="top-costs-title" hidden>
<h2 id="top-costs-title">Top self time</h2>
<table id="top-self"><thead><tr><th>Frame</th><th>Self time</th><th>Calls</th></tr></thead><tbody></tbody></table>
</section>
</div>
</section>
<script type="application/json" id="profile-data">"#;

const HTML_END: &str = "</main></body></html>\n";

const SCRIPT: &str = r#"<script>
(() => {
  const byId = (id) => document.getElementById(id);
  const theme = byId('theme');
  theme.addEventListener('change', () => {
    if (theme.value === 'system') document.documentElement.removeAttribute('data-theme');
    else document.documentElement.dataset.theme = theme.value;
  });

  function duration(ns) {
    if (ns >= 1e9) return `${(ns / 1e9).toFixed(3)} s`;
    if (ns >= 1e6) return `${(ns / 1e6).toFixed(3)} ms`;
    if (ns >= 1e3) return `${(ns / 1e3).toFixed(3)} µs`;
    return `${ns} ns`;
  }

  const data = JSON.parse(byId('profile-data').textContent);
  const {coverage, childrenTime, overlap, frames, omitted, dropped, summary} = data;
  if (summary) {
    const parts = [];
    if (summary.project) parts.push(summary.project);
    parts.push(`${duration(summary.wall)} wall time`);
    parts.push(`${summary.commands} commands`);
    byId('summary').textContent = parts.join(' · ');
    if (Array.isArray(summary.topSelf) && summary.topSelf.length) {
      const tbody = byId('top-self').querySelector('tbody');
      for (const [label, selfNs, count] of summary.topSelf) {
        const tr = document.createElement('tr');
        const tdLabel = document.createElement('td');
        tdLabel.textContent = label;
        const tdTime = document.createElement('td');
        tdTime.textContent = duration(selfNs);
        const tdCalls = document.createElement('td');
        tdCalls.textContent = String(count);
        tr.append(tdLabel, tdTime, tdCalls);
        tbody.append(tr);
      }
      byId('top-costs').hidden = false;
    }
  } else if (coverage) {
    byId('summary').textContent = `${duration(coverage)} wall time`;
  }
  if (omitted || dropped) {
    const notes = [];
    if (dropped) notes.push(`Recording omitted ${dropped} samples.`);
    if (omitted) notes.push(`The HTML limit omitted ${omitted} graph frames.`);
    byId('omissions').textContent = notes.join(' ');
    byId('omissions').hidden = false;
  }
  if (!coverage || !frames.length) {
    byId('empty').textContent = !frames.length && (omitted || dropped)
      ? 'A profile limit excluded all graph frames.'
      : !frames.length ? 'No operations were recorded.'
        : 'No retained operation has a positive duration.';
    byId('empty').hidden = false;
    return;
  }

  const nodes = [{parent: 0, short: 'Whole profile', label: 'Whole profile', inclusive: coverage,
    childrenTime, children: [], end: frames.length + 1}];
  for (const [parent, kind, short, label, inclusive, self, count, childrenTime] of frames) {
    const id = nodes.length;
    const siblings = nodes[parent].children;
    nodes.push({id, parent, kind, short, label, inclusive, self, count, childrenTime,
      match: false, sibling: siblings.length, children: [], end: id + 1});
    siblings.push(id);
  }
  for (let id = nodes.length - 1; id > 0; id--) {
    const node = nodes[id];
    const parent = nodes[node.parent];
    parent.end = Math.max(parent.end, node.end);
  }
  byId('overlap').hidden = !overlap;

  const svg = byId('graph');
  const wrap = byId('graph-wrap');
  const search = byId('search');
  const matches = byId('matches');
  const next = byId('next');
  const up = byId('up');
  const reset = byId('reset');
  const crumbs = byId('breadcrumbs');
  const status = byId('render-status');
  const ROW = 24;
  const MAX_BARS = 4096;
  const ns = 'http://www.w3.org/2000/svg';
  const rendered = new Map();
  let rows = [];
  let zoom = 0;
  let active = 1;
  let query = '';
  let hits = [];
  let timer;
  let renderQueued = false;

  function svgElement(tag, attrs) {
    const element = document.createElementNS(ns, tag);
    for (const [key, value] of Object.entries(attrs)) element.setAttribute(key, value);
    return element;
  }

  function inZoom(id) {
    return id >= (zoom || 1) && id < nodes[zoom].end;
  }

  function layout() {
    const width = Math.max(1, wrap.clientWidth);
    rows = [];
    const pending = [[zoom, zoom ? 0 : -1, 0, width]];
    while (pending.length) {
      const [id, row, x, width] = pending.pop();
      const node = nodes[id];
      if (row >= 0) {
        node.row = row;
        node.x = x;
        node.width = width;
        (rows[row] ??= []).push(id);
      }
      const scale = width / (Math.max(node.inclusive, node.childrenTime) || 1);
      let from = x + node.children.reduce((sum, child) => sum + nodes[child].inclusive * scale, 0);
      from = Math.min(from, x + width);
      for (let i = node.children.length - 1; i >= 0; i--) {
        const child = node.children[i];
        const start = Math.max(x, from - nodes[child].inclusive * scale);
        pending.push([child, row + 1, start, from - start]);
        from = start;
      }
    }
    const height = Math.max(1, rows.length * ROW);
    svg.setAttribute('viewBox', `-1 -1 ${width + 2} ${height + 2}`);
    svg.setAttribute('height', height + 2);
  }

  function percent(time, total) {
    const value = total ? time / total * 100 : 0;
    if (value > 0 && value < 0.01) return '<0.01%';
    return total ? `${value.toFixed(2)}%` : '—';
  }

  function detail() {
    const node = nodes[active];
    byId('details-path').textContent = node.label;
    byId('inclusive').textContent = duration(node.inclusive);
    byId('self-time').textContent = duration(node.self);
    byId('calls').textContent = node.count;
    byId('root-percent').textContent = percent(node.inclusive, coverage);
    byId('zoom-percent').textContent = percent(node.inclusive, nodes[zoom].inclusive);
    const element = rendered.get(active);
    if (element) {
      element.classList.add('active');
      element.setAttribute('aria-selected', 'true');
      svg.setAttribute('aria-activedescendant', `frame-${active}`);
    } else {
      svg.removeAttribute('aria-activedescendant');
    }
  }

  function bar(id) {
    const node = nodes[id];
    const group = svgElement('g', {
      id: `frame-${id}`, class: `frame ${node.kind}`, 'data-id': id, role: 'treeitem', 'aria-level': node.row + 1,
      'aria-selected': 'false', 'aria-label': `${node.label}, ${duration(node.inclusive)}`,
      transform: `translate(${node.x} ${node.row * ROW})`,
    });
    if (node.children.length) group.setAttribute('aria-expanded', 'true');
    highlight(id, group);
    const width = Math.max(0, node.width - (node.width > 3 ? 1 : 0));
    group.append(svgElement('rect', {width, height: ROW - 2, rx: Math.min(2, width / 2)}));
    if (width >= 28) {
      const label = svgElement('svg', {width, height: ROW - 2, overflow: 'hidden'});
      const text = svgElement('text', {x: 5, y: 15});
      const capacity = Math.floor((width - 10) / 6.5);
      if (capacity >= 2) {
        const prefix = node.short.slice(0, capacity * 2 + 1);
        const chars = Array.from(prefix);
        if (chars.length <= capacity && prefix.length === node.short.length) {
          text.textContent = prefix;
        } else if (node.kind === 'listfile') {
          const head = Math.floor((capacity - 1) / 2);
          const tail = capacity - head - 1;
          const suffix = Array.from(node.short.slice(-tail * 2)).slice(-tail);
          text.textContent = chars.slice(0, head).join('') + '…' + suffix.join('');
        } else {
          text.textContent = chars.slice(0, capacity - 1).join('') + '…';
        }
      }
      label.append(text);
      group.append(label);
    }
    rendered.set(id, group);
    return group;
  }

  function highlight(id, element) {
    const state = nodes[id].match ? 'match' : query ? 'dim' : '';
    if (state) {
      if (element.dataset.search !== state) element.dataset.search = state;
    } else {
      delete element.dataset.search;
    }
  }

  function render() {
    const first = Math.max(0, Math.floor(wrap.scrollTop / ROW) - 1);
    const last = Math.min(rows.length, Math.ceil((wrap.scrollTop + wrap.clientHeight) / ROW) + 1);
    const fragment = document.createDocumentFragment();
    const activeRow = nodes[active].row;
    const addActive = activeRow >= first && activeRow < last;
    const cap = MAX_BARS - Number(addActive);
    let candidates = Number(addActive);
    let others = 0;
    rendered.clear();
    for (let row = first; row < last; row++) {
      candidates += rows[row].length - Number(addActive && row === activeRow);
      for (const id of rows[row]) {
        if (id === active) fragment.append(bar(id));
        else {
          if (others === cap) break;
          fragment.append(bar(id));
          others++;
        }
      }
      if (row === activeRow && !rendered.has(active)) fragment.append(bar(active));
    }
    svg.replaceChildren(fragment);
    status.hidden = candidates <= MAX_BARS;
    status.textContent = status.hidden ? '' :
      `Showing ${MAX_BARS} of ${candidates} frames in these rows. Search or use the arrow keys to select more frames.`;
    detail();
  }

  function queueRender() {
    if (renderQueued) return;
    renderQueued = true;
    requestAnimationFrame(() => {
      renderQueued = false;
      render();
    });
  }

  function select(id, scroll) {
    const previous = rendered.get(active);
    if (previous) {
      previous.classList.remove('active');
      previous.setAttribute('aria-selected', 'false');
    }
    active = id;
    if (scroll) {
      const top = nodes[active].row * ROW;
      if (top < wrap.scrollTop) wrap.scrollTop = top;
      else if (top + ROW > wrap.scrollTop + wrap.clientHeight) {
        wrap.scrollTop = top + ROW - wrap.clientHeight;
      }
      if (!rendered.has(active)) render();
      else detail();
    } else {
      detail();
    }
  }

  function firstHitAtOrAfter(id) {
    let low = 0;
    let high = hits.length;
    while (low < high) {
      const middle = Math.floor((low + high) / 2);
      if (hits[middle] < id) low = middle + 1;
      else high = middle;
    }
    return low;
  }

  function matchCount() {
    const visible = firstHitAtOrAfter(nodes[zoom].end) - firstHitAtOrAfter(zoom || 1);
    matches.textContent = query ? `${visible} in view · ${hits.length} total` : '';
    next.disabled = !hits.length;
  }

  function findMatches() {
    clearTimeout(timer);
    query = search.value.trim().toLowerCase();
    hits = [];
    for (let id = 1; id < nodes.length; id++) {
      const node = nodes[id];
      node.match = query !== '' && node.label.toLowerCase().includes(query);
      if (node.match) hits.push(id);
    }
    matchCount();
    for (const [id, element] of rendered) highlight(id, element);
  }

  function breadcrumbs() {
    const path = [];
    for (let id = zoom; id && path.length < 5; id = nodes[id].parent) path.push(id);
    const shortened = path.length === 5 && nodes[path[path.length - 1]].parent !== 0;
    const entries = [0, ...path.reverse()];
    crumbs.replaceChildren();
    for (const id of entries) {
      if (id) crumbs.append(document.createTextNode(shortened && id === entries[1] ? ' / … / ' : ' / '));
      const node = nodes[id];
      if (id === zoom) {
        const label = document.createElement('span');
        label.textContent = node.short;
        label.setAttribute('aria-current', 'location');
        crumbs.append(label);
      } else {
        const button = document.createElement('button');
        button.type = 'button';
        button.textContent = node.short;
        button.title = node.label;
        button.addEventListener('click', () => setZoom(id));
        crumbs.append(button);
      }
    }
  }

  function setZoom(id, selection = id || nodes[0].children[0]) {
    zoom = id;
    active = selection;
    wrap.scrollTop = 0;
    up.disabled = reset.disabled = zoom === 0;
    layout();
    breadcrumbs();
    matchCount();
    render();
    svg.focus({preventScroll: true});
    select(active, true);
  }

  function jump(event) {
    if (event?.type === 'keydown') event.preventDefault();
    if (query !== search.value.trim().toLowerCase()) findMatches();
    if (!hits.length) return;
    const outside = hits[0] < zoom ? hits[0] : hits[firstHitAtOrAfter(nodes[zoom].end)];
    const id = !nodes[active].match && outside !== undefined
      ? outside : hits[firstHitAtOrAfter(active + 1)] ?? hits[0];
    setZoom(id);
  }

  function frameAt(event) {
    const id = Number(event.target.closest('.frame')?.dataset.id);
    return inZoom(id) ? id : undefined;
  }

  svg.addEventListener('pointerover', (event) => {
    const id = frameAt(event);
    if (id !== undefined && id !== active) select(id, false);
  });
  svg.addEventListener('click', (event) => {
    const id = frameAt(event);
    if (id !== undefined) setZoom(id);
  });
  svg.addEventListener('keydown', (event) => {
    const node = nodes[active];
    const siblings = nodes[node.parent].children;
    const directions = {
      ArrowLeft: siblings[node.sibling - 1],
      ArrowRight: siblings[node.sibling + 1],
      ArrowUp: node.parent,
      ArrowDown: node.children[0],
      Home: zoom || nodes[0].children[0],
      End: nodes[zoom].end - 1,
    };
    if (event.key === 'Enter' || event.key === ' ') {
      event.preventDefault();
      setZoom(active);
    } else if (Object.hasOwn(directions, event.key)) {
      event.preventDefault();
      const id = directions[event.key];
      if (inZoom(id)) select(id, true);
    }
  });
  wrap.addEventListener('scroll', queueRender);
  new ResizeObserver(() => { layout(); queueRender(); }).observe(wrap);
  search.addEventListener('input', () => {
    clearTimeout(timer);
    timer = setTimeout(findMatches, 120);
  });
  search.addEventListener('keydown', (event) => { if (event.key === 'Enter') jump(event); });
  next.addEventListener('click', jump);
  up.addEventListener('click', () => setZoom(nodes[zoom].parent, zoom));
  reset.addEventListener('click', () => setZoom(0));
  document.addEventListener('keydown', (event) => {
    if (event.defaultPrevented) return;
    if (event.key === '/' && !event.target.matches('input, textarea, select, [contenteditable]')) {
      event.preventDefault();
      search.focus();
    } else if (event.key === 'Escape' && zoom) {
      event.preventDefault();
      const keepSearch = event.target === search;
      setZoom(0);
      if (keepSearch) search.focus();
    }
  });

  byId('profile').hidden = false;
  layout();
  breadcrumbs();
  render();
})();
</script>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn span(name: &[u8], start: u64, elapsed: u64, self_time: u64, parent: Option<usize>) -> Span {
        Span {
            site: Site {
                name: name.into(),
                file: b"".as_slice().into(),
                line: 0,
                kind: ProfileKind::Builtin,
            },
            depth: u32::from(parent.is_some()),
            infer_depth: false,
            start: Duration::from_micros(start),
            elapsed: Duration::from_micros(elapsed),
            thread: std::thread::current().id(),
            parent,
            tid: 1,
            self_time: Duration::from_micros(self_time),
        }
    }

    #[test]
    fn folds_repeated_paths_and_keeps_complete_rows_within_the_limit() {
        let spans = [
            span(b"root", 0, 80, 55, None),
            span(b"z", 1, 10, 10, Some(0)),
            span(b"a;\n%", 20, 15, 15, Some(0)),
            span(b"root", 100, 20, 15, None),
            span(b"a;\n%", 101, 5, 5, Some(3)),
        ];
        let graph = Graph::from_spans(&spans, Duration::from_micros(100));
        let (folded, omitted) = graph.folded(128, 0);
        assert_eq!(folded, "root 70\nroot;a%3B%0A%25 20\nroot;z 10\n");
        assert_eq!(omitted, 0);

        let (folded, omitted) = graph.folded(78, 2);
        assert!(folded.starts_with("[rcmake omitted 2 samples and 1 folded paths] 0\nroot 70\n"));
        assert!(folded.len() <= 78);
        assert_eq!(omitted, 1);
    }

    #[test]
    fn encodes_script_terminators_and_control_characters_as_json_text() {
        let spans = [span(b"</script><img src=x>\"\x00'&\xff", 0, 80, 80, None)];
        let graph = Graph::from_spans(&spans, Duration::from_micros(80));
        let (html, omitted) = graph.html(100_000, 0, None);
        assert_eq!(omitted, 0);
        assert_eq!(html.matches("</script>").count(), 2);
        assert!(html.contains(r#"\u003c/script\u003e\u003cimg src=x\u003e\"\u0000'\u0026�"#));
        assert!(!html.contains("<img"));
        assert!(!html.contains('\0'));
    }
}
