use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use indexmap::IndexSet;
use rcmake_base::{BString, list, path_lex::Lexical, truth};
use rcmake_diag::{Backtrace, Location, MessageKind, PolicyId, PolicyStatus};
use rcmake_genex::{
    ArtifactKind, ArtifactPart, ConfigId, EvalContext, GenexError, GenexHost, InterfaceView,
    RuleProperty, TargetHandle, UsageTraversal,
};
use rcmake_host::Host;
use rcmake_model::{DirId, FileSetId, PropScope, TargetBinding, TargetId, UsageScope};

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

type TimestampFormatter<'a> = dyn Fn(Option<&[u8]>, bool) -> Option<BString> + 'a;

/// Generator expression adapter for a project model and a default lookup directory.
pub struct ModelGenexHost<'model, 'resolver> {
    pub(super) resolver: &'resolver mut UsageResolver<'model>,
    pub(super) lookup_dir: DirId,
    pub(crate) current: Option<TargetHandle>,
    pub(crate) cmp0189: Option<PolicyStatus>,
    pub(crate) artifact_dependencies: Option<&'resolver RefCell<IndexSet<(TargetId, ConfigId)>>>,
    objects: Option<&'resolver ObjectPaths<'resolver>>,
    clock: Option<&'resolver dyn Host>,
    timestamp: Option<&'resolver TimestampFormatter<'resolver>>,
    /// Call site for warnings that a host callback issues without a context.
    pub(crate) warning_site: Option<Backtrace>,
    /// Keep warnings here instead of writing them, so that the caller can order them
    /// with the errors of the same command.
    pub(crate) deferred_warnings: Option<Rc<RefCell<Vec<BString>>>>,
    /// Target whose `SOURCES` this host evaluates.
    pub(super) sources_of: Option<TargetId>,
    /// Set when the evaluated `SOURCES` ask for the objects of their own target.
    pub(super) self_objects: Cell<bool>,
}

impl<'model, 'resolver> ModelGenexHost<'model, 'resolver> {
    /// Use the given resolver's model, memo cache, and active property chain.
    pub fn new(resolver: &'resolver mut UsageResolver<'model>, lookup_dir: DirId) -> Self {
        Self {
            resolver,
            lookup_dir,
            current: None,
            cmp0189: None,
            artifact_dependencies: None,
            objects: None,
            clock: None,
            timestamp: None,
            warning_site: None,
            deferred_warnings: None,
            sources_of: None,
            self_objects: Cell::new(false),
        }
    }

    /// Use the final compile units for object file queries.
    #[must_use]
    pub fn with_object_paths(mut self, paths: &'resolver ObjectPaths<'resolver>) -> Self {
        self.objects = Some(paths);
        self
    }

    /// Use the supplied clock and time zone to format timestamps.
    #[must_use]
    pub fn with_clock(
        mut self,
        host: &'resolver dyn Host,
        format: &'resolver TimestampFormatter<'resolver>,
    ) -> Self {
        self.clock = Some(host);
        self.timestamp = Some(format);
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

    fn source_scope(&self, dir: DirId, scope: Option<(&[u8], &[u8])>) -> Result<DirId, BString> {
        let model = self.resolver.model;
        match scope {
            Some((b"DIRECTORY", name)) => {
                let path = absolute_path(&model.dir(dir).source_dir, name);
                self.directory_id(&path)
                    .ok_or_else(|| format!("Directory \"{path}\" is not known from CMake.").into())
            }
            Some((_, name)) => model
                .find_target(dir, name)
                .and_then(|id| model.target(id))
                .map(|target| target.dir)
                .ok_or_else(|| format!("Non-existent target: {}", BString::from(name)).into()),
            None => Ok(dir),
        }
    }

    fn directory_id(&self, path: &[u8]) -> Option<DirId> {
        self.resolver
            .model
            .dirs()
            .find_map(|(id, dir)| (dir.source_dir == path || dir.binary_dir == path).then_some(id))
    }

    fn registered_source(&self, dir: DirId, path: &[u8]) -> bool {
        let model = self.resolver.model;
        let directory = model.dir(dir);
        if directory.source_props.contains_key(path) || directory.generated_sources.contains(path) {
            return true;
        }
        let source = &directory.source_dir;
        let matches_path = |value: &[u8]| {
            !value.windows(2).any(|bytes| bytes == b"$<")
                && list::split(value, list::Empty::Drop)
                    .iter()
                    .any(|entry| absolute_path(source, entry) == path)
        };
        directory.targets.iter().any(|id| {
            model.target(*id).is_some_and(|target| {
                target
                    .sources
                    .iter()
                    .any(|entry| matches_path(&entry.value))
                    || target.file_sets.iter().any(|id| {
                        model.file_set(*id).is_some_and(|set| {
                            set.files.iter().any(|entry| matches_path(&entry.value))
                        })
                    })
            })
        })
    }

    fn named_file_set(&self, target: TargetHandle, name: &[u8]) -> Option<FileSetId> {
        let model = self.resolver.model;
        let target = model.target(self.id(target)?)?;
        target
            .file_sets
            .iter()
            .copied()
            .find(|id| model.file_set(*id).is_some_and(|set| set.name == name))
    }
}

impl GenexHost for ModelGenexHost<'_, '_> {
    fn read_target_sources(&mut self, target: TargetHandle) {
        if let Some(id) = self.id(target)
            && let Some(reads) = &self.resolver.source_reads
        {
            reads.borrow_mut().insert(id);
        }
    }

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

    fn source_property(
        &self,
        source: &[u8],
        scope: Option<(&[u8], &[u8])>,
        property: &[u8],
        ctx: &EvalContext,
    ) -> Result<BString, BString> {
        let model = self.resolver.model;
        let origin = self.lookup_dir;
        let dir = self.source_scope(origin, scope)?;
        let lookup = model.dir(origin);
        let directory = model.dir(dir);
        let explicit = scope.is_some();
        let paths = [
            Some(absolute_path(&lookup.source_dir, source)),
            (!explicit).then(|| absolute_path(&directory.binary_dir, source)),
        ];
        let path = paths
            .iter()
            .flatten()
            .find(|path| self.registered_source(dir, path))
            .or_else(|| {
                paths
                    .iter()
                    .flatten()
                    .find(|path| model.is_globally_generated(path))
            })
            .ok_or_else(|| {
                BString::from(format!(
                    "Source file \"{}\" is not known from CMake.",
                    BString::from(source)
                ))
            })?;
        if property == b"OBJECT_NAME" {
            if ctx.site.property.as_ref().is_some_and(|key| {
                matches!(key.as_slice(), b"OBJECT_NAME" | b"INSTALL_OBJECT_NAME")
            }) {
                return Err(
                    "The OBJECT_NAME property cannot read an OBJECT_NAME source property.".into(),
                );
            }
            return crate::sources::custom_object_name(model, dir, path)
                .map_err(|error| error.to_string().into());
        }
        Ok(model
            .get_prop(
                &PropScope::Source {
                    dir,
                    path: path.clone(),
                },
                property,
            )
            .unwrap_or_default())
    }

    fn source_exists(
        &self,
        source: &[u8],
        scope: Option<(&[u8], &[u8])>,
        _ctx: &EvalContext,
    ) -> Result<bool, BString> {
        let model = self.resolver.model;
        let origin = self.lookup_dir;
        let dir = self.source_scope(origin, scope)?;
        let absolute = absolute_path(&model.dir(origin).source_dir, source);
        Ok(self.registered_source(dir, &absolute)
            || (scope.is_none()
                && self.registered_source(dir, &absolute_path(&model.dir(dir).binary_dir, source))))
    }

    fn file_set_exists(&self, target: TargetHandle, name: &[u8]) -> bool {
        self.named_file_set(target, name).is_some()
    }

    fn file_set_property(
        &self,
        target: TargetHandle,
        name: &[u8],
        property: &[u8],
    ) -> Option<BString> {
        let model = self.resolver.model;
        let id = self.named_file_set(target, name)?;
        let set = model.file_set(id)?;
        let entries = match property {
            b"TYPE" => return Some(set.kind.as_str().into()),
            b"SCOPE" => {
                return Some(
                    match set.scope {
                        UsageScope::Private => "PRIVATE",
                        UsageScope::Public => "PUBLIC",
                        UsageScope::Interface => "INTERFACE",
                    }
                    .into(),
                );
            }
            b"BASE_DIRS" => &set.base_dirs,
            b"SOURCES" if set.scope != UsageScope::Interface => &set.files,
            b"INTERFACE_SOURCES" if set.scope != UsageScope::Private => &set.files,
            b"SOURCES" | b"INTERFACE_SOURCES" => return None,
            _ => return model.get_prop(&PropScope::FileSet(id), property),
        };
        Some(list::join(entries.iter().map(|entry| &entry.value)))
    }

    fn global_property(&self, name: &[u8]) -> Option<BString> {
        let model = self.resolver.model;
        match name {
            b"CMAKE_ROLE" => Some("PROJECT".into()),
            b"GENERATOR_IS_MULTI_CONFIG" => Some(
                if model
                    .generator
                    .is_some_and(rcmake_model::GeneratorKind::multi_config)
                {
                    "1"
                } else {
                    "0"
                }
                .into(),
            ),
            _ => model.get_prop(&PropScope::Global, name),
        }
    }

    fn directory_property(
        &self,
        directory: &[u8],
        property: &[u8],
        _ctx: &EvalContext,
    ) -> Result<Option<BString>, BString> {
        let model = self.resolver.model;
        let path = absolute_path(&model.dir(self.lookup_dir).source_dir, directory);
        let dir = self.directory_id(&path).ok_or_else(|| {
            BString::from(format!(
                "Directory \"{}\" was not found.",
                BString::from(directory)
            ))
        })?;
        let scope = PropScope::Directory(dir);
        if model
            .global
            .defined_property(b"DIRECTORY", property)
            .is_some_and(|prop| prop.inherited)
        {
            Ok(model.get_prop_inherited(&scope, property))
        } else {
            Ok(model.get_prop(&scope, property))
        }
    }

    fn rule_property(&self, rule: &[u8], property: &[u8]) -> Option<RuleProperty> {
        let props = self.resolver.model.custom_rule(self.lookup_dir, rule)?;
        Some(RuleProperty {
            rule_name: props.get(b"NAME").cloned().unwrap_or_else(|| rule.into()),
            value: props.get(property).cloned(),
        })
    }

    fn directory_policy(&self, policy: &[u8], _ctx: &EvalContext) -> Option<bool> {
        self.directory_policy_status(PolicyId::parse(policy)?, _ctx)
            .map(|status| status == PolicyStatus::New)
    }

    fn directory_policy_status(
        &self,
        policy: PolicyId,
        _ctx: &EvalContext,
    ) -> Option<PolicyStatus> {
        if policy == PolicyId::new(189) && self.cmp0189.is_some() {
            return self.cmp0189;
        }
        Some(
            self.resolver
                .model
                .dir(self.lookup_dir)
                .final_policies
                .as_ref()?
                .get(policy),
        )
    }

    fn policy_warning(&mut self, policy: PolicyId, detail: &[u8], ctx: &EvalContext) {
        let Some(paths) = self.resolver.source_paths() else {
            return;
        };
        let model = self.resolver.model;
        if definition(model, self.lookup_dir, b"CMAKE_SUPPRESS_DEVELOPER_WARNINGS")
            .is_some_and(|value| truth::is_on(value))
        {
            return;
        }
        let mut body: BString = policy.warning_text().into();
        if !detail.is_empty() {
            body.push(b'\n');
            body.extend_from_slice(detail);
        }
        if let Some(text) = paths.render_once(
            model,
            MessageKind::AuthorWarning,
            Some(policy),
            Location::Backtrace(ctx.site.backtrace.clone()),
            body,
        ) {
            match &self.deferred_warnings {
                Some(deferred) => deferred.borrow_mut().push(text),
                None => paths.emit_rendered_warning_once(text),
            }
        }
    }

    fn timestamp(&self, format: Option<&[u8]>, utc: bool) -> Option<BString> {
        self.timestamp?(format, utc)
    }

    fn random_seed(&self) -> Option<u64> {
        let host = self
            .clock
            .or_else(|| self.resolver.source_paths().map(|paths| paths.host))?;
        let now = host.now();
        Some((now.secs as u64).rotate_left(11) ^ u64::from(now.nanos))
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
        // CMP0166: before CMake 3.30, these link properties did not follow
        // the private dependencies of static libraries.
        let base = name.strip_prefix(b"INTERFACE_").unwrap_or(name);
        let traversal = (matches!(
            base,
            b"LINK_OPTIONS" | b"LINK_DIRECTORIES" | b"LINK_DEPENDS"
        ) && self.directory_policy(b"CMP0166", ctx) != Some(true))
        .then_some(UsageTraversal::Compile);
        self.resolver.property_reads = self.resolver.property_reads.saturating_add(1);
        let value = self
            .resolver
            .collect_property_with(id, name, ctx, traversal);
        self.resolver.property_reads = self.resolver.property_reads.saturating_sub(1);
        let value = value.map_err(genex_error)?;
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
            .expression_artifact(id, kind, part, config)
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
                if !full_file {
                    self.warn_cmp0112(id);
                }
                dependencies.borrow_mut().insert((id, config.clone()));
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
        // The objects of a target come from its sources, so its own SOURCES
        // cannot name them. The caller reports the cycle.
        if self.sources_of == Some(id) {
            self.self_objects.set(true);
            return Some(BString::default());
        }
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
                    .map(|unit| absolute_path(binary_root, &unit.object))
                    .chain(
                        sources
                            .is_none()
                            .then(|| self.resolver.pch_objects.get(&(id, config.clone())))
                            .flatten()
                            .into_iter()
                            .flatten()
                            .map(|object| absolute_path(binary_root, object)),
                    ),
            )
        } else {
            self.resolver.target_objects(id, config, sources)?
        };
        if let Some(dependencies) = self.artifact_dependencies {
            dependencies.borrow_mut().insert((id, config.clone()));
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

    fn language_enabled(&self, language: &[u8]) -> Option<bool> {
        let enabled = self.resolver.model.global.props.get(b"ENABLED_LANGUAGES")?;
        Some(
            list::split(enabled, list::Empty::Drop)
                .iter()
                .any(|item| item == language),
        )
    }

    fn imported_config_matches(
        &self,
        target: TargetHandle,
        head: Option<TargetHandle>,
        consumer: &ConfigId,
    ) -> Option<Vec<BString>> {
        let target = self.resolver.model.target(self.id(target)?)?;
        if !target.is_imported() {
            return None;
        }
        let cps = super::artifacts::imported_from_cps(target);
        let mut configurations = Vec::new();
        if !cps {
            configurations.push(consumer.0.clone());
            if head.and_then(|head| self.target_policy(head, b"CMP0199")) != Some(true)
                && let Some(mapped) = super::artifacts::imported_config_map(target, consumer)
            {
                configurations.extend(list::split(mapped, list::Empty::Keep));
            }
        }
        configurations.extend(super::artifacts::selected_imported_config(target, consumer));
        Some(configurations)
    }

    fn imported_config_matches_ctx(
        &mut self,
        target: TargetHandle,
        ctx: &EvalContext,
    ) -> Option<Vec<BString>> {
        let model = self.resolver.model;
        let target_id = self.id(target)?;
        let imported = model.target(target_id)?;
        if !imported.is_imported() {
            return None;
        }
        let cps = super::artifacts::imported_from_cps(imported);
        if let Some(paths) = self.resolver.source_paths() {
            super::artifacts::warn_cmp0200(model, paths, imported, &ctx.config);
            if !cps
                && let Some(head_id) = ctx.head.and_then(|h| self.id(h))
                && let Some(head_target) = model.target(head_id)
            {
                let policy = PolicyId::new(199);
                if head_target.policies.policies.get(policy) == PolicyStatus::Warn
                    && model
                        .definition(head_target.dir, b"CMAKE_POLICY_WARNING_CMP0199")
                        .is_some_and(|value| truth::is_on(value))
                {
                    let body = format!(
                        "{}\nEvaluation of $<CONFIG> for imported target \"{}\", used by \"{}\", may match multiple configurations.",
                        policy.warning_text(),
                        imported.name,
                        head_target.name
                    );
                    let site = self
                        .resolver
                        .current_link_bt
                        .clone()
                        .unwrap_or_else(|| ctx.site.backtrace.clone());
                    paths.warn_once(
                        model,
                        MessageKind::AuthorWarning,
                        Some(policy),
                        Location::Backtrace(site),
                        body.into(),
                    );
                }
            }
        }
        self.imported_config_matches(target, ctx.head, &ctx.config)
    }

    fn warning(&mut self, body: BString, ctx: &EvalContext) {
        let Some(paths) = self.resolver.source_paths() else {
            return;
        };
        let location = Location::Backtrace(ctx.site.backtrace.clone());
        let Some(text) = paths.render_once(
            self.resolver.model,
            MessageKind::Warning,
            None,
            location,
            body,
        ) else {
            return;
        };
        match &self.deferred_warnings {
            Some(deferred) => deferred.borrow_mut().push(text),
            None => paths.emit_rendered_warning_once(text),
        }
    }

    fn target_policy(&self, target: TargetHandle, policy: &[u8]) -> Option<bool> {
        let target = self.resolver.model.target(self.id(target)?)?;
        Some(target.policies.policies.get(PolicyId::parse(policy)?) == PolicyStatus::New)
    }
}

impl ModelGenexHost<'_, '_> {
    /// Write warnings that `deferred_warnings` kept.
    pub(crate) fn write_warnings(&self, texts: impl IntoIterator<Item = BString>) {
        if let Some(paths) = self.resolver.source_paths() {
            for text in texts {
                paths.emit_rendered_warning_once(text);
            }
        }
    }

    /// Warn that a target file component expression adds a target dependency while
    /// `CMP0112` is not set. CMake issues this optional warning only when
    /// `CMAKE_POLICY_WARNING_CMP0112` is true in the evaluating directory.
    fn warn_cmp0112(&self, id: TargetId) {
        let model = self.resolver.model;
        let (Some(paths), Some(site), Some(target), Some(policy)) = (
            self.resolver.source_paths(),
            self.warning_site.as_ref(),
            model.target(id),
            PolicyId::parse(b"CMP0112"),
        ) else {
            return;
        };
        let unset = target.policies.policies.get(policy) == PolicyStatus::Warn;
        let enabled = model
            .definition(self.lookup_dir, b"CMAKE_POLICY_WARNING_CMP0112")
            .is_some_and(|value| truth::is_on(value));
        if !unset || !enabled {
            return;
        }
        let summary = policy
            .info()
            .map_or("", |info| info.summary)
            .replace("``", "");
        let body = format!(
            "Policy {policy} is not set: {summary}  Run \"cmake --help-policy {policy}\" for policy details.  Use the cmake_policy command to set the policy and suppress this warning.\nDependency being added to target:\n  \"{}\"\n",
            target.name
        );
        paths.warn_once(
            model,
            MessageKind::AuthorWarning,
            Some(policy),
            Location::Backtrace(site.clone()),
            body.into(),
        );
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
