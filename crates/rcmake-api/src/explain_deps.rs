//! Bounded enumeration of link and order-only paths for `rcmake explain --dep`.
//!
//! The query walks resolved edges per configuration. It does not use the resolver's
//! provenance chains, because those keep only the first chain for each link item.

use std::collections::{BTreeSet, HashSet};

use rcmake_base::BString;
use rcmake_diag::{
    Backtrace,
    suggest::{Namespace, suggest_candidates_in},
};
use rcmake_genex::{ConfigId, EvalContext, UsageTraversal};
use rcmake_model::{DirId, TargetId, TargetKind, UsageScope};
use rcmake_resolve::usage::UsageResolver;

use crate::ConfigureResult;

const MAX_PATHS: usize = 16;
const MAX_DEPTH: usize = 32;
const MAX_VISITS: usize = 20_000;

/// Kind of one dependency edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DependencyEdge {
    /// Link edge with the scope of the original entry.
    Link {
        /// Scope of the edge.
        scope: UsageScope,
        /// The edge exists only for linking, for example through `$<LINK_ONLY:...>`.
        link_only: bool,
    },
    /// Build-order edge from `add_dependencies()` or a utility item.
    OrderOnly,
}

impl DependencyEdge {
    fn key(self) -> u8 {
        match self {
            Self::Link { scope, link_only } => {
                let scope = match scope {
                    UsageScope::Private => 0,
                    UsageScope::Public => 1,
                    UsageScope::Interface => 2,
                };
                scope | if link_only { 4 } else { 0 }
            }
            Self::OrderOnly => 8,
        }
    }
}

/// One hop of a dependency path.
#[derive(Clone, Debug)]
pub struct DependencyHop {
    /// Target at the start of the edge.
    pub from: BString,
    /// Target at the end of the edge.
    pub to: BString,
    /// Identifier of the target at the end of the edge.
    pub id: TargetId,
    /// Edge kind.
    pub edge: DependencyEdge,
    /// Call stack that created the edge.
    pub origin: Backtrace,
}

/// One path from the explained target to the requested dependency.
#[derive(Clone, Debug)]
pub struct DependencyPath {
    /// Configuration of the path. Empty for the default configuration.
    pub config: BString,
    /// Hops in order.
    pub hops: Vec<DependencyHop>,
    /// A hop is link-only.
    pub link_only: bool,
    /// A hop is an order-only edge.
    pub order_only: bool,
}

/// Result of a dependency query.
#[derive(Clone, Debug, Default)]
pub struct DependencyReport {
    /// Requested target name.
    pub target: BString,
    /// Name of the real target, when the name resolves.
    pub resolved: Option<BString>,
    /// Distinct paths, shortest first.
    pub paths: Vec<DependencyPath>,
    /// The path, depth, or visit limit stopped the walk.
    pub truncated: bool,
    /// Number of edges skipped because they close a cycle on the current path.
    pub cycles: u32,
    /// Order-only edges were available. They need a completed generation.
    pub order_only_available: bool,
    /// Resolver errors that left part of the graph unknown.
    pub errors: Vec<BString>,
    /// Similar target names when the requested name does not resolve.
    pub suggestions: Vec<BString>,
}

impl DependencyReport {
    /// Return whether every requested edge kind was available and the walk finished.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.order_only_available && !self.truncated && self.errors.is_empty()
    }
}

struct Edge {
    to: TargetId,
    edge: DependencyEdge,
    origin: Backtrace,
}

struct Walk<'r, 'm> {
    result: &'m ConfigureResult,
    resolver: &'r mut UsageResolver<'m>,
    config: BString,
    goal: TargetId,
    visits: usize,
    found: Vec<DependencyPath>,
    report: DependencyReport,
}

impl ConfigureResult {
    /// Enumerate link and order-only paths from `head` to the target `wanted`.
    #[must_use]
    pub fn dependency_paths(
        &self,
        dir: DirId,
        head: TargetId,
        wanted: &[u8],
        configs: &[BString],
    ) -> DependencyReport {
        let mut report = DependencyReport {
            target: wanted.into(),
            order_only_available: self.generation.is_some(),
            ..DependencyReport::default()
        };
        let Some(goal) = self.model.find_target(dir, wanted) else {
            report.suggestions = suggest_candidates_in(
                Namespace::Query,
                wanted,
                self.model.targets().map(|(_, target)| target.name.clone()),
            )
            .into_iter()
            .take(3)
            .map(|(name, _)| name)
            .collect();
            return report;
        };
        report.resolved = self.model.target(goal).map(|target| target.name.clone());
        let configs: Vec<BString> = if configs.is_empty() {
            vec![BString::default()]
        } else {
            configs.to_vec()
        };
        let mut resolver = UsageResolver::new(&self.model);
        let mut paths = Vec::new();
        for config in configs {
            let mut walk = Walk {
                result: self,
                resolver: &mut resolver,
                config,
                goal,
                visits: 0,
                found: Vec::new(),
                report: std::mem::take(&mut report),
            };
            let mut on_path = HashSet::from([head]);
            let mut hops = Vec::new();
            walk.visit(head, true, &mut on_path, &mut hops);
            paths.append(&mut walk.found);
            report = walk.report;
        }
        paths.sort_by_key(|path| path.hops.len());
        let mut seen = BTreeSet::new();
        paths.retain(|path| {
            seen.insert((
                path.config.clone(),
                path.hops
                    .iter()
                    .map(|hop| (hop.id, hop.edge.key()))
                    .collect::<Vec<_>>(),
            ))
        });
        if paths.len() > MAX_PATHS {
            paths.truncate(MAX_PATHS);
            report.truncated = true;
        }
        report.paths = paths;
        report
    }
}

impl Walk<'_, '_> {
    fn name(&self, id: TargetId) -> BString {
        self.result.model.target(id).map_or_else(
            || format!("<target {}>", id.index()).into(),
            |target| target.name.clone(),
        )
    }

    fn visit(
        &mut self,
        node: TargetId,
        builds: bool,
        on_path: &mut HashSet<TargetId>,
        hops: &mut Vec<DependencyHop>,
    ) {
        if hops.len() >= MAX_DEPTH || self.visits >= MAX_VISITS || self.found.len() > 4 * MAX_PATHS
        {
            self.report.truncated = true;
            return;
        }
        self.visits += 1;
        for edge in self.edges(node, builds) {
            if !on_path.insert(edge.to) {
                self.report.cycles = self.report.cycles.saturating_add(1);
                continue;
            }
            hops.push(DependencyHop {
                from: self.name(node),
                to: self.name(edge.to),
                id: edge.to,
                edge: edge.edge,
                origin: edge.origin,
            });
            if edge.to == self.goal {
                self.found.push(DependencyPath {
                    config: self.config.clone(),
                    link_only: hops.iter().any(|hop| {
                        matches!(
                            hop.edge,
                            DependencyEdge::Link {
                                link_only: true,
                                ..
                            }
                        )
                    }),
                    order_only: hops.iter().any(|hop| hop.edge == DependencyEdge::OrderOnly),
                    hops: hops.clone(),
                });
            } else {
                let next_builds = edge.edge == DependencyEdge::OrderOnly;
                self.visit(edge.to, next_builds, on_path, hops);
            }
            hops.pop();
            on_path.remove(&edge.to);
        }
    }

    /// Edges out of `node`. A node that builds on its own uses its link
    /// implementation. A node reached through a link edge uses its link interface.
    fn edges(&mut self, node: TargetId, builds: bool) -> Vec<Edge> {
        let ctx = |traversal| EvalContext {
            config: ConfigId(self.config.clone()),
            traversal,
            quiet: true,
            ..EvalContext::default()
        };
        let mut edges = Vec::new();
        let usage_only = self.result.model.target(node).is_none_or(|target| {
            target.kind == TargetKind::InterfaceLibrary || target.is_imported()
        });
        let interface = !builds || usage_only;
        let link = self
            .resolver
            .direct_link_items(node, interface, &ctx(UsageTraversal::Link));
        let compile =
            self.resolver
                .direct_link_items(node, interface, &ctx(UsageTraversal::Compile));
        match (link, compile) {
            (Ok(link), Ok(compile)) => {
                let compile: HashSet<TargetId> =
                    compile.iter().flat_map(|item| item.targets()).collect();
                for item in link.iter() {
                    for to in item.targets() {
                        edges.push(Edge {
                            to,
                            edge: DependencyEdge::Link {
                                scope: item.scope,
                                link_only: !compile.contains(&to),
                            },
                            origin: item.text.backtrace.clone(),
                        });
                    }
                }
            }
            (Err(error), _) | (_, Err(error)) => {
                if self.report.errors.len() < 8 {
                    self.report.errors.push(error.to_string().into());
                }
            }
        }
        if let Some(generation) = &self.result.generation {
            for item in generation.target_depends.utility_items(node) {
                edges.push(Edge {
                    to: item.target,
                    edge: DependencyEdge::OrderOnly,
                    origin: item.backtrace.clone(),
                });
            }
        }
        edges
    }
}

#[cfg(test)]
#[path = "explain_deps_tests.rs"]
mod tests;
