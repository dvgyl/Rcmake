use std::collections::HashSet;

use rcmake_genex::{EvalContext, UsageTraversal};
use rcmake_model::{BT, TargetId, UsageField};

use super::{
    InterfaceInclude, ResolveError, UsageResolver, UsageValue, context, definition, entry_origin,
    handle, on, truth, without_trailing_slashes,
};

impl UsageResolver<'_> {
    /// Collect includes in search order and mark system paths.
    pub fn collect_includes(
        &mut self,
        target: TargetId,
        ctx: &EvalContext,
    ) -> Result<Vec<InterfaceInclude>, ResolveError> {
        self.collect_includes_impl(target, ctx, false)
    }

    pub(crate) fn collect_includes_for_result(
        &mut self,
        target: TargetId,
        ctx: &EvalContext,
    ) -> Result<Vec<InterfaceInclude>, ResolveError> {
        self.collect_includes_impl(target, ctx, self.provenance.is_some())
    }

    fn collect_includes_impl(
        &mut self,
        target: TargetId,
        ctx: &EvalContext,
        record: bool,
    ) -> Result<Vec<InterfaceInclude>, ResolveError> {
        use UsageField::*;
        let (usage, chains) = self.collect_usage(target, IncludeDirectories, ctx, record)?;
        let mut ctx = context(target, ctx, UsageTraversal::Compile);
        let head = ctx.head.get_or_insert(handle(target));
        let no_imported_system = self
            .ids
            .get(head.0 as usize)
            .and_then(|id| self.model.target(*id))
            .is_some_and(|head| on(head, b"NO_SYSTEM_FROM_IMPORTED"));
        let owner = self.target(target)?;
        let mut system = UsageValue::default();
        self.eval_entries(
            target,
            b"SYSTEM_INCLUDE_DIRECTORIES",
            self.model
                .dir(owner.dir)
                .usage
                .entries(InterfaceSystemIncludeDirectories),
            &ctx,
            &mut system,
        )?;
        // A SYSTEM PUBLIC include has matching entries from the same command.
        let own_includes: HashSet<_> = owner
            .usage
            .entries(IncludeDirectories)
            .iter()
            .map(entry_origin)
            .collect();
        self.eval_entries(
            target,
            InterfaceSystemIncludeDirectories.name().as_bytes(),
            owner
                .usage
                .entries(InterfaceSystemIncludeDirectories)
                .iter()
                .filter(|entry| own_includes.contains(&entry_origin(entry))),
            &ctx,
            &mut system,
        )?;
        if let Some(raw) = owner.props.get(b"SYSTEM_INCLUDE_DIRECTORIES") {
            self.eval_entries(
                target,
                b"SYSTEM_INCLUDE_DIRECTORIES",
                &[BT::new(raw.clone(), owner.backtrace.clone())],
                &ctx,
                &mut system,
            )?;
        }

        let (links, _) = self.usage_links(target, false, &ctx)?;
        let mut todo: Vec<_> = links.iter().flat_map(|item| item.edge_targets()).collect();
        let mut visited = HashSet::new();
        while let Some(id) = todo.pop() {
            if !visited.insert(id) {
                continue;
            }
            let dependency = self.target(id)?;
            let default_system = dependency
                .props
                .get(b"SYSTEM")
                .map_or(dependency.is_imported(), |value| truth::is_on(value));
            self.eval_entries(
                id,
                InterfaceSystemIncludeDirectories.name().as_bytes(),
                dependency.usage.entries(InterfaceSystemIncludeDirectories),
                &ctx,
                &mut system,
            )?;
            if default_system && !(dependency.is_imported() && no_imported_system) {
                self.eval_entries(
                    id,
                    InterfaceIncludeDirectories.name().as_bytes(),
                    dependency.usage.entries(InterfaceIncludeDirectories),
                    &ctx,
                    &mut system,
                )?;
            }
            let (links, _) = self.usage_links(id, true, &ctx)?;
            todo.extend(links.iter().flat_map(|item| item.edge_targets()));
        }

        let system: HashSet<_> = system
            .entries
            .iter()
            .map(|entry| without_trailing_slashes(&entry.value))
            .collect();
        let directory = self.model.dir(owner.dir);
        let current_dirs = definition(self.model, owner.dir, b"CMAKE_INCLUDE_CURRENT_DIR")
            .filter(|value| truth::is_on(value))
            .into_iter()
            .flat_map(|_| [&directory.binary_dir, &directory.source_dir])
            .filter(|path| !path.is_empty())
            .map(|path| (BT::new(path.clone(), Default::default()), Vec::new()));
        let mut seen = HashSet::new();
        let mut includes: Vec<_> = current_dirs
            .chain(
                usage.entries.into_iter().zip(
                    chains
                        .into_iter()
                        .flatten()
                        .chain(std::iter::repeat_with(Vec::new)),
                ),
            )
            .map(|(mut path, hops)| {
                let len = without_trailing_slashes(&path.value).len();
                path.value.truncate(len);
                (path, hops)
            })
            .filter(|(path, _)| seen.insert(path.value.clone()))
            .map(|(path, hops)| {
                (
                    InterfaceInclude {
                        system: system.contains(path.value.as_slice()),
                        path,
                    },
                    hops,
                )
            })
            .collect();
        includes.sort_by_key(|(include, _)| include.system);
        if record && let Some(provenance) = self.provenance.as_mut() {
            for (include, hops) in &includes {
                provenance.record(
                    target,
                    &ctx.config,
                    crate::ChainKind::Usage(IncludeDirectories),
                    &include.path,
                    hops.clone(),
                    Vec::new(),
                );
            }
        }
        Ok(includes.into_iter().map(|(include, _)| include).collect())
    }
}

pub(crate) fn finalize_current_dir_interfaces(
    model: &rcmake_model::ProjectModel,
) -> Option<rcmake_model::ProjectModel> {
    use rcmake_model::TargetKind;

    let mut targets = model
        .targets()
        .filter(|(_, target)| {
            !target.is_imported()
                && (matches!(
                    target.kind,
                    TargetKind::SharedLibrary
                        | TargetKind::StaticLibrary
                        | TargetKind::ModuleLibrary
                        | TargetKind::InterfaceLibrary
                ) || target.kind == TargetKind::Executable
                    && target
                        .props
                        .get(b"ENABLE_EXPORTS")
                        .is_some_and(|value| truth::is_on(value)))
                && definition(model, target.dir, b"CMAKE_INCLUDE_CURRENT_DIR_IN_INTERFACE")
                    .is_some_and(|value| truth::is_on(value))
        })
        .peekable();
    targets.peek()?;
    let mut result = model.clone();
    for (id, target) in targets {
        let dir = model.dir(target.dir);
        let includes = [
            b"$<BUILD_INTERFACE:".as_slice(),
            &dir.binary_dir,
            b";",
            &dir.source_dir,
            b">",
        ]
        .concat();
        if let Some(target) = result.target_mut(id) {
            target.usage.push(
                UsageField::InterfaceIncludeDirectories,
                BT::new(includes.into(), Default::default()),
            );
        }
    }
    Some(result)
}
