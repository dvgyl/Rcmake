use rcmake_base::{BString, list, path_lex::Lexical, truth};
use rcmake_genex::{ConfigId, EvalContext};
use rcmake_host::{Host, ReadRange, WriteMode, path::to_native};
use rcmake_model::{ProjectModel, Target, TargetId, TargetKind};

use super::{build_relative, definition, join, shell::direct};
use crate::{ArtifactSet, CustomStep, ResolveError};

/// Apple bundle form of a target.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum BundleKind {
    /// Not a bundle.
    #[default]
    None,
    /// `MACOSX_BUNDLE` executable (`.app`).
    App {
        /// `BUNDLE_EXTENSION` (default `app`).
        extension: BString,
    },
    /// `FRAMEWORK` library.
    Framework {
        /// `BUNDLE_EXTENSION` (default `framework`).
        extension: BString,
        /// `FRAMEWORK_VERSION` (default `A`).
        version: BString,
    },
    /// `BUNDLE` module or library.
    Bundle {
        /// `BUNDLE_EXTENSION` (default `bundle`).
        extension: BString,
    },
}

/// Return the Apple bundle layout specified by the target properties.
pub fn bundle_kind(target: &Target) -> BundleKind {
    let on = |key| {
        target
            .props
            .get(key)
            .is_some_and(|value| truth::is_on(value))
    };
    let value = |key, fallback: &str| {
        target
            .props
            .get(key)
            .cloned()
            .unwrap_or_else(|| fallback.into())
    };
    match target.kind {
        TargetKind::Executable if on(b"MACOSX_BUNDLE") => BundleKind::App {
            extension: value(b"BUNDLE_EXTENSION", "app"),
        },
        TargetKind::StaticLibrary | TargetKind::SharedLibrary if on(b"FRAMEWORK") => {
            BundleKind::Framework {
                extension: value(b"BUNDLE_EXTENSION", "framework"),
                version: target
                    .props
                    .get(b"FRAMEWORK_VERSION")
                    .or_else(|| target.props.get(b"VERSION"))
                    .cloned()
                    .unwrap_or_else(|| "A".into()),
            }
        }
        TargetKind::ModuleLibrary if on(b"BUNDLE") => BundleKind::Bundle {
            extension: value(
                b"BUNDLE_EXTENSION",
                if on(b"XCTEST") { "xctest" } else { "bundle" },
            ),
        },
        _ => BundleKind::None,
    }
}

impl BundleKind {
    /// Return the bundle wrapper extension, without the leading dot.
    #[must_use]
    pub fn extension(&self) -> Option<&BString> {
        match self {
            Self::None => None,
            Self::App { extension }
            | Self::Framework { extension, .. }
            | Self::Bundle { extension } => Some(extension),
        }
    }

    pub(crate) fn root<'a>(&self, executable: &'a [u8]) -> Option<&'a [u8]> {
        let suffix = [b".".as_slice(), self.extension()?.as_slice()].concat();
        wrapper_root(Lexical::new(executable).parent(), &suffix)
    }
}

pub(crate) fn wrapper_root<'a>(mut path: &'a [u8], suffix: &[u8]) -> Option<&'a [u8]> {
    while !path.is_empty() {
        let current = Lexical::new(path);
        if current.filename().ends_with(suffix) {
            return Some(path);
        }
        let parent = current.parent();
        if path == parent {
            break;
        }
        path = parent;
    }
    None
}

pub(super) fn prepare(
    model: &ProjectModel,
    target: &Target,
    id: TargetId,
    config: &ConfigId,
    artifacts: &ArtifactSet,
    host: &dyn Host,
) -> Result<CustomStep, ResolveError> {
    let layout = bundle_kind(target);
    let BundleKind::Framework { version, .. } = &layout else {
        return Ok(CustomStep::default());
    };
    let Some(file) = &artifacts.file else {
        return Ok(CustomStep::default());
    };
    let Some(root) = layout.root(file) else {
        return Ok(CustomStep::default());
    };
    let binary_dir = &model.dir(target.dir).binary_dir;
    let source_dir = &model.dir(target.dir).source_dir;
    let relative = |path: &[u8]| build_relative(model, path);
    let content = Lexical::new(file).parent();
    let desktop = content != root;
    let executable = Lexical::new(file).filename();
    let resources = if desktop {
        join(content, b"Resources")
    } else {
        content.into()
    };
    let plist = join(&resources, b"Info.plist");
    let input_plist = join(
        binary_dir,
        &[b"CMakeFiles/", target.name.as_slice(), b".dir/Info.plist"].concat(),
    );
    let mut step = CustomStep::default();
    let cmake = definition(model, target.dir, b"CMAKE_COMMAND")
        .cloned()
        .unwrap_or_else(|| "cmake".into());
    let mut run = |operation: &str, args: &[BString]| {
        let mut argv = vec![cmake.clone(), "-E".into(), operation.into()];
        argv.extend_from_slice(args);
        step.invocations.push(direct(argv, None));
    };
    run("make_directory", &[relative(&resources)]);
    let template = target
        .props
        .get(b"MACOSX_FRAMEWORK_INFO_PLIST")
        .map(|path| join(source_dir, path))
        .or_else(|| {
            definition(model, target.dir, b"CMAKE_ROOT")
                .map(|root| join(root, b"Modules/MacOSXFrameworkInfo.plist.in"))
        });
    let input = if let Some(template) = template {
        step.depends.push(relative(&template));
        host.read(&to_native(&template), ReadRange::All { limit: 1024 * 1024 })
            .map_err(|error| {
                ResolveError::Message(format!("Cannot read {template}: {error}").into())
            })?
    } else {
        br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>CFBundleExecutable</key><string>${MACOSX_FRAMEWORK_NAME}</string><key>CFBundlePackageType</key><string>FMWK</string><key>CFBundleIdentifier</key><string>${MACOSX_FRAMEWORK_IDENTIFIER}</string></dict></plist>
"#.to_vec()
    };
    let info = expand(&input, |key| {
        if key == b"MACOSX_FRAMEWORK_NAME" {
            executable
        } else {
            target
                .props
                .get(key)
                .or_else(|| definition(model, target.dir, key))
                .map_or(b"", |value| value.as_slice())
        }
    });
    let native = to_native(&input_plist);
    let write = || {
        if let Some(parent) = native.parent() {
            host.create_dir_all(parent)?;
        }
        host.write(&native, &info, WriteMode::Truncate)
    };
    if !host
        .read(&native, ReadRange::All { limit: 1024 * 1024 })
        .is_ok_and(|old| old == info)
    {
        write().map_err(|error| ResolveError::Message(error.to_string().into()))?;
    }
    run(
        "copy_if_different",
        &[relative(&input_plist), relative(&plist)],
    );
    step.depends.push(relative(&input_plist));
    step.byproducts.push(relative(&plist));

    if desktop {
        for (destination, source) in [
            (join(root, b"Versions/Current"), version.clone()),
            (
                join(root, executable),
                join(b"Versions/Current", executable),
            ),
            (
                join(root, b"Resources"),
                "Versions/Current/Resources".into(),
            ),
        ] {
            run("create_symlink", &[source, relative(&destination)]);
            step.byproducts.push(relative(&destination));
        }
    }
    for (property, directory) in [
        (b"PUBLIC_HEADER".as_slice(), b"Headers".as_slice()),
        (b"PRIVATE_HEADER", b"PrivateHeaders"),
        (b"RESOURCE", b"Resources"),
    ] {
        let Some(values) = target.props.get(property).filter(|value| !value.is_empty()) else {
            continue;
        };
        let dest = if directory == b"Resources" {
            resources.clone()
        } else {
            join(content, directory)
        };
        run("make_directory", &[relative(&dest)]);
        if desktop && directory != b"Resources" {
            let link = relative(&join(root, directory));
            run(
                "create_symlink",
                &[join(b"Versions/Current", directory), link.clone()],
            );
            step.byproducts.push(link);
        }
        for value in list::split(values, list::Empty::Drop) {
            let source = join(source_dir, &value);
            let output = join(&dest, Lexical::new(&source).filename());
            let source = relative(&source);
            run("copy_if_different", &[source.clone(), relative(&output)]);
            step.depends.push(source);
            step.byproducts.push(relative(&output));
        }
    }
    let context = EvalContext {
        config: config.clone(),
        ..Default::default()
    };
    let mut usage = crate::usage::UsageResolver::new(model);
    let directory = model.dir(target.dir);
    for entry in usage.collect_property(id, b"SOURCES", &context)?.entries {
        let source = join(source_dir, &entry.value);
        let Some(location) =
            crate::sources::properties(directory, &source).get(b"MACOSX_PACKAGE_LOCATION")
        else {
            continue;
        };
        let location = if desktop {
            location.as_slice()
        } else if location == b"Resources" {
            b""
        } else {
            location.strip_prefix(b"Resources/").unwrap_or(location)
        };
        let dest = join(content, location);
        let output = relative(&join(&dest, Lexical::new(&source).filename()));
        let copy = if host
            .metadata(&to_native(&source), rcmake_host::Follow::Yes)
            .is_ok_and(|metadata| metadata.kind == rcmake_host::FileKind::Dir)
        {
            "copy_directory"
        } else {
            "copy"
        };
        let source = relative(&source);
        run("make_directory", &[relative(&dest)]);
        run(copy, &[source.clone(), output.clone()]);
        step.depends.push(source);
        step.byproducts.push(output);
    }
    Ok(step)
}

pub(super) fn append_text_stub(
    model: &ProjectModel,
    target: &Target,
    id: TargetId,
    config: &ConfigId,
    artifacts: &ArtifactSet,
    step: &mut CustomStep,
) -> Result<(), ResolveError> {
    let (Some(stub), Some(runtime), Some(rule)) = (
        artifacts.import_file.as_ref(),
        artifacts.file.as_ref(),
        definition(model, target.dir, b"CMAKE_CREATE_TEXT_STUBS"),
    ) else {
        return Ok(());
    };
    let relative = |path: &[u8]| build_relative(model, path);
    let quote = |path: &[u8]| rcmake_base::shell::quote(path);
    let cmake = definition(model, target.dir, b"CMAKE_COMMAND")
        .cloned()
        .unwrap_or_else(|| "cmake".into());
    let run = |operation: &str, args: &[BString]| {
        let mut argv = vec![cmake.clone(), "-E".into(), operation.into()];
        argv.extend_from_slice(args);
        direct(argv, None)
    };
    let parent = relative(Lexical::new(stub).parent());
    if !parent.is_empty() {
        step.invocations.push(run("make_directory", &[parent]));
    }
    let command = super::expand_rule(rule, |key| match key {
        b"TARGET" => Some(quote(&relative(runtime))),
        b"TARGET_IMPLIB" => Some(quote(&relative(stub))),
        b"CMAKE_OSX_SYSROOT" => Some(quote(
            definition(model, target.dir, b"CMAKE_OSX_SYSROOT")
                .filter(|value| !value.is_empty())
                .map_or(b"/", |value| value.as_slice()),
        )),
        _ => definition(model, target.dir, key).map(|value| quote(value)),
    });
    step.invocations.push(super::shell::command(command));
    step.byproducts.push(relative(stub));
    let usage = crate::usage::UsageResolver::new(model);
    for kind in [
        rcmake_genex::ArtifactKind::SonameImportFile,
        rcmake_genex::ArtifactKind::LinkerImportFile,
    ] {
        let Some(path) = usage.artifact(id, kind, rcmake_genex::ArtifactPart::Full, config) else {
            continue;
        };
        let path = relative(&path);
        if step.byproducts.contains(&path) {
            continue;
        }
        let source = if let Some(root) = bundle_kind(target).root(stub)
            && Lexical::new(stub).parent() != root
        {
            join(b"Versions/Current", Lexical::new(stub).filename())
        } else {
            Lexical::new(stub).filename().into()
        };
        step.invocations
            .push(run("create_symlink", &[source, path.clone()]));
        step.byproducts.push(path);
    }
    Ok(())
}

fn expand<'a>(input: &[u8], lookup: impl Fn(&[u8]) -> &'a [u8]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut rest = input;
    while let Some((&byte, tail)) = rest.split_first() {
        let token = match byte {
            b'@' => Some((tail, b'@')),
            b'$' => tail.strip_prefix(b"{").map(|key| (key, b'}')),
            _ => None,
        };
        if let Some((key, close)) = token
            && let Some(end) = key
                .iter()
                .position(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_')
            && key.get(end) == Some(&close)
            && end > 0
            && let Some((key, remainder)) = key.get(..end).zip(key.get(end + 1..))
        {
            output.extend_from_slice(lookup(key));
            rest = remainder;
        } else {
            output.push(byte);
            rest = tail;
        }
    }
    output
}
