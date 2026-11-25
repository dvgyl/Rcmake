use std::borrow::Cow;

use indexmap::IndexSet;
use rcmake_base::{BString, ByteSlice, shell::quote, truth};
use rcmake_genex::ConfigId;
use rcmake_host::{ReadRange, WriteMode, path};
use rcmake_model::{DirId, ProjectModel, Target, TargetKind};

use crate::{
    ArtifactSet, Invocation, ResolveError,
    lower::{
        artifacts::build_relative,
        definition, join, object_dir,
        policy_flags::Flags,
        shell::{direct, join_words, simple_args, windows_arguments, windows_host},
    },
    usage::UsageResolver,
    usage::link_options::{Wrapper, shell_arguments},
};

pub(super) struct LinkerDriver<'a> {
    wrapper: Wrapper<'a>,
    windows: bool,
}

impl<'a> LinkerDriver<'a> {
    pub(super) fn new(model: &'a ProjectModel, target: &Target, language: &[u8]) -> Self {
        Self {
            wrapper: Wrapper::new(model, target, language, b"LINKER"),
            windows: windows_host(model, target.dir),
        }
    }

    pub(super) fn driver_flags(&self, flags: &[u8]) -> BString {
        if self.windows {
            join_words(windows_arguments(flags).iter().map(|arg| quote(arg)))
        } else {
            flags.into()
        }
    }

    pub(super) fn flag_string(&self, flags: &[u8]) -> Result<BString, ResolveError> {
        let args = if self.windows {
            Some(windows_arguments(flags))
        } else {
            simple_args(flags)
        };
        let Some(args) = args else {
            return Ok(flags.into());
        };
        args.iter()
            .map(|arg| {
                if arg.starts_with(b"LINKER:") {
                    self.argument(arg)
                } else {
                    Ok(quote(arg))
                }
            })
            .collect::<Result<Vec<_>, _>>()
            .map(join_words)
    }

    pub(super) fn argument(&self, argument: &[u8]) -> Result<BString, ResolveError> {
        if let Some(payload) = argument.strip_prefix(b"SHELL:") {
            let mut args = Vec::new();
            for word in shell_arguments(payload) {
                args.extend(self.wrapper.expand(&word)?);
            }
            return Ok(join_words(args.iter().map(|arg| quote(arg))));
        }
        if !argument.starts_with(b"LINKER:") {
            return Ok(argument.into());
        }
        self.wrapper
            .expand(argument)
            .map(|args| join_words(args.iter().map(|arg| quote(arg))))
    }
}

pub(super) fn without_empty_output_flags<'a>(
    template: &'a [u8],
    artifacts: &ArtifactSet,
) -> Cow<'a, [u8]> {
    let mut rule = Cow::Borrowed(template);
    for (suffix, output) in [
        (b"pdb:<TARGET_PDB>".as_slice(), &artifacts.pdb_file),
        (b"implib:<TARGET_IMPLIB>", &artifacts.import_file),
    ] {
        if output.is_none() {
            for prefix in [b"/".as_slice(), b"-"] {
                for suffix in [suffix.to_vec(), suffix.to_ascii_uppercase()] {
                    let flag = [prefix, &suffix].concat();
                    if rule.find(&flag).is_some() {
                        rule = Cow::Owned(rule.replace(&flag, b""));
                    }
                }
            }
        }
    }
    rule
}

#[derive(Default)]
pub(super) struct DefinitionFile {
    pub(super) flag: Option<BString>,
    pub(super) invocation: Option<Invocation>,
    pub(super) inputs: Vec<BString>,
}

pub(super) fn definition_file(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    usage: &UsageResolver<'_>,
    sources: Option<&IndexSet<BString>>,
    objects: &[BString],
    language: &[u8],
) -> Result<DefinitionFile, ResolveError> {
    if !matches!(
        target.kind,
        TargetKind::SharedLibrary | TargetKind::ModuleLibrary | TargetKind::Executable
    ) {
        return Ok(DefinitionFile::default());
    }
    let Some(flag) = definition(
        model,
        target.dir,
        &[b"CMAKE_", language, b"_LINK_DEF_FILE_FLAG"].concat(),
    )
    .or_else(|| definition(model, target.dir, b"CMAKE_LINK_DEF_FILE_FLAG"))
    .filter(|flag| !flag.is_empty()) else {
        return Ok(DefinitionFile::default());
    };
    let dir = &model.dir(target.dir).source_dir;
    let fallback;
    let sources = if let Some(sources) = sources {
        sources
    } else {
        fallback = target
            .sources
            .iter()
            .map(|source| join(dir, &source.value))
            .collect::<IndexSet<_>>();
        &fallback
    };
    let mut result = DefinitionFile {
        inputs: sources
            .iter()
            .filter(|path| {
                path.last_chunk::<4>()
                    .is_some_and(|suffix| suffix.eq_ignore_ascii_case(b".def"))
            })
            .map(|path| build_relative(model, path))
            .collect(),
        ..DefinitionFile::default()
    };
    let export_all = target
        .props
        .get(b"WINDOWS_EXPORT_ALL_SYMBOLS")
        .is_some_and(|value| truth::is_on(value))
        && definition(
            model,
            target.dir,
            b"CMAKE_SUPPORT_WINDOWS_EXPORT_ALL_SYMBOLS",
        )
        .is_none_or(|value| truth::is_on(value));
    let generated = result.inputs.len() > 1 || export_all;
    let file = if generated
        && matches!(
            usage.ide(),
            None | Some(crate::ide::IdeFlavor::VisualStudio)
        )
        && let Some(paths) = usage.source_paths()
    {
        let dir = object_dir(model, target, config);
        let file = join(&dir, b"exports.def");
        let list = join(&dir, b"exports.def.objs");
        let content = objects
            .iter()
            .filter(|path| {
                export_all
                    && path
                        .last_chunk::<4>()
                        .is_some_and(|suffix| suffix.eq_ignore_ascii_case(b".obj"))
            })
            .chain(&result.inputs)
            .map(|path| path.as_slice())
            .collect::<Vec<_>>()
            .join(&b'\n');
        let full = join(&model.dir(DirId::ROOT).binary_dir, &list);
        write_definition_list(paths.host, &full, &content)?;
        let cmake = definition(model, target.dir, b"CMAKE_COMMAND")
            .map_or_else(|| "cmake".into(), Clone::clone);
        let mut argv = vec![
            cmake,
            "-E".into(),
            "__create_def".into(),
            file.clone(),
            list.clone(),
        ];
        if let Some(nm) = definition(model, target.dir, b"CMAKE_NM").filter(|nm| !truth::is_off(nm))
        {
            argv.push([b"--nm=", nm.as_slice()].concat().into());
        }
        result.inputs.push(list);
        result.invocation = Some(direct(argv, None));
        Some(file)
    } else {
        result.inputs.first().cloned()
    };
    result.flag = file.map(|file| quote(&[flag.as_slice(), &file].concat()));
    Ok(result)
}

fn write_definition_list(
    host: &dyn rcmake_host::Host,
    path: &[u8],
    content: &[u8],
) -> Result<(), ResolveError> {
    let path = path::to_native(path);
    let limit = u64::try_from(content.len())
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    if host
        .read(&path, ReadRange::All { limit })
        .is_ok_and(|old| old == content)
    {
        return Ok(());
    }
    let result = (|| {
        if let Some(parent) = path.parent() {
            host.create_dir_all(parent)?;
        }
        host.write(&path, content, WriteMode::Truncate)
    })();
    result.map_err(|error| {
        ResolveError::Message(format!("Cannot write {}: {error}", path.display()).into())
    })
}

pub(super) fn archive_commands(
    output: &BString,
    objects: &[BString],
    cmake: BString,
    policy: &Flags<'_>,
    (ipo, msvc): (bool, bool),
    template: Option<&BString>,
    expand: impl Fn(&[u8], &[BString]) -> Vec<Invocation>,
) -> Vec<Invocation> {
    let mut commands = Vec::new();
    if !msvc {
        commands.push(direct(
            vec![cmake, "-E".into(), "rm".into(), "-f".into(), output.clone()],
            None,
        ));
    }
    let archive = |key: &[u8]| {
        ipo.then(|| policy.option(&[key, b"_IPO"].concat()))
            .flatten()
            .or_else(|| policy.option(key))
    };
    if let Some(template) =
        template.filter(|_| !(ipo && policy.option(b"ARCHIVE_CREATE_IPO").is_some()))
    {
        commands.extend(expand(template, objects));
    } else if msvc && archive(b"ARCHIVE_CREATE").is_none() {
        commands.extend(expand(
            b"<CMAKE_AR> /nologo <LINK_FLAGS> /out:<TARGET> <OBJECTS>",
            objects,
        ));
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
    for command in &mut commands {
        if let Some(response) = &mut command.link_response {
            response.archive_batch = true;
        }
    }
    commands.extend(expand(finish, &[]));
    commands
}

pub(super) fn deduplicate_darwin_flags(flags: &mut [BString]) {
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

pub(super) fn library_file(arg: &[u8]) -> bool {
    let library_extension = arg.ends_with(b".a")
        || arg.ends_with(b".o")
        || arg.ends_with(b".tbd")
        || shared_library(arg);
    !arg.starts_with(b"-")
        && (!arg.starts_with(b"/")
            || arg.get(1..).is_some_and(|arg| {
                arg.contains(&b'/') || (!arg.contains(&b':') && library_extension)
            }))
        && (arg.contains(&b'/') || arg.contains(&b'\\') || library_extension)
}

pub(in crate::lower) fn shared_library(name: &[u8]) -> bool {
    name.ends_with(b".dylib")
        || name.ends_with(b".so")
        || name.windows(4).any(|part| part == b".so.")
}
