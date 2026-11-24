//! Artifact names and source classification.

use rcmake_base::{BString, ByteSlice, hash::HashAlgo, list, path_lex::Lexical, truth};
use rcmake_genex::ConfigId;
use rcmake_model::{DirId, GeneratorKind, ProjectModel, Target, TargetKind};

use super::{definition, join, target_property};
use crate::ArtifactSet;

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

fn configured_property<'a>(
    target: &'a Target,
    name: &[u8],
    config: &ConfigId,
) -> Option<&'a BString> {
    if !config.0.is_empty()
        && let Some(value) = target
            .props
            .get(&[name, b"_", &config.0.to_ascii_uppercase()].concat())
    {
        return Some(value);
    }
    target.props.get(name)
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
            linker_file: import_file.clone().or_else(|| file.clone()),
            file,
            import_file,
            ..ArtifactSet::default()
        };
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
    let configured_dir =
        config_dir.or_else(|| target_property(model, target, output_dir.as_bytes()));
    let mut dir = configured_dir.map_or_else(|| dir.clone(), |output| join(dir, output));
    if config_dir.is_none()
        && multi_config(model)
        && configured_dir.is_none_or(|p| !p.windows(2).any(|part| part == b"$<"))
    {
        dir = join(&dir, &config.0);
    }

    let name = configured_property(target, format!("{class}_OUTPUT_NAME").as_bytes(), config)
        .or_else(|| configured_property(target, b"OUTPUT_NAME", config))
        .unwrap_or(&target.name);
    let postfix_key = [&config.0.to_ascii_uppercase()[..], b"_POSTFIX"].concat();
    let postfix = if !config.0.is_empty() {
        target_property(model, target, &postfix_key).map_or(b"".as_slice(), |v| v.as_slice())
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

fn multi_config(model: &ProjectModel) -> bool {
    model
        .generator
        .is_some_and(|generator| generator.multi_config())
}

pub(crate) fn object_dir(model: &ProjectModel, target: &Target, config: &ConfigId) -> BString {
    let binary = &model.dir(target.dir).binary_dir;
    let relative = build_relative(model, binary);
    let dir = if short_object_paths(model) {
        let directory = if relative.is_empty() {
            b".".as_slice()
        } else {
            &relative
        };
        let name_hash = sha3_prefix(&target.name, 4);
        let directory_hash = sha3_prefix(directory, 4);
        let short = format!(".o/{name_hash}{directory_hash}");
        if model.generator == Some(GeneratorKind::UnixMakefiles) {
            join(&relative, short.as_bytes())
        } else {
            short.into()
        }
    } else {
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

pub(crate) fn short_object_paths(model: &ProjectModel) -> bool {
    model.generator.is_some()
        && model
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
    let path = Lexical::new(path).normalize();
    let root = Lexical::new(root).normalize();
    let path = Lexical::new(&path);
    let root = Lexical::new(&root);
    if root.prefix(path) {
        list_path(path.components().skip(root.components().count()))
    } else {
        path.normalize()
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
    ["C", "CXX", "OBJC", "OBJCXX", "ASM", "Fortran", "CUDA"]
        .into_iter()
        .find(|language| source_extension(model, target, language, extension))
        .map(BString::from)
}

fn source_extension(
    model: &ProjectModel,
    target: &Target,
    language: &str,
    extension: &[u8],
) -> bool {
    let extension = extension.strip_prefix(b".").unwrap_or(extension);
    if let Some(values) = definition(
        model,
        target.dir,
        format!("CMAKE_{language}_SOURCE_FILE_EXTENSIONS").as_bytes(),
    ) {
        return list::split(values, list::Empty::Drop)
            .iter()
            .any(|value| value == extension);
    }
    match language {
        "C" => extension == b"c",
        "CXX" => matches!(
            extension,
            b"C" | b"cc" | b"cpp" | b"CPP" | b"cxx" | b"c++" | b"ixx" | b"cppm"
        ),
        "OBJC" => extension == b"m",
        "OBJCXX" => extension == b"mm",
        "ASM" => matches!(extension, b"s" | b"S" | b"asm"),
        "Fortran" => matches!(extension, b"f" | b"F" | b"f90" | b"F90"),
        "CUDA" => extension == b"cu",
        _ => false,
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
    let dir = model.dir(target.dir);
    let root = model.dir(DirId::ROOT);
    let absolute = Lexical::new(&join(&dir.source_dir, source)).normalize();
    let absolute = Lexical::new(&absolute);
    let parent_count = absolute
        .components()
        .take_while(|part| *part == b"..")
        .count();

    let relative_to_tree = |current: &[u8], top: &[u8]| {
        let current = Lexical::new(current).normalize();
        let top = Lexical::new(top).normalize();
        let current = Lexical::new(&current);
        let top = Lexical::new(&top);
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

    let suffix = source_language(model, target, source)
        .and_then(|lang| {
            definition(
                model,
                target.dir,
                &[b"CMAKE_", lang.as_slice(), b"_OUTPUT_EXTENSION"].concat(),
            )
        })
        .map_or(b".o".as_slice(), |v| v.as_slice());
    if short_object_paths(model) {
        let hash_input = relative.unwrap_or_else(|| absolute.normalize());
        let hash = sha3_prefix(&hash_input, 8);
        let name = [hash.as_bytes(), suffix].concat();
        return join(&object_dir(model, target, config), &name);
    }

    let name = relative.unwrap_or_else(|| absolute.normalize());
    let mut safe = name.trim_start_with(|ch| ch == '/').replace(b"../", b"__/");
    for byte in &mut safe {
        if matches!(*byte, b':' | b' ') {
            *byte = b'_';
        }
    }
    safe.extend_from_slice(suffix);
    join(&object_dir(model, target, config), &safe)
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
