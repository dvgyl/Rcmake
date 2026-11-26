use super::*;

impl TargetPlan<'_> {
    pub(super) fn trace_source_commands<'a>(
        &mut self,
        commands: &Commands<'_>,
        unity: Option<&'a TargetUnity>,
    ) -> Result<IndexMap<&'a BString, IndexSet<BString>>, ResolveError> {
        let (model, config, target, resolved) =
            (self.model, self.config, self.target, self.resolved);
        let dir = model.dir(target.dir);
        let mut unity_inputs = IndexMap::<_, IndexSet<_>>::new();
        let ninja = matches!(
            model.generator,
            Some(GeneratorKind::Ninja | GeneratorKind::NinjaMultiConfig)
        );
        let paths = if let Some(inputs) = self.inputs {
            Box::new(inputs.sources.iter().cloned()) as Box<dyn Iterator<Item = BString>>
        } else {
            Box::new(
                resolved
                    .compile_units
                    .iter()
                    .map(|unit| unit.source.value.clone())
                    .chain(
                        target
                            .sources
                            .iter()
                            .flat_map(|source| list::split(&source.value, list::Empty::Drop)),
                    ),
            )
        };
        for path in paths {
            let source = crate::sources::absolute_path(&dir.source_dir, &path);
            let input = build_relative(model, &Lexical::new(&source).normalize());
            if let Some(step) = commands.source_attachment(target.dir, &input) {
                self.trace_roots.sources.insert(step);
                commands.add_producers([step], &mut self.steps, &mut self.dependencies)?;
            }
            let object_depends = crate::sources::properties(dir, &source)
                .get(b"OBJECT_DEPENDS")
                .into_iter()
                .flat_map(|value| list::iter(value, list::Empty::Drop));
            for dependency in object_depends {
                let dependency = Lexical::new(&dependency).normalize();
                let direct = build_relative(model, &dependency);
                let local = command_path(model, &dir.binary_dir, &dependency);
                let input = [direct.clone(), local]
                    .into_iter()
                    .find(|input| {
                        commands.file_producer(input).is_some()
                            || commands.byproduct_owners.contains_key(input)
                    })
                    .unwrap_or(direct);
                if let Some(producer) = commands.file_producer(&input) {
                    self.trace_roots.sources.insert(producer);
                    commands.add_producers([producer], &mut self.steps, &mut self.dependencies)?;
                }
                if let Some(&owner) = commands
                    .byproduct_owners
                    .get(&input)
                    .filter(|&&owner| owner != resolved.id)
                {
                    self.dependencies.insert(owner);
                }
                self.source_file_inputs
                    .entry(path.clone())
                    .or_default()
                    .insert(input);
            }
            for base in [&dir.source_dir, &dir.binary_dir] {
                let input = command_path(model, base, &path);
                if let Some(step) = commands.producer(&input) {
                    self.trace_roots.sources.insert(step);
                    commands.add_producers([step], &mut self.steps, &mut self.dependencies)?;
                    if ninja
                        && let Some(file) =
                            unity.and_then(|plan| plan.file_for_source(&path, config))
                    {
                        unity_inputs.entry(file).or_default().insert(input);
                    }
                }
            }
        }

        Ok(unity_inputs)
    }
}

pub(super) fn record_owners(
    owners: &mut IndexMap<usize, crate::CustomOutputOwners>,
    id: TargetId,
    steps: &IndexSet<usize>,
) {
    for step in steps {
        if let Some(owners) = owners.get_mut(step) {
            owners.targets.insert(id);
        }
    }
}
