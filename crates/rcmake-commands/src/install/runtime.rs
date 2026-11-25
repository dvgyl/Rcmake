use rcmake_base::{BString, path_lex::Lexical};
use rcmake_eval::{CmdResult, Cx, Flow};
use rcmake_model::{InstallOptions, RuntimeDependencyInstall};

use super::{Result, common, fail, keyword, record_request, values, warn_absolute_destination};

#[derive(Default)]
struct DependencySetOptions {
    common: InstallOptions,
    library: InstallOptions,
    runtime: InstallOptions,
    framework: InstallOptions,
}

pub(super) fn set(cx: &mut Cx<'_>, input: &[&[u8]]) -> CmdResult {
    let Some((name, options)) = input.split_first().filter(|(name, _)| !name.is_empty()) else {
        return Err(fail(
            "RUNTIME_DEPENDENCY_SET not given a runtime dependency set.",
        ));
    };
    let system = require_supported(cx, b"RUNTIME_DEPENDENCY_SET")?;

    let mut groups = DependencySetOptions::default();
    let mut group = &mut groups.common;
    let mut options = options.iter().copied().peekable();
    while let Some(key) = options.next() {
        match key {
            b"LIBRARY" => group = &mut groups.library,
            b"RUNTIME" => group = &mut groups.runtime,
            b"FRAMEWORK" => group = &mut groups.framework,
            b"DESTINATION" | b"COMPONENT" if options.peek().is_none_or(|arg| keyword(arg)) => {}
            key if filter_keyword(key) => {
                values(&mut options);
            }
            key if common(cx, b"RUNTIME_DEPENDENCY_SET", group, key, &mut options)? => {}
            _ => {
                return Err(fail(
                    [
                        b"RUNTIME_DEPENDENCY_SET given unknown argument \"".as_slice(),
                        key,
                        b"\".",
                    ]
                    .concat(),
                ));
            }
        }
    }

    let libraries = if system == b"Windows" {
        groups.runtime
    } else {
        groups.library
    };
    let frameworks = (system == b"Darwin").then_some(groups.framework);
    for artifact in std::iter::once(&libraries).chain(frameworks.as_ref()) {
        if let Some(destination) = artifact
            .destination
            .as_ref()
            .or(groups.common.destination.as_ref())
            .filter(|destination| Lexical::new(destination).absolute())
        {
            warn_absolute_destination(cx, destination);
        }
    }
    if !cx.fatal_occurred() {
        record_request(cx, "_RCMAKE_RUNTIME_DEPENDENCY_SET", input);
        groups
            .common
            .component
            .get_or_insert_with(|| super::default_component(cx));
        let dir = cx.current_dir_id();
        let backtrace = cx.backtrace();
        let model = cx.project();
        model
            .runtime_dependency_installs
            .push(RuntimeDependencyInstall {
                dir,
                position: model.installs.len(),
                name: (*name).into(),
                backtrace,
                library: groups.common.with_group(&libraries),
                framework: frameworks
                    .as_ref()
                    .map(|group| groups.common.with_group(group)),
            });
    }
    Ok(Flow::Normal)
}

pub(super) fn require_supported(cx: &Cx<'_>, signature: &[u8]) -> Result<BString> {
    let system = cx.var_or_empty(b"CMAKE_SYSTEM_NAME");
    let system = if system.is_empty() {
        cx.host().system().sysname
    } else {
        system
    };
    if matches!(system.as_slice(), b"Linux" | b"Darwin" | b"Windows") {
        Ok(system)
    } else {
        Err(fail(
            [signature, b" is not supported on system \"", &system, b"\""].concat(),
        ))
    }
}

pub(super) fn filter_keyword(key: &[u8]) -> bool {
    matches!(
        key,
        b"DIRECTORIES"
            | b"PRE_INCLUDE_REGEXES"
            | b"PRE_EXCLUDE_REGEXES"
            | b"POST_INCLUDE_REGEXES"
            | b"POST_EXCLUDE_REGEXES"
            | b"POST_INCLUDE_FILES"
            | b"POST_EXCLUDE_FILES"
    )
}

const IMPORTED: &[u8] = b"IMPORTED_RUNTIME_ARTIFACTS";

pub(super) fn imported(cx: &mut Cx<'_>, input: &[&[u8]]) -> CmdResult {
    use rcmake_model::{BT, InstallKind, InstallRule, TargetKind};

    let mut args = input.iter().copied().peekable();
    let names = values(&mut args);
    let mut groups = std::collections::BTreeMap::new();
    let mut group = b"".as_slice();
    let mut dependency_set = None;
    while let Some(key) = args.next() {
        match key {
            b"RUNTIME_DEPENDENCY_SET" if group.is_empty() => {
                dependency_set = Some(super::value(&mut args, key)?);
            }
            b"LIBRARY" | b"RUNTIME" | b"FRAMEWORK" | b"BUNDLE" => group = key,
            _ if common(
                cx,
                IMPORTED,
                groups.entry(group).or_default(),
                key,
                &mut args,
            )? => {}
            _ => {
                return Err(fail(format!(
                    "IMPORTED_RUNTIME_ARTIFACTS given unknown argument \"{}\".",
                    BString::from(key)
                )));
            }
        }
    }

    let system = cx.var_or_empty(b"CMAKE_SYSTEM_NAME");
    let apple = system == b"Darwin";
    let dll = system.starts_with(b"Windows") || system == b"CYGWIN";
    let dir = cx.current_dir_id();
    let trace = cx.backtrace();
    let default_component = super::default_component(cx);
    let base = groups.remove(b"".as_slice()).unwrap_or_default();
    let dist = cx.project();
    let mut rules = Vec::new();
    for name in names {
        let Some(target) = dist
            .find_target(dir, name)
            .and_then(|id| dist.target(id))
            .filter(|target| target.is_imported())
        else {
            return Err(fail(format!(
                "IMPORTED_RUNTIME_ARTIFACTS given target \"{}\" which does not exist.",
                BString::from(name)
            )));
        };
        let property = |key: &[u8]| {
            target
                .props
                .get(key)
                .is_some_and(|value| rcmake_base::truth::is_on(value))
        };
        let kind = match target.kind {
            TargetKind::Executable if apple && property(b"MACOSX_BUNDLE") => b"BUNDLE".as_slice(),
            TargetKind::SharedLibrary if apple && property(b"FRAMEWORK") => b"FRAMEWORK",
            TargetKind::ModuleLibrary if apple && property(b"BUNDLE") => b"BUNDLE",
            TargetKind::Executable => b"RUNTIME",
            TargetKind::SharedLibrary if dll => b"RUNTIME",
            TargetKind::SharedLibrary | TargetKind::ModuleLibrary => b"LIBRARY",
            _ => {
                return Err(fail(format!(
                    "IMPORTED_RUNTIME_ARTIFACTS given target \"{}\" which is not an executable, shared library, or module library.",
                    BString::from(name)
                )));
            }
        };
        let grouped = groups.get(kind).cloned().unwrap_or_default();
        let options = merge_imported_options(&base, grouped, &default_component, target);
        let program = target.kind == TargetKind::Executable || dll;
        rules.push((name, kind, program, options));
    }
    for (name, kind, program, mut options) in rules {
        if options
            .destination
            .as_ref()
            .is_none_or(|path| path.is_empty())
        {
            options.destination = Some(match kind {
                b"RUNTIME" => super::type_destination(cx, b"BIN")?,
                b"LIBRARY" => super::type_destination(cx, b"LIB")?,
                _ => {
                    return Err(fail(format!(
                        "IMPORTED_RUNTIME_ARTIFACTS given no {} DESTINATION for imported target \"{}\".",
                        BString::from(kind),
                        BString::from(name)
                    )));
                }
            });
        }
        if let Some(path) = options
            .destination
            .as_ref()
            .filter(|path| Lexical::new(path).absolute())
        {
            warn_absolute_destination(cx, path);
        }
        let file = BT::new(
            [b"$<TARGET_FILE:".as_slice(), name, b">"].concat().into(),
            trace.clone(),
        );
        let index = cx.project().add_install(InstallRule {
            dir,
            backtrace: trace.clone(),
            kind: if program {
                InstallKind::Programs(vec![file])
            } else {
                InstallKind::Files(vec![file])
            },
            options,
        });
        let mut recorded = vec![IMPORTED, name];
        if let Some(set) = dependency_set {
            recorded.extend([b"RUNTIME_DEPENDENCY_SET", set]);
        }
        super::record_args(cx, &format!("_RCMAKE_INSTALL_{index}"), &recorded);
    }
    Ok(Flow::Normal)
}

fn merge_imported_options(
    base: &InstallOptions,
    group: InstallOptions,
    default_component: &BString,
    target: &rcmake_model::Target,
) -> InstallOptions {
    InstallOptions {
        destination: group.destination.or_else(|| base.destination.clone()),
        component: group
            .component
            .or_else(|| base.component.clone())
            .or_else(|| Some(default_component.clone())),
        permissions: if group.permissions.is_empty() {
            base.permissions.clone()
        } else {
            group.permissions
        },
        configurations: if group.configurations.is_empty() {
            base.configurations.clone()
        } else {
            group.configurations
        },
        optional: group.optional || base.optional,
        exclude_from_all: group.exclude_from_all || base.exclude_from_all,
        build_depends: target
            .utility_deps
            .iter()
            .map(|dep| dep.value.clone())
            .collect(),
        ..Default::default()
    }
}
