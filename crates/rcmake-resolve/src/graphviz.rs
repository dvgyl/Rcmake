//! Direct target relationships for Graphviz output.

use std::collections::{BTreeMap, BTreeSet};

use rcmake_base::{BString, list};
use rcmake_genex::{ConfigId, EvalContext, UsageTraversal};
use rcmake_model::{ProjectModel, TargetId, UsageField};

use crate::{ResolveError, usage::UsageResolver};

/// A target or an external link item.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Node {
    /// Target identity.
    Target(TargetId),
    /// External item spelling.
    External(BString),
}

/// The use of a direct dependency.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Implementation and interface or a build dependency.
    Public,
    /// Implementation only.
    Private,
    /// Interface only.
    Interface,
}

/// A target's direct dependencies.
#[derive(Default)]
pub struct Dependencies {
    /// Edges keyed by destination.
    pub edges: BTreeMap<Node, Scope>,
    /// Alias spellings in links, keyed by destination target.
    pub aliases: BTreeMap<TargetId, BTreeSet<BString>>,
}

/// Read a target's direct implementation, interface and build dependencies.
pub fn dependencies(
    model: &ProjectModel,
    usage: &mut UsageResolver<'_>,
    id: TargetId,
    configs: &[ConfigId],
) -> Result<Dependencies, ResolveError> {
    let mut result = Dependencies::default();
    let Some(target) = model.target(id) else {
        return Ok(result);
    };
    for config in configs {
        let mut context = EvalContext {
            config: config.clone(),
            head: Some(crate::usage::handle(id)),
            ..Default::default()
        };
        let (implementation, _) = usage.direct_links(id, &context)?;
        context.traversal = UsageTraversal::Compile;
        let (interface, _) = if target.kind == rcmake_model::TargetKind::Executable
            && !target
                .props
                .get(b"ENABLE_EXPORTS")
                .is_some_and(|value| rcmake_base::truth::is_on(value))
        {
            (std::sync::Arc::from([]), Default::default())
        } else {
            usage.graphviz_interface_links(id, &context)?
        };
        for (entries, scope) in [
            (implementation, Scope::Private),
            (interface, Scope::Interface),
        ] {
            for item in entries.iter() {
                let targets = item.targets().collect::<Vec<_>>();
                if targets.is_empty() {
                    result.add(Node::External(item.text.value.clone()), scope);
                }
                for dep in targets {
                    if let Some(real) = model.target(dep)
                        && real.name != item.text.value
                        && model.find_target(target.dir, &item.text.value) == Some(dep)
                    {
                        result
                            .aliases
                            .entry(dep)
                            .or_default()
                            .insert(item.text.value.clone());
                    }
                    result.add(Node::Target(dep), scope);
                }
            }
        }
    }
    for dep in &target.utility_deps {
        if let Some(id) = model.find_target(target.dir, &dep.value) {
            result.add(Node::Target(id), Scope::Public);
        }
    }
    for source in target
        .sources
        .iter()
        .chain(target.usage.entries(UsageField::InterfaceSources))
    {
        for value in list::split(&source.value, list::Empty::Drop) {
            if let Some(name) = value
                .strip_prefix(b"$<TARGET_OBJECTS:")
                .and_then(|name| name.strip_suffix(b">"))
                && let Some(id) = model.find_target(target.dir, name)
            {
                result.add(Node::Target(id), Scope::Public);
            }
        }
    }
    result.edges.remove(&Node::Target(id));
    Ok(result)
}

impl Dependencies {
    fn add(&mut self, node: Node, scope: Scope) {
        self.edges
            .entry(node)
            .and_modify(|old| {
                if *old != scope {
                    *old = Scope::Public;
                }
            })
            .or_insert(scope);
    }
}
