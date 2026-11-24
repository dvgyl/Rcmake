//! Link and archive rules.

use indexmap::IndexSet;
use rcmake_base::{BString, list, path_lex::Lexical, shell::quote, truth};
use rcmake_genex::{ConfigId, EvalContext};
use rcmake_model::{DirId, ProjectModel, Target, TargetId};

use super::artifacts::{build_relative, is_darwin};
use super::compile::{apple_flags, language_flags};
use super::install_rpath::MachORpathEdit;
use super::launcher;
use super::policy_flags::Flags;
use super::shell::{direct, join_words, rule_invocations, simple_args};
use super::{definition, join, target_property};
use crate::{
    ArtifactSet, BuildStep, Invocation, LinkFragmentRole, LinkKind, LinkStep, ResolveError,
    ResolvedTarget, usage::UsageResolver,
};

pub(super) fn link(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    artifacts: &ArtifactSet,
    mut step: LinkStep,
    objects: Vec<BString>,
) -> Result<BuildStep, ResolveError> {
    if step.output.is_empty()
        && let Some(file) = &artifacts.file
    {
        step.output = build_relative(model, file);
    }
    let language = step.language.as_slice();
    let darwin = is_darwin(model, target.dir);
    let kind = step.kind;
    let policy = Flags::new(model, target, language);
    let ipo = policy.ipo(config);
    let mut driver = language_flags(model, target, config, language);
    if kind != LinkKind::StaticArchive {
        driver.extend(policy.link(ipo));
        driver.extend(apple_flags(model, target, config, language));
    }
    let driver = join_words(driver);
    let option = |key: &[u8]| policy.option(key);
    let cmake = || {
        definition(model, target.dir, b"CMAKE_COMMAND").map_or_else(|| "cmake".into(), Clone::clone)
    };
    let mut flags = Vec::new();
    if kind != LinkKind::StaticArchive {
        flags.extend(option(b"LINK_FLAGS").cloned());
    }
    let base = [b"CMAKE_", kind.flag_class(), b"_LINKER_FLAGS"].concat();
    let configured = [base.as_slice(), b"_", &config.0.to_ascii_uppercase()].concat();
    flags.extend(
        [base.as_slice(), configured.as_slice()]
            .into_iter()
            .filter_map(|key| definition(model, target.dir, key).cloned()),
    );
    flags.push(
        definition(model, target.dir, &kind.create_flags_key(language))
            .map_or_else(|| kind.default_create_flag(darwin).into(), Clone::clone),
    );
    flags.extend(policy.pie_link());
    if kind == LinkKind::SharedLibrary && darwin {
        flags.extend(darwin_versions(model, target, language));
    }
    let raw_flags = kind.target_flags_property();
    flags.extend(target.props.get(raw_flags).cloned());
    flags.extend(
        target
            .props
            .get(&[raw_flags, b"_", &config.0.to_ascii_uppercase()].concat())
            .cloned(),
    );
    flags.extend(
        step.fragments
            .iter()
            .filter(|f| f.role == LinkFragmentRole::Flags)
            .map(|f| f.text.value.clone()),
    );
    if darwin {
        deduplicate_darwin_flags(&mut flags);
    }
    let flags = join_words(flags);

    let mut inputs = objects.clone();
    let libraries = link_libraries(model, target, config, &step, darwin, &mut inputs)?;
    let soname = if kind == LinkKind::SharedLibrary {
        artifacts.soname_file.as_ref()
    } else {
        None
    };
    let soname = soname.map(|name| Lexical::new(name).filename());
    let soname_flag = soname.map_or_else(BString::default, |_| {
        definition(
            model,
            target.dir,
            &[b"CMAKE_SHARED_LIBRARY_SONAME_", language, b"_FLAG"].concat(),
        )
        .or_else(|| definition(model, target.dir, b"CMAKE_SHARED_LIBRARY_SONAME_C_FLAG"))
        .cloned()
        .unwrap_or_else(|| {
            if darwin {
                "-install_name".into()
            } else {
                "-Wl,-soname,".into()
            }
        })
    });
    let install_name_dir = if darwin && soname.is_some() {
        install_name_dir(model, target, artifacts)
    } else {
        BString::default()
    };
    let shared_flags_key = [b"CMAKE_SHARED_LIBRARY_", language, b"_FLAGS"].concat();
    let output = quote(&step.output);
    let lookup = |key: &[u8]| match key {
        b"TARGET" => Some(output.clone()),
        b"TARGET_SONAME" => Some(soname.map_or_else(BString::default, quote)),
        b"SONAME_FLAG" => Some(soname_flag.clone()),
        b"TARGET_INSTALLNAME_DIR" => Some(if install_name_dir.is_empty() {
            BString::default()
        } else {
            quote(&install_name_dir)
        }),
        b"TARGET_PDB" | b"TARGET_IMPLIB" => Some({
            let file = if key == b"TARGET_PDB" {
                &artifacts.pdb_file
            } else {
                &artifacts.import_file
            };
            file.as_ref()
                .map_or_else(BString::default, |path| quote(path))
        }),
        b"TARGET_VERSION_MAJOR" | b"TARGET_VERSION_MINOR" => Some(
            target
                .props
                .get(b"VERSION")
                .and_then(|v| {
                    v.split(|&b| b == b'.')
                        .nth(usize::from(key == b"TARGET_VERSION_MINOR"))
                })
                .unwrap_or(b"0")
                .into(),
        ),
        b"FLAGS" | b"LANGUAGE_COMPILE_FLAGS" => Some(driver.clone()),
        b"LINK_FLAGS" => Some(flags.clone()),
        b"LINK_LIBRARIES" => Some(libraries.clone()),
        key if key == shared_flags_key => Some(
            definition(model, target.dir, key)
                .cloned()
                .unwrap_or_default(),
        ),
        _ => None,
    };
    let expand = |template: &[u8], objects: &[BString]| {
        let objects = join_words(objects.iter().map(|v| quote(v)));
        rule_invocations(model, target.dir, template, |key| {
            if key == b"OBJECTS" {
                Some(objects.clone())
            } else {
                lookup(key)
            }
        })
    };

    let compiler = [b"<CMAKE_", language, b"_COMPILER>"].concat();
    let soname_template: &[u8] = if darwin {
        b" <SONAME_FLAG> <TARGET_INSTALLNAME_DIR><TARGET_SONAME>"
    } else {
        b" <SONAME_FLAG><TARGET_SONAME>"
    };
    let fallback = [
        compiler.as_slice(),
        b" <FLAGS> <LINK_FLAGS>",
        if kind == LinkKind::SharedLibrary {
            soname_template
        } else {
            b""
        },
        b" <OBJECTS> -o <TARGET> <LINK_LIBRARIES>",
    ]
    .concat();

    let template = option(kind.rule_variable()).filter(|v| !v.is_empty());
    let mut invocations = if kind == LinkKind::StaticArchive {
        archive_commands(
            &step.output,
            &objects,
            cmake(),
            &policy,
            ipo,
            template,
            expand,
        )
    } else {
        expand(
            template.map_or(fallback.as_slice(), |v| v.as_slice()),
            &objects,
        )
    };
    for invocation in invocations
        .iter_mut()
        .skip(usize::from(kind == LinkKind::StaticArchive))
    {
        let command = launcher::Command {
            role: "link",
            config,
            output: &step.output,
            source: None,
            language: &step.language,
        };
        let mut link = std::mem::take(invocation);
        if kind != LinkKind::StaticArchive {
            link = command.launch_tool(model, target, b"_LINKER_LAUNCHER", link)?;
        }
        *invocation = command.wrap(model, target, link);
    }
    if kind == LinkKind::SharedLibrary
        && let (Some(file), Some(soname), Some(linker)) = (
            &artifacts.file,
            &artifacts.soname_file,
            &artifacts.linker_file,
        )
        && (file != soname || soname != linker)
    {
        invocations.push(direct(
            vec![
                cmake(),
                "-E".into(),
                "cmake_symlink_library".into(),
                step.output.clone(),
                build_relative(model, soname),
                build_relative(model, linker),
            ],
            None,
        ));
    }
    if kind == LinkKind::Executable
        && let (Some(file), Some(linker)) = (&artifacts.file, &artifacts.linker_file)
        && file != linker
    {
        invocations.push(direct(
            vec![
                cmake(),
                "-E".into(),
                "cmake_symlink_executable".into(),
                step.output.clone(),
                build_relative(model, linker),
            ],
            None,
        ));
    }
    Ok(BuildStep::Link {
        inputs,
        step,
        invocations,
    })
}

fn link_libraries(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    step: &LinkStep,
    darwin: bool,
    inputs: &mut Vec<BString>,
) -> Result<BString, ResolveError> {
    if step.kind == LinkKind::StaticArchive {
        return Ok(BString::default());
    }
    let link_fragments = step
        .fragments
        .iter()
        .filter(|f| f.role != LinkFragmentRole::Flags);
    let libraries = linked_libraries(step);
    let rpaths = rpaths(model, target, config, step, darwin, &libraries)?;
    inputs.extend(libraries);
    Ok(join_words(
        rpaths
            .into_iter()
            .chain(link_fragments.map(|f| f.text.value.clone())),
    ))
}

fn archive_commands(
    output: &BString,
    objects: &[BString],
    cmake: BString,
    policy: &Flags<'_>,
    ipo: bool,
    template: Option<&BString>,
    expand: impl Fn(&[u8], &[BString]) -> Vec<Invocation>,
) -> Vec<Invocation> {
    let mut commands = vec![direct(
        vec![cmake, "-E".into(), "rm".into(), "-f".into(), output.clone()],
        None,
    )];
    let archive = |key: &[u8]| {
        ipo.then(|| policy.option(&[key, b"_IPO"].concat()))
            .flatten()
            .or_else(|| policy.option(key))
    };
    if let Some(template) =
        template.filter(|_| !(ipo && policy.option(b"ARCHIVE_CREATE_IPO").is_some()))
    {
        commands.extend(expand(template, objects));
    } else {
        let create = archive(b"ARCHIVE_CREATE").map_or(
            b"<CMAKE_AR> qc <TARGET> <LINK_FLAGS> <OBJECTS>".as_slice(),
            |v| v.as_slice(),
        );
        let append = archive(b"ARCHIVE_APPEND").map_or(
            b"<CMAKE_AR> q <TARGET> <LINK_FLAGS> <OBJECTS>".as_slice(),
            |v| v.as_slice(),
        );
        let finish = archive(b"ARCHIVE_FINISH")
            .map_or(b"<CMAKE_RANLIB> <TARGET>".as_slice(), |v| v.as_slice());
        commands.extend(archive_invocations(expand, create, append, finish, objects));
    }
    commands
}

fn deduplicate_darwin_flags(flags: &mut [BString]) {
    let mut seen = IndexSet::new();
    for flag in flags {
        let Some(args) = simple_args(flag) else {
            continue;
        };
        let filtered: Vec<_> = args
            .iter()
            .filter(|arg| {
                !matches!(
                    arg.as_slice(),
                    b"-Wl,-search_paths_first" | b"-Wl,-headerpad_max_install_names"
                ) || seen.insert((*arg).clone())
            })
            .collect();
        if filtered.len() != args.len() {
            *flag = join_words(filtered.into_iter().map(|arg| quote(arg)));
        }
    }
}

fn archive_invocations(
    expand: impl Fn(&[u8], &[BString]) -> Vec<Invocation>,
    create: &[u8],
    append: &[u8],
    finish: &[u8],
    objects: &[BString],
) -> Vec<Invocation> {
    // Leave space for the rest of the command and the environment.
    const OBJECT_BYTES: usize = 32 * 1024;
    let mut start = 0;
    let mut bytes = 0;
    let mut commands = Vec::new();
    if !append.is_empty() {
        for (index, object) in objects.iter().enumerate() {
            let size = quote(object).len() + 1;
            if index > start && bytes + size > OBJECT_BYTES {
                if let Some(chunk) = objects.get(start..index) {
                    commands.extend(expand(if start == 0 { create } else { append }, chunk));
                }
                start = index;
                bytes = 0;
            }
            bytes += size;
        }
    }
    commands.extend(expand(
        if start == 0 { create } else { append },
        objects.get(start..).unwrap_or_default(),
    ));
    commands.extend(expand(finish, &[]));
    commands
}

fn library_file(arg: &[u8]) -> bool {
    !arg.starts_with(b"-")
        && (arg.contains(&b'/')
            || arg.ends_with(b".a")
            || arg.ends_with(b".o")
            || arg.ends_with(b".tbd")
            || shared_library(arg))
}

fn shared_library(name: &[u8]) -> bool {
    name.ends_with(b".dylib")
        || name.ends_with(b".so")
        || name.windows(4).any(|part| part == b".so.")
}

fn darwin_versions(model: &ProjectModel, target: &Target, language: &[u8]) -> Vec<BString> {
    let version = target.props.get(b"VERSION").filter(|v| !v.is_empty());
    let soversion = target.props.get(b"SOVERSION").filter(|v| !v.is_empty());
    let current = target
        .props
        .get(b"MACHO_CURRENT_VERSION")
        .or(version)
        .or(soversion);
    let compatibility = target
        .props
        .get(b"MACHO_COMPATIBILITY_VERSION")
        .or(soversion)
        .or(version);
    let mut flags = Vec::new();
    for (value, key, fallback) in [
        (
            compatibility,
            b"OSX_COMPATIBILITY_VERSION_FLAG".as_slice(),
            b"-compatibility_version ".as_slice(),
        ),
        (current, b"OSX_CURRENT_VERSION_FLAG", b"-current_version "),
    ] {
        if let Some(value) = value.and_then(|v| macho_version(v)) {
            let flag = Flags::new(model, target, language)
                .option(key)
                .map_or(fallback, |v| v.as_slice());
            if !flag.is_empty() {
                flags.push([flag, quote(&value).as_slice()].concat().into());
            }
        }
    }
    flags
}

fn macho_version(value: &[u8]) -> Option<BString> {
    let mut version = value.split(|&byte| byte == b'.');
    let mut component = || {
        version
            .next()
            .and_then(|part| std::str::from_utf8(part).ok())
            .and_then(|part| part.parse::<u32>().ok())
            .unwrap_or_default()
    };
    let (major, minor, patch) = (component(), component(), component());
    (major != 0 || minor != 0 || patch != 0).then(|| format!("{major}.{minor}.{patch}").into())
}

pub(crate) fn install_name_dir(
    model: &ProjectModel,
    target: &Target,
    artifacts: &ArtifactSet,
) -> BString {
    let property = |key| target_property(model, target, key);
    let install = property(b"BUILD_WITH_INSTALL_NAME_DIR").is_some_and(|v| truth::is_on(v));
    let rpath = property(b"MACOSX_RPATH").map_or_else(
        || {
            rcmake_diag::PolicyId::parse(b"CMP0042")
                .is_none_or(|id| target.policies.policies.get(id) != rcmake_diag::PolicyStatus::Old)
        },
        |v| truth::is_on(v),
    );
    let mut dir = if install {
        property(b"INSTALL_NAME_DIR").cloned().unwrap_or_else(|| {
            if rpath {
                "@rpath".into()
            } else {
                BString::default()
            }
        })
    } else if rpath {
        "@rpath".into()
    } else {
        artifacts
            .file
            .as_ref()
            .map(|file| Lexical::new(file).parent().into())
            .unwrap_or_else(|| model.dir(target.dir).binary_dir.clone())
    };
    if !dir.is_empty() && !dir.ends_with(b"/") {
        dir.push(b'/');
    }
    dir
}

fn rpaths(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    step: &LinkStep,
    darwin: bool,
    libraries: &[BString],
) -> Result<Vec<BString>, ResolveError> {
    let Some((property, installs)) = rpath_property(model, target) else {
        return Ok(Vec::new());
    };
    let prefix = runtime_flag(model, target, &step.language);
    if prefix.is_empty() {
        return Ok(Vec::new());
    }
    let value = rpath_value(model, target, config, step, property)?;
    let paths = rpath_entries(model, target, step, darwin, libraries, installs, &value);
    Ok(if darwin {
        paths
            .iter()
            .map(|path| [prefix, quote(path).as_slice()].concat().into())
            .collect()
    } else if paths.is_empty() {
        Vec::new()
    } else {
        let paths = paths
            .iter()
            .map(|v| v.as_slice())
            .collect::<Vec<_>>()
            .join(&b':');
        vec![[prefix, quote(&paths).as_slice()].concat().into()]
    })
}

fn rpath_value(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    link: &LinkStep,
    property: &[u8],
) -> Result<BString, ResolveError> {
    let Some(value) = target_property(model, target, property) else {
        return Ok(BString::default());
    };
    if let Some(id) = model.find_target(target.dir, &target.name)
        && value.windows(2).any(|part| part == b"$<")
    {
        let ctx = EvalContext {
            config: config.clone(),
            link_language: Some(link.language.clone()),
            ..EvalContext::default()
        };
        return UsageResolver::new(model).eval_ide_property(id, property, value, &ctx);
    }
    Ok(value.clone())
}

fn linked_libraries(step: &LinkStep) -> Vec<BString> {
    step.fragments
        .iter()
        .filter(|f| f.role == LinkFragmentRole::Libraries)
        .flat_map(|fragment| simple_args(&fragment.text.value).into_iter().flatten())
        .filter(|arg| library_file(arg))
        .collect()
}

fn runtime_flag<'a>(model: &'a ProjectModel, target: &Target, language: &[u8]) -> &'a [u8] {
    definition(
        model,
        target.dir,
        &[b"CMAKE_SHARED_LIBRARY_RUNTIME_", language, b"_FLAG"].concat(),
    )
    .or_else(|| definition(model, target.dir, b"CMAKE_SHARED_LIBRARY_RUNTIME_C_FLAG"))
    .map_or(b"-Wl,-rpath,".as_slice(), |v| v.as_slice())
}

pub(super) fn install_rpath<E>(
    model: &ProjectModel,
    target: &Target,
    resolved: &ResolvedTarget,
    file_name: &[u8],
    mut eval: impl FnMut(&[u8]) -> Result<BString, E>,
) -> Result<Option<MachORpathEdit>, E> {
    let Some(link) = resolved.link.as_ref() else {
        return Ok(None);
    };
    if link.kind == LinkKind::StaticArchive
        || !is_darwin(model, target.dir)
        || runtime_flag(model, target, &link.language).is_empty()
        || [b"CMAKE_SKIP_RPATH".as_slice(), b"CMAKE_NO_BUILTIN_CHRPATH"]
            .into_iter()
            .any(|key| definition(model, target.dir, key).is_some_and(|v| truth::is_on(v)))
        || target_property(model, target, b"BUILD_WITH_INSTALL_RPATH")
            .is_some_and(|v| truth::is_on(v))
    {
        return Ok(None);
    }
    let libraries = linked_libraries(link);
    let mut property = |key| {
        target_property(model, target, key)
            .map_or_else(|| Ok(BString::default()), |value| eval(value))
    };
    let old = if rpath_property(model, target).is_some() {
        let value = property(b"BUILD_RPATH")?;
        rpath_entries(model, target, link, true, &libraries, false, &value)
    } else {
        IndexSet::new()
    };
    let new =
        if target_property(model, target, b"SKIP_INSTALL_RPATH").is_some_and(|v| truth::is_on(v)) {
            IndexSet::new()
        } else {
            let value = property(b"INSTALL_RPATH")?;
            rpath_entries(model, target, link, true, &libraries, true, &value)
        };
    if old == new {
        return Ok(None);
    }
    let tool = definition(model, target.dir, b"CMAKE_INSTALL_NAME_TOOL")
        .filter(|tool| !truth::is_off(tool.as_slice()))
        .cloned()
        .unwrap_or_else(|| "install_name_tool".into());
    Ok(Some(MachORpathEdit {
        file_name: file_name.into(),
        tool,
        old: old.into_iter().collect(),
        new: new.into_iter().collect(),
    }))
}

pub(crate) fn ide_rpaths(
    model: &ProjectModel,
    target: &Target,
    id: TargetId,
    ctx: &EvalContext,
    usage: &mut UsageResolver<'_>,
    step: &LinkStep,
    libraries: &[BString],
) -> Result<Vec<BString>, ResolveError> {
    let Some((property, installs)) = rpath_property(model, target) else {
        return Ok(Vec::new());
    };
    let value = target_property(model, target, property)
        .map(|value| usage.eval_ide_property(id, property, value, ctx))
        .transpose()?
        .unwrap_or_default();
    Ok(
        rpath_entries(model, target, step, true, libraries, installs, &value)
            .into_iter()
            .collect(),
    )
}

fn rpath_property(model: &ProjectModel, target: &Target) -> Option<(&'static [u8], bool)> {
    if definition(model, target.dir, b"CMAKE_SKIP_RPATH").is_some_and(|value| truth::is_on(value)) {
        return None;
    }
    let installs = target_property(model, target, b"BUILD_WITH_INSTALL_RPATH")
        .is_some_and(|value| truth::is_on(value));
    let (property, skip): (&[u8], &[u8]) = if installs {
        (b"INSTALL_RPATH", b"SKIP_INSTALL_RPATH")
    } else {
        (b"BUILD_RPATH", b"SKIP_BUILD_RPATH")
    };
    (!target_property(model, target, skip).is_some_and(|value| truth::is_on(value)))
        .then_some((property, installs))
}

fn rpath_entries(
    model: &ProjectModel,
    target: &Target,
    step: &LinkStep,
    darwin: bool,
    libraries: &[BString],
    installs: bool,
    value: &[u8],
) -> IndexSet<BString> {
    let mut paths: IndexSet<BString> = list::split(value, list::Empty::Drop).into_iter().collect();
    if !installs {
        let build = &model.dir(DirId::ROOT).binary_dir;
        let build_path = Lexical::new(build);
        let output = join(build, &step.output);
        let output_dir = Lexical::new(Lexical::new(&output).parent());
        let origin = !darwin
            && target_property(model, target, b"BUILD_RPATH_USE_ORIGIN")
                .is_some_and(|value| truth::is_on(value));
        for library in libraries {
            let file = Lexical::new(&join(build, library)).normalize();
            let file = Lexical::new(&file);
            if build_path.prefix(file) && shared_library(file.filename()) {
                let parent = Lexical::new(file.parent());
                let path = if origin {
                    parent.relative(output_dir).map_or_else(
                        || file.parent().into(),
                        |rel| {
                            if rel == b"." {
                                "$ORIGIN".into()
                            } else {
                                join(b"$ORIGIN", &rel)
                            }
                        },
                    )
                } else {
                    file.parent().into()
                };
                paths.insert(path);
            }
        }
    }
    paths
}
