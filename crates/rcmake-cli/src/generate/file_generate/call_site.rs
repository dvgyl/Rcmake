//! Generator expression host that reports the policies of a `file(GENERATE)` call.

use rcmake_diag::{
    DiagCategory, Diagnostic, Effect, Location, MessageKind, PolicyId, RenderOptions, render,
};
use rcmake_host::{Host, Stream};

use rcmake_base::{BString, Limits};
use rcmake_genex::{
    ArtifactKind, ArtifactPart, ConfigId, EvalContext, GenexError, GenexHost, InterfaceView,
    TargetHandle,
};

/// Forward every query to `inner`, except the policies recorded at the call site.
///
/// CMake evaluates `file(GENERATE)` with the policies of the call, not with the
/// policies at the end of the directory.
pub(super) struct CallSitePolicies<'a> {
    pub(super) inner: &'a mut dyn GenexHost,
    pub(super) policies: &'a [(&'static [u8], bool)],
    pub(super) host: &'a dyn Host,
    pub(super) options: &'a RenderOptions,
    pub(super) suppress_warnings: bool,
}

impl GenexHost for CallSitePolicies<'_> {
    fn find_target(&self, name: &[u8]) -> Option<TargetHandle> {
        self.inner.find_target(name)
    }

    fn find_target_in_context(&self, name: &[u8], ctx: &EvalContext) -> Option<TargetHandle> {
        self.inner.find_target_in_context(name, ctx)
    }

    fn target_name(&self, target: TargetHandle) -> BString {
        self.inner.target_name(target)
    }

    fn alias_property(&self, name: &[u8], property: &[u8], ctx: &EvalContext) -> Option<BString> {
        self.inner.alias_property(name, property, ctx)
    }

    fn source_property(
        &self,
        source: &[u8],
        scope: Option<(&[u8], &[u8])>,
        property: &[u8],
        ctx: &EvalContext,
    ) -> Result<BString, BString> {
        self.inner.source_property(source, scope, property, ctx)
    }

    fn source_exists(
        &self,
        source: &[u8],
        scope: Option<(&[u8], &[u8])>,
        ctx: &EvalContext,
    ) -> Result<bool, BString> {
        self.inner.source_exists(source, scope, ctx)
    }

    fn file_set_exists(&self, target: TargetHandle, name: &[u8]) -> bool {
        self.inner.file_set_exists(target, name)
    }

    fn file_set_property(
        &self,
        target: TargetHandle,
        name: &[u8],
        property: &[u8],
    ) -> Option<BString> {
        self.inner.file_set_property(target, name, property)
    }

    fn global_property(&self, name: &[u8]) -> Option<BString> {
        self.inner.global_property(name)
    }

    fn directory_property(
        &self,
        directory: &[u8],
        property: &[u8],
        ctx: &EvalContext,
    ) -> Result<Option<BString>, BString> {
        self.inner.directory_property(directory, property, ctx)
    }

    fn directory_policy(&self, policy: &[u8], ctx: &EvalContext) -> Option<bool> {
        self.policies
            .iter()
            .find(|(name, _)| *name == policy)
            .map(|(_, new)| *new)
            .or_else(|| self.inner.directory_policy(policy, ctx))
    }

    fn directory_policy_status(
        &self,
        policy: rcmake_diag::PolicyId,
        ctx: &EvalContext,
    ) -> Option<rcmake_diag::PolicyStatus> {
        self.inner.directory_policy_status(policy, ctx)
    }

    fn policy_warning(&mut self, policy: PolicyId, detail: &[u8], ctx: &EvalContext) {
        if self.suppress_warnings {
            return;
        }
        let mut body: BString = policy.warning_text().into();
        if !detail.is_empty() {
            body.push(b'\n');
            body.extend_from_slice(detail);
        }
        let text = render(
            &Diagnostic {
                annotations: None,
                kind: MessageKind::AuthorWarning,
                effect: Effect::None,
                category: Some(DiagCategory::Policy),
                policy: Some(policy),
                location: Location::Backtrace(ctx.site.backtrace.clone()),
                body,
            },
            self.options,
        );
        self.host.write_out(Stream::Stderr, &text);
    }

    fn raw_property(&self, target: TargetHandle, name: &[u8]) -> Option<BString> {
        self.inner.raw_property(target, name)
    }

    fn scalar_property(
        &mut self,
        target: TargetHandle,
        name: &[u8],
        ctx: &EvalContext,
    ) -> Result<InterfaceView, GenexError> {
        self.inner.scalar_property(target, name, ctx)
    }

    fn transitive_property(
        &mut self,
        target: TargetHandle,
        name: &[u8],
        ctx: &EvalContext,
    ) -> Result<InterfaceView, GenexError> {
        self.inner.transitive_property(target, name, ctx)
    }

    fn artifact(
        &self,
        target: TargetHandle,
        kind: ArtifactKind,
        part: ArtifactPart,
        config: &ConfigId,
    ) -> Option<BString> {
        self.inner.artifact(target, kind, part, config)
    }

    fn checked_artifact(
        &self,
        target: TargetHandle,
        kind: ArtifactKind,
        part: ArtifactPart,
        config: &ConfigId,
    ) -> Result<Option<BString>, GenexError> {
        self.inner.checked_artifact(target, kind, part, config)
    }

    fn link_closure(
        &mut self,
        target: TargetHandle,
        ctx: &EvalContext,
    ) -> Result<Vec<TargetHandle>, GenexError> {
        self.inner.link_closure(target, ctx)
    }

    fn definition(&self, name: &[u8]) -> Option<BString> {
        self.inner.definition(name)
    }

    fn language_enabled(&self, language: &[u8]) -> Option<bool> {
        self.inner.language_enabled(language)
    }

    fn imported_config_matches(
        &self,
        target: TargetHandle,
        head: Option<TargetHandle>,
        consumer: &ConfigId,
    ) -> Option<Vec<BString>> {
        self.inner.imported_config_matches(target, head, consumer)
    }

    fn target_policy(&self, target: TargetHandle, policy: &[u8]) -> Option<bool> {
        self.inner.target_policy(target, policy)
    }

    fn target_objects(
        &self,
        target: TargetHandle,
        config: &ConfigId,
        sources: Option<&[u8]>,
    ) -> Option<BString> {
        self.inner.target_objects(target, config, sources)
    }

    fn target_intermediate_directory(
        &self,
        target: TargetHandle,
        config: &ConfigId,
    ) -> Option<BString> {
        self.inner.target_intermediate_directory(target, config)
    }

    fn artifact_base_name_without_postfix(
        &self,
        target: TargetHandle,
        kind: ArtifactKind,
        config: &ConfigId,
    ) -> Option<BString> {
        self.inner
            .artifact_base_name_without_postfix(target, kind, config)
    }

    fn is_device_link(&self, ctx: &EvalContext) -> bool {
        self.inner.is_device_link(ctx)
    }

    fn install_prefix(&self, ctx: &EvalContext) -> Option<BString> {
        self.inner.install_prefix(ctx)
    }

    fn timestamp(&self, format: Option<&[u8]>, utc: bool) -> Option<BString> {
        self.inner.timestamp(format, utc)
    }

    fn random_seed(&self) -> Option<u64> {
        self.inner.random_seed()
    }

    fn limits(&self) -> Limits {
        self.inner.limits()
    }
}
