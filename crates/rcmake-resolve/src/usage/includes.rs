use std::collections::HashSet;

use rcmake_genex::{EvalContext, UsageTraversal};
use rcmake_model::{BT, ProjectModel, TargetId, UsageField};

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

        if self.has_system_interface_includes {
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
                let sys_entries = dependency.usage.entries(InterfaceSystemIncludeDirectories);
                if !sys_entries.is_empty() {
                    self.eval_entries(
                        id,
                        InterfaceSystemIncludeDirectories.name().as_bytes(),
                        sys_entries,
                        &ctx,
                        &mut system,
                    )?;
                }
                if default_system && !(dependency.is_imported() && no_imported_system) {
                    let iface_entries = dependency.usage.entries(InterfaceIncludeDirectories);
                    if !iface_entries.is_empty() {
                        self.eval_entries(
                            id,
                            InterfaceIncludeDirectories.name().as_bytes(),
                            iface_entries,
                            &ctx,
                            &mut system,
                        )?;
                    }
                }
                let (links, _) = self.usage_links(id, true, &ctx)?;
                todo.extend(links.iter().flat_map(|item| item.edge_targets()));
            }
        }

        let has_system = !system.entries.is_empty();
        let system: HashSet<_> = if has_system {
            system
                .entries
                .iter()
                .map(|entry| without_trailing_slashes(&entry.value))
                .collect()
        } else {
            HashSet::new()
        };
        let directory = self.model.dir(owner.dir);
        let current_dirs = definition(self.model, owner.dir, b"CMAKE_INCLUDE_CURRENT_DIR")
            .filter(|value| truth::is_on(value))
            .into_iter()
            .flat_map(|_| [&directory.binary_dir, &directory.source_dir])
            .filter(|path| !path.is_empty())
            .map(|path| (BT::new(path.clone(), Default::default()), Vec::new()));
        // CMake automatically adds Swift module output directories of linked Swift
        // dependencies as include search paths (`cmGeneratorTarget_IncludeDirectories.cxx:96-139`).
        let mut swift_module_dirs = Vec::new();
        if ctx.language.as_deref().map(Vec::as_slice) == Some(b"Swift") {
            let (direct, _) = self.usage_links(target, false, &ctx)?;
            let mut visited_direct = HashSet::from([target]);
            for dep_id in direct.iter().flat_map(|item| item.edge_targets()) {
                if !visited_direct.insert(dep_id) {
                    continue;
                }
                let dep = self.target(dep_id)?;
                if dep.is_imported() {
                    continue;
                }
                if !self
                    .target_compile_languages(dep_id, &ctx.config)?
                    .contains(b"Swift".as_slice())
                {
                    continue;
                }
                let dep = self.target(dep_id)?;
                let mod_dir =
                    crate::lower::swift::module_directory(self.model, dep, &ctx.config, self)?;
                if !mod_dir.is_empty() {
                    swift_module_dirs.push((BT::new(mod_dir, Default::default()), Vec::new()));
                }
                let mut seen_iface = HashSet::from([target, dep_id]);
                self.collect_swift_interface_module_dirs(
                    dep_id,
                    &ctx,
                    &mut seen_iface,
                    &mut swift_module_dirs,
                )?;
            }
        }
        let mut standard_includes = Vec::new();
        if let Some(lang) = ctx.language.as_deref() {
            let key = [
                b"CMAKE_".as_slice(),
                lang.as_slice(),
                b"_STANDARD_INCLUDE_DIRECTORIES",
            ]
            .concat();
            if let Some(raw) = definition(self.model, owner.dir, &key) {
                for dir in rcmake_base::list::split(raw, rcmake_base::list::Empty::Drop) {
                    if !dir.is_empty() {
                        standard_includes.push((BT::new(dir, Default::default()), Vec::new()));
                    }
                }
            }
        }
        let has_standard = !standard_includes.is_empty();
        let mut seen = HashSet::new();
        let mut includes: Vec<_> = current_dirs
            .chain(swift_module_dirs)
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
                (path, hops, false)
            })
            .chain(standard_includes.into_iter().map(|(mut path, hops)| {
                let len = without_trailing_slashes(&path.value).len();
                path.value.truncate(len);
                (path, hops, true)
            }))
            .filter(|(path, _, _)| seen.insert(path.value.clone()))
            .map(|(path, hops, standard_sys)| {
                (
                    InterfaceInclude {
                        system: standard_sys
                            || (has_system && system.contains(path.value.as_slice())),
                        path,
                    },
                    hops,
                )
            })
            .collect();
        if has_system || has_standard {
            includes.sort_by_key(|(include, _)| include.system);
        }
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

    fn collect_swift_interface_module_dirs(
        &mut self,
        owner: TargetId,
        ctx: &EvalContext,
        seen: &mut HashSet<TargetId>,
        out: &mut Vec<(BT<rcmake_base::BString>, Vec<crate::Hop>)>,
    ) -> Result<(), ResolveError> {
        let (iface_links, _) = self.usage_links(owner, true, ctx)?;
        for dep_id in iface_links.iter().flat_map(|item| item.edge_targets()) {
            if !seen.insert(dep_id) {
                continue;
            }
            let dep = self.target(dep_id)?;
            if !dep.is_imported()
                && self
                    .target_compile_languages(dep_id, &ctx.config)?
                    .contains(b"Swift".as_slice())
            {
                let dep = self.target(dep_id)?;
                // Mirrors `AddLangSpecificInterfaceIncludeDirectories` (`cmGeneratorTarget_IncludeDirectories.cxx:76`).
                let raw_dir = dep
                    .props
                    .get(b"Swift_MODULE_DIRECTORY")
                    .filter(|value| !value.is_empty())
                    .cloned()
                    .unwrap_or_else(|| self.model.dir(dep.dir).binary_dir.clone());
                if !raw_dir.is_empty() {
                    out.push((BT::new(raw_dir, Default::default()), Vec::new()));
                }
            }
            self.collect_swift_interface_module_dirs(dep_id, ctx, seen, out)?;
        }
        Ok(())
    }
}

pub(crate) fn pending_current_dir_interfaces(
    model: &ProjectModel,
) -> impl Iterator<Item = (TargetId, BT<rcmake_base::BString>)> + '_ {
    use rcmake_model::TargetKind;

    model
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
        .filter_map(|(id, target)| {
            let dir = model.dir(target.dir);
            let includes = [
                b"$<BUILD_INTERFACE:".as_slice(),
                &dir.binary_dir,
                b";",
                &dir.source_dir,
                b">",
            ]
            .concat();
            let entry = BT::new(includes.into(), Default::default());
            let entries = target
                .usage
                .entries(UsageField::InterfaceIncludeDirectories);
            (!entries.contains(&entry)).then_some((id, entry))
        })
}

pub(crate) fn append_current_dir_interfaces(
    model: &mut ProjectModel,
    entries: impl IntoIterator<Item = (TargetId, BT<rcmake_base::BString>)>,
) {
    for (id, entry) in entries {
        if let Some(target) = model.target_mut(id) {
            target
                .usage
                .push(UsageField::InterfaceIncludeDirectories, entry);
        }
    }
}

/// Add the current build and source directories to eligible target interfaces.
/// Call this after configuration and before evaluating generation expressions.
pub fn finalize_current_dir_interfaces(model: &mut ProjectModel) {
    let entries: Vec<_> = pending_current_dir_interfaces(model).collect();
    append_current_dir_interfaces(model, entries);
}
