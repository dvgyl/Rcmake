//! `cmake_install.cmake` scripts for `cmake --install`.

use std::path::Path;

pub(crate) mod file_sets;
mod supplementary;

#[cfg(test)]
mod file_set_tests;
pub use supplementary::write_supplementary_outputs;

use rcmake_base::BString;
use rcmake_host::{Host, path::from_native};
use rcmake_resolve::{InstallAction, InstallActionKind, InstallManifest};

use crate::{GenError, configuration_pattern, quote, quoted_content, write_if_changed};

/// Values to embed in the install script.
#[derive(Clone, Copy, Debug)]
pub struct InstallScriptOptions<'a> {
    /// Default install prefix.
    pub prefix: &'a [u8],
    /// Default configuration when the caller supplies no build type.
    pub configuration: &'a [u8],
    /// Default for `CMAKE_INSTALL_SO_NO_EXE`.
    pub shared_libraries_without_execute: bool,
    /// Whether the project was configured to cross-compile.
    pub cross_compiling: bool,
    /// Whether to write the install manifest at the end of the script.
    pub top_level: bool,
    /// Paths to the generated export files, indexed by export plan.
    pub export_files: &'a [BString],
}

impl Default for InstallScriptOptions<'_> {
    fn default() -> Self {
        Self {
            prefix: b"/usr/local",
            configuration: b"",
            shared_libraries_without_execute: false,
            cross_compiling: false,
            top_level: true,
            export_files: &[],
        }
    }
}

/// Render the top-level script with the default install options.
pub fn render(manifest: &InstallManifest, binary_dir: &[u8]) -> Result<BString, GenError> {
    render_with_options(manifest, binary_dir, &InstallScriptOptions::default())
}

/// Render the script with the install options of the configured project.
pub fn render_with_options(
    manifest: &InstallManifest,
    binary_dir: &[u8],
    options: &InstallScriptOptions<'_>,
) -> Result<BString, GenError> {
    let mut out = BString::from(
        "# CMake generated install script\n\nif(NOT DEFINED CMAKE_INSTALL_PREFIX)\n  set(CMAKE_INSTALL_PREFIX ",
    );
    quote(&mut out, options.prefix);
    out.extend_from_slice(
        br#")
endif()
string(REGEX REPLACE "/+$" "" CMAKE_INSTALL_PREFIX "${CMAKE_INSTALL_PREFIX}")

if(NOT DEFINED CMAKE_INSTALL_CONFIG_NAME)
  if(BUILD_TYPE)
    string(REGEX REPLACE "^[^A-Za-z0-9_]+" "" CMAKE_INSTALL_CONFIG_NAME "${BUILD_TYPE}")
  else()
    set(CMAKE_INSTALL_CONFIG_NAME "#,
    );
    quote(&mut out, options.configuration);
    out.extend_from_slice(
        br#")
  endif()
  message(STATUS "Install configuration: \"${CMAKE_INSTALL_CONFIG_NAME}\"")
endif()

if(NOT CMAKE_INSTALL_COMPONENT)
  if(COMPONENT)
    message(STATUS "Install component: \"${COMPONENT}\"")
    set(CMAKE_INSTALL_COMPONENT "${COMPONENT}")
  else()
    set(CMAKE_INSTALL_COMPONENT)
  endif()
endif()

"#,
    );
    for (name, value) in [
        (
            "CMAKE_INSTALL_SO_NO_EXE",
            options.shared_libraries_without_execute,
        ),
        ("CMAKE_CROSSCOMPILING", options.cross_compiling),
    ] {
        let value = if value { "TRUE" } else { "FALSE" };
        out.extend_from_slice(
            format!("if(NOT DEFINED {name})\n  set({name} \"{value}\")\nendif()\n\n").as_bytes(),
        );
    }
    for action in &manifest.actions {
        render_action(&mut out, action, options)?;
    }
    if options.top_level {
        render_manifest(&mut out, binary_dir);
    }
    Ok(out)
}

/// Return a serial include for a child install script.
pub fn include_child_script(path: &[u8]) -> BString {
    let mut code = BString::from("if(NOT CMAKE_INSTALL_LOCAL_ONLY)\n  include(");
    quote(&mut code, path);
    code.extend_from_slice(b")\nendif()");
    code
}

/// Write the install script to an existing binary directory.
pub fn write_install_script(
    manifest: &InstallManifest,
    binary_dir: &Path,
    options: &InstallScriptOptions<'_>,
    host: &dyn Host,
) -> Result<(), GenError> {
    let content = render_with_options(manifest, &from_native(binary_dir), options)?;
    write_if_changed(host, &binary_dir.join("cmake_install.cmake"), &content)
}

fn render_action(
    out: &mut BString,
    action: &InstallAction,
    options: &InstallScriptOptions<'_>,
) -> Result<(), GenError> {
    let component_guard = !action.all_components || action.exclude_from_all;
    if action.all_components && action.exclude_from_all {
        out.extend_from_slice(b"if(CMAKE_INSTALL_COMPONENT)\n");
    } else if !action.all_components {
        out.extend_from_slice(b"if(CMAKE_INSTALL_COMPONENT STREQUAL ");
        quote(out, &action.component);
        if !action.exclude_from_all {
            out.extend_from_slice(b" OR NOT CMAKE_INSTALL_COMPONENT");
        }
        out.extend_from_slice(b")\n");
    }

    let guarded = !action.configurations.is_empty();
    let indent = if guarded {
        out.extend_from_slice(b"  if(CMAKE_INSTALL_CONFIG_NAME MATCHES ");
        let pattern = configuration_pattern(&action.configurations);
        out.push(b'"');
        quoted_content(out, pattern.strip_suffix(b"$").unwrap_or(&pattern));
        out.extend_from_slice(b"$\"");
        out.extend_from_slice(b")\n");
        b"    ".as_slice()
    } else {
        b"  ".as_slice()
    };

    render_payload(out, action, options, indent)?;
    if guarded {
        out.extend_from_slice(b"  endif()\n");
    }
    if component_guard {
        out.extend_from_slice(b"endif()\n\n");
    }
    Ok(())
}

fn render_payload(
    out: &mut BString,
    action: &InstallAction,
    options: &InstallScriptOptions<'_>,
    indent: &[u8],
) -> Result<(), GenError> {
    out.extend_from_slice(indent);
    match &action.kind {
        InstallActionKind::Files {
            file_type,
            files,
            rename,
            permissions,
        } => {
            install_command(out, action, file_type);
            if let Some(rename) = rename {
                out.extend_from_slice(b" RENAME ");
                quote(out, rename);
            }
            if !permissions.is_empty() {
                out.extend_from_slice(b" PERMISSIONS");
                arguments(out, permissions);
            }
            out.extend_from_slice(b" FILES");
            arguments(out, files);
            out.extend_from_slice(b")\n");
        }
        InstallActionKind::Directory { dirs, options } => {
            install_command(out, action, b"DIRECTORY");
            out.extend_from_slice(b" FILES");
            if dirs.is_empty() {
                out.extend_from_slice(b" \"\"");
            } else {
                arguments(out, dirs);
            }
            directory_options(out, options);
            out.extend_from_slice(b")\n");
        }
        InstallActionKind::Script(script) => {
            out.extend_from_slice(b"include(");
            if let Some(rest) = script.strip_prefix(b"${CMAKE_INSTALL_PREFIX}") {
                out.extend_from_slice(b"\"${CMAKE_INSTALL_PREFIX}");
                quoted_content(out, rest);
                out.push(b'"');
            } else {
                quote(out, script);
            }
            if action.optional {
                out.extend_from_slice(b" OPTIONAL");
            }
            out.extend_from_slice(b")\n");
        }
        InstallActionKind::Code(code) => {
            // Keep the contents of multiline arguments in install(CODE) intact.
            out.extend_from_slice(code);
            out.push(b'\n');
        }
        InstallActionKind::Export { plan } => {
            let file = options
                .export_files
                .get(*plan)
                .filter(|file| !file.is_empty())
                .ok_or(GenError::MissingExport(*plan))?;
            install_command(out, action, b"FILE");
            out.extend_from_slice(b" FILES ");
            quote(out, file);
            out.extend_from_slice(b")\n");
        }
    }
    Ok(())
}

fn install_command(out: &mut BString, action: &InstallAction, file_type: &[u8]) {
    out.extend_from_slice(b"file(INSTALL DESTINATION \"");
    if !rcmake_host::path::to_native(&action.destination).is_absolute() {
        out.extend_from_slice(b"${CMAKE_INSTALL_PREFIX}/");
    }
    quoted_content(out, &action.destination);
    out.extend_from_slice(b"\" TYPE ");
    quote(out, file_type);
    if action.optional {
        out.extend_from_slice(b" OPTIONAL");
    }
    let message = match &action.kind {
        InstallActionKind::Directory { options, .. }
            if options
                .iter()
                .take_while(|option| !matches!(option.as_slice(), b"PATTERN" | b"REGEX"))
                .any(|option| option == b"MESSAGE_NEVER") =>
        {
            b"NEVER"
        }
        _ => action.message.as_slice(),
    };
    if matches!(message, b"ALWAYS" | b"LAZY" | b"NEVER") {
        out.extend_from_slice(b" MESSAGE_");
        out.extend_from_slice(message);
    }
}

fn arguments(out: &mut BString, values: &[BString]) {
    for value in values {
        out.push(b' ');
        quote(out, value);
    }
}

fn directory_options(out: &mut BString, options: &[BString]) {
    let mut options = options.iter();
    while let Some(option) = options.next() {
        if option == b"MESSAGE_NEVER" {
            continue;
        }
        out.push(b' ');
        if option == b"PATTERN" {
            out.extend_from_slice(b"REGEX");
            if let Some(pattern) = options.next() {
                out.push(b' ');
                quote(out, &directory_pattern(pattern));
            }
        } else {
            quote(out, option);
            if option == b"REGEX"
                && let Some(pattern) = options.next()
            {
                out.push(b' ');
                quote(out, pattern);
            }
        }
    }
}

fn directory_pattern(pattern: &[u8]) -> BString {
    let mut out = BString::from("/");
    let mut pattern = pattern;
    let mut can_open_class = true;
    while let Some((&byte, rest)) = pattern.split_first() {
        pattern = rest;
        match byte {
            b'*' => out.extend_from_slice(b"[^/]*"),
            b'?' => out.extend_from_slice(b"[^/]"),
            b'[' if can_open_class => {
                if let Some((class, rest)) = split_pattern_class(pattern) {
                    out.push(b'[');
                    if let Some(negated) = class.strip_prefix(b"!") {
                        out.push(b'^');
                        out.extend_from_slice(negated);
                    } else {
                        out.extend_from_slice(class);
                    }
                    out.push(b']');
                    pattern = rest;
                } else {
                    can_open_class = false;
                    regex_literal(&mut out, byte);
                }
            }
            b'\\' if let Some((&literal, rest)) = pattern.split_first() => {
                regex_literal(&mut out, literal);
                pattern = rest;
            }
            _ => regex_literal(&mut out, byte),
        }
    }
    out.push(b'$');
    out
}

fn split_pattern_class(pattern: &[u8]) -> Option<(&[u8], &[u8])> {
    let members = pattern
        .strip_prefix(b"!")
        .or_else(|| pattern.strip_prefix(b"^"))
        .unwrap_or(pattern);
    let skip = pattern.len() - members.len() + usize::from(members.starts_with(b"]"));
    let end = skip + pattern.get(skip..)?.iter().position(|&byte| byte == b']')?;
    let (class, close) = pattern.split_at_checked(end)?;
    Some((class, close.strip_prefix(b"]")?))
}

fn regex_literal(out: &mut BString, byte: u8) {
    if b".^$[()|*+?\\".contains(&byte) {
        out.push(b'\\');
    }
    out.push(byte);
}

/// Append the commands that write the top-level install manifest.
pub fn render_manifest(out: &mut BString, binary_dir: &[u8]) {
    out.extend_from_slice(
        br#"if(CMAKE_INSTALL_COMPONENT)
  if(CMAKE_INSTALL_COMPONENT MATCHES "^[a-zA-Z0-9_.+-]+$")
    set(CMAKE_INSTALL_MANIFEST "install_manifest_${CMAKE_INSTALL_COMPONENT}.txt")
  else()
    string(MD5 CMAKE_INST_COMP_HASH "${CMAKE_INSTALL_COMPONENT}")
    set(CMAKE_INSTALL_MANIFEST "install_manifest_${CMAKE_INST_COMP_HASH}.txt")
    unset(CMAKE_INST_COMP_HASH)
  endif()
else()
  set(CMAKE_INSTALL_MANIFEST "install_manifest.txt")
endif()

string(REPLACE ";" "\n" CMAKE_INSTALL_MANIFEST_CONTENT "${CMAKE_INSTALL_MANIFEST_FILES}")
file(WRITE ""#,
    );
    quoted_content(out, binary_dir);
    out.extend_from_slice(b"/${CMAKE_INSTALL_MANIFEST}\" \"${CMAKE_INSTALL_MANIFEST_CONTENT}\")\n");
}
