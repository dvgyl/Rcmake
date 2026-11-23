use rcmake_base::{BString, list};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_genex::{
    ArtifactKind, ArtifactPart, ConfigId, EvalContext, GenexError, GenexHost, InterfaceView,
    TargetHandle,
};
use rcmake_model::{DirId, ProjectModel, PropScope, TargetId};

use super::{UsageResolver, handle};
use crate::ResolveError;

/// Generator expression adapter for a project model and a default lookup directory.
pub struct ModelGenexHost<'model, 'resolver> {
    pub(super) resolver: &'resolver mut UsageResolver<'model>,
    pub(super) lookup_dir: DirId,
    pub(super) current: Option<TargetHandle>,
}

impl<'model, 'resolver> ModelGenexHost<'model, 'resolver> {
    /// Use the given resolver's model, memo cache, and active property chain.
    pub fn new(resolver: &'resolver mut UsageResolver<'model>, lookup_dir: DirId) -> Self {
        Self {
            resolver,
            lookup_dir,
            current: None,
        }
    }

    pub(super) fn id(&self, target: TargetHandle) -> Option<TargetId> {
        self.resolver.ids.get(target.0 as usize).copied()
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
        self.resolver.artifact(self.id(target)?, kind, part, config)
    }

    fn target_objects(
        &self,
        target: TargetHandle,
        config: &ConfigId,
        sources: Option<&[u8]>,
    ) -> Option<BString> {
        self.resolver
            .target_objects(self.id(target)?, config, sources)
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
        definition(self.resolver.model, self.lookup_dir, name)
    }

    fn target_policy(&self, target: TargetHandle, policy: &[u8]) -> Option<bool> {
        let target = self.resolver.model.target(self.id(target)?)?;
        Some(target.policies.policies.get(PolicyId::parse(policy)?) == PolicyStatus::New)
    }
}

pub(super) fn definition(model: &ProjectModel, dir: DirId, name: &[u8]) -> Option<BString> {
    crate::lower::definition(model, dir, name).cloned()
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
