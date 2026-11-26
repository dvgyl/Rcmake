use super::*;
use crate::lower::compile::{Settings, compile};
use crate::lower::{fortran, modules, pch::PchFlags};

impl TargetPlan<'_> {
    #[expect(
        clippy::too_many_arguments,
        reason = "Compile lowering writes graph steps and prerequisite inputs."
    )]
    pub(super) fn add_compile_steps(
        &mut self,
        artifacts: &ArtifactSet,
        commands: &Commands<'_>,
        precompiled: &pch::Plans,
        (lint, framework_dirs): (&IndexMap<BString, bool>, &[(BString, bool)]),
        fortran: Option<&fortran::FortranPlan>,
        usage: &mut UsageResolver<'_>,
        (graph_steps, step_offset, compile_launchers): (
            &mut Vec<BuildStep>,
            usize,
            &mut IndexMap<BString, BString>,
        ),
        (compile_inputs, ide_pch): (
            &mut BTreeMap<BString, Vec<BString>>,
            Option<&mut Vec<crate::ide::IdePch>>,
        ),
    ) -> Result<Vec<BString>, ResolveError> {
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
            // are compiled directly during the link step, which owns their `.o` outputs.
            if one_step_swift && group.language == b"Swift" {
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
                        let (step, launcher) = compile(
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
                        if let Some(launcher) = launcher {
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
                        self.steps
                            .insert(push_offset(graph_steps, step_offset, step));
                    }
                }
            }
            let fortran_group = fortran
                .filter(|_| group.language == "Fortran")
                .map(|plan| plan.compile_group(model, target, unit, group));
            let (mut step, launcher) = compile(
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
            self.steps
                .insert(push_offset(graph_steps, step_offset, step));
        }
        Ok(objects
            .into_iter()
            .collect::<IndexSet<_>>()
            .into_iter()
            .collect())
    }
}
