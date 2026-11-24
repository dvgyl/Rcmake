use std::cell::RefCell;
use std::collections::HashMap;

use indexmap::IndexSet;
use rcmake_base::{BString, list, path_lex::Lexical};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_genex::{
    ArtifactKind, ArtifactPart, ConfigId, EvalContext, GenexError, GenexHost, InterfaceView,
    TargetHandle,
};
use rcmake_model::{DirId, PropScope, TargetBinding, TargetId};

use super::{UsageResolver, handle};
use crate::{ResolveError, ResolvedTarget, lower::definition, sources::absolute_path};

/// Index of resolved compile units by target and configuration.
pub struct ObjectPaths<'a>(HashMap<(TargetId, &'a [u8]), &'a ResolvedTarget>);

impl<'a> ObjectPaths<'a> {
    /// Build an index of the supplied targets.
    #[must_use]
    pub fn new(targets: impl IntoIterator<Item = &'a ResolvedTarget>) -> Self {
        Self(
            targets
                .into_iter()
                .map(|target| ((target.id, target.config.0.as_slice()), target))
                .collect(),
        )
    }
}

/// Generator expression adapter for a project model and a default lookup directory.
pub struct ModelGenexHost<'model, 'resolver> {
    pub(super) resolver: &'resolver mut UsageResolver<'model>,
    pub(super) lookup_dir: DirId,
    pub(crate) current: Option<TargetHandle>,
    pub(crate) artifact_dependencies: Option<&'resolver RefCell<IndexSet<TargetId>>>,
    objects: Option<&'resolver ObjectPaths<'resolver>>,
}

impl<'model, 'resolver> ModelGenexHost<'model, 'resolver> {
    /// Use the given resolver's model, memo cache, and active property chain.
    pub fn new(resolver: &'resolver mut UsageResolver<'model>, lookup_dir: DirId) -> Self {
        Self {
            resolver,
            lookup_dir,
            current: None,
            artifact_dependencies: None,
            objects: None,
        }
    }

    /// Use the final compile units for object file queries.
    #[must_use]
    pub fn with_object_paths(mut self, paths: &'resolver ObjectPaths<'resolver>) -> Self {
        self.objects = Some(paths);
        self
    }

    pub(super) fn id(&self, target: TargetHandle) -> Option<TargetId> {
        self.resolver.ids.get(target.0 as usize).copied()
    }

    pub(crate) fn source_root(&self) -> &[u8] {
        &self.resolver.model.dir(DirId::ROOT).source_dir
    }

    fn directory(&self, ctx: &EvalContext) -> DirId {
        if ctx.current == self.current {
            return self.lookup_dir;
        }
        ctx.current
            .or(ctx.head)
            .and_then(|target| self.id(target))
            .and_then(|id| self.resolver.model.target(id))
            .map_or(self.lookup_dir, |target| target.dir)
    }
}

impl GenexHost for ModelGenexHost<'_, '_> {
    fn find_target(&self, name: &[u8]) -> Option<TargetHandle> {
        self.resolver
            .model
            .find_target(self.lookup_dir, name)
            .map(handle)
    }

    fn find_target_in_context(&self, name: &[u8], ctx: &EvalContext) -> Option<TargetHandle> {
        self.resolver
            .model
            .find_target(self.directory(ctx), name)
            .map(handle)
    }

    fn target_name(&self, target: TargetHandle) -> BString {
        self.id(target)
            .and_then(|id| self.resolver.model.target(id))
            .map(|target| target.name.clone())
            .unwrap_or_default()
    }

    fn alias_property(&self, name: &[u8], property: &[u8], ctx: &EvalContext) -> Option<BString> {
        if !matches!(property, b"ALIASED_TARGET" | b"ALIAS_GLOBAL") {
            return None;
        }
        let model = self.resolver.model;
        let dir = self.directory(ctx);
        let TargetBinding::Alias(id) = model.lookup_target(dir, name)? else {
            return None;
        };
        if property == b"ALIASED_TARGET" {
            return model.target(id).map(|target| target.name.clone());
        }
        Some(
            if model.dir(dir).local_targets.contains_key(name) {
                "FALSE"
            } else {
                "TRUE"
            }
            .into(),
        )
    }

    fn raw_property(&self, target: TargetHandle, name: &[u8]) -> Option<BString> {
        let scope = PropScope::Target(self.id(target)?);
        let model = self.resolver.model;
        if model
            .global
            .defined_property(b"TARGET", name)
            .is_some_and(|prop| prop.inherited)
        {
            model.get_prop_inherited(&scope, name)
        } else {
            model.get_prop(&scope, name)
        }
    }

    fn scalar_property(
        &mut self,
        target: TargetHandle,
        name: &[u8],
        ctx: &EvalContext,
    ) -> Result<InterfaceView, GenexError> {
        let id = self.id(target).ok_or_else(missing_target)?;
        let raw = self.raw_property(target, name);
        self.resolver
            .scalar_property(id, name, raw, ctx)
            .map_err(genex_error)
    }

    fn transitive_property(
        &mut self,
        target: TargetHandle,
        name: &[u8],
        ctx: &EvalContext,
    ) -> Result<InterfaceView, GenexError> {
        let id = self.id(target).ok_or_else(missing_target)?;
        let value = self
            .resolver
            .collect_property(id, name, ctx)
            .map_err(genex_error)?;
        Ok(InterfaceView {
            value: list::join(value.entries.into_iter().map(|entry| entry.value)),
            flags: value.flags,
        })
    }

    fn artifact(
        &self,
        target: TargetHandle,
        kind: ArtifactKind,
        part: ArtifactPart,
        config: &ConfigId,
    ) -> Option<BString> {
        self.checked_artifact(target, kind, part, config)
            .ok()
            .flatten()
    }

    fn checked_artifact(
        &self,
        target: TargetHandle,
        kind: ArtifactKind,
        part: ArtifactPart,
        config: &ConfigId,
    ) -> Result<Option<BString>, GenexError> {
        let Some(id) = self.id(target) else {
            return Ok(None);
        };
        let path = self
            .resolver
            .checked_artifact(id, kind, part, config)
            .map_err(genex_error)?;
        if path.is_none() {
            return Ok(None);
        }
        if let Some(dependencies) = self.artifact_dependencies {
            let full_file = part == ArtifactPart::Full
                && !matches!(
                    kind,
                    ArtifactKind::BundleDir | ArtifactKind::BundleContentDir
                );
            if full_file || self.target_policy(target, b"CMP0112") == Some(false) {
                dependencies.borrow_mut().insert(id);
            }
        }
        Ok(path)
    }

    fn target_objects(
        &self,
        target: TargetHandle,
        config: &ConfigId,
        sources: Option<&[u8]>,
    ) -> Option<BString> {
        let id = self.id(target)?;
        let model = self.resolver.model;
        let target = model.target(id)?;
        let resolved = self
            .objects
            .and_then(|paths| paths.0.get(&(id, config.0.as_slice())))
            .filter(|_| target.kind.compiles_objects())
            .filter(|_| !target.is_imported());
        let objects = if let Some(resolved) = resolved {
            let source_root = &model.dir(target.dir).source_dir;
            let binary_root = &model.dir(DirId::ROOT).binary_dir;
            let source_filter = sources.map(|values| {
                list::split(values, list::Empty::Drop)
                    .into_iter()
                    .map(|path| absolute_path(source_root, &path))
                    .collect::<IndexSet<_>>()
            });
            list::join(
                resolved
                    .compile_units
                    .iter()
                    .filter(|unit| {
                        source_filter.as_ref().is_none_or(|filter| {
                            filter.contains(&Lexical::new(&unit.source.value).normalize())
                        })
                    })
                    .map(|unit| absolute_path(binary_root, &unit.object)),
            )
        } else {
            self.resolver.target_objects(id, config, sources)?
        };
        if let Some(dependencies) = self.artifact_dependencies {
            dependencies.borrow_mut().insert(id);
        }
        Some(objects)
    }

    fn target_intermediate_directory(
        &self,
        target: TargetHandle,
        config: &ConfigId,
    ) -> Option<BString> {
        if self.resolver.ide == Some(crate::ide::IdeFlavor::Xcode) {
            return None;
        }
        self.resolver.object_directory(self.id(target)?, config)
    }
    fn link_closure(
        &mut self,
        target: TargetHandle,
        ctx: &EvalContext,
    ) -> Result<Vec<TargetHandle>, GenexError> {
        let id = self.id(target).ok_or_else(missing_target)?;
        self.resolver
            .link_targets(id, ctx)
            .map(|targets| targets.into_iter().map(handle).collect())
            .map_err(genex_error)
    }

    fn definition(&self, name: &[u8]) -> Option<BString> {
        definition(self.resolver.model, self.lookup_dir, name).cloned()
    }

    fn target_policy(&self, target: TargetHandle, policy: &[u8]) -> Option<bool> {
        let target = self.resolver.model.target(self.id(target)?)?;
        Some(target.policies.policies.get(PolicyId::parse(policy)?) == PolicyStatus::New)
    }
}

fn missing_target() -> GenexError {
    genex_error(ResolveError::Message("Unknown target.".into()))
}

fn genex_error(error: ResolveError) -> GenexError {
    match error {
        ResolveError::Genex(error) => error,
        other => GenexError::Message {
            expression: BString::default(),
            message: other.to_string().into(),
        },
    }
}
