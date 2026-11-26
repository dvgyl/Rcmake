use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use rcmake_base::{BString, list};
use rcmake_diag::{Backtrace, Location, MessageKind};
use rcmake_genex::{ConfigId, EvalContext};
use rcmake_model::{BT, TargetId};

use super::{MemoKey, UsageResolver, handle};
use crate::{LinkKind, ResolveError, lower};

impl UsageResolver<'_> {
    /// Evaluate a target toolchain setting for one configuration and language.
    ///
    /// # Errors
    ///
    /// Returns an error if the target or its generator expression is invalid.
    pub fn target_toolchain_property(
        &mut self,
        id: TargetId,
        config: &[u8],
        language: &[u8],
        property: &[u8],
        default: Option<&BString>,
    ) -> Result<Option<BString>, ResolveError> {
        let target = self.target(id)?;
        let Some(value) = target.props.get(property).or(default) else {
            return Ok(None);
        };
        let handle = handle(id);
        let ctx = EvalContext {
            config: ConfigId(config.into()),
            current: Some(handle),
            head: Some(handle),
            language: Some(language.into()),
            ..EvalContext::default()
        };
        self.eval_ide_property(id, property, value, &ctx).map(Some)
    }

    pub(crate) fn tool_wrappers(
        &mut self,
        id: TargetId,
        prefix: &[u8],
        ctx: &EvalContext,
    ) -> Result<Vec<BString>, ResolveError> {
        let result = self.resolve_wrappers(id, prefix, ctx);
        if result.is_err() {
            // Lowering stops on its first error. Check the other wrapper kind
            // while the target and configuration are still available.
            let mut other = ctx.clone();
            let kind = if prefix == b"COMPILE" {
                let target = self.target(id)?;
                if LinkKind::for_target(target.kind).is_none() {
                    return result;
                }
                let languages = target
                    .sources
                    .iter()
                    .flat_map(|source| list::split(&source.value, list::Empty::Drop))
                    .filter_map(|source| lower::source_language(self.model, target, &source))
                    .collect::<Vec<_>>();
                other.link_language = lower::linker_language(
                    self.model,
                    target,
                    languages.iter().map(|language| language.as_slice()),
                )
                .or_else(|| ctx.language.clone());
                other.language.clone_from(&other.link_language);
                b"LINK".as_slice()
            } else {
                other.link_language = None;
                b"COMPILE"
            };
            let _ = self.resolve_wrappers(id, kind, &other);
        } else if prefix == b"COMPILE"
            && let Some(language) = &ctx.language
        {
            let target = self.target(id)?;
            let is_msvc = lower::definition(
                self.model,
                target.dir,
                b"CMAKE_MSVC_RUNTIME_LIBRARY_DEFAULT",
            )
            .is_some_and(|value| !value.is_empty())
                && [b"_COMPILER_ID".as_slice(), b"_SIMULATE_ID"]
                    .iter()
                    .any(|suffix| {
                        let key = [b"CMAKE_", language.as_slice(), *suffix].concat();
                        lower::definition(self.model, target.dir, &key)
                            .is_some_and(|id| id == b"MSVC")
                    });
            if is_msvc {
                self.validate_msvc_runtime(id, &ctx.config.0, language)?;
            }
        }
        result
    }

    /// Check the target's MSVC runtime setting when runtime selection is active.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsupported runtime value or invalid generator expression.
    pub fn validate_msvc_runtime(
        &mut self,
        id: TargetId,
        config: &[u8],
        language: &[u8],
    ) -> Result<(), ResolveError> {
        let target = self.target(id)?;
        let Some(default) = lower::definition(
            self.model,
            target.dir,
            b"CMAKE_MSVC_RUNTIME_LIBRARY_DEFAULT",
        )
        .filter(|value| !value.is_empty()) else {
            return Ok(());
        };
        let Some(value) = self.target_toolchain_property(
            id,
            config,
            language,
            b"MSVC_RUNTIME_LIBRARY",
            Some(default),
        )?
        else {
            return Ok(());
        };
        if matches!(
            value.as_slice(),
            b"" | b"MultiThreaded"
                | b"MultiThreadedDebug"
                | b"MultiThreadedDLL"
                | b"MultiThreadedDebugDLL"
        ) || lower::definition(
            self.model,
            target.dir,
            &[
                b"CMAKE_",
                language,
                b"_COMPILE_OPTIONS_MSVC_RUNTIME_LIBRARY_",
                &value,
            ]
            .concat(),
        )
        .is_some()
        {
            return Ok(());
        }
        let language = BString::from(language);
        Err(self.toolchain_error(
            &target.backtrace,
            format!("MSVC_RUNTIME_LIBRARY value '{value}' not known for this {language} compiler.")
                .into(),
        ))
    }

    fn toolchain_error(&self, origin: &Backtrace, body: BString) -> ResolveError {
        if let Some(paths) = self.source_paths() {
            paths.emit(
                self.model,
                MessageKind::Error,
                Location::Backtrace(origin.clone()),
                body.clone(),
            );
            ResolveError::Reported(body)
        } else {
            ResolveError::Message(body)
        }
    }

    fn resolve_wrappers(
        &mut self,
        id: TargetId,
        prefix: &[u8],
        ctx: &EvalContext,
    ) -> Result<Vec<BString>, ResolveError> {
        let property = [prefix, b"_WRAPPERS"].concat();
        let ctx = self.property_context(id, &property, ctx)?;
        let target = self.target(id)?;
        let usage = self.walk(id, &property, &ctx)?;
        let mut origins = HashMap::new();
        if usage.contributions.iter().any(|(owner, _)| *owner != id) {
            let (links, _) = self.usage_links(id, false, &ctx)?;
            let interface = [b"INTERFACE_", property.as_slice()].concat();
            for item in links.iter() {
                for dependency in item.edge_targets() {
                    let value = self.walk(dependency, &interface, &ctx)?;
                    for (owner, _) in &value.contributions {
                        origins
                            .entry(*owner)
                            .or_insert_with(|| item.text.backtrace.clone());
                    }
                }
            }
        }
        let mut names = HashSet::new();
        let origins = &origins;
        let mut entries = usage
            .contributions
            .iter()
            .flat_map(|(owner, entries)| {
                entries.iter().map(move |entry| {
                    let origin = if *owner == id {
                        &entry.backtrace
                    } else {
                        origins.get(owner).unwrap_or(&entry.backtrace)
                    };
                    (&entry.value, origin)
                })
            })
            .filter(|(name, _)| names.insert(*name))
            .collect::<Vec<_>>();
        self.log_wrappers(id, prefix, &property, &entries)?;
        entries.sort_by_key(|(name, _)| *name);
        let mut args = Vec::new();
        for (name, origin) in entries {
            let definition = [b"CMAKE_", prefix, b"_WRAPPER_", name.as_slice()].concat();
            let raw = self.model.global.props.get(&definition).filter(|value| {
                !value.is_empty()
                    && value.as_slice() != b"NOTFOUND"
                    && !value.ends_with(b"-NOTFOUND")
            });
            if let Some(raw) = raw {
                let raw = BT::new(raw.clone(), origin.clone());
                let (value, _) = self.eval_entry(id, target.dir, &property, &raw, &ctx, false)?;
                if !value.is_empty() {
                    args.extend(list::split(&value, list::Empty::Keep));
                }
            } else {
                let definition = BString::from(definition);
                let body = BString::from(format!(
                    "Specified wrapper '{name}' for target '{}' does not exist.  Set the GLOBAL property '{definition}'.",
                    target.name
                ));
                return Err(self.toolchain_error(origin, body));
            }
        }
        Ok(args)
    }

    fn log_wrappers(
        &mut self,
        id: TargetId,
        prefix: &[u8],
        property: &[u8],
        entries: &[(&BString, &Backtrace)],
    ) -> Result<(), ResolveError> {
        let Some(paths) = self.source_paths() else {
            return Ok(());
        };
        let target = self.target(id)?;
        if !crate::lower::definition(self.model, target.dir, b"CMAKE_DEBUG_TARGET_PROPERTIES")
            .is_some_and(|value| {
                list::split(value, list::Empty::Drop)
                    .iter()
                    .any(|name| name == property)
            })
        {
            return Ok(());
        }
        // Keep the log marker in the resolution session. The empty config and
        // language fields give all calls for one target and property one key.
        let key = MemoKey {
            target: id,
            property: Cow::Owned([b"\0wrapper-log:", property].concat()),
            config: Cow::Borrowed(b""),
            language: None,
            head: None,
            traversal: Default::default(),
            link_language: None,
            device_link: false,
            property_read: false,
            command_config: None,
            output_config: None,
        };
        if self.memo.contains_key(&key) {
            return Ok(());
        }
        self.memo.insert(key, Default::default());
        let kind = if prefix == b"COMPILE" {
            "compile"
        } else {
            "link"
        };
        let mut groups: Vec<(&Backtrace, Vec<&BString>)> = Vec::new();
        for &(name, origin) in entries {
            if let Some((_, names)) = groups
                .iter_mut()
                .find(|(entry, _)| entry.frames().eq(origin.frames()))
            {
                names.push(name);
            } else {
                groups.push((origin, vec![name]));
            }
        }
        for (origin, names) in groups {
            let mut body = BString::from(format!(
                "Used {kind} wrappers for target {}:\n",
                target.name
            ));
            for name in names {
                body.extend_from_slice(b" * ");
                body.extend_from_slice(name);
                body.push(b'\n');
            }
            paths.emit(
                self.model,
                MessageKind::Log,
                Location::Backtrace(origin.clone()),
                body,
            );
        }
        Ok(())
    }
}
