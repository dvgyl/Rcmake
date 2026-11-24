//! Target settings, dependencies, and build phases.

use std::collections::BTreeSet;

use rcmake_base::truth;
use rcmake_gen_common::GenError;

use crate::{
    object::{Attrs, Isa, Objects, Reference, Value, attrs, list},
    phases, scheme,
    settings::{Product, TargetSettings},
};

use super::{
    Project, TargetEntry, file_ref,
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
    ) -> Result<(Reference, Option<Reference>), GenError> {
        let TargetEntry {
            id,
            raw,
            reference,
            isa,
        } = target;
        let name = &reference.comment;
        let key = target_key(name);
        let mut custom = phases::Custom::default();
        let mut dependencies = BTreeSet::new();
        let mut target_settings = Vec::new();
        let mut sources: Option<Vec<Source<'_>>> = None;
        let native = matches!(isa, Isa::PBXNativeTarget);
        let first = self.configs.first().cloned().unwrap_or_default();
        let first_target = self.resolved.get(&(*id, first.clone())).copied();
        let product = Product::new(self.model, raw, first_target, &first, self.bin);
        let plist = if native {
            self.info_plist(raw, name, &product, first_target)?
        } else {
            None
        };
        for config in &self.configs {
            let facts = self.resolved.get(&(*id, config.clone())).copied();
            let configured_product = Product::new(self.model, raw, facts, config, self.bin);
            custom.post.version_links(config, &configured_product)?;
            let graph = self
                .result
                .graphs
                .iter()
                .find(|g| g.config.0.as_slice() == config.as_bytes());
            if let Some(facts) = facts {
                dependencies.extend(facts.dependencies.iter().copied());
                custom.collect(raw, facts, self.bin);
                let items = source_files(self.model, raw, facts)?;
                if let Some(original) = &sources {
                    validate_source_flags(original, &items)?;
                } else {
                    sources = Some(items);
                }
            }
            if let Some(graph) = graph
                && let Some(item) = graph.targets.iter().find(|item| item.name == raw.name)
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
                    arch: &self.arch,
                    raw,
                    target: facts,
                }
                .build(&configured_product, plist.as_deref())
            } else {
                Attrs::new()
            };
            target_settings.push(settings);
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
        let (tree, files) = self.add_sources(
            &key,
            raw,
            sources.into_iter().flatten(),
            native,
            &product.bundle,
        )?;
        let mut product_ref = None;
        if native {
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
            product_ref = Some(file_ref(
                &mut self.objects,
                &format!("product/{name}"),
                &product.file(),
                ("explicitFileType", product.types().1),
                "BUILT_PRODUCTS_DIR",
            )?);
            build_phases.extend(self.embeds(&key, *id, raw)?);
        }
        build_phases.extend(phase(
            &mut self.objects,
            custom.post,
            "post",
            "CMake PostBuild Rules",
        )?);
        let mut fields = target_fields(reference, config_list, build_phases, dependencies);
        if let Some(product_ref) = &product_ref {
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
        let group = tree.finish(&mut self.objects, &format!("{key}/group"), name)?;
        Ok((group, product_ref))
    }
}
