//! Evaluate entries and check their paths.

use std::sync::Arc;

use rcmake_base::{BString, list, path_lex::Lexical};
use rcmake_diag::{Location, MessageKind};
use rcmake_genex::{
    EvalContext, EvalFlags, EvalSite, GenexError, GenexNode, UsageTraversal, evaluate,
};
use rcmake_model::{BT, DirId, TargetId};

use super::paths::{PathProblem, interface_path_problem, usage_path_is_full, windows_path_rules};
use super::{UsageResolver, UsageValue, context, handle};
use crate::ResolveError;

impl UsageResolver<'_> {
    pub(super) fn eval_entries<'entry>(
        &mut self,
        target: TargetId,
        property: &[u8],
        entries: impl IntoIterator<Item = &'entry BT<BString>>,
        ctx: &EvalContext,
        result: &mut UsageValue,
    ) -> Result<(), ResolveError> {
        let dir = self.target(target)?.dir;
        let link_ctx;
        let ctx = if matches!(
            property,
            b"LINK_OPTIONS" | b"INTERFACE_LINK_OPTIONS" | b"STATIC_LIBRARY_OPTIONS"
        ) && ctx.traversal != UsageTraversal::Link
        {
            link_ctx = EvalContext {
                traversal: UsageTraversal::Link,
                ..ctx.clone()
            };
            &link_ctx
        } else {
            ctx
        };
        for entry in entries {
            let (value, flags) = self.eval_entry(target, dir, property, entry, ctx, false)?;
            result.flags = result.flags.union(flags);
            let values = list::split(&value, list::Empty::Drop)
                .into_iter()
                .map(|value| self.usage_path(dir, property, value))
                .collect::<Vec<_>>();
            if matches!(
                property,
                b"INTERFACE_INCLUDE_DIRECTORIES" | b"INTERFACE_SOURCES"
            ) && self.property_reads == 0
                && let Some(paths) = self.source_paths()
                && let Some(problem) = interface_path_problem(
                    paths,
                    self.target(target)?,
                    property,
                    &values,
                    property != b"INTERFACE_SOURCES"
                        && windows_path_rules(self.model, dir, Some(paths)),
                )
            {
                result.flags.head_sensitive = true;
                if let Some(head) = ctx.head.filter(|head| *head != handle(target))
                    && let Some(consumer) = self
                        .ids
                        .get(head.0 as usize)
                        .and_then(|id| self.model.target(*id))
                {
                    let name = &self.target(target)?.name;
                    let property = BString::from(property);
                    let body: BString = match problem {
                        PathProblem::Missing(path) => format!(
                            "Imported target \"{name}\" includes non-existent path\n  \"{path}\"\nin its INTERFACE_INCLUDE_DIRECTORIES. Possible reasons include:\n* The path was deleted, renamed, or moved to another location.\n* An install or uninstall procedure did not complete successfully.\n* The installation package was faulty and references files it does not provide.\n"
                        ),
                        PathProblem::Relative(path) => format!(
                            "Target \"{name}\" contains relative path in its {property}:\n  \"{path}\""
                        ),
                    }
                    .into();
                    let file = Lexical::new(&self.model.dir(consumer.dir).source_dir)
                        .append(b"CMakeLists.txt");
                    paths.emit(
                        self.model,
                        MessageKind::Error,
                        Location::FileLine {
                            file: file.as_slice().into(),
                            line: 0,
                        },
                        body.clone(),
                    );
                    return Err(ResolveError::Reported(body));
                }
            }
            if property == b"INCLUDE_DIRECTORIES"
                && self.property_reads == 0
                && ctx.head.is_none_or(|head| head == handle(target))
                && let Some(paths) = self.source_paths()
                && let Some(path) = values
                    .iter()
                    .find(|value| !usage_path_is_full(self.model, paths, dir, value))
            {
                let owner = self.target(target)?;
                let body: BString = format!(
                    "Found relative path while evaluating include directories of \"{}\":\n  \"{path}\"\n",
                    owner.name
                )
                .into();
                let file =
                    Lexical::new(&self.model.dir(owner.dir).source_dir).append(b"CMakeLists.txt");
                paths.emit(
                    self.model,
                    MessageKind::Error,
                    Location::FileLine {
                        file: file.as_slice().into(),
                        line: 0,
                    },
                    body.clone(),
                );
                return Err(ResolveError::Reported(body));
            }
            result.entries.extend(
                values
                    .into_iter()
                    .map(|value| BT::new(value, entry.backtrace.clone())),
            );
        }
        Ok(())
    }

    pub(super) fn eval_entry(
        &mut self,
        target: TargetId,
        dir: DirId,
        property: &[u8],
        entry: &BT<BString>,
        ctx: &EvalContext,
        link_edges: bool,
    ) -> Result<(BString, EvalFlags), ResolveError> {
        if !entry.value.windows(2).any(|window| window == b"$<") {
            return Ok((entry.value.clone(), EvalFlags::default()));
        }
        let mut ctx = context(target, ctx, ctx.traversal);
        ctx.site = EvalSite {
            backtrace: entry.backtrace.clone(),
            property: Some(property.into()),
        };
        let mut ast = self.parse_genex(&entry.value)?;
        let may_rewrite = |name: &[u8]| entry.value.windows(name.len()).any(|value| value == name);
        let rewrite = match ctx.traversal {
            UsageTraversal::Compile => {
                may_rewrite(b"LINK_ONLY")
                    || (link_edges && (may_rewrite(b"LINK_LIBRARY") || may_rewrite(b"LINK_GROUP")))
            }
            UsageTraversal::Link => link_edges && may_rewrite(b"COMPILE_ONLY"),
        };
        if rewrite {
            prepare_nodes(
                &mut Arc::make_mut(&mut ast).nodes,
                ctx.traversal,
                link_edges,
            );
        }
        let root = &self.model.dir(DirId::ROOT).source_dir;
        let mut host = self.host(dir);
        host.current = ctx.current;
        evaluate(&ast, &ctx, &mut host).map_err(|error| {
            if matches!(property, b"LINK_DEPENDS" | b"INTERFACE_LINK_DEPENDS") {
                return error.without_location().into();
            }
            // CMake reports a sources cycle at the call that created the target.
            let backtrace = match &error {
                GenexError::SourcesCycle { target } => self
                    .model
                    .find_target(dir, target)
                    .and_then(|id| self.model.target(id))
                    .map_or(&entry.backtrace, |target| &target.backtrace),
                _ => &entry.backtrace,
            };
            error.at(backtrace, root).into()
        })
    }
}

fn prepare_nodes(nodes: &mut Vec<GenexNode>, traversal: UsageTraversal, link_edges: bool) {
    let skipped = match traversal {
        UsageTraversal::Compile => b"LINK_ONLY".as_slice(),
        UsageTraversal::Link => b"COMPILE_ONLY".as_slice(),
    };
    let mut expanded = Vec::with_capacity(nodes.len());
    for mut node in std::mem::take(nodes) {
        if let GenexNode::Expr(expr) = &mut node {
            if let [GenexNode::Text(name)] = expr.identifier.as_slice() {
                if name == skipped && expr.params.as_ref().is_some_and(|args| args.len() == 1) {
                    continue;
                }
                // The libraries inside a link feature also supply compile usage.
                if link_edges
                    && traversal == UsageTraversal::Compile
                    && matches!(name.as_slice(), b"LINK_LIBRARY" | b"LINK_GROUP")
                    && let Some(params) = &mut expr.params
                    && params.len() > 1
                {
                    for (index, mut param) in params.drain(1..).enumerate() {
                        if index > 0 {
                            expanded.push(GenexNode::Text(";".into()));
                        }
                        prepare_nodes(&mut param, traversal, link_edges);
                        expanded.extend(param);
                    }
                    continue;
                }
            }
            prepare_nodes(&mut expr.identifier, traversal, link_edges);
            if let Some(params) = &mut expr.params {
                for param in params {
                    prepare_nodes(param, traversal, link_edges);
                }
            }
        }
        expanded.push(node);
    }
    *nodes = expanded;
}
