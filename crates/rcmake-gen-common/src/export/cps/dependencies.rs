use rcmake_cps::{CpsPackage, CpsUsage};
use rcmake_genex::{EvalContext, PreprocessMode, TargetHandle, UsageTraversal};
use rcmake_model::{PackageInfoSpec, ProjectModel, Target, TargetId, TargetKind};
use rcmake_resolve::{GenerationResult, ResolvedTarget, usage::UsageResolver};

use super::{
    BString, GenError, component, export_name, exports, generation_error, genex, imported_package,
    imported_version, list, package_variable, push_unique, unknown_import,
};

pub(super) struct Dependency<'a> {
    package: Option<BString>,
    component: BString,
    imported: Option<&'a Target>,
}

impl Dependency<'_> {
    pub(super) fn record(
        self,
        model: &ProjectModel,
        package: &mut CpsPackage,
    ) -> Result<BString, GenError> {
        let Some(name) = self.package else {
            return Ok([b":", self.component.as_slice()].concat().into());
        };
        let pkg_config = self.imported.is_some_and(|target| {
            target
                .props
                .get(b"_RCMAKE_PKG_CONFIG_PACKAGE")
                .is_some_and(|name| !name.is_empty())
        });
        let requirement = match package.requires.entry(name.clone()) {
            std::collections::btree_map::Entry::Occupied(entry) => {
                let existing_pkg_config = !entry.get().domains.is_empty();
                if existing_pkg_config != pkg_config {
                    return Err(generation_error([format!(
                        r#"Package "{}" requires package "{name}" in multiple domain sets.  This is not supported."#,
                        package.name,
                    ).into()]));
                }
                entry.into_mut()
            }
            std::collections::btree_map::Entry::Vacant(entry) => {
                let requirement = entry.insert(Default::default());
                if pkg_config {
                    requirement.domains.push("pkg-config".into());
                } else if let Some(target) = self.imported {
                    requirement.version = imported_version(model, target, &name).cloned();
                    if let Some(dir) = package_variable(model, target.dir, &name, b"_DIR") {
                        requirement.hints.push(dir.clone());
                        requirement.version.get_or_insert_with(BString::default);
                    }
                }
                requirement
            }
        };
        push_unique(&mut requirement.components, self.component.clone());
        Ok([name.as_slice(), b":", &self.component].concat().into())
    }
}

pub(super) struct Dependencies<'a> {
    pub(super) model: &'a ProjectModel,
    pub(super) result: &'a GenerationResult,
    pub(super) spec: &'a PackageInfoSpec,
    pub(super) mode: PreprocessMode,
}

impl<'a> Dependencies<'a> {
    pub(super) fn name(
        &self,
        id: TargetId,
        consumer: &Target,
    ) -> Result<Dependency<'a>, Vec<BString>> {
        let Self {
            model,
            result,
            spec,
            mode,
        } = *self;
        let target = model
            .target(id)
            .ok_or_else(|| vec!["Unknown dependency target.".into()])?;
        let contains = |set: &[u8]| {
            model
                .exports
                .get(set)
                .is_some_and(|set| set.targets.contains(&id))
        };
        if contains(&spec.export) {
            return Ok(Dependency {
                package: None,
                component: export_name(target),
                imported: None,
            });
        }
        if target.is_imported() {
            if let Some(name) = target
                .props
                .get(b"_RCMAKE_PKG_CONFIG_PACKAGE")
                .filter(|name| !name.is_empty())
            {
                return Ok(Dependency {
                    package: Some(name.clone()),
                    component: name.clone(),
                    imported: Some(target),
                });
            }
            let package = imported_package(model, target)
                .ok_or_else(|| vec![unknown_import(consumer, target)])?;
            let namespace = [package, b"::"].concat();
            if !target.name.starts_with(&namespace) {
                return Err(vec![format!(
                r#"Target "{}" references target "{}", which comes from the "{}" package, but does not belong to the package's canonical namespace ("{}").  This is not allowed."#,
                consumer.name, target.name, BString::from(package), BString::from(namespace),
            ).into()]);
            }
            return Ok(Dependency {
                package: Some(package.into()),
                component: component(target, &namespace),
                imported: Some(target),
            });
        }
        let candidates =
            exports(model, result, id, mode).map_err(|error| vec![error.to_string().into()])?;
        let export_message = |kind: &str, name: &[u8]| {
            if mode == PreprocessMode::Install {
                format!(
                    r#"install({kind} "{}" ...) includes target "{}" which requires target "{}""#,
                    BString::from(name),
                    consumer.name,
                    target.name,
                )
            } else {
                format!(
                    r#"export called with target "{}" which requires target "{}""#,
                    consumer.name, target.name
                )
            }
        };
        let subject = export_message("PACKAGE_INFO", &spec.package);
        let Some(first) = candidates.first() else {
            return Err(vec![
                format!("{subject} that is not in any export set.").into(),
            ]);
        };
        let multiple_sets = candidates.iter().any(|other| other.set != first.set);
        if multiple_sets
            || candidates
                .iter()
                .any(|other| other.namespace != first.namespace)
        {
            let files = candidates
                .iter()
                .map(|entry| entry.file.as_slice())
                .collect::<Vec<_>>()
                .join(b", ".as_slice());
            let reason = if multiple_sets {
                "in multiple other export sets"
            } else {
                "in another export set which is exported multiple times with different namespaces"
            };
            let details = format!(
                "that is not in this export set, but {reason}: {}.\n\nAn exported target cannot depend upon another target which is exported in more than one export set or with more than one namespace.  Consider consolidating the exports of the \"{}\" target to a single export.",
                BString::from(files),
                target.name,
            );
            let mut messages = Vec::new();
            for export in model
                .find_target(consumer.dir, &consumer.name)
                .map(|id| exports(model, result, id, mode))
                .transpose()
                .map_err(|error| vec![error.to_string().into()])?
                .into_iter()
                .flatten()
            {
                if let Some(set) = export.set.filter(|_| export.file.ends_with(b".cmake")) {
                    messages.push(format!("{} {details}", export_message("EXPORT", &set)).into());
                }
            }
            messages.push(format!("{subject} {details}").into());
            return Err(messages);
        }
        let Some(package) = first.namespace.strip_suffix(b"::") else {
            return Err(vec![format!(
            r#"Target "{}" references target "{}{}", which does not use the standard namespace separator.  This is not allowed."#,
            consumer.name, first.namespace, export_name(target),
        ).into()]);
        };
        Ok(Dependency {
            package: (package != spec.package).then(|| package.into()),
            component: export_name(target),
            imported: None,
        })
    }
}

pub(super) fn runtime_dependencies(
    dependencies: &Dependencies<'_>,
    resolver: &mut UsageResolver<'_>,
    facts: &ResolvedTarget,
    target: &Target,
    package: &mut CpsPackage,
    common: &CpsUsage,
    config: &mut CpsUsage,
) -> Result<(), GenError> {
    let Dependencies { model, mode, .. } = *dependencies;
    let handle = TargetHandle(facts.id.index() as u32);
    let context = EvalContext {
        head: Some(handle),
        current: Some(handle),
        config: facts.config.clone(),
        traversal: UsageTraversal::Link,
        link_language: facts.link.as_ref().map(|link| link.language.clone()),
        ..Default::default()
    };
    let entries = target.usage.link_libraries.as_ref().map_or_else(
        || {
            target
                .props
                .get(b"LINK_LIBRARIES")
                .into_iter()
                .map(|value| (target.dir, value))
                .collect::<Vec<_>>()
        },
        |items| {
            items
                .iter()
                .map(|item| (item.lookup_dir, &item.text.value))
                .collect()
        },
    );
    for (dir, value) in entries {
        for name in list::split(
            &genex(resolver, dir, &context, value, Some(mode))?,
            list::Empty::Drop,
        ) {
            let Some(id) = model.find_target(dir, &name) else {
                continue;
            };
            if !model
                .target(id)
                .is_some_and(|target| target.kind == TargetKind::SharedLibrary)
            {
                continue;
            }
            let dependency = dependencies.name(id, target).map_err(generation_error)?;
            let reference = dependency.record(model, package)?;
            if !common.requires.contains(&reference) && !common.link_requires.contains(&reference) {
                push_unique(&mut config.dyld_requires, reference);
            }
        }
    }
    Ok(())
}
