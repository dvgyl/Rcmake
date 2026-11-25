//! Install scripts for named runtime dependency sets.

use std::collections::BTreeSet;

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{list, path_lex::Lexical};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_genex::{ArtifactKind, ArtifactPart};
use rcmake_model::RuntimeDependencyInstall;

use super::{
    BString, Backtrace, ConfigId, DirId, InstallAction, InstallActionKind, InstallKind,
    ProjectModel, ResolvedTarget, TargetId, TargetKind, UsageResolver, action,
    cmake_quoted_content, definition, matches_config, recorded_args,
};

pub(super) fn lower<E>(
    model: &ProjectModel,
    targets: &[ResolvedTarget],
    configs: &[ConfigId],
    set: &RuntimeDependencyInstall,
    eval: &mut impl FnMut(DirId, &ConfigId, &Backtrace, &[u8]) -> Result<BString, E>,
    actions: &mut Vec<InstallAction>,
) -> Result<(), E> {
    let members = members(model, &set.name);
    let default = ConfigId::default();
    let multi = configs.len() > 1 || model.generator.is_some_and(|kind| kind.multi_config());
    let configs = if configs.is_empty() {
        std::slice::from_ref(&default)
    } else {
        configs
    };
    let usage = UsageResolver::new(model);
    let apple = set.framework.is_some();
    let dll = super::super::artifacts::is_dll_platform(model, set.dir);
    for config in configs {
        let groups = std::iter::once((false, &set.library))
            .chain(set.framework.iter().map(|options| (true, options)))
            .filter(|(_, options)| matches_config(options, config));
        let inputs = Inputs::new(model, targets, &usage, &members, config);
        if inputs.files.iter().all(Vec::is_empty) {
            continue;
        }
        let mut evaluate = |value: &[u8]| eval(set.dir, config, &set.backtrace, value);
        let scan = inputs.scan(model, set, &mut evaluate)?;
        for (framework, options) in groups {
            let key = if dll {
                b"CMAKE_INSTALL_BINDIR".as_slice()
            } else {
                b"CMAKE_INSTALL_LIBDIR"
            };
            let default = if framework {
                b"".as_slice()
            } else {
                definition(model, set.dir, key)
                    .filter(|path| !path.is_empty())
                    .map_or(if dll { b"bin".as_slice() } else { b"lib" }, |path| {
                        path.as_slice()
                    })
            };
            let destination = evaluate(
                options
                    .destination
                    .as_deref()
                    .map_or(default, |v| v.as_ref()),
            )?;
            let mut scan = action(
                model,
                set.dir,
                options,
                b"",
                InstallActionKind::Code(scan.clone()),
            );
            let copy = copy_code(&destination, &options.permissions, apple, framework, dll);
            if multi {
                scan.configurations = vec![config.0.clone()];
            }
            let mut copy_action = scan.clone();
            copy_action.kind = InstallActionKind::Code(copy);
            actions.extend([scan, copy_action]);
        }
    }
    Ok(())
}

fn members(model: &ProjectModel, name: &[u8]) -> IndexSet<TargetId> {
    let mut members = IndexSet::new();
    for (index, rule) in model.installs.iter().enumerate() {
        let args = recorded_args(model, index);
        if !args
            .windows(2)
            .any(|pair| pair == [b"RUNTIME_DEPENDENCY_SET", name])
        {
            continue;
        }
        if let InstallKind::Targets { targets, .. } = &rule.kind {
            members.extend(targets);
        } else if let [b"IMPORTED_RUNTIME_ARTIFACTS", target, ..] = args.as_slice() {
            members.extend(model.find_target(rule.dir, target));
        }
    }
    members
}

#[derive(Default)]
struct Inputs {
    files: [Vec<BString>; 3],
    bundle: Option<BString>,
    excluded: BTreeSet<BString>,
}

impl Inputs {
    fn new(
        model: &ProjectModel,
        targets: &[ResolvedTarget],
        usage: &UsageResolver<'_>,
        members: &IndexSet<TargetId>,
        config: &ConfigId,
    ) -> Self {
        let mut inputs = Self::default();
        let facts: IndexMap<_, _> = targets
            .iter()
            .filter(|target| target.config == *config)
            .map(|target| (target.id, target))
            .collect();
        for &id in members {
            let Some(target) = model.target(id) else {
                continue;
            };
            let Some(group) = group(target.kind) else {
                continue;
            };
            let Some(file) = facts
                .get(&id)
                .and_then(|target| target.artifacts.file.clone())
                .or_else(|| usage.artifact(id, ArtifactKind::File, ArtifactPart::Full, config))
            else {
                continue;
            };
            if matches!(
                crate::ide::bundle_kind(target),
                crate::ide::BundleKind::App { .. }
            ) {
                inputs.bundle = Some(file.clone());
            }
            if let Some(files) = inputs.files.get_mut(group) {
                files.push(file.clone());
            }
            if let Some(target) = facts.get(&id) {
                inputs.excluded.insert(file);
                let mut pending = target.dependencies.clone();
                let mut seen = IndexSet::new();
                while let Some(id) = pending.pop() {
                    if seen.insert(id)
                        && let Some(dep) = facts.get(&id)
                    {
                        if group_for(model, id).is_some()
                            && let Some(file) = &dep.artifacts.file
                        {
                            inputs.excluded.insert(file.clone());
                        }
                        pending.extend(&dep.dependencies);
                    }
                }
            }
        }
        inputs
    }

    fn scan<E>(
        &self,
        model: &ProjectModel,
        set: &RuntimeDependencyInstall,
        mut eval: impl FnMut(&[u8]) -> Result<BString, E>,
    ) -> Result<BString, E> {
        let policy = model
            .dir(set.dir)
            .final_policies
            .map_or(PolicyStatus::Warn, |policies| {
                policies.get(PolicyId::new(207))
            });
        let mut code = BString::default();
        if policy != PolicyStatus::Warn {
            code.extend_from_slice(b"block(SCOPE_FOR POLICIES)\ncmake_policy(SET CMP0207 ");
            code.extend_from_slice(if policy == PolicyStatus::New {
                b"NEW"
            } else {
                b"OLD"
            });
            code.extend_from_slice(b")\n");
        }
        code.extend_from_slice(
            b"file(GET_RUNTIME_DEPENDENCIES RESOLVED_DEPENDENCIES_VAR _CMAKE_DEPS",
        );
        for (keyword, values) in [b"EXECUTABLES".as_slice(), b"LIBRARIES", b"MODULES"]
            .into_iter()
            .zip(&self.files)
        {
            append_list(&mut code, keyword, values.iter());
        }
        append_list(&mut code, b"BUNDLE_EXECUTABLE", &self.bundle);
        let filters = &set.filters;
        for (keyword, values) in [
            (b"DIRECTORIES".as_slice(), &filters.directories),
            (b"PRE_INCLUDE_REGEXES", &filters.pre_include_regexes),
            (b"PRE_EXCLUDE_REGEXES", &filters.pre_exclude_regexes),
            (b"POST_INCLUDE_REGEXES", &filters.post_include_regexes),
            (b"POST_EXCLUDE_REGEXES", &filters.post_exclude_regexes),
            (b"POST_INCLUDE_FILES", &filters.post_include_files),
            (b"POST_EXCLUDE_FILES", &filters.post_exclude_files),
        ] {
            let mut entries = Vec::new();
            for value in values {
                entries.extend(list::split(&eval(value)?, list::Empty::Drop));
            }
            append_list(&mut code, keyword, &entries);
        }
        append_list(&mut code, b"POST_EXCLUDE_FILES_STRICT", &self.excluded);
        code.extend_from_slice(b")");
        if policy != PolicyStatus::Warn {
            code.extend_from_slice(b"\nendblock()");
        }
        Ok(code)
    }
}

fn group_for(model: &ProjectModel, id: TargetId) -> Option<usize> {
    group(model.target(id)?.kind)
}

fn group(kind: TargetKind) -> Option<usize> {
    match kind {
        TargetKind::Executable => Some(0),
        TargetKind::SharedLibrary => Some(1),
        TargetKind::ModuleLibrary => Some(2),
        _ => None,
    }
}

fn append_list<'a>(
    out: &mut BString,
    keyword: &[u8],
    values: impl IntoIterator<Item = &'a BString>,
) {
    let mut values = values.into_iter().peekable();
    if values.peek().is_some() {
        out.extend_from_slice(b"\n  ");
        out.extend_from_slice(keyword);
        for value in values {
            out.extend_from_slice(b"\n    \"");
            cmake_quoted_content(out, value);
            out.push(b'"');
        }
    }
}

fn copy_code(
    destination: &[u8],
    permissions: &[BString],
    apple: bool,
    framework: bool,
    dll: bool,
) -> BString {
    let mut code = BString::from("foreach(_CMAKE_TMP_dep IN LISTS _CMAKE_DEPS)\n");
    if framework {
        code.extend_from_slice(
            b"  if(_CMAKE_TMP_dep MATCHES \"^(.*/)?([^/]*\\\\.framework)/\")\n    set(_CMAKE_TMP_path \"\x24{CMAKE_MATCH_1}\x24{CMAKE_MATCH_2}\")\n",
        );
    } else if apple {
        code.extend_from_slice(b"  if(NOT _CMAKE_TMP_dep MATCHES \"\\\\.framework/\")\n");
    }
    code.extend_from_slice(b"  file(INSTALL DESTINATION \"");
    if !Lexical::new(destination).absolute() {
        code.extend_from_slice(b"\x24{CMAKE_INSTALL_PREFIX}");
        if !destination.is_empty() {
            code.push(b'/');
        }
    }
    cmake_quoted_content(&mut code, destination);
    code.extend_from_slice(b"\" TYPE ");
    code.extend_from_slice(if framework {
        b"DIRECTORY"
    } else if dll {
        b"EXECUTABLE"
    } else {
        b"SHARED_LIBRARY"
    });
    if !permissions.is_empty() {
        code.extend_from_slice(if framework {
            b" FILE_PERMISSIONS"
        } else {
            b" PERMISSIONS"
        });
        for permission in permissions {
            code.push(b' ');
            code.extend_from_slice(permission);
        }
    }
    code.extend_from_slice(if framework {
        b" FILES \"\x24{_CMAKE_TMP_path}\" USE_SOURCE_PERMISSIONS)\n"
    } else {
        b" FILES \"\x24{_CMAKE_TMP_dep}\" FOLLOW_SYMLINK_CHAIN)\n"
    });
    if apple {
        code.extend_from_slice(b"  endif()\n");
    }
    code.extend_from_slice(b"endforeach()");
    code
}
