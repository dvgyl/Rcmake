//! Source references, groups, and build files.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    path::{Path, PathBuf},
};

use rcmake_gen_common::GenError;
use rcmake_host::path::to_native;
use rcmake_model::{DirId, ProjectModel, Target};
use rcmake_resolve::{
    ResolvedTarget,
    ide::{BundleKind, IdeCompileSettings, IdeSourceKind},
};

use crate::{
    object::{Attrs, Objects, Reference, list},
    phases, settings, text,
};

use super::{Project, file_ref, group};

impl Project<'_> {
    pub(super) fn add_sources<'a>(
        &mut self,
        key: &str,
        raw: &Target,
        sources: impl IntoIterator<Item = Source<'a>>,
        native: bool,
        bundle: &BundleKind,
    ) -> Result<(Group, BuildFiles), GenError> {
        let mut tree = Group::default();
        let mut files = BuildFiles::default();
        let source_dir = to_native(&self.model.dir(DirId::ROOT).source_dir);
        for file in sources {
            let path = Path::new(&file.path);
            let role = if let Ok(relative) = path.strip_prefix(self.bin) {
                format!("bin:{}", relative.display())
            } else if let Ok(relative) = path.strip_prefix(&source_dir) {
                format!("src:{}", relative.display())
            } else {
                format!("abs:{}", path.display())
            };
            let file_key = format!("{key}/file/{role}");
            let props = self
                .model
                .dir(raw.dir)
                .source_props
                .get(file.path.as_bytes());
            let explicit = props
                .and_then(|props| props.get(b"XCODE_EXPLICIT_FILE_TYPE"))
                .filter(|value| !value.is_empty())
                .map(|value| text(value));
            let last_known = props
                .and_then(|props| props.get(b"XCODE_LAST_KNOWN_FILE_TYPE"))
                .filter(|value| !value.is_empty())
                .map(|value| text(value));
            let file_type = if let Some(value) = explicit.as_deref() {
                ("explicitFileType", value)
            } else if let Some(value) = last_known.as_deref() {
                ("lastKnownFileType", value)
            } else {
                ("explicitFileType", file_type(&file))
            };
            let file_ref = file_ref(
                &mut self.objects,
                &file_key,
                &file.path,
                file_type,
                "<absolute>",
            )?;
            let folder = if file.group.is_empty() {
                match file.kind {
                    IdeSourceKind::Header => "Header Files",
                    IdeSourceKind::Resource => "Resources",
                    IdeSourceKind::ListFile => "CMake Rules",
                    _ => "Source Files",
                }
            } else {
                &file.group
            };
            tree.add(folder, file_ref.clone())?;
            if native {
                let phase = match file.kind {
                    IdeSourceKind::Compile { .. } => Some((&mut files.sources, "Sources")),
                    IdeSourceKind::ListFile if file.explicit => {
                        Some((&mut files.sources, "Sources"))
                    }
                    IdeSourceKind::Header if matches!(*bundle, BundleKind::Framework { .. }) => {
                        Some((&mut files.headers, "Headers"))
                    }
                    IdeSourceKind::Resource if *bundle != BundleKind::None => {
                        Some((&mut files.resources, "Resources"))
                    }
                    IdeSourceKind::Object | IdeSourceKind::ExternalObject => {
                        Some((&mut files.frameworks, "Frameworks"))
                    }
                    _ => None,
                };
                if let Some((items, label)) = phase {
                    if let IdeSourceKind::Compile { language } = &file.kind
                        && matches!(language.as_slice(), b"Fortran" | b"ASM")
                    {
                        let key = format!("{file_key}/build");
                        let build = if language == "Fortran" {
                            let format = props.and_then(|props| props.get(b"Fortran_FORMAT"));
                            crate::fortran_asm::fortran_build_file(
                                &mut self.objects,
                                &key,
                                &file_ref,
                                format.map(|format| format.as_slice()),
                                &file.flags,
                            )?
                        } else {
                            crate::fortran_asm::source_build_file(
                                &mut self.objects,
                                &key,
                                &file_ref,
                                &file.flags,
                            )?
                        };
                        items.push(build);
                        continue;
                    }
                    let mut build_settings = Attrs::new();
                    if let Some(visibility) = file.visibility {
                        build_settings.insert("ATTRIBUTES".into(), list([visibility]));
                    }
                    if !file.flags.is_empty() && matches!(file.kind, IdeSourceKind::Compile { .. })
                    {
                        build_settings.insert("COMPILER_FLAGS".into(), file.flags.into());
                    }
                    items.push(phases::build_file(
                        &mut self.objects,
                        &format!("{file_key}/build"),
                        label,
                        &file_ref,
                        build_settings,
                    )?);
                }
            }
        }
        Ok((tree, files))
    }
}

#[derive(Default)]
pub(super) struct Group {
    files: Vec<Reference>,
    groups: BTreeMap<String, Group>,
}

impl Group {
    pub(super) fn push(&mut self, file: Reference) {
        self.files.push(file);
    }

    pub(super) fn add(&mut self, path: &str, file: Reference) -> Result<(), GenError> {
        let mut group = self;
        for (depth, part) in path
            .split(['/', '\\'])
            .filter(|part| !part.is_empty())
            .enumerate()
        {
            if depth >= 64 {
                return Err(GenError::Invalid(
                    "Xcode group nesting exceeds 64 levels".into(),
                ));
            }
            group = group.groups.entry(part.into()).or_default();
        }
        group.files.push(file);
        Ok(())
    }

    pub(super) fn finish(
        self,
        objects: &mut Objects,
        key: &str,
        name: &str,
    ) -> Result<Reference, GenError> {
        let mut children = self.files;
        for (name, child) in self.groups {
            children.push(child.finish(objects, &format!("{key}/{name}"), &name)?);
        }
        group(objects, key, name, children)
    }
}

#[derive(Default)]
pub(super) struct BuildFiles {
    pub(super) sources: Vec<Reference>,
    pub(super) frameworks: Vec<Reference>,
    pub(super) headers: Vec<Reference>,
    pub(super) resources: Vec<Reference>,
}

pub(super) struct Source<'a> {
    path: String,
    kind: IdeSourceKind,
    group: String,
    visibility: Option<&'static str>,
    flags: String,
    compile: Option<&'a IdeCompileSettings>,
    explicit: bool,
}

pub(super) fn source_files<'a>(
    model: &ProjectModel,
    raw: &Target,
    target: &'a ResolvedTarget,
    per_language_flags: bool,
) -> Result<Vec<Source<'a>>, GenError> {
    let dir = model.dir(raw.dir);
    let source_dir = to_native(&dir.source_dir);
    let binary_dir = to_native(&dir.binary_dir);
    let header_set = |key: &[u8]| -> HashSet<PathBuf> {
        raw.props
            .get(key)
            .map(|value| {
                settings::strings(value)
                    .into_iter()
                    .map(|name| source_dir.join(name))
                    .collect()
            })
            .unwrap_or_default()
    };
    let public_headers = header_set(b"PUBLIC_HEADER");
    let private_headers = header_set(b"PRIVATE_HEADER");
    let mut files = Vec::new();
    let mut seen = BTreeSet::new();
    let prefix = settings::compile::prefix_header(target);
    let mut add = |path: String,
                   mut kind,
                   group,
                   compile: Option<&'a IdeCompileSettings>,
                   explicit,
                   skip_pch| {
        if !seen.insert(path.clone()) {
            return Ok(());
        }
        let props = dir.source_props.get(path.as_bytes());
        if props
            .and_then(|props| props.get(b"MACOSX_PACKAGE_LOCATION"))
            .is_some_and(|location| {
                location.as_slice() == b"Resources" || location.starts_with(b"Resources/")
            })
        {
            kind = IdeSourceKind::Resource;
        }
        let path_ref = Path::new(&path);
        let visibility = if public_headers.contains(path_ref) {
            Some("Public")
        } else if private_headers.contains(path_ref) {
            Some("Private")
        } else {
            None
        };
        if matches!(kind, IdeSourceKind::Compile { .. }) {
            let relative_props = path_ref
                .strip_prefix(&binary_dir)
                .ok()
                .and_then(|relative| {
                    let path = source_dir.join(relative);
                    dir.source_props.get(path.to_str()?.as_bytes())
                });
            for name in SOURCE_COMPILE_PROPERTIES {
                let value = props
                    .and_then(|props| props.get(name.as_bytes()))
                    .or_else(|| relative_props.and_then(|props| props.get(name.as_bytes())));
                if let Some(value) = value {
                    let value = text(value);
                    if references_config(&value) {
                        return Err(per_config_source_error(name, &value, &path));
                    }
                }
            }
        }
        let path = if kind == IdeSourceKind::Object {
            object_reference_path(&path, &text(&target.config.0))
        } else {
            path
        };
        let mut flags = compile.map_or_else(String::new, settings::source_flags);
        if let IdeSourceKind::Compile { language } = &kind
            && settings::compile::supported(language)
        {
            if prefix.is_some() {
                if flags.contains("-include")
                    && let Some(hidden) = settings::compile::hide_force_includes(&flags)
                {
                    flags = hidden;
                }
                if skip_pch
                    || target
                        .ide
                        .as_ref()
                        .is_none_or(|ide| !ide.pch.iter().any(|pch| pch.language == *language))
                {
                    flags.push_str(" -DCMAKE_SKIP_PRECOMPILE_HEADERS");
                }
            }
            if per_language_flags {
                flags = format!("$({}) {flags}", settings::compile::setting(language));
            }
        }
        files.push(Source {
            path,
            kind,
            group,
            visibility,
            flags,
            compile,
            explicit,
        });
        Ok(())
    };
    if let Some(ide) = target.ide.as_deref() {
        for source in &ide.sources {
            let path = std::str::from_utf8(&source.path).map_err(|_| {
                GenError::Invalid(
                    format!("Xcode source path is not valid UTF-8: {:?}", source.path).into(),
                )
            })?;
            add(
                path.into(),
                source.kind.clone(),
                text(&source.group),
                source.compile.as_ref(),
                true,
                source.skip_precompile_headers,
            )?;
        }
    }
    let listfile = source_dir.join("CMakeLists.txt");
    add(
        listfile.to_string_lossy().into_owned(),
        IdeSourceKind::ListFile,
        String::new(),
        None,
        false,
        false,
    )?;
    Ok(files)
}

const SOURCE_COMPILE_PROPERTIES: [&str; 4] = [
    "COMPILE_FLAGS",
    "COMPILE_OPTIONS",
    "COMPILE_DEFINITIONS",
    "INCLUDE_DIRECTORIES",
];

pub(super) fn references_config(value: &str) -> bool {
    ["$<CONFIG>", "$<CONFIG:", "$<CONFIGURATION>"]
        .iter()
        .any(|expression| value.contains(expression))
}

fn per_config_source_error(property: &str, value: &str, path: &str) -> GenError {
    GenError::Invalid(
        format!(
            "Xcode does not support per-config per-source {property}:
  {value}
specified for source:
  {path}"
        )
        .into(),
    )
}

pub(super) fn validate_source_flags(
    first: &[Source<'_>],
    current: &[Source<'_>],
) -> Result<(), GenError> {
    let originals: BTreeMap<_, _> = first.iter().map(|source| (&source.path, source)).collect();
    for source in current {
        if let Some(original) = originals.get(&source.path)
            && original.flags != source.flags
        {
            let first = original.compile;
            let current = source.compile;
            let property = if first.map(|compile| &compile.defines)
                != current.map(|compile| &compile.defines)
                && first
                    .into_iter()
                    .chain(current)
                    .any(|compile| !compile.defines.is_empty())
            {
                "COMPILE_DEFINITIONS"
            } else if first.map(|compile| &compile.includes)
                != current.map(|compile| &compile.includes)
                && first
                    .into_iter()
                    .chain(current)
                    .any(|compile| !compile.includes.is_empty())
            {
                "INCLUDE_DIRECTORIES"
            } else {
                "COMPILE_OPTIONS"
            };
            let flags = if source.flags.is_empty() {
                &original.flags
            } else {
                &source.flags
            };
            return Err(per_config_source_error(property, flags, &source.path));
        }
    }
    Ok(())
}

fn object_reference_path(value: &str, config: &str) -> String {
    let path = Path::new(value);
    let Some(objects) = path.parent().and_then(Path::parent) else {
        return value.into();
    };
    let Some(directory) = objects.parent() else {
        return value.into();
    };
    if objects.file_name().and_then(|part| part.to_str()) != Some("Objects-normal")
        || directory.file_name().and_then(|part| part.to_str()) != Some(config)
    {
        return value.into();
    }
    let Some(root) = directory.parent() else {
        return value.into();
    };
    let Ok(suffix) = path.strip_prefix(directory) else {
        return value.into();
    };
    rcmake_host::path::from_native_cmake(&root.join(settings::CONFIG_DIR).join(suffix)).to_string()
}

fn file_type(file: &Source<'_>) -> &'static str {
    match &file.kind {
        IdeSourceKind::Compile { language } => {
            crate::fortran_asm::file_type(file.path.as_bytes(), language).unwrap_or(match language
                .as_slice()
            {
                b"C" => "sourcecode.c.c",
                b"CXX" => "sourcecode.cpp.cpp",
                b"OBJC" => "sourcecode.c.objc",
                b"OBJCXX" => "sourcecode.cpp.objcpp",
                b"Swift" => "sourcecode.swift",
                _ => "text",
            })
        }
        IdeSourceKind::Object | IdeSourceKind::ExternalObject => "compiled.mach-o.objfile",
        IdeSourceKind::Header => "sourcecode.c.h",
        IdeSourceKind::ListFile => "sourcecode.text",
        _ => extension_file_type(&file.path),
    }
}

pub(super) fn extension_file_type(path: &str) -> &'static str {
    match Path::new(path).extension().and_then(|ext| ext.to_str()) {
        Some("plist") => "text.plist.xml",
        Some("xcassets") => "folder.assetcatalog",
        Some("storyboard") => "file.storyboard",
        Some("xib") => "file.xib",
        Some("app") => "wrapper.application",
        Some("appex") => "wrapper.app-extension",
        Some("bundle" | "plugin") => "wrapper.plug-in",
        Some("framework") => "wrapper.framework",
        Some("xpc") => "wrapper.xpc-service",
        _ => "default",
    }
}

#[cfg(test)]
mod tests {
    use super::object_reference_path;

    #[test]
    fn replaces_only_the_object_configuration_directory() {
        assert_eq!(
            object_reference_path(
                "/work/Debug/build/lib.build/Debug/Objects-normal/Debug/a.o",
                "Debug"
            ),
            "/work/Debug/build/lib.build/$(CONFIGURATION)$(EFFECTIVE_PLATFORM_NAME)/Objects-normal/Debug/a.o"
        );
        assert_eq!(
            object_reference_path("/work/Debug/a.o", "Debug"),
            "/work/Debug/a.o"
        );
    }
}
