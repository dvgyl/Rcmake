use super::*;
use crate::lower::compile::{Settings, compile};
use crate::lower::{fortran, modules, pch::PchFlags};

pub(super) struct CompileSink<'a> {
    pub(super) steps: &'a mut Vec<BuildStep>,
    pub(super) step_offset: usize,
    pub(super) compile_launchers: &'a mut IndexMap<BString, BString>,
    pub(super) compile_db_commands: &'a mut BTreeMap<usize, BString>,
    pub(super) swift_actions: &'a mut IndexMap<usize, crate::SwiftAction>,
    pub(super) swift_output_file_maps: &'a mut IndexMap<BString, BString>,
}

impl TargetPlan<'_> {
    #[expect(
        clippy::too_many_arguments,
        reason = "Compile lowering writes graph steps, Swift actions, and prerequisite inputs."
    )]
    pub(super) fn add_compile_steps(
        &mut self,
        artifacts: &ArtifactSet,
        commands: &Commands<'_>,
        precompiled: &pch::Plans,
        resolved_targets: &IndexMap<TargetId, &ResolvedTarget>,
        (lint, framework_dirs): (&IndexMap<BString, bool>, &[(BString, bool)]),
        fortran: Option<&fortran::FortranPlan>,
        usage: &mut UsageResolver<'_>,
        sink: CompileSink<'_>,
        (compile_inputs, ide_pch): (
            &mut BTreeMap<BString, Vec<BString>>,
            Option<&mut Vec<crate::ide::IdePch>>,
        ),
    ) -> Result<Vec<BString>, ResolveError> {
        let CompileSink {
            steps: graph_steps,
            step_offset,
            compile_launchers,
            compile_db_commands,
            swift_actions,
            swift_output_file_maps,
        } = sink;
        let (model, config, target, resolved) =
            (self.model, self.config, self.target, self.resolved);
        let dir = model.dir(target.dir);
        let mut objects = self
            .inputs
            .into_iter()
            .flat_map(|inputs| inputs.objects.iter().cloned())
            .collect::<Vec<_>>();
        let cxx_standard = self
            .inputs
            .and_then(|inputs| inputs.standard_requirements.get(b"CXX".as_slice()).copied());
        let position_independent = self.inputs.and_then(|inputs| inputs.position_independent);
        for step in modules::lower_modules_with_standard(
            model,
            resolved,
            cxx_standard,
            position_independent,
        )? {
            self.steps
                .insert(push_offset(graph_steps, step_offset, step));
        }
        if let Some(plan) = fortran {
            for step in plan.scan_steps(
                model,
                target,
                resolved,
                position_independent,
                usage.source_paths().map(|paths| paths.host),
            )? {
                self.steps
                    .insert(push_offset(graph_steps, step_offset, step));
            }
        }
        let mut pch_languages = IndexSet::new();
        let mut pdb_copies = IndexMap::new();
        let mut ide_pch = ide_pch;
        let one_step_swift = super::super::compile::is_one_step_swift(model, target)
            && resolved
                .link
                .as_ref()
                .is_some_and(|link| link.language == b"Swift");
        let split_swift = super::super::swift::is_split_build(model, target);
        if split_swift
            && let Some(lowered_swift) = super::super::swift::lower_target(
                model,
                target,
                config,
                resolved,
                resolved_targets,
                self.inputs,
                &self.dependencies,
                framework_dirs,
                position_independent,
                usage,
            )?
        {
            // Collect any per-source generated/OBJECT_DEPENDS inputs for Swift sources.
            let mut swift_extra_inputs = IndexSet::new();
            for unit in &resolved.compile_units {
                let is_swift = resolved
                    .compile_groups
                    .get(unit.group)
                    .is_some_and(|group| group.language == b"Swift");
                if is_swift && let Some(extra) = self.source_file_inputs.get(&unit.source.value) {
                    swift_extra_inputs.extend(extra.iter().cloned());
                }
            }
            for (step, action, launcher) in lowered_swift.steps {
                if let Some(primary) = action.outputs.first() {
                    if let Some(launcher) = launcher {
                        compile_launchers.insert(primary.clone(), launcher);
                    }
                    if !swift_extra_inputs.is_empty() {
                        compile_inputs
                            .entry(primary.clone())
                            .or_default()
                            .extend(swift_extra_inputs.iter().cloned());
                    }
                }
                let idx = push_offset(graph_steps, step_offset, step);
                self.steps.insert(idx);
                swift_actions.insert(idx, action);
            }
            objects.extend(lowered_swift.objects);
            if let Some((path, content)) = lowered_swift.output_file_map {
                swift_output_file_maps.insert(path, content);
            }
            self.swift_module_output = lowered_swift.module_output;
        }

        for unit in &resolved.compile_units {
            let Some(group) = resolved.compile_groups.get(unit.group) else {
                return Err(ResolveError::Message(
                    [
                        b"The compile group is missing for ",
                        unit.source.value.as_slice(),
                    ]
                    .concat()
                    .into(),
                ));
            };
            // In one-step Swift mode (`CMAKE_Swift_COMPILE_OBJECT == ":"`), Swift sources
            // are compiled directly during the link step; in split Swift mode (`CMP0157` NEW),
            // Swift sources are compiled as grouped module/object actions above.
            if (one_step_swift || split_swift) && group.language == b"Swift" {
                continue;
            }
            let required_standard = self
                .inputs
                .and_then(|inputs| inputs.standard_requirements.get(&group.language).copied());
            let pch = if pch::skip_unit(model, target, unit, self.inputs) {
                None
            } else {
                precompiled.find(model, target, resolved, &group.language)?
            };
            if let Some((owner, plans)) = &pch
                && let Some(plan) = plans.iter().find(|plan| plan.companion_object.is_some())
                && let indexmap::map::Entry::Vacant(entry) =
                    pdb_copies.entry(group.language.clone())
            {
                let result =
                    pch::pdb::prepare(model, target, resolved, *owner, plan, group, usage)?;
                if let Some(copy) = &result {
                    if let Some(step) = &copy.step {
                        if let Some(events) = &mut self.ide_steps {
                            events.push(ide_custom_step(
                                model,
                                step,
                                IdeCustomPhase::PreBuild,
                                false,
                            ));
                        } else {
                            self.steps.insert(push_offset(
                                graph_steps,
                                step_offset,
                                BuildStep::Custom(step.clone()),
                            ));
                        }
                    }
                    if let Some(headers) = &mut ide_pch
                        && let Some(header) = headers
                            .iter_mut()
                            .find(|header| header.language == group.language)
                    {
                        header.compile_pdb = Some(super::super::join(
                            &model.dir(DirId::ROOT).binary_dir,
                            &copy.destination,
                        ));
                    }
                }
                entry.insert(result);
            }
            let pdb = pdb_copies.get(&group.language).and_then(Option::as_ref);
            if let Some((owner, plans)) = &pch {
                if *owner != resolved.id {
                    self.dependencies.insert(*owner);
                    if target.kind != rcmake_model::TargetKind::ObjectLibrary
                        || model.target(*owner).is_some_and(|provider| {
                            provider.kind == rcmake_model::TargetKind::ObjectLibrary
                        })
                    {
                        objects.extend(
                            plans
                                .iter()
                                .filter_map(|plan| plan.companion_object.clone()),
                        );
                    }
                } else if pch_languages.insert(&group.language) {
                    if let Some(host) = usage.source_paths().map(|paths| paths.host) {
                        let compile_pdb = if let Some(copy) = pdb {
                            Some(copy.destination.clone())
                        } else if super::super::artifacts::is_msvc(
                            model,
                            target.dir,
                            &group.language,
                        ) {
                            Some(super::super::artifacts::compile_pdb(
                                model, target, config, usage,
                            )?)
                        } else {
                            None
                        };
                        if let Some(pdb_path) = compile_pdb.filter(|path| !path.is_empty()) {
                            let full_pdb =
                                super::super::join(&model.dir(DirId::ROOT).binary_dir, &pdb_path);
                            let pdb_dir = if pdb_path.ends_with(b"/") {
                                rcmake_host::path::to_native(&full_pdb)
                            } else {
                                rcmake_host::path::to_native(&full_pdb)
                                    .parent()
                                    .map(std::path::Path::to_path_buf)
                                    .unwrap_or_else(|| rcmake_host::path::to_native(&full_pdb))
                            };
                            let _ = host.create_dir_all(&pdb_dir);
                        }
                    }
                    for plan in plans {
                        let unit = CompileUnit {
                            source: BT::new(plan.source.clone(), target.backtrace.clone()),
                            object: plan
                                .companion_object
                                .as_ref()
                                .unwrap_or(&plan.artifact)
                                .clone(),
                            ..CompileUnit::default()
                        };
                        let lowered = compile(
                            model,
                            target,
                            config,
                            artifacts,
                            &unit,
                            group,
                            Settings {
                                standard: required_standard,
                                pch: Some(PchFlags::Create(plan)),
                                position_independent,
                                skip_file_set_linting: None,
                                extra_framework_dirs: framework_dirs,
                                compile_input: None,
                                pch_compile_pdb: pdb.map(|copy| &copy.destination),
                            },
                            usage,
                        )?;
                        if let Some(launcher) = lowered.launcher {
                            compile_launchers.insert(unit.object.clone(), launcher);
                        }
                        let mut requires = vec![plan.header.clone()];
                        for header in &plan.project_headers {
                            let path = command_path(model, &dir.binary_dir, header);
                            if commands
                                .producer(&path)
                                .is_some_and(|producer| self.steps.contains(&producer))
                            {
                                requires.push(path);
                            }
                        }
                        compile_inputs.insert(unit.object.clone(), requires);
                        if let Some(object) = &plan.companion_object {
                            objects.push(object.clone());
                        }
                        let step_idx = push_offset(graph_steps, step_offset, lowered.step);
                        if let Some(cmd) = lowered.compile_db_command {
                            compile_db_commands.insert(step_idx, cmd);
                        }
                        self.steps.insert(step_idx);
                    }
                }
            }
            let fortran_group = fortran
                .filter(|_| group.language == "Fortran")
                .map(|plan| plan.compile_group(model, target, unit, group));
            let lowered = compile(
                model,
                target,
                config,
                artifacts,
                unit,
                fortran_group.as_ref().unwrap_or(group),
                Settings {
                    standard: required_standard,
                    pch: pch.as_ref().map(|(_, plans)| PchFlags::Use(plans)),
                    position_independent,
                    skip_file_set_linting: lint
                        .get(&Lexical::new(&unit.source.value).normalize())
                        .copied(),
                    extra_framework_dirs: framework_dirs,
                    pch_compile_pdb: pdb.map(|copy| &copy.destination),
                    compile_input: fortran.and_then(|plan| plan.compile_input(&unit.object)),
                },
                usage,
            )?;
            let mut step = lowered.step;
            let launcher = lowered.launcher;
            if let BuildStep::Compile { object, dyndep, .. } = &mut step {
                if let Some(launcher) = launcher {
                    compile_launchers.insert(object.clone(), launcher);
                }
                let mut requires = self
                    .source_file_inputs
                    .get(&unit.source.value)
                    .cloned()
                    .unwrap_or_default();
                if let Some((_, plans)) = pch {
                    requires.extend(
                        plans
                            .iter()
                            .flat_map(|plan| [plan.header.clone(), plan.artifact.clone()]),
                    );
                }
                requires.extend(
                    pdb.and_then(|copy| copy.step.as_ref())
                        .into_iter()
                        .flat_map(|step| step.outputs.iter().cloned()),
                );
                if !requires.is_empty() {
                    compile_inputs.insert(object.clone(), requires.into_iter().collect());
                }
                if !modules::is_std_bmi(model, target) {
                    objects.push(object.clone());
                }
                if unit.scan {
                    *dyndep = resolved
                        .cxx_modules
                        .as_ref()
                        .map(|info| info.dyndep.clone());
                }
            }
            if let Some(plan) = fortran {
                plan.bind_compile(&mut step);
            }
            let step_idx = push_offset(graph_steps, step_offset, step);
            if let Some(cmd) = lowered.compile_db_command {
                compile_db_commands.insert(step_idx, cmd);
            }
            self.steps.insert(step_idx);
        }
        Ok(objects
            .into_iter()
            .collect::<IndexSet<_>>()
            .into_iter()
            .collect())
    }
}
