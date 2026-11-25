//! Target settings, dependencies, and build phases.

use std::collections::BTreeSet;

use crate::text;
use rcmake_base::truth;
use rcmake_gen_common::GenError;
use rcmake_model::DirId;
use rcmake_resolve::ide::IdeCustomPhase;

use crate::{
    object::{Attrs, Isa, Objects, Reference, Value, attrs, list},
    phases, scheme,
    settings::{Product, TargetSettings},
};

use super::{
    Group, Project, TargetEntry, file_ref,
    link::xcframework_phase_allowed,
    sources::{Source, source_files, validate_source_flags},
    target_fields, target_key,
};

impl Project<'_> {
    pub(super) fn target(
        &mut self,
        target: &TargetEntry<'_>,
        all: &[TargetEntry<'_>],
        project: &Reference,
        zero: Option<&Reference>,
    ) -> Result<Reference, GenError> {
        let TargetEntry {
            id,
            raw,
            reference,
            isa,
            product,
            product_ref,
        } = target;
        let name = &reference.comment;
        let key = target_key(name);
        let mut custom = phases::Custom::default();
        let mut dependencies = BTreeSet::new();
        let mut target_settings = Vec::new();
        let mut config_files = Group::default();
        let mut sources: Option<Vec<Source<'_>>> = None;
        let native = matches!(isa, Isa::PBXNativeTarget);
        let per_language_flags = self
            .configs
            .iter()
            .filter_map(|config| self.resolved.get(&(*id, config.clone())))
            .any(|target| crate::settings::compile::needs_per_language_flags(target));
        let first = self.configs.first().cloned().unwrap_or_default();
        let first_target = self.resolved.get(&(*id, first.clone())).copied();
        let plist = if native {
            self.info_plist(raw, name, product, first_target)?
        } else {
            None
        };
        let phase_mode = raw.props.get(b"XCODE_LINK_BUILD_PHASE_MODE").or_else(|| {
            self.model
                .definition(raw.dir, b"CMAKE_XCODE_LINK_BUILD_PHASE_MODE")
        });
        let mut xcframeworks = BTreeSet::new();
        if phase_mode.is_some_and(|mode| mode == "KNOWN_LOCATION")
            && let Some(ide) = first_target.and_then(|facts| facts.ide.as_deref())
        {
            xcframeworks.extend(
                ide.xcframeworks
                    .keys()
                    .filter(|root| {
                        self.configs.iter().all(|config| {
                            self.resolved
                                .get(&(*id, config.clone()))
                                .is_some_and(|facts| {
                                    xcframework_phase_allowed(facts, root, self.bin)
                                })
                        })
                    })
                    .cloned(),
            );
        }
        let phase_links = self.phase_links(target, phase_mode, &xcframeworks);
        for config in self.configs.clone() {
            let config = &config;
            let facts = self.resolved.get(&(*id, config.clone())).copied();
            let configured_product = Product::new(self.model, raw, facts, config, self.bin);
            custom.post.version_links(config, &configured_product)?;
            let graph = self
                .result
                .graphs
                .iter()
                .find(|g| g.config.0.as_slice() == config.as_bytes());
            let graph_target =
                graph.and_then(|graph| graph.targets.iter().find(|item| item.name == raw.name));
            if let Some(facts) = facts {
                if let Some(error) = facts
                    .ide
                    .as_deref()
                    .and_then(|ide| ide.xcode_config_sensitive_embed.as_ref())
                {
                    return Err(self.project_error(raw.dir, GenError::Invalid(error.clone())));
                }
                dependencies.extend(facts.dependencies.iter().copied());
                let steps = graph_target
                    .into_iter()
                    .flat_map(|target| &target.steps)
                    .filter_map(|&index| {
                        graph.and_then(|graph| graph.steps.get(index).map(|step| (index, step)))
                    });
                custom.collect(facts, self.model, self.bin, steps);
                let items = source_files(self.model, raw, facts, per_language_flags)
                    .map_err(|error| self.source_error(raw.dir, error))?;
                if let Some(original) = &sources {
                    validate_source_flags(original, &items)
                        .map_err(|error| self.source_error(raw.dir, error))?;
                } else {
                    sources = Some(items);
                }
            }
            if let Some(graph) = graph
                && let Some(item) = graph_target
            {
                for index in &item.depends {
                    if let Some(dep) = graph.targets.get(*index)
                        && let Some(binding) = self.model.find_target(raw.dir, &dep.name)
                    {
                        dependencies.insert(binding);
                    }
                }
            }
            let settings = if native {
                TargetSettings {
                    model: self.model,
                    result: self.result,
                    bin: self.bin,
                    config,
                    raw,
                    target: facts,
                    xcframeworks: &xcframeworks,
                    phase_links: &phase_links,
                    per_language_flags,
                }
                .build(&configured_product, plist.as_deref())
            } else {
                Attrs::new()
            };
            let base_config = facts
                .and_then(|facts| facts.ide.as_deref())
                .and_then(|ide| ide.attributes.get(b"XCODE_XCCONFIG".as_slice()))
                .map(|path| {
                    self.xcconfig(
                        raw.dir,
                        &key,
                        config,
                        &text(path),
                        Some(name),
                        &mut config_files,
                    )
                })
                .transpose()?
                .flatten();
            target_settings.push((settings, base_config));
        }
        for dep in &raw.utility_deps {
            if let Some(binding) = self.model.find_target(raw.dir, &dep.value) {
                dependencies.insert(binding);
            }
        }
        let dependencies = self.dependencies(
            &key,
            project,
            zero.into_iter().chain(
                all.iter()
                    .filter(|dep| dep.id != *id && dependencies.contains(&dep.id))
                    .map(|dep| &dep.reference),
            ),
        )?;
        let has_xcconfig = target_settings.iter().any(|(_, file)| file.is_some());
        let config_list = self.config_list(&key, name, *isa, target_settings)?;
        let configs = self.configs.clone();
        let phase = |objects: &mut Objects, script: phases::Script, role: &str, label: &str| {
            script.shell(objects, &format!("{key}/phase/{role}"), label, &configs)
        };
        let mut build_phases = Vec::new();
        build_phases.extend(phase(
            &mut self.objects,
            custom.pre,
            "pre",
            "CMake PreBuild Rules",
        )?);
        let rule_name = if native {
            name.clone()
        } else {
            format!("Generate CMakeFiles/{name}")
        };
        build_phases.extend(phase(&mut self.objects, custom.rules, "rules", &rule_name)?);
        let (mut tree, mut files) = self.add_sources(
            &key,
            raw,
            sources.into_iter().flatten(),
            native,
            &product.bundle,
        )?;
        if has_xcconfig {
            tree.push(config_files.finish(
                &mut self.objects,
                &format!("{key}/xcconfig"),
                "Configurations",
            )?);
        }
        if native {
            self.remove_object_inputs(first_target, &mut files.frameworks);
            if matches!(
                product.bundle,
                rcmake_resolve::ide::BundleKind::Framework { .. }
            ) && let Some(ide) = first_target.and_then(|target| target.ide.as_deref())
            {
                self.framework_headers(raw, ide, &mut files);
            }
            build_phases.push(phases::files(
                &mut self.objects,
                Isa::PBXSourcesBuildPhase,
                &format!("{key}/phase/sources"),
                "Sources",
                files.sources,
            )?);
            build_phases.extend(phase(
                &mut self.objects,
                custom.link,
                "prelink",
                "CMake PreLink Rules",
            )?);
            for root in xcframeworks {
                let path = text(&root);
                let file_key = format!("{key}/xcframework/{path}");
                let reference = file_ref(
                    &mut self.objects,
                    &file_key,
                    &path,
                    ("lastKnownFileType", "wrapper.xcframework"),
                    "<absolute>",
                )?;
                tree.add("Frameworks", reference.clone())?;
                files.frameworks.push(phases::build_file(
                    &mut self.objects,
                    &format!("{file_key}/build"),
                    "Frameworks",
                    &reference,
                    Attrs::new(),
                )?);
            }
            for (index, item) in phase_links.iter().enumerate() {
                let file_key = format!("{key}/link/{index}");
                let reference = file_ref(
                    &mut self.objects,
                    &file_key,
                    &item.path,
                    ("lastKnownFileType", item.file_type),
                    item.source_tree,
                )?;
                tree.add("Frameworks", reference.clone())?;
                files.frameworks.push(phases::build_file(
                    &mut self.objects,
                    &format!("{file_key}/build"),
                    "Frameworks",
                    &reference,
                    if item.weak {
                        attrs([("ATTRIBUTES", list(["Weak"]))])
                    } else {
                        Attrs::new()
                    },
                )?);
            }
            build_phases.push(phases::files(
                &mut self.objects,
                Isa::PBXFrameworksBuildPhase,
                &format!("{key}/phase/frameworks"),
                "Frameworks",
                files.frameworks,
            )?);
            if !files.headers.is_empty() {
                build_phases.push(phases::files(
                    &mut self.objects,
                    Isa::PBXHeadersBuildPhase,
                    &format!("{key}/phase/headers"),
                    "Headers",
                    files.headers,
                )?);
            }
            if !files.resources.is_empty() {
                build_phases.push(phases::files(
                    &mut self.objects,
                    Isa::PBXResourcesBuildPhase,
                    &format!("{key}/phase/resources"),
                    "Resources",
                    files.resources,
                )?);
            }
            build_phases.extend(
                self.embeds(&key, *id, raw, all)
                    .map_err(|error| self.source_error(raw.dir, error))?,
            );
        }
        build_phases.extend(phase(
            &mut self.objects,
            custom.post,
            "post",
            "CMake PostBuild Rules",
        )?);
        let mut fields = target_fields(reference, config_list, build_phases, dependencies);
        if let Some(product_ref) = product_ref {
            fields.extend(attrs([
                ("buildRules", list(Vec::<Value>::new())),
                ("productReference", product_ref.clone().into()),
                (
                    "productType",
                    format!("com.apple.product-type.{}", product.types().0).into(),
                ),
            ]));
        }
        self.objects.insert(*isa, reference, fields);
        let enabled = raw.props.get(b"XCODE_GENERATE_SCHEME").or_else(|| {
            self.model
                .definition(raw.dir, b"CMAKE_XCODE_GENERATE_SCHEME")
        });
        if enabled.is_some_and(|value| truth::is_on(value)) {
            let filename = product_ref
                .as_ref()
                .map_or_else(|| name.clone(), |_| product.file());
            scheme::write(
                self.host,
                &self.path,
                raw,
                reference,
                &filename,
                &self.default_config,
                &self.upgrade,
            )?;
        }
        tree.finish(&mut self.objects, &format!("{key}/group"), name)
    }

    pub(super) fn validate_custom_command_owners(
        &self,
        dir: DirId,
        targets: &[TargetEntry<'_>],
    ) -> Result<(), GenError> {
        if self
            .model
            .definition(dir, b"CMAKE_XCODE_BUILD_SYSTEM")
            .is_some_and(|value| value == "1")
        {
            return Ok(());
        }
        for config in &self.configs {
            let mut owners = std::collections::BTreeMap::new();
            for entry in targets {
                if let Some(ide) = self
                    .resolved
                    .get(&(entry.id, config.clone()))
                    .and_then(|target| target.ide.as_deref())
                {
                    for command in &ide.custom {
                        if command.phase == IdeCustomPhase::Rule
                            && !command.always_run
                            && let Some(output) = command.outputs.first()
                        {
                            owners
                                .entry(output)
                                .or_insert_with(BTreeSet::new)
                                .insert(&entry.raw.name);
                        }
                    }
                }
            }
            if let Some((output, names)) = owners.into_iter().find(|(_, names)| names.len() > 1) {
                let names = names
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n  ");
                return Err(self.project_error(dir, GenError::Invalid(format!(
                    "The custom command generating\n\n  {output}\n\nis attached to multiple targets:\n\n  {names}\n\nbut none of these is a common dependency of the other(s).  This is not allowed by the Xcode \"new build system\"."
                ).into())));
            }
        }
        Ok(())
    }
}
