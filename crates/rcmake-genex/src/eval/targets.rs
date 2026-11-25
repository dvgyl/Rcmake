use std::{cell::RefCell, collections::HashSet, rc::Rc};

use rcmake_base::{BString, list, path_lex::Lexical, truth};

use super::{Call, Evaluator, Value, boolean, unknown};
use crate::{ArtifactKind, ArtifactPart, EvalContext, GenexError, TargetHandle, UsageTraversal};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum FrameKind {
    Property,
    Expression,
    RuntimeDlls,
}

struct PropertyFrame {
    target: Option<TargetHandle>,
    kind: FrameKind,
    key: BString,
    expression: Option<BString>,
    parent: Option<Rc<Self>>,
    depth: usize,
}

thread_local! {
    // A host callback can call evaluate again. Keep its active property chain.
    static ACTIVE: RefCell<Option<Rc<PropertyFrame>>> = const { RefCell::new(None) };
}

pub(super) struct DAGChecker(Option<Rc<PropertyFrame>>);

pub(super) enum DAGEntry {
    Entered(DAGChecker),
    SelfReference,
    CyclicReference,
}

impl DAGChecker {
    pub(super) fn root(ctx: &EvalContext) -> Option<Self> {
        let target = ctx.current.or(ctx.head);
        let property = ctx.site.property.as_ref()?;
        ACTIVE.with(|active| {
            let parent = active.borrow().clone();
            if parent
                .as_ref()
                .is_some_and(|frame| frame.kind != FrameKind::RuntimeDlls)
            {
                return None;
            }
            active.replace(Some(Rc::new(PropertyFrame {
                target,
                kind: FrameKind::Property,
                key: property.clone(),
                expression: None,
                depth: parent.as_ref().map_or(1, |frame| frame.depth + 1),
                parent: parent.clone(),
            })));
            Some(Self(parent))
        })
    }

    pub(super) fn enter(
        target: Option<TargetHandle>,
        kind: FrameKind,
        key: &[u8],
        limit: u32,
    ) -> Result<DAGEntry, GenexError> {
        Self::enter_frame(target, kind, key, None, limit)
    }

    pub(super) fn enter_expression(
        target: TargetHandle,
        key: &[u8],
        expression: BString,
        limit: u32,
    ) -> Result<DAGEntry, GenexError> {
        Self::enter_frame(
            Some(target),
            FrameKind::Expression,
            key,
            Some(expression),
            limit,
        )
    }

    fn enter_frame(
        target: Option<TargetHandle>,
        kind: FrameKind,
        key: &[u8],
        expression: Option<BString>,
        limit: u32,
    ) -> Result<DAGEntry, GenexError> {
        ACTIVE.with(|active| {
            let parent = active.borrow().clone();
            let mut frame = parent.as_deref();
            let mut direct = true;
            while let Some(entry) = frame {
                if entry.target == target && entry.kind == kind && entry.key == key {
                    return Ok(if direct {
                        DAGEntry::SelfReference
                    } else {
                        DAGEntry::CyclicReference
                    });
                }
                direct = false;
                frame = entry.parent.as_deref();
            }
            let depth = parent.as_ref().map_or(1, |entry| entry.depth + 1);
            if depth > limit as usize {
                return Err(GenexError::TooDeep);
            }
            active.replace(Some(Rc::new(PropertyFrame {
                target,
                kind,
                key: key.into(),
                expression,
                depth,
                parent: parent.clone(),
            })));
            Ok(DAGEntry::Entered(Self(parent)))
        })
    }

    pub(super) fn root_property() -> Option<BString> {
        ACTIVE.with(|active| {
            let active = active.borrow();
            let mut frame = active.as_deref();
            let mut property = None;
            while let Some(entry) = frame {
                if entry.kind == FrameKind::Property {
                    property = Some(&entry.key);
                }
                frame = entry.parent.as_deref();
            }
            property.cloned()
        })
    }

    pub(super) fn loop_steps(expression: &BString) -> Vec<BString> {
        ACTIVE.with(|active| {
            let active = active.borrow();
            let mut frame = active.as_deref();
            let mut steps = Vec::new();
            while let Some(entry) = frame {
                steps.push(entry.expression.as_ref().unwrap_or(expression).clone());
                frame = entry.parent.as_deref();
            }
            steps
        })
    }
}

impl Drop for DAGChecker {
    fn drop(&mut self) {
        ACTIVE.with(|active| {
            active.replace(self.0.take());
        });
    }
}

impl Evaluator<'_> {
    pub(super) fn target(&mut self, call: Call<'_>) -> Value {
        match call.name {
            b"TARGET_NAME" => {
                if call
                    .expr
                    .params
                    .iter()
                    .flatten()
                    .flatten()
                    .any(|node| matches!(node, crate::GenexNode::Expr(_)))
                {
                    return Err(call.error("$<TARGET_NAME> expression requires literal input."));
                }
                self.content(call)
            }
            b"TARGET_EXISTS" | b"TARGET_NAME_IF_EXISTS" => {
                let [name] = self.fixed(call)?;
                if !valid_target(&name) {
                    return Err(call.error(format!(
                        "$<{}:tgt> expression requires a non-empty valid target name.",
                        BString::from(call.name)
                    )));
                }
                self.flags.context_sensitive = true;
                let exists = self.host.find_target_in_context(&name, call.ctx).is_some();
                if call.name == b"TARGET_EXISTS" {
                    return Ok(boolean(exists));
                }
                Ok(if exists { name } else { BString::default() })
            }
            b"TARGET_POLICY" => self.policy(call),
            b"TARGET_PROPERTY" => self.property(call),
            b"TARGET_OBJECTS" => self.objects(call),
            b"TARGET_RUNTIME_DLLS" | b"TARGET_RUNTIME_DLL_DIRS" => self.runtime_dlls(call),
            b"TARGET_INTERMEDIATE_DIR" => {
                let [name] = self.fixed(call)?;
                if !valid_target(&name) {
                    return Err(call.error("Target name not supported."));
                }
                let target = self.require_target(call, &name)?;
                self.record_lookup(&name, target);
                self.host.target_intermediate_directory(target, &call.ctx.config).ok_or_else(|| {
                    call.error("The evaluation of the TARGET_INTERMEDIATE_DIR generator expression is only suitable for consumption by CMake (limited under Xcode with multiple architectures).  It is not suitable for writing out elsewhere.")
                })
            }
            _ => self.artifact(call),
        }
    }

    pub(super) fn require_target(
        &mut self,
        call: Call<'_>,
        name: &[u8],
    ) -> Result<TargetHandle, GenexError> {
        self.flags.context_sensitive = true;
        self.host
            .find_target_in_context(name, call.ctx)
            .ok_or_else(|| {
                let name = BString::from(name);
                call.error(match call.name {
                    b"TARGET_PROPERTY" | b"TARGET_INTERMEDIATE_DIR" => {
                        format!("Target \"{name}\" not found.")
                    }
                    b"TARGET_OBJECTS" | b"TARGET_RUNTIME_DLLS" | b"TARGET_RUNTIME_DLL_DIRS" => {
                        format!(
                            "Objects of target \"{name}\" referenced but no such target exists."
                        )
                    }
                    _ => format!("No target \"{name}\""),
                })
            })
    }

    fn policy(&mut self, call: Call<'_>) -> Value {
        let [policy] = self.fixed(call)?;
        self.flags.head_sensitive = true;
        let Some(head) = call.ctx.head else {
            return Err(call.error(
                "$<TARGET_POLICY:prop> may only be used with binary targets.  It may not be used with add_custom_command or add_custom_target.",
            ));
        };
        if !TARGET_POLICIES.contains(&policy.as_slice()) {
            let mut message = String::from(
                "$<TARGET_POLICY:prop> may only be used with a limited number of policies.  Currently it may be used with the following policies:",
            );
            for policy in TARGET_POLICIES {
                message.push_str(&format!("\n * {}", BString::from(*policy)));
            }
            message.push('\n');
            return Err(call.error(message));
        }
        Ok(boolean(
            self.host.target_policy(head, &policy).unwrap_or(false),
        ))
    }

    pub(super) fn source_property(&mut self, call: Call<'_>) -> Value {
        let arguments = self.arguments(call)?;
        let (source, scope, property) = match arguments.as_slice() {
            [source, property] => (source, None, property),
            [source, option, property] => (
                source,
                Some(super::properties::source_scope(call, option)?),
                property,
            ),
            [] | [_] => {
                return Err(
                    call.error("$<SOURCE_PROPERTY> expression requires at least two parameters.")
                );
            }
            _ => {
                return Err(call.error(
                    "$<SOURCE_PROPERTY:...> expression requires at most three parameters.",
                ));
            }
        };
        if source.is_empty() {
            return Err(call.error(
                "$<SOURCE_PROPERTY:src,prop> expression requires a non-empty source name.",
            ));
        }
        if property.is_empty() {
            return Err(call.error(
                "$<SOURCE_PROPERTY:src,prop> expression requires a non-empty property name.",
            ));
        }
        super::properties::validate_property(call, property)?;
        self.flags.context_sensitive = true;
        let value = self
            .host
            .source_property(source, scope, property, call.ctx)
            .map_err(|error| call.error(error.to_string()))?;
        if !matches!(
            property.as_slice(),
            b"INCLUDE_DIRECTORIES"
                | b"COMPILE_DEFINITIONS"
                | b"COMPILE_OPTIONS"
                | b"COMPILE_FLAGS"
                | b"OBJECT_OUTPUTS"
                | b"VS_DEPLOYMENT_CONTENT"
                | b"VS_SETTINGS"
        ) {
            return Ok(value);
        }
        let key = crate::parse::render_expr(call.expr);
        let DAGEntry::Entered(_active) = DAGChecker::enter(
            call.ctx.head,
            FrameKind::Property,
            &key,
            self.limits.max_recursion,
        )?
        else {
            return Err(call.error("A source property depends on itself."));
        };
        let parsed = crate::parse(&value)?;
        let evaluated = self.nodes(&parsed.nodes, call.ctx)?;
        Ok(list::join(list::split(&evaluated, list::Empty::Drop)))
    }

    pub(super) fn self_reference(&self, target: Option<TargetHandle>) -> GenexError {
        GenexError::SelfReference {
            target: target.map_or_else(BString::default, |target| self.host.target_name(target)),
        }
    }

    fn artifact(&mut self, call: Call<'_>) -> Value {
        let Some((kind, part)) = artifact_operator(call.name) else {
            return Err(unknown(call));
        };
        let operator = BString::from(call.name);
        let args = self.arguments(call)?;
        let (name, postfix) = match (part, args.as_slice()) {
            (_, [name]) => (name, None),
            (ArtifactPart::BaseName, [name, postfix]) => (name, Some(postfix)),
            (ArtifactPart::BaseName, []) => {
                return Err(call.error(format!(
                    "$<{operator}> expression requires at least one parameter."
                )));
            }
            (ArtifactPart::BaseName, _) => {
                return Err(call.error("Unexpected parameters, require one or two parameters."));
            }
            _ => return Err(call.arity(1)),
        };
        if !valid_target(name) {
            return Err(call.error("Expression syntax not recognized."));
        }
        let target = self.require_target(call, name)?;
        self.record_lookup(name, target);
        let target_type = self.host.raw_property(target, b"TYPE");
        let target_type = target_type.as_ref().map(|kind| kind.as_slice());
        let diagnostic_operator = artifact_diagnostic_operator(call.name, kind, part);
        self.validate_artifact(call, name, target, target_type, kind, &diagnostic_operator)?;

        if kind == ArtifactKind::LinkerLibraryFile
            && target_type != Some(b"STATIC_LIBRARY")
            && self.dll_platform()
        {
            return Ok(BString::default());
        }

        let exclude_postfix = match postfix.map(|value| value.as_slice()) {
            None | Some(b"POSTFIX:INCLUDE") => false,
            Some(b"POSTFIX:EXCLUDE") => true,
            Some(_) => {
                return Err(call.error(
                    "Wrong second parameter: \"POSTFIX:INCLUDE\" or \"POSTFIX:EXCLUDE\" is expected",
                ));
            }
        };
        let config = &call.ctx.config;
        let read_part = |kind| {
            if exclude_postfix {
                // Check evaluation errors before stripping the postfix.
                self.host.checked_artifact(target, kind, part, config)?;
                Ok(self
                    .host
                    .artifact_base_name_without_postfix(target, kind, config))
            } else {
                self.host.checked_artifact(target, kind, part, config)
            }
        };
        let mut value = read_part(kind)?;
        if value.is_none()
            && ((matches!(
                kind,
                ArtifactKind::LinkerFile | ArtifactKind::LinkerLibraryFile
            ) && target_type == Some(b"MODULE_LIBRARY"))
                || (kind == ArtifactKind::SonameFile && self.target_option(target, b"NO_SONAME")))
        {
            value = read_part(ArtifactKind::File)?;
        }
        if value.is_none()
            && kind == ArtifactKind::SonameFile
            && self.target_option(target, b"IMPORTED")
            && let Some(dir) =
                self.host
                    .checked_artifact(target, ArtifactKind::File, ArtifactPart::Dir, config)?
        {
            return Ok(match part {
                ArtifactPart::Full => [dir.as_slice(), b"/"].concat().into(),
                ArtifactPart::Name => BString::default(),
                ArtifactPart::Dir => Lexical::new(&dir).parent().into(),
                _ => BString::default(),
            });
        }
        if let Some(value) = value {
            return Ok(value);
        }
        match kind {
            ArtifactKind::ImportFile
            | ArtifactKind::LinkerImportFile
            | ArtifactKind::SonameImportFile => Ok(BString::default()),
            ArtifactKind::PdbFile => Err(call.error(format!(
                "{diagnostic_operator} is not supported by the target linker."
            ))),
            ArtifactKind::BundleDir | ArtifactKind::BundleContentDir => {
                Err(call.error(format!("{operator} is allowed only for Bundle targets.")))
            }
            _ if self.target_option(target, b"IMPORTED") => {
                if self.host.target_policy(target, b"CMP0111") != Some(true) {
                    return Ok(format!("{name}-NOTFOUND").into());
                }
                let property = if target_type == Some(b"SHARED_LIBRARY") {
                    "IMPORTED_LOCATION or IMPORTED_IMPLIB"
                } else {
                    "IMPORTED_LOCATION"
                };
                let configuration = if config.0.is_empty() {
                    String::new()
                } else {
                    format!(" configuration \"{}\"", config.0)
                };
                Err(GenexError::Generation {
                    message: format!(
                        "{property} not set for imported target \"{name}\"{configuration}."
                    )
                    .into(),
                })
            }
            _ => Err(call.error(format!(
                "Target \"{name}\" has no artifact for $<{operator}>."
            ))),
        }
    }

    fn validate_artifact(
        &self,
        call: Call<'_>,
        name: &BString,
        target: TargetHandle,
        target_type: Option<&[u8]>,
        kind: ArtifactKind,
        operator: &BString,
    ) -> Result<(), GenexError> {
        use ArtifactKind::*;
        if target_type.is_some_and(|kind| {
            !matches!(
                kind,
                b"EXECUTABLE"
                    | b"STATIC_LIBRARY"
                    | b"SHARED_LIBRARY"
                    | b"MODULE_LIBRARY"
                    | b"UNKNOWN_LIBRARY"
            )
        }) {
            return Err(call.error(format!(
                "Target \"{name}\" is not an executable or library."
            )));
        }
        if call.ctx.head == Some(target)
            && matches!(
                call.ctx.site.property.as_deref().map(Vec::as_slice),
                Some(b"LINK_LIBRARIES" | b"SOURCES" | b"INTERFACE_SOURCES")
            )
        {
            return Err(call.error(
                "Expressions which require the linker language may not be used while evaluating link libraries",
            ));
        }
        let message = match kind {
            PdbFile | BundleDir | BundleContentDir if self.target_option(target, b"IMPORTED") => {
                "not allowed for IMPORTED targets."
            }
            LinkerFile | LinkerImportFile
                if target_type == Some(b"EXECUTABLE")
                    && !self.target_option(target, b"ENABLE_EXPORTS") =>
            {
                "is allowed only for libraries and executables with ENABLE_EXPORTS."
            }
            LinkerLibraryFile if target_type == Some(b"EXECUTABLE") => {
                "is allowed only for libraries with ENABLE_EXPORTS."
            }
            SonameFile | SonameImportFile
                if target_type.is_some_and(|kind| kind != b"SHARED_LIBRARY") =>
            {
                "is allowed only for SHARED libraries."
            }
            PdbFile => {
                let language = self
                    .host
                    .raw_property(target, b"LINKER_LANGUAGE")
                    .or_else(|| {
                        call.ctx
                            .link_language
                            .clone()
                            .filter(|_| call.ctx.current == Some(target))
                    });
                let supported = language.as_ref().and_then(|language| {
                    self.host
                        .definition(format!("CMAKE_{language}_LINKER_SUPPORTS_PDB").as_bytes())
                });
                if supported
                    .as_ref()
                    .map_or_else(|| !self.dll_platform(), |value| !truth::is_on(value))
                {
                    "is not supported by the target linker."
                } else if target_type == Some(b"STATIC_LIBRARY") {
                    "is allowed only for targets with linker created artifacts."
                } else {
                    return Ok(());
                }
            }
            _ => return Ok(()),
        };
        Err(call.error(format!("{operator} {message}")))
    }

    fn target_option(&self, target: TargetHandle, name: &[u8]) -> bool {
        self.host
            .raw_property(target, name)
            .is_some_and(|value| truth::is_on(&value))
    }

    fn dll_platform(&self) -> bool {
        self.host
            .definition(b"CMAKE_IMPORT_LIBRARY_SUFFIX")
            .is_some_and(|suffix| !suffix.is_empty())
    }

    fn runtime_dlls(&mut self, call: Call<'_>) -> Value {
        let [name] = self.fixed(call)?;
        let target = self.require_target(call, &name)?;
        if self.host.raw_property(target, b"TYPE").is_some_and(|kind| {
            !matches!(
                kind.as_slice(),
                b"EXECUTABLE" | b"SHARED_LIBRARY" | b"MODULE_LIBRARY"
            )
        }) {
            return Err(call.error(format!(
                "Objects of target \"{name}\" referenced but is not one of the allowed target types (EXECUTABLE, SHARED, MODULE)."
            )));
        }
        let _active = match DAGChecker::enter(
            Some(target),
            FrameKind::RuntimeDlls,
            b"",
            self.limits.max_recursion,
        )? {
            DAGEntry::Entered(active) => active,
            DAGEntry::SelfReference | DAGEntry::CyclicReference => {
                return Err(GenexError::SourcesCycle { target: name });
            }
        };
        let ctx = EvalContext {
            head: Some(target),
            current: Some(target),
            traversal: UsageTraversal::Link,
            ..call.ctx.clone()
        };
        // CMake checks link dependencies even when the platform has no DLLs.
        let targets = self.host.link_closure(target, &ctx)?;
        if !self.dll_platform() {
            return Ok(BString::default());
        }
        let mut seen_targets = HashSet::new();
        let mut seen_dirs = HashSet::new();
        let mut paths = BString::default();
        let part = if call.name == b"TARGET_RUNTIME_DLL_DIRS" {
            ArtifactPart::Dir
        } else {
            ArtifactPart::Full
        };
        for dependency in targets {
            if !seen_targets.insert(dependency)
                || !self
                    .host
                    .raw_property(dependency, b"TYPE")
                    .is_some_and(|kind| kind == "SHARED_LIBRARY")
            {
                continue;
            }
            let Some(path) =
                self.host
                    .checked_artifact(dependency, ArtifactKind::File, part, &ctx.config)?
            else {
                continue;
            };
            self.charge(path.len().max(1))?;
            if !path.is_empty() && (part == ArtifactPart::Full || seen_dirs.insert(path.clone())) {
                if !paths.is_empty() {
                    self.append(&mut paths, b";")?;
                }
                self.append(&mut paths, &path)?;
            }
        }
        Ok(paths)
    }

    fn objects(&mut self, call: Call<'_>) -> Value {
        let args = self.arguments(call)?;
        let [name, options @ ..] = args.as_slice() else {
            return Err(call.error("$<TARGET_OBJECTS> expression requires at least one parameter."));
        };
        let target = self.require_target(call, name)?;
        let invalid_type = || {
            call.error(format!(
                "Objects of target \"{name}\" referenced but is not one of the allowed target types (EXECUTABLE, STATIC, SHARED, MODULE, OBJECT)."
            ))
        };
        let target_type = self.host.raw_property(target, b"TYPE");
        if target_type.as_ref().is_some_and(|kind| {
            !matches!(
                kind.as_slice(),
                b"EXECUTABLE"
                    | b"STATIC_LIBRARY"
                    | b"SHARED_LIBRARY"
                    | b"MODULE_LIBRARY"
                    | b"OBJECT_LIBRARY"
            )
        }) {
            return Err(invalid_type());
        }
        let config = &call.ctx.config;
        let imported = self.target_option(target, b"IMPORTED");
        if options.is_empty() {
            if imported
                && target_type
                    .as_ref()
                    .is_some_and(|kind| kind != "OBJECT_LIBRARY")
            {
                return Ok(self
                    .host
                    .checked_artifact(target, ArtifactKind::File, ArtifactPart::Full, config)?
                    .unwrap_or_default());
            }
            return self
                .host
                .target_objects(target, config, None)
                .ok_or_else(invalid_type);
        }

        let mut sources = BString::default();
        for (index, option) in options.iter().enumerate() {
            let filter = option
                .strip_prefix(b"SOURCE_FILES:")
                .ok_or_else(|| call.error(format!("Unrecognized argument:\n  {option}")))?;
            if index != 0 {
                self.append(&mut sources, b";")?;
            }
            self.append(&mut sources, filter)?;
        }
        if imported {
            return Err(call.error(format!(
                "Cannot use SOURCE_FILES argument on imported target \"{name}\""
            )));
        }
        self.validate_object_sources(call, target, name, &sources)?;
        self.host
            .target_objects(target, config, Some(&sources))
            .ok_or_else(invalid_type)
    }

    fn validate_object_sources(
        &mut self,
        call: Call<'_>,
        target: TargetHandle,
        name: &BString,
        sources: &[u8],
    ) -> Result<(), GenexError> {
        let ctx = EvalContext {
            head: Some(target),
            current: Some(target),
            ..call.ctx.clone()
        };
        let invalid_source = |source: &[u8], message: &str| {
            call.error(format!(
                "Source file:\n  {}\n{message}",
                BString::from(source)
            ))
        };
        let mut filters = Vec::new();
        list::split_with_empty_input(
            sources,
            list::Empty::Keep,
            list::EmptyInput::OneEmptyElement,
            &mut filters,
        );
        for path in &filters {
            if !Lexical::new(path).absolute() {
                return Err(invalid_source(path, "is not an absolute path"));
            }
        }
        let source_dir = self
            .host
            .raw_property(target, b"SOURCE_DIR")
            .unwrap_or_default();
        let binary_dir = self
            .host
            .raw_property(target, b"BINARY_DIR")
            .unwrap_or_default();
        let view = self.host.transitive_property(target, b"SOURCES", &ctx)?;
        self.flags = self.flags.union(view.flags);
        self.charge(view.value.len())?;
        let mut known = HashSet::new();
        for path in list::iter(&view.value, list::Empty::Drop) {
            for dir in [&source_dir, &binary_dir] {
                let path = Lexical::new(dir).append(&path);
                known.insert(Lexical::new(&path).normalize());
            }
        }
        for path in filters {
            if known.contains(&Lexical::new(&path).normalize()) {
                continue;
            }
            // The host can also supply objects from file sets and generated sources.
            let value = self.host.target_objects(target, &ctx.config, Some(&path));
            if value.is_none_or(|objects| objects.is_empty()) {
                return Err(invalid_source(
                    &path,
                    &format!("does not exist for target \"{name}\""),
                ));
            }
        }
        Ok(())
    }
}

pub(super) fn valid_target(name: &[u8]) -> bool {
    !name.is_empty()
        && name
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.+:-".contains(byte))
}

fn artifact_operator(name: &[u8]) -> Option<(ArtifactKind, ArtifactPart)> {
    use ArtifactKind::*;
    use ArtifactPart::*;

    let kinds: &[(&[u8], ArtifactKind)] = &[
        (b"TARGET_FILE", File),
        (b"TARGET_LINKER_FILE", LinkerFile),
        (b"TARGET_LINKER_LIBRARY_FILE", LinkerLibraryFile),
        (b"TARGET_LINKER_IMPORT_FILE", LinkerImportFile),
        (b"TARGET_SONAME_FILE", SonameFile),
        (b"TARGET_SONAME_IMPORT_FILE", SonameImportFile),
        (b"TARGET_PDB_FILE", PdbFile),
        (b"TARGET_BUNDLE_DIR", BundleDir),
        (b"TARGET_BUNDLE_CONTENT_DIR", BundleContentDir),
        (b"TARGET_IMPORT_FILE", ImportFile),
    ];

    kinds.iter().find_map(|&(prefix, kind)| {
        let part = match name.strip_prefix(prefix)? {
            b"" => Full,
            b"_DIR" if !matches!(kind, BundleDir | BundleContentDir) => Dir,
            b"_NAME" if kind != BundleContentDir => Name,
            b"_BASE_NAME"
                if !matches!(
                    kind,
                    BundleDir | BundleContentDir | SonameFile | SonameImportFile
                ) =>
            {
                BaseName
            }
            b"_PREFIX"
                if matches!(
                    kind,
                    File | LinkerFile | LinkerLibraryFile | LinkerImportFile | ImportFile
                ) =>
            {
                Prefix
            }
            b"_SUFFIX"
                if matches!(
                    kind,
                    File | LinkerFile | LinkerLibraryFile | LinkerImportFile | ImportFile
                ) =>
            {
                Suffix
            }
            _ => return None,
        };
        Some((kind, part))
    })
}

fn artifact_diagnostic_operator(name: &[u8], kind: ArtifactKind, part: ArtifactPart) -> BString {
    if !matches!(
        kind,
        ArtifactKind::BundleDir | ArtifactKind::BundleContentDir
    ) {
        match part {
            ArtifactPart::Dir => return name.strip_suffix(b"_DIR").unwrap_or(name).into(),
            ArtifactPart::Name => return name.strip_suffix(b"_NAME").unwrap_or(name).into(),
            _ => {}
        }
    }
    name.into()
}

const TARGET_POLICIES: &[&[u8]] = &[
    b"CMP0003", b"CMP0004", b"CMP0008", b"CMP0020", b"CMP0021", b"CMP0022", b"CMP0027", b"CMP0037",
    b"CMP0038", b"CMP0041", b"CMP0042", b"CMP0046", b"CMP0052", b"CMP0060", b"CMP0063", b"CMP0065",
    b"CMP0068", b"CMP0069", b"CMP0073", b"CMP0076", b"CMP0081", b"CMP0083", b"CMP0095", b"CMP0099",
    b"CMP0104", b"CMP0105", b"CMP0108", b"CMP0112", b"CMP0113", b"CMP0119", b"CMP0131", b"CMP0142",
    b"CMP0154", b"CMP0155", b"CMP0156", b"CMP0157", b"CMP0160", b"CMP0162", b"CMP0179", b"CMP0181",
    b"CMP0182", b"CMP0195", b"CMP0199", b"CMP0200", b"CMP0202", b"CMP0203", b"CMP0204", b"CMP0209",
    b"CMP0210", b"CMP0211", b"CMP0214", b"CMP0215", b"CMP0216", b"CMP0226",
];
