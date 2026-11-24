//! Merge call stacks and write a self-contained SVG flamegraph.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

use super::{ProfileKind, ProfilePhase, Site, Span};

const WIDTH: f64 = 1200.0;
const ROW_HEIGHT: usize = 24;
const MIN_BAR_WIDTH: f64 = 0.5;

/// Merged flamegraph nodes in parent-first order, with a synthetic root at index zero.
pub(super) struct Graph<'a> {
    nodes: Vec<Node<'a>>,
}

#[derive(Default)]
struct Node<'a> {
    parent: usize,
    children: BTreeMap<&'a Site, usize>,
    own_time: Duration,
    total: Duration,
}

struct Active<'a> {
    span: &'a Span,
    node: usize,
    covered_until: Duration,
    covered: Duration,
}

struct Bar<'a> {
    site: &'a Site,
    node: &'a Node<'a>,
    row: usize,
    x: f64,
    width: f64,
}

impl<'a> Graph<'a> {
    /// Build the merged graph from samples in start-time order.
    pub(super) fn from_spans(spans: &[&'a Span]) -> Self {
        let mut graph = Self {
            nodes: vec![Node::default()],
        };
        let mut active = Vec::<Active<'_>>::new();
        for &span in spans {
            while active
                .last()
                .is_some_and(|frame| !contains(frame.span, span))
            {
                graph.close(&mut active);
            }
            let parent = if let Some(parent) = active.last_mut() {
                let from = parent.covered_until.max(span.start);
                parent.covered = parent
                    .covered
                    .saturating_add(span.end().saturating_sub(from));
                parent.covered_until = parent.covered_until.max(span.end());
                parent.node
            } else {
                0
            };
            let Some(node) = graph.child(parent, &span.site) else {
                continue;
            };
            active.push(Active {
                span,
                node,
                covered_until: span.start,
                covered: Duration::ZERO,
            });
        }
        while !active.is_empty() {
            graph.close(&mut active);
        }
        graph.sum_children();
        graph
    }

    fn child(&mut self, parent: usize, site: &'a Site) -> Option<usize> {
        let index = self.nodes.len();
        let children = &mut self.nodes.get_mut(parent)?.children;
        if let Some(child) = children.get(site) {
            return Some(*child);
        }
        children.insert(site, index);
        self.nodes.push(Node {
            parent,
            ..Node::default()
        });
        Some(index)
    }

    fn close(&mut self, active: &mut Vec<Active<'a>>) {
        if let Some(frame) = active.pop()
            && let Some(node) = self.nodes.get_mut(frame.node)
        {
            let own_time = frame.span.elapsed.saturating_sub(frame.covered);
            node.own_time = node.own_time.saturating_add(own_time);
        }
    }

    fn sum_children(&mut self) {
        for index in (1..self.nodes.len()).rev() {
            let Some(node) = self.nodes.get_mut(index) else {
                continue;
            };
            node.total = node.total.saturating_add(node.own_time);
            let parent = node.parent;
            let total = node.total;
            if let Some(node) = self.nodes.get_mut(parent) {
                node.total = node.total.saturating_add(total);
            }
        }
    }

    /// Write folded stacks with self time in microseconds if they fit within `limit`.
    pub(super) fn folded(&self, limit: usize) -> Option<String> {
        let mut out = String::new();
        let mut stack = Vec::<String>::new();
        let mut pending = vec![(0, 0, None)];
        while let Some((index, depth, site)) = pending.pop() {
            let Some(node) = self.nodes.get(index) else {
                continue;
            };
            stack.truncate(depth);
            if let Some(site) = site {
                stack.push(folded_label(site));
            }
            let own_micros = node.own_time.as_micros();
            if own_micros > 0 {
                let time = own_micros.to_string();
                let line_bytes = stack.iter().fold(time.len() + 1, |len, label| {
                    len.saturating_add(label.len()).saturating_add(1)
                });
                if out.len().saturating_add(line_bytes) > limit {
                    return None;
                }
                let _ = writeln!(out, "{} {time}", stack.join(";"));
            }
            pending.extend(
                node.children
                    .iter()
                    .rev()
                    .map(|(site, &index)| (index, stack.len(), Some(*site))),
            );
        }
        Some(out)
    }

    /// Return the interactive HTML report with inline SVG, CSS, and JavaScript.
    pub(super) fn html(&self) -> String {
        let mut out = String::from(HTML_START);
        let bars = self.bars();
        if bars.is_empty() {
            out.push_str("<p>No recorded operation has a positive duration.</p>\n");
        } else {
            let height = bars.iter().map(|bar| bar.row + 1).max().unwrap_or(1) * ROW_HEIGHT;
            out.push_str(CONTROLS);
            let _ = writeln!(
                out,
                "<svg id=\"graph\" width=\"100%\" viewBox=\"0 0 {WIDTH} {height}\" role=\"group\" aria-label=\"Profile flamegraph\">"
            );
            for bar in bars {
                write_bar(&mut out, &bar);
            }
            out.push_str("</svg></div>\n");
            out.push_str(SCRIPT);
        }
        out.push_str("</main></body></html>\n");
        out
    }

    fn bars(&self) -> Vec<Bar<'_>> {
        let Some(root) = self.nodes.first() else {
            return Vec::new();
        };
        if root.total.is_zero() {
            return Vec::new();
        }
        let scale = WIDTH / root.total.as_secs_f64();
        let mut bars = Vec::new();
        let mut pending = Vec::new();
        self.push_bars(&mut pending, root, 0.0, 0, scale);
        while let Some(bar) = pending.pop() {
            self.push_bars(&mut pending, bar.node, bar.x, bar.row + 1, scale);
            bars.push(bar);
        }
        bars
    }

    fn push_bars<'graph>(
        &'graph self,
        pending: &mut Vec<Bar<'graph>>,
        node: &'graph Node<'a>,
        x: f64,
        row: usize,
        scale: f64,
    ) {
        let previous = pending.len();
        let mut x = x;
        for (site, &index) in &node.children {
            let Some(node) = self.nodes.get(index) else {
                continue;
            };
            let width = node.total.as_secs_f64() * scale;
            if width >= MIN_BAR_WIDTH {
                pending.push(Bar {
                    site,
                    node,
                    row,
                    x,
                    width,
                });
            }
            x += width;
        }
        if let Some(children) = pending.get_mut(previous..) {
            children.reverse();
        }
    }
}

fn contains(parent: &Span, child: &Span) -> bool {
    if parent.start > child.start || parent.end() < child.end() || parent.elapsed.is_zero() {
        return false;
    }
    match (parent.site.kind, child.site.kind) {
        (ProfileKind::Phase(parent_phase), ProfileKind::Phase(child_phase)) => {
            parent_phase == ProfilePhase::Eval
                && matches!(child_phase, ProfilePhase::Parse | ProfilePhase::Probes)
        }
        (ProfileKind::Phase(_), _) => true,
        (_, ProfileKind::Phase(ProfilePhase::Parse | ProfilePhase::Probes)) => {
            parent.start < child.start || parent.end() > child.end()
        }
        (_, ProfileKind::Phase(_)) => false,
        (ProfileKind::ListFile, kind) if kind.is_command() => parent.depth <= child.depth,
        (kind, ProfileKind::ListFile) if kind.is_command() => {
            parent.depth < child.depth
                || (parent.depth == child.depth
                    && (parent.start < child.start || parent.end() > child.end()))
        }
        _ => parent.depth < child.depth,
    }
}

fn short_label(site: &Site) -> &[u8] {
    if site.kind == ProfileKind::ListFile && !site.file.is_empty() {
        site.file.as_ref()
    } else {
        site.name.as_ref()
    }
}

fn detailed_label(site: &Site) -> String {
    let label = String::from_utf8_lossy(short_label(site));
    if site.kind.is_command() && !site.file.is_empty() {
        format!(
            "{label} ({}:{})",
            String::from_utf8_lossy(&site.file),
            site.line
        )
    } else {
        label.into_owned()
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

fn write_bar(out: &mut String, bar: &Bar<'_>) {
    let Bar {
        site,
        node,
        row,
        x,
        width,
    } = bar;
    let y = row * ROW_HEIGHT;
    let category = site.kind.category();
    let _ = write!(
        out,
        "<g class=\"frame {category}\" tabindex=\"0\" role=\"button\" data-row=\"{row}\" data-x=\"{x:.6}\" data-width=\"{width:.6}\" transform=\"translate({x:.6} {y})\"><title>"
    );
    escape(out, &detailed_label(site));
    let _ = write!(
        out,
        " | {:.3} ms total, {:.3} ms self</title><rect width=\"{width:.6}\" height=\"22\" rx=\"2\"/><svg width=\"{width:.6}\" height=\"22\" overflow=\"hidden\"><text x=\"5\" y=\"15\">",
        node.total.as_secs_f64() * 1000.0,
        node.own_time.as_secs_f64() * 1000.0
    );
    escape(out, &String::from_utf8_lossy(short_label(site)));
    out.push_str("</text></svg></g>\n");
}

fn escape(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            '\u{fffe}' | '\u{ffff}' => out.push('\u{fffd}'),
            ch if ch.is_control() => out.push('\u{fffd}'),
            ch => out.push(ch),
        }
    }
}

const HTML_START: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Rcmake profile</title>
<style>
:root { color-scheme: dark; font: 15px system-ui, sans-serif; background: #111820; color: #e7eef8; }
main { max-width: 1280px; margin: 0 auto; padding: 24px; }
h1 { font-size: 1.6rem; margin: 0 0 16px; }
.controls { display: flex; flex-wrap: wrap; align-items: center; gap: 12px; margin: 0 0 8px; }
input, button { font: inherit; color: inherit; background: #1c2834; border: 1px solid #64768a; border-radius: 5px; padding: 6px 10px; }
#graph-wrap { overflow: auto; }
#graph { display: block; min-width: 720px; }
.frame { cursor: pointer; }
.frame rect { fill: #afcee6; stroke: #111820; }
.phase rect { fill: #94cfac; }
.listfile rect { fill: #c3b9ed; }
.macro rect { fill: #f3c198; }
.function rect { fill: #ecd698; }
.frame text { fill: #151e28; font: 12px system-ui, sans-serif; pointer-events: none; }
.frame:hover rect, .frame:focus rect, .frame.match rect { stroke: white; stroke-width: 2px; }
.frame.dim { opacity: .32; }
.frame.hidden { display: none; }
.hint { color: #b1c1d3; margin: 8px 0 16px; }
</style></head><body><main><h1>Rcmake profile</h1>
"#;

const CONTROLS: &str = r#"<div class="controls">
<label>Search <input type="search" id="search"></label>
<button type="button" id="reset" disabled>Reset zoom</button>
<output id="matches" aria-live="polite"></output>
</div>
<p class="hint">Select a bar to zoom. Bar width shows total time. See trace.json for very short operations.</p>
<div id="graph-wrap">
"#;

const SCRIPT: &str = r#"<script>
(() => {
  const graph = document.getElementById('graph');
  const search = document.getElementById('search');
  const reset = document.getElementById('reset');
  const matches = document.getElementById('matches');
  const canvasWidth = graph.viewBox.baseVal.width;
  const frames = Array.from(graph.querySelectorAll('.frame'), (element) => ({
    element, row: Number(element.dataset.row), x: Number(element.dataset.x),
    width: Number(element.dataset.width), label: element.querySelector('title').textContent.toLowerCase(),
    rect: element.querySelector('rect'), text: element.querySelector('svg'),
  }));

  function highlight() {
    const query = search.value.trim().toLowerCase();
    let count = 0;
    for (const {element, label} of frames) {
      const match = query !== '' && label.includes(query);
      element.classList.toggle('match', match);
      element.classList.toggle('dim', query !== '' && !match);
      if (match && !element.classList.contains('hidden')) count += 1;
    }
    matches.textContent = query === '' ? '' : `${count} ${count === 1 ? 'match' : 'matches'}`;
  }

  function zoom(index) {
    const root = frames[index];
    const row = root?.row ?? 0;
    const x = root?.x ?? 0;
    const scale = canvasWidth / (root?.width ?? canvasWidth);
    const end = root ? frames.findIndex((frame, i) => i > index && frame.row <= row) : -1;
    let rows = 1;
    frames.forEach((frame, i) => {
      const visible = !root || (i >= index && (end < 0 || i < end));
      frame.element.classList.toggle('hidden', !visible);
      if (!visible) return;
      const level = frame.row - row;
      const width = frame.width * scale;
      frame.element.setAttribute('transform', `translate(${(frame.x - x) * scale} ${level * 24})`);
      frame.rect.setAttribute('width', width);
      frame.text.setAttribute('width', width);
      rows = Math.max(rows, level + 1);
    });
    graph.setAttribute('viewBox', `0 0 ${canvasWidth} ${rows * 24}`);
    reset.disabled = !root;
    highlight();
  }

  function select(event) {
    const bar = event.target.closest('.frame');
    if (!bar) return;
    event.preventDefault();
    zoom(frames.findIndex((frame) => frame.element === bar));
  }

  graph.addEventListener('click', select);
  graph.addEventListener('keydown', (event) => {
    if (event.key === 'Enter' || event.key === ' ') select(event);
  });
  search.addEventListener('input', highlight);
  reset.addEventListener('click', () => { zoom(-1); search.focus(); });
})();
</script>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn span(name: &[u8], start: u64, elapsed: u64, depth: u32) -> Span {
        Span {
            site: Site {
                name: name.into(),
                file: b"".as_slice().into(),
                line: 0,
                kind: ProfileKind::Builtin,
            },
            depth,
            start: Duration::from_micros(start),
            elapsed: Duration::from_micros(elapsed),
        }
    }

    #[test]
    fn folds_repeated_stack_paths_with_self_time_and_bytewise_order() {
        let root = span(b"root", 0, 80, 0);
        let later_root = span(b"root", 100, 20, 0);
        let z = span(b"z", 1, 10, 1);
        let a = span(b"a;\n%", 20, 15, 1);
        let later_a = span(b"a;\n%", 101, 5, 1);
        let graph = Graph::from_spans(&[&root, &z, &a, &later_root, &later_a]);
        assert_eq!(
            graph.folded(128).as_deref(),
            Some("root 70\nroot;a%3B%0A%25 20\nroot;z 10\n"),
        );
        assert_eq!(graph.folded(12), None);
    }

    #[test]
    fn escapes_labels_without_adding_elements_or_script() {
        let attack = span(b"</title><script>\"\x00'&\xff", 0, 80, 0);
        let graph = Graph::from_spans(&[&attack]);
        let html = graph.html();
        assert_eq!(html.matches("<script>").count(), 1);
        assert!(html.contains("&lt;/title&gt;&lt;script&gt;&quot;\u{fffd}&#39;&amp;\u{fffd}"));
        assert!(!html.contains("</title><script>"));
        assert!(!html.contains('\0'));
    }
}
