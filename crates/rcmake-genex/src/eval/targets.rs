use std::{cell::RefCell, rc::Rc};

use rcmake_base::{BString, list};

use super::{Call, Evaluator, Value, boolean, unknown};
use crate::{ArtifactKind, ArtifactPart, EvalContext, GenexError, TargetHandle};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum FrameKind {
    Property,
    Expression,
}

struct PropertyFrame {
    target: Option<TargetHandle>,
    kind: FrameKind,
    key: BString,
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
        let target = ctx.current.or(ctx.head)?;
        let property = ctx.site.property.as_ref()?;
        ACTIVE.with(|active| {
            let parent = active.borrow().clone();
            if parent.is_some() {
                return None;
            }
            active.replace(Some(Rc::new(PropertyFrame {
                target: Some(target),
                kind: FrameKind::Property,
                key: property.clone(),
                depth: 1,
                parent: None,
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
                depth,
                parent: parent.clone(),
            })));
            Ok(DAGEntry::Entered(Self(parent)))
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
            b"TARGET_INTERMEDIATE_DIR" => {
                let [name] = self.fixed(call)?;
                if !valid_target(&name) {
                    return Err(call.error("Target name not supported."));
                }
                let target = self.require_target(call, &name)?;
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
                    b"TARGET_OBJECTS" => format!(
                        "Objects of target \"{name}\" referenced but no such target exists."
                    ),
                    _ => format!("No target \"{name}\""),
                })
            })
    }

    fn policy(&mut self, call: Call<'_>) -> Value {
        let [policy] = self.fixed(call)?;
        self.flags.head_sensitive = true;
        let Some(head) = call.ctx.head else {
            return Err(call.error("$<TARGET_POLICY:prop> may only be used with binary targets."));
        };
        if !TARGET_POLICIES.contains(&policy.as_slice()) {
            let mut message = String::from(
                "$<TARGET_POLICY:prop> may only be used with a limited number of policies.  Currently it may be used with the following policies:",
            );
            for policy in TARGET_POLICIES {
                message.push_str(&format!("\n * {}", BString::from(*policy)));
            }
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
            [source, option, property] => {
                let scope = option
                    .strip_prefix(b"DIRECTORY:")
                    .map(|value| (b"DIRECTORY".as_slice(), value))
                    .or_else(|| {
                        option
                            .strip_prefix(b"TARGET_DIRECTORY:")
                            .map(|value| (b"TARGET_DIRECTORY".as_slice(), value))
                    })
                    .ok_or_else(|| {
                        call.error("Invalid option.  DIRECTORY: or TARGET_DIRECTORY: expected.")
                    })?;
                if scope.1.is_empty() {
                    return Err(call.error(format!(
                        "No value provided for the {}: option.",
                        BString::from(scope.0)
                    )));
                }
                (source, Some(scope), property)
            }
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
        if !property
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            return Err(call.error("Property name not supported."));
        }
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

    fn property(&mut self, call: Call<'_>) -> Value {
        let arguments = self.arguments(call)?;
        let (target, property) = match arguments.as_slice() {
            [property] => {
                self.flags.head_sensitive = true;
                let target = call.ctx.head.ok_or_else(|| call.error(
                    "$<TARGET_PROPERTY:prop> may only be used with binary targets. It may not be used with add_custom_command or add_custom_target. Specify the target name $<TARGET_PROPERTY:tgt,prop>.",
                ))?;
                (target, property)
            }
            [name, property] => {
                if name.is_empty() {
                    return Err(call.error(
                        "$<TARGET_PROPERTY:...> expression requires a non-empty target name.",
                    ));
                }
                if !valid_target(name) {
                    return Err(call.error("Target name not supported."));
                }
                if let Some(value) = self.host.alias_property(name, property, call.ctx) {
                    self.flags.context_sensitive = true;
                    return Ok(value);
                }
                (self.require_target(call, name)?, property)
            }
            _ => {
                return Err(
                    call.error("$<TARGET_PROPERTY> expression requires one or two parameters.")
                );
            }
        };
        if property.is_empty() {
            return Err(
                call.error("$<TARGET_PROPERTY:...> expression requires a non-empty property name.")
            );
        }
        if !property
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            return Err(call.error("Property name not supported."));
        }
        if !self.transitive(target, property) {
            let view = self.host.scalar_property(target, property, call.ctx)?;
            self.flags = self.flags.union(view.flags);
            return Ok(view.value);
        }

        let limit = self.limits.max_recursion;
        let _active = match DAGChecker::enter(Some(target), FrameKind::Property, property, limit)? {
            DAGEntry::Entered(active) => active,
            DAGEntry::SelfReference
                if call.ctx.site.property.is_none()
                    || (call.ctx.current == Some(target)
                        && call.ctx.site.property.as_ref() == Some(property)) =>
            {
                let error = self.self_reference(Some(target));
                return Err(if call.ctx.site.backtrace.is_empty() {
                    error
                } else {
                    call.error(error.to_string())
                });
            }
            DAGEntry::SelfReference | DAGEntry::CyclicReference => return Ok(BString::default()),
        };
        let view = self.host.transitive_property(target, property, call.ctx)?;
        self.flags = self.flags.union(view.flags);
        Ok(view.value)
    }

    pub(super) fn self_reference(&self, target: Option<TargetHandle>) -> GenexError {
        GenexError::SelfReference {
            target: target.map_or_else(BString::default, |target| self.host.target_name(target)),
        }
    }

    fn transitive(&self, target: TargetHandle, property: &[u8]) -> bool {
        let base = property.strip_prefix(b"INTERFACE_").unwrap_or(property);
        matches!(
            base,
            b"AUTOMOC_MACRO_NAMES"
                | b"AUTOUIC_OPTIONS"
                | b"COMPILE_DEFINITIONS"
                | b"COMPILE_FEATURES"
                | b"COMPILE_OPTIONS"
                | b"INCLUDE_DIRECTORIES"
                | b"LINK_DEPENDS"
                | b"LINK_DIRECTORIES"
                | b"LINK_LIBRARIES"
                | b"LINK_LIBRARIES_DIRECT"
                | b"LINK_LIBRARIES_DIRECT_EXCLUDE"
                | b"LINK_OPTIONS"
                | b"LINK_WRAPPER"
                | b"PRECOMPILE_HEADERS"
                | b"SOURCES"
                | b"SYSTEM_INCLUDE_DIRECTORIES"
        ) || [
            b"TRANSITIVE_COMPILE_PROPERTIES".as_slice(),
            b"TRANSITIVE_LINK_PROPERTIES",
        ]
        .iter()
        .filter_map(|key| self.host.raw_property(target, key))
        .any(|names| {
            list::split(&names, list::Empty::Drop)
                .iter()
                .any(|name| name == base)
        })
    }

    fn artifact(&mut self, call: Call<'_>) -> Value {
        let Some((kind, part)) = artifact_operator(call.name) else {
            return Err(unknown(call));
        };
        let args = self.arguments(call)?;
        let (name, exclude_postfix) = match args.as_slice() {
            [name] => (name, false),
            [name, postfix] if part == ArtifactPart::BaseName => match postfix.as_slice() {
                b"POSTFIX:INCLUDE" => (name, false),
                b"POSTFIX:EXCLUDE" => (name, true),
                _ => return Err(call.error("The POSTFIX option must be INCLUDE or EXCLUDE.")),
            },
            _ => return Err(call.arity(1)),
        };
        let target = self.require_target(call, name)?;
        let config = &call.ctx.config;
        let value = if exclude_postfix {
            self.host
                .artifact_base_name_without_postfix(target, kind, config)
        } else {
            self.host.checked_artifact(target, kind, part, config)?
        };
        value.ok_or_else(|| {
            call.error(format!(
                "Target \"{name}\" has no artifact for $<{}>.",
                BString::from(call.name)
            ))
        })
    }

    fn objects(&mut self, call: Call<'_>) -> Value {
        let args = self.arguments(call)?;
        let (name, sources) = match args.as_slice() {
            [name] => (name, None),
            [name, filter] => {
                let sources = filter
                    .strip_prefix(b"SOURCE_FILES:")
                    .ok_or_else(|| call.error("Expected the SOURCE_FILES option."))?;
                (name, Some(sources))
            }
            _ => return Err(call.arity(1)),
        };
        let target = self.require_target(call, name)?;
        self.host
            .target_objects(target, &call.ctx.config, sources)
            .ok_or_else(|| {
                call.error(format!(
                    "Objects of target \"{name}\" referenced but is not one of the allowed target types (EXECUTABLE, STATIC, SHARED, MODULE, OBJECT)."
                ))
            })
    }
}

fn valid_target(name: &[u8]) -> bool {
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

const TARGET_POLICIES: &[&[u8]] = &[
    b"CMP0003", b"CMP0004", b"CMP0008", b"CMP0020", b"CMP0021", b"CMP0022", b"CMP0027", b"CMP0037",
    b"CMP0038", b"CMP0041", b"CMP0042", b"CMP0046", b"CMP0052", b"CMP0060", b"CMP0063", b"CMP0065",
    b"CMP0068", b"CMP0069", b"CMP0073", b"CMP0076", b"CMP0081", b"CMP0083", b"CMP0095", b"CMP0099",
    b"CMP0104", b"CMP0105", b"CMP0108", b"CMP0112", b"CMP0113", b"CMP0119", b"CMP0131", b"CMP0142",
    b"CMP0154", b"CMP0155", b"CMP0156", b"CMP0157", b"CMP0160", b"CMP0162", b"CMP0179", b"CMP0181",
    b"CMP0182", b"CMP0195", b"CMP0199", b"CMP0200", b"CMP0202", b"CMP0203", b"CMP0204", b"CMP0209",
    b"CMP0210", b"CMP0211", b"CMP0214", b"CMP0215", b"CMP0216", b"CMP0226",
];
