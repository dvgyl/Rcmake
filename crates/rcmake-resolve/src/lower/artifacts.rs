//! Artifact names and source classification.

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, ByteSlice, hash::HashAlgo, list, path_lex::Lexical, truth};
use rcmake_genex::ConfigId;
use rcmake_model::{DirId, GeneratorKind, ProjectModel, Target, TargetId, TargetKind};

use super::{definition, join, target_property};
use crate::{
    ArtifactSet, ConfigureMetadata, ResolveError, Toolchain,
    ide::{self, IdeFlavor},
    unity::UnityFile,
    usage::UsageResolver,
};

pub(crate) fn is_dll_platform(model: &ProjectModel, dir: DirId) -> bool {
    definition(model, dir, b"CMAKE_SYSTEM_NAME").map_or_else(
        || definition(model, dir, b"WIN32").is_some_and(|value| truth::is_on(value)),
        |system| system.starts_with(b"Windows") || system == b"CYGWIN",
    )
}

fn imported_linker_file(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
) -> Option<BString> {
    let property = |name| {
        crate::usage::artifacts::imported_property(target, name, config).map(|(file, _)| file)
    };
    let file = property(b"IMPORTED_LOCATION");
    let import = property(b"IMPORTED_IMPLIB");
    if is_dll_platform(model, target.dir)
        || is_darwin(model, target.dir)
            && import.as_ref().is_some_and(|path| path.ends_with(b".tbd"))
    {
        import.or(file)
    } else {
        file.or(import)
    }
}

pub(crate) fn is_msvc(model: &ProjectModel, dir: DirId, language: &[u8]) -> bool {
    [b"_COMPILER_ID".as_slice(), b"_COMPILER_FRONTEND_VARIANT"]
        .into_iter()
        .any(|suffix| {
            definition(model, dir, &[b"CMAKE_", language, suffix].concat())
                .is_some_and(|value| value == b"MSVC")
        })
        || definition(model, dir, b"MSVC").is_some_and(|value| truth::is_on(value))
}

pub(super) fn compile_pdb(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    usage: &mut UsageResolver<'_>,
) -> Result<BString, ResolveError> {
    let reuse = target
        .props
        .get(b"PRECOMPILE_HEADERS_REUSE_FROM")
        .and_then(|name| model.find_target(target.dir, name))
        .and_then(|id| model.target(id).map(|target| (id, target)));
    let (id, owner) = reuse.map_or_else(
        || (model.find_target(target.dir, &target.name), target),
        |(id, owner)| (Some(id), owner),
    );
    compile_pdb_path(model, owner, id, reuse.is_some(), config, usage)
}

pub(super) fn compile_pdb_path(
    model: &ProjectModel,
    owner: &Target,
    id: Option<TargetId>,
    reused: bool,
    config: &ConfigId,
    usage: &mut UsageResolver<'_>,
) -> Result<BString, ResolveError> {
    let ctx = rcmake_genex::EvalContext {
        config: config.clone(),
        ..Default::default()
    };
    let mut property = |name: &[u8]| {
        let specific = [name, b"_", &config.0.to_ascii_uppercase()].concat();
        let configured = !config.0.is_empty() && owner.props.get(&specific).is_some();
        let key = if configured { &specific } else { name };
        let value = owner.props.get(key).filter(|value| !value.is_empty());
        value
            .map(|value| {
                let append_config = !configured
                    && multi_config(model)
                    && !value.windows(2).any(|part| part == b"$<");
                let value = if let Some(id) = id {
                    usage.eval_ide_property(id, key, value, &ctx)?
                } else {
                    value.clone()
                };
                Ok::<_, ResolveError>((value, append_config))
            })
            .transpose()
    };
    let name = property(b"COMPILE_PDB_NAME")?.map(|(name, _)| name);
    let reused = reused
        || (!owner
            .usage
            .entries(rcmake_model::UsageField::PrecompileHeaders)
            .is_empty()
            && model.targets().any(|(_, candidate)| {
                candidate
                    .props
                    .get(b"PRECOMPILE_HEADERS_REUSE_FROM")
                    .is_some_and(|name| model.find_target(candidate.dir, name) == id)
            }));
    let named = name.is_some() || reused;
    let file = name.or_else(|| {
        (reused || owner.kind == TargetKind::StaticLibrary).then(|| owner.name.clone())
    });
    let mut dir = if let Some((path, append_config)) = property(b"COMPILE_PDB_OUTPUT_DIRECTORY")? {
        let mut dir = join(&model.dir(owner.dir).binary_dir, &path);
        if append_config {
            dir = join(&dir, &config.0);
        }
        build_relative(model, &dir)
    } else if named
        && let Some(id) = id
        && let Some(path) = usage.artifact(
            id,
            rcmake_genex::ArtifactKind::PdbFile,
            rcmake_genex::ArtifactPart::Dir,
            config,
        )
    {
        build_relative(model, &path)
    } else {
        object_dir(model, owner, config)
    };
    if let Some(file) = file {
        dir = join(&dir, &[file.as_slice(), b".pdb"].concat());
    } else {
        if dir.is_empty() {
            dir.push(b'.');
        }
        if !dir.ends_with(b"/") {
            dir.push(b'/');
        }
    }
    Ok(dir)
}

pub(crate) fn is_darwin(model: &ProjectModel, dir: DirId) -> bool {
    if let Some(system) = definition(model, dir, b"CMAKE_SYSTEM_NAME") {
        return matches!(
            system.as_slice(),
            b"Darwin" | b"iOS" | b"tvOS" | b"watchOS" | b"visionOS"
        );
    }
    definition(model, dir, b"APPLE")
        .map(|value| truth::is_on(value))
        .or_else(|| definition(model, dir, b"CMAKE_SHARED_LIBRARY_SUFFIX").map(|s| s == b".dylib"))
        .unwrap_or(cfg!(target_vendor = "apple"))
}

fn imported_property<'a>(target: &'a Target, key: &[u8], config: &ConfigId) -> Option<&'a BString> {
    let config = config.0.to_ascii_uppercase();
    let mapped = target
        .props
        .get(&[b"MAP_IMPORTED_CONFIG_", config.as_slice()].concat());
    let candidates = match mapped {
        Some(value) if value.is_empty() => vec![BString::default()],
        Some(value) => list::split(value, list::Empty::Keep),
        None => vec![config.into(), BString::default()],
    };
    let get = |config: &BString| {
        if config.is_empty() {
            target.props.get(key)
        } else {
            target
                .props
                .get(&[key, b"_", &config.to_ascii_uppercase()].concat())
        }
    };
    candidates.iter().find_map(get).or_else(|| {
        if mapped.is_some() {
            return None;
        }
        let configs = target.props.get(b"IMPORTED_CONFIGURATIONS")?;
        list::split(configs, list::Empty::Drop).iter().find_map(get)
    })
}

/// Compute the output files of a target for one configuration.
///
/// Output files use absolute paths when the model has an absolute binary directory.
/// The object directory is relative to the top-level build directory when possible.
#[must_use]
pub fn compute_artifacts(model: &ProjectModel, target: &Target, config: &ConfigId) -> ArtifactSet {
    if target.is_imported() {
        let file = imported_property(target, b"IMPORTED_LOCATION", config).cloned();
        let import_file = imported_property(target, b"IMPORTED_IMPLIB", config).cloned();
        return ArtifactSet {
            linker_file: imported_linker_file(model, target, config),
            file,
            import_file,
            ..ArtifactSet::default()
        };
    }
    if is_dll_platform(model, target.dir)
        && let Some(id) = model.find_target(target.dir, &target.name)
        && let Ok((artifacts, _)) = UsageResolver::new(model).ide_artifacts(id, config)
    {
        return artifacts;
    }

    let mut artifacts = ArtifactSet::default();
    let darwin = is_darwin(model, target.dir);
    let (class, platform_kind, fallback_prefix, fallback_suffix) = match target.kind {
        TargetKind::Executable => ("RUNTIME", "EXECUTABLE", "", ""),
        TargetKind::StaticLibrary => ("ARCHIVE", "STATIC_LIBRARY", "lib", ".a"),
        TargetKind::SharedLibrary => (
            "LIBRARY",
            "SHARED_LIBRARY",
            "lib",
            if darwin { ".dylib" } else { ".so" },
        ),
        TargetKind::ModuleLibrary => ("LIBRARY", "SHARED_MODULE", "lib", ".so"),
        TargetKind::ObjectLibrary => {
            artifacts.object_dir = Some(object_dir(model, target, config));
            return artifacts;
        }
        _ => return artifacts,
    };
    artifacts.object_dir = Some(object_dir(model, target, config));
    let dir = &model.dir(target.dir).binary_dir;
    let output_dir = format!("{class}_OUTPUT_DIRECTORY");
    let key = [output_dir.as_bytes(), b"_", &config.0.to_ascii_uppercase()].concat();
    let config_dir = (!config.0.is_empty())
        .then(|| target_property(model, target, &key))
        .flatten();
    let configured_dir = config_dir
        .or_else(|| target_property(model, target, output_dir.as_bytes()))
        .or_else(|| legacy_library_output_directory(model, target));
    let mut dir = configured_dir.map_or_else(|| dir.clone(), |output| join(dir, output));
    if config_dir.is_none()
        && multi_config(model)
        && configured_dir.is_none_or(|p| !p.windows(2).any(|part| part == b"$<"))
    {
        dir = join(&dir, &config.0);
    }

    let name = super::output_name_keys(class, &config.0.to_ascii_uppercase())
        .iter()
        .find_map(|key| target.props.get(key.as_slice()))
        .filter(|value| !value.is_empty())
        .unwrap_or(&target.name);
    let postfix_key = [&config.0.to_ascii_uppercase()[..], b"_POSTFIX"].concat();
    let postfix = if !config.0.is_empty() {
        let postfix = if target.kind == TargetKind::Executable {
            target.props.get(&postfix_key)
        } else {
            target_property(model, target, &postfix_key)
        };
        postfix.map_or(b"".as_slice(), |value| value.as_slice())
    } else {
        b""
    };
    let affix = |key: &str, fallback: &'static str| {
        target
            .props
            .get(key.as_bytes())
            .or_else(|| {
                definition(
                    model,
                    target.dir,
                    format!("CMAKE_{platform_kind}_{key}").as_bytes(),
                )
            })
            .map_or(fallback.as_bytes(), |v| v.as_slice())
    };
    let stem = [affix("PREFIX", fallback_prefix), name, postfix].concat();
    let suffix = affix("SUFFIX", fallback_suffix);
    let plain = join(&dir, &[stem.as_slice(), suffix].concat());
    let version = target.props.get(b"VERSION").filter(|v| !v.is_empty());
    let soversion = target.props.get(b"SOVERSION").filter(|v| !v.is_empty());
    let no_soname = target
        .props
        .get(b"NO_SONAME")
        .is_some_and(|v| truth::is_on(v));

    artifacts.file = Some(plain.clone());
    match target.kind {
        TargetKind::SharedLibrary => {
            artifacts.linker_file = Some(plain.clone());
            if !no_soname {
                let versioned = |version: Option<&BString>| {
                    let Some(version) = version else {
                        return plain.clone();
                    };
                    let filename = if darwin {
                        [stem.as_slice(), b".", version, suffix].concat()
                    } else {
                        [stem.as_slice(), suffix, b".", version].concat()
                    };
                    join(&dir, &filename)
                };
                artifacts.file = Some(versioned(version.or(soversion)));
                artifacts.soname_file = Some(versioned(soversion.or(version)));
            }
        }
        TargetKind::Executable => {
            if let Some(version) = version {
                artifacts.file = Some(join(
                    &dir,
                    &[stem.as_slice(), suffix, b"-", version].concat(),
                ));
                artifacts.linker_file = Some(plain);
            }
        }
        _ => artifacts.linker_file = Some(plain),
    }
    artifacts
}

pub(crate) fn legacy_library_output_directory<'a>(
    model: &'a ProjectModel,
    target: &Target,
) -> Option<&'a BString> {
    matches!(
        target.kind,
        TargetKind::StaticLibrary | TargetKind::SharedLibrary | TargetKind::ModuleLibrary
    )
    .then(|| definition(model, target.dir, b"LIBRARY_OUTPUT_PATH"))
    .flatten()
    .filter(|path| !path.is_empty())
}

fn multi_config(model: &ProjectModel) -> bool {
    model
        .generator
        .is_some_and(|generator| generator.multi_config())
}

pub(crate) fn object_dir(model: &ProjectModel, target: &Target, config: &ConfigId) -> BString {
    let dir = if short_object_paths(model) {
        short_support_dir(model, target)
    } else {
        let relative = build_relative(model, &model.dir(target.dir).binary_dir);
        join(
            &relative,
            &[b"CMakeFiles/", target.name.as_slice(), b".dir"].concat(),
        )
    };
    if multi_config(model) {
        join(&dir, &config.0)
    } else {
        dir
    }
}

pub(crate) fn short_support_dir(model: &ProjectModel, target: &Target) -> BString {
    let relative = build_relative(model, &model.dir(target.dir).binary_dir);
    let directory = if relative.is_empty() {
        b".".as_slice()
    } else {
        &relative
    };
    let name_hash = sha3_prefix(&target.name, 4);
    let directory_hash = sha3_prefix(directory, 4);
    let short = format!(".o/{name_hash}{directory_hash}");
    if matches!(
        model.generator,
        Some(GeneratorKind::UnixMakefiles | GeneratorKind::VisualStudio(_))
    ) {
        join(&relative, short.as_bytes())
    } else {
        short.into()
    }
}

/// Return whether targets use `SHORT` hashed support directories.
///
/// The Makefile, Ninja, and Visual Studio generators support the `SHORT`
/// strategy. Xcode names each support directory after its target, so it always
/// uses the full layout.
pub(crate) fn short_object_paths(model: &ProjectModel) -> bool {
    matches!(
        model.generator,
        Some(
            GeneratorKind::Ninja
                | GeneratorKind::NinjaMultiConfig
                | GeneratorKind::UnixMakefiles
                | GeneratorKind::VisualStudio(_)
        )
    ) && model
        .cache
        .get(b"CMAKE_INTERMEDIATE_DIR_STRATEGY")
        .is_some_and(|entry| entry.value == "SHORT")
}

fn sha3_prefix(input: &[u8], length: usize) -> String {
    let mut digest = HashAlgo::Sha3_512.hex_digest(input);
    digest.truncate(length);
    digest
}

pub(crate) fn build_relative(model: &ProjectModel, path: &[u8]) -> BString {
    let root = &model.dir(DirId::ROOT).binary_dir;
    let path = model_path(path).normalize();
    let root = model_path(root).normalize();
    let path = model_path(&path);
    let root = model_path(&root);
    if root.prefix(path) {
        list_path(path.components().skip(root.components().count()))
    } else {
        path.normalize()
    }
}

pub(crate) fn model_path(path: &[u8]) -> Lexical<'_> {
    let windows = Lexical::windows(path);
    if windows.absolute() {
        windows
    } else {
        Lexical::new(path)
    }
}

fn list_path<'a>(parts: impl Iterator<Item = &'a [u8]>) -> BString {
    let mut out = BString::default();
    for part in parts {
        if !out.is_empty() {
            out.push(b'/');
        }
        out.extend_from_slice(part);
    }
    out
}

/// Return the compile language of a source. Return no language for headers and unknown types.
///
/// The source can be absolute or relative to the target's source directory.
#[must_use]
pub fn source_language(model: &ProjectModel, target: &Target, source: &[u8]) -> Option<BString> {
    let directory = model.dir(target.dir);
    let absolute = Lexical::new(&join(&directory.source_dir, source)).normalize();
    let props = crate::sources::properties(directory, &absolute);
    if props
        .get(b"HEADER_FILE_ONLY")
        .is_some_and(|v| truth::is_on(v))
    {
        return None;
    }
    if let Some(language) = props.get(b"LANGUAGE").filter(|v| !v.is_empty()) {
        return Some(language.clone());
    }
    let (_, extension) = Lexical::new(source).stem_extension(true);
    crate::sources::language_for_extension(model, target.dir, extension)
}

pub(crate) fn compile_object_path(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    source: &[u8],
    unity_file: Option<&UnityFile>,
    flavor: Option<IdeFlavor>,
) -> BString {
    if let Some(flavor) = flavor {
        build_relative(
            model,
            &ide::object_path(model, target, config, source, flavor),
        )
    } else {
        unity_file.map_or_else(
            || object_path(model, target, config, source),
            |file| file.object_path(model, target, config, source),
        )
    }
}

/// Return the build path of a source's object file.
///
/// Use paths relative to the source or build tree when possible.
#[must_use]
pub fn object_path(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    source: &[u8],
) -> BString {
    let name = object_name(model, target, source, short_object_paths(model), None);
    join(&object_dir(model, target, config), &name)
}

fn object_name(
    model: &ProjectModel,
    target: &Target,
    source: &[u8],
    short: bool,
    custom_install_name: Option<&[u8]>,
) -> BString {
    let dir = model.dir(target.dir);
    let root = model.dir(DirId::ROOT);
    let absolute = model_path(&join(&dir.source_dir, source)).normalize();
    let absolute = model_path(&absolute);
    let parent_count = absolute
        .components()
        .take_while(|part| *part == b"..")
        .count();

    let relative_to_tree = |current: &[u8], top: &[u8]| {
        let current = model_path(current).normalize();
        let top = model_path(top).normalize();
        let current = model_path(&current);
        let top = model_path(&top);
        (parent_count == 0
            && (current.prefix(absolute) || (top.prefix(current) && top.prefix(absolute))))
        .then(|| absolute.relative(current))
        .flatten()
    };
    let relative_source = relative_to_tree(&dir.source_dir, &root.source_dir);
    let relative_binary = relative_to_tree(&dir.binary_dir, &root.binary_dir);
    let rank = |path: &BString| (path.starts_with(b"."), path.len());
    let relative = [relative_source, relative_binary]
        .into_iter()
        .flatten()
        .min_by_key(rank);

    let suffix = crate::sources::properties(dir, &source.into())
        .get(b"LANGUAGE")
        .cloned()
        .or_else(|| source_language(model, target, source))
        .and_then(|lang| {
            if lang == "CUDA"
                && let Some(extension) = super::cuda::output_extension(target)
            {
                return Some(extension);
            }
            definition(
                model,
                target.dir,
                &[b"CMAKE_", lang.as_slice(), b"_OUTPUT_EXTENSION"].concat(),
            )
            .map(|value| value.as_slice())
        })
        .unwrap_or(b".o");
    if short {
        let hash_input = relative.unwrap_or_else(|| absolute.normalize());
        let hash = sha3_prefix(&hash_input, 8);
        return [hash.as_bytes(), suffix].concat().into();
    }
    if let Some(name) = custom_install_name.filter(|name| !name.is_empty()) {
        return [name, suffix].concat().into();
    }

    if matches!(
        model.generator,
        Some(GeneratorKind::UnixMakefiles | GeneratorKind::Ninja | GeneratorKind::NinjaMultiConfig)
    ) && let Ok(name) = crate::sources::custom_object_name(model, target.dir, source)
        && !name.is_empty()
    {
        return [name.as_slice(), suffix].concat().into();
    }

    let name = relative.unwrap_or_else(|| absolute.normalize());
    let mut safe = name.trim_start_with(|ch| ch == '/').replace(b"../", b"__/");
    for byte in &mut safe {
        if matches!(*byte, b':' | b' ') {
            *byte = b'_';
        }
    }
    safe.extend_from_slice(suffix);
    safe.into()
}

pub(super) fn object_install_directory(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    dest: &[u8],
) -> BString {
    if target_property(model, target, b"INSTALL_OBJECT_ONLY_USE_DESTINATION")
        .is_some_and(|value| truth::is_on(value))
    {
        return dest.into();
    }
    let component = if config.0.is_empty() {
        BString::from("objects")
    } else {
        [b"objects-", config.0.as_slice()].concat().into()
    };
    join(&join(dest, &component), &target.name)
}

pub(crate) fn install_object_name_strategy<'a>(
    model: &'a ProjectModel,
    target: &'a Target,
) -> Option<&'a BString> {
    target_property(model, target, b"INSTALL_OBJECT_NAME_STRATEGY")
        .filter(|value| !value.is_empty())
}

pub(super) fn install_object_name(
    model: &ProjectModel,
    target: &Target,
    source: &[u8],
    custom_name: &[u8],
) -> BString {
    if model.generator == Some(rcmake_model::GeneratorKind::Xcode) {
        let name = if custom_name.is_empty() {
            let name = Lexical::new(source).filename();
            name.iter()
                .rposition(|&byte| byte == b'.')
                .and_then(|dot| name.get(..dot))
                .unwrap_or(name)
        } else {
            custom_name
        };
        return [name, b".o"].concat().into();
    }
    let short =
        install_object_name_strategy(model, target).is_some_and(|strategy| strategy == b"SHORT");
    object_name(model, target, source, short, Some(custom_name))
}

/// Choose a link language from the languages of the target's translation units.
#[must_use]
pub fn linker_language<'a>(
    model: &ProjectModel,
    target: &Target,
    languages: impl IntoIterator<Item = &'a [u8]>,
) -> Option<BString> {
    if let Some(language) = target
        .props
        .get(b"LINKER_LANGUAGE")
        .filter(|v| !v.is_empty())
    {
        return Some(language.clone());
    }
    languages
        .into_iter()
        .filter(|language| *language != b"RC")
        .max_by_key(|language| {
            definition(
                model,
                target.dir,
                &[b"CMAKE_", *language, b"_LINKER_PREFERENCE"].concat(),
            )
            .and_then(|v| std::str::from_utf8(v).ok())
            .and_then(|v| v.parse::<i32>().ok())
            .unwrap_or(match *language {
                b"CXX" | b"OBJCXX" => 30,
                b"C" | b"OBJC" => 10,
                _ => 0,
            })
        })
        .map(BString::from)
}

/// Collect listfiles and compiler data from the model.
///
/// Listfiles keep the order of their first appearance. Read compiler values
/// from directory variable snapshots and the cache.
#[must_use]
pub fn collect_metadata(model: &ProjectModel) -> ConfigureMetadata {
    let mut listfiles = IndexSet::new();
    let mut languages = IndexMap::new();
    for (dir, directory) in model.dirs() {
        listfiles.extend(directory.listfiles.iter().cloned());
        for key in directory.vars.keys().chain(directory.props.names().iter()) {
            if let Some(language) = compiler_language(key) {
                languages.entry(language).or_insert(dir);
            }
        }
    }
    for key in model
        .global
        .props
        .names()
        .iter()
        .chain(model.cache.iter().map(|(key, _)| key))
    {
        if let Some(language) = compiler_language(key) {
            languages.entry(language).or_insert(DirId::ROOT);
        }
    }
    let toolchains = languages
        .into_iter()
        .filter_map(|(language, dir)| {
            let read = |suffix: &[u8]| {
                definition(
                    model,
                    dir,
                    &[b"CMAKE_", language.as_slice(), b"_", suffix].concat(),
                )
            };
            let compiler = read(b"COMPILER")?;
            if rcmake_base::truth::is_off(compiler) {
                return None;
            }
            let value = |suffix| read(suffix).cloned().unwrap_or_default();
            let optional = |suffix| read(suffix).filter(|v| !v.is_empty()).cloned();
            let paths =
                |suffix| read(suffix).map_or_else(Vec::new, |v| list::split(v, list::Empty::Drop));
            let toolchain = Toolchain {
                compiler_path: compiler.clone(),
                compiler_id: value(b"COMPILER_ID"),
                compiler_version: value(b"COMPILER_VERSION"),
                implicit_includes: paths(b"IMPLICIT_INCLUDE_DIRECTORIES"),
                implicit_link_dirs: paths(b"IMPLICIT_LINK_DIRECTORIES"),
                source_extensions: paths(b"SOURCE_FILE_EXTENSIONS"),
                scandep_rule: optional(b"SCANDEP_SOURCE"),
                module_map_format: optional(b"MODULE_MAP_FORMAT"),
                module_map_flag: optional(b"MODULE_MAP_FLAG"),
                compile_bmi_rule: optional(b"MODULE_BMI_ONLY_FLAG"),
            };
            Some((language, toolchain))
        })
        .collect();
    ConfigureMetadata {
        listfiles: listfiles.into_iter().collect(),
        toolchains,
        configure_log: definition(model, DirId::ROOT, b"CMAKE_CONFIGURE_LOG").cloned(),
        cmake_command: definition(model, DirId::ROOT, b"CMAKE_COMMAND").cloned(),
        ctest_command: definition(model, DirId::ROOT, b"CMAKE_CTEST_COMMAND").cloned(),
        ctest_arguments: definition(model, DirId::ROOT, b"CMAKE_CTEST_ARGUMENTS")
            .map_or_else(Vec::new, |value| list::split(value, list::Empty::Drop)),
        jobserver_steps: model
            .commands()
            .map(|(_, command)| command)
            .filter(|command| command.kind == rcmake_model::CustomCommandKind::Output)
            .enumerate()
            .filter_map(|(step, command)| command.job_server_aware.then_some(step))
            .collect(),
        ..ConfigureMetadata::default()
    }
}

fn compiler_language(key: &[u8]) -> Option<BString> {
    let lang = key.strip_prefix(b"CMAKE_")?.strip_suffix(b"_COMPILER")?;
    (!lang.is_empty()).then(|| lang.into())
}
