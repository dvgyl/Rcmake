use std::path::{Path, PathBuf};

use rcmake_base::{BString, list};
use rcmake_diag::{
    Backtrace, Diagnostic, Effect, Frame, FrameKind, Location, MessageKind, PolicyId, PolicyStatus,
    RenderOptions,
};
use rcmake_eval::{CmdError, Cx};
use rcmake_expr::ExpandedArg;
use rcmake_host::{Host, WriteMode, path};

use super::args::{Options, Source};
use super::seed::{enabled_languages, language_properties};
#[cfg(all(feature = "native", any(unix, windows)))]
use super::seed::{enabled_languages_peek, language_properties_peek};
use super::{absolute, error_text, probe_error, split_byte};

pub(super) fn write_project(
    cx: &mut Cx<'_>,
    options: &Options<'_>,
    sources: &[Source<'_>],
    directory: &Path,
    target: &str,
    run: bool,
) -> Result<Vec<BString>, CmdError> {
    let target_type = cx.var_or_empty(b"CMAKE_TRY_COMPILE_TARGET_TYPE");
    let archive = match (run, target_type.as_slice()) {
        (true, _) | (_, b"" | b"EXECUTABLE") => false,
        (_, b"STATIC_LIBRARY") => true,
        _ => {
            return Err(probe_error(
                cx,
                [b"Invalid value '".as_slice(), &target_type, b"' for CMAKE_TRY_COMPILE_TARGET_TYPE.  Only 'EXECUTABLE' and 'STATIC_LIBRARY' are allowed."].concat(),
            ));
        }
    };
    let mut files = Vec::new();
    let mut languages = Vec::new();
    let enabled = enabled_languages(cx);
    for &source in sources {
        let file = match source {
            Source::File(file) => path::to_native(file),
            Source::Content(name, value) => write_source(cx.host(), directory, name, value)?,
            Source::Variable(name, var) => {
                write_source(cx.host(), directory, name, &cx.var_or_empty(var))?
            }
            Source::Copy(name, file) => {
                let source = absolute(&cx.host().initial_cwd(), file);
                let target = directory.join(path::to_native(name));
                let contents = cx
                    .host()
                    .read(
                        &source,
                        rcmake_host::ReadRange::All {
                            limit: cx.limits().max_file_read,
                        },
                    )
                    .map_err(|error| source_copy_error(cx, file, &error))?;
                cx.host()
                    .write(&target, &contents, WriteMode::CreateNew)
                    .map_err(|error| source_copy_error(cx, file, &error))?;
                target
            }
        };
        let file_path = path::from_native_cmake(&file);
        let Some(language) = source_language(cx, &file, &enabled) else {
            let source_name = if let Source::File(name) = source {
                name
            } else {
                &file_path
            };
            let extension = file.extension().map_or_else(Vec::new, |value| {
                [b".".as_slice(), value.as_encoded_bytes()].concat()
            });
            let message = [
                b"Unknown extension \"".as_slice(),
                &extension,
                b"\" for file\n  ",
                source_name,
                b"\ntry_compile() works only for enabled languages.  Currently these are:\n  ",
                &enabled
                    .iter()
                    .map(|language| language.as_slice())
                    .collect::<Vec<_>>()
                    .join(b" ".as_slice()),
                b"\nSee project() command to enable other languages.",
            ]
            .concat();
            return Err(probe_error(cx, message));
        };
        if !languages.contains(&language) {
            languages.push(language);
        }
        if let Source::File(name) = source {
            let input = absolute(directory, name);
            if !input.starts_with(directory) {
                let input = path::from_native_cmake(&input);
                let dir = cx.current_dir_id();
                let inputs = &mut cx.project().dir_mut(dir).listfiles;
                if !inputs.contains(&input) {
                    inputs.push(input);
                }
            }
        }
        files.push(file_path);
    }
    let language = options.one(b"LINKER_LANGUAGE");
    if let Some(language) = language
        && enabled.iter().any(|item| item == language)
        && !languages.contains(&language)
    {
        languages.push(language);
    }

    let compile_flags = compile_flags(cx, options);
    super::seed::warn_ignored_standards(cx, options, &languages);
    let properties = language_properties(cx, options, &languages);
    let probe_languages = languages.iter().map(|&lang| BString::from(lang)).collect();
    let code = render_project(
        ProjectInputs {
            archive,
            files,
            languages,
            compile_flags,
            language,
            policies: outer_policies(cx),
            properties: &properties,
        },
        options,
        target,
    );
    let lists = directory.join("CMakeLists.txt");
    cx.host()
        .write(&lists, &code, WriteMode::CreateNew)
        .map_err(error_text)?;
    for (key, value) in &properties {
        let Some(lang) = key.strip_suffix(b"_STANDARD") else {
            continue;
        };
        let default = [b"CMAKE_", lang, b"_STANDARD_DEFAULT"].concat();
        if valid_standard(lang, value) || cx.var_or_empty(&default).is_empty() {
            continue;
        }
        let command = if archive {
            b"add_library".as_slice()
        } else {
            b"add_executable"
        };
        let line = code
            .as_slice()
            .split(|&byte| byte == b'\n')
            .position(|line| line.starts_with(command))
            .and_then(|index| u32::try_from(index + 1).ok())
            .unwrap_or(1);
        let file = path::from_native_cmake(&lists);
        let trace = Backtrace::default().push(Frame {
            file: file.as_slice().into(),
            line,
            name: Some(command.into()),
            kind: FrameKind::Command,
        });
        let diagnostic = Diagnostic {
            annotations: None,
            kind: MessageKind::Error,
            effect: Effect::Fatal,
            category: None,
            policy: None,
            location: Location::Backtrace(trace),
            body: format!("{key} is set to invalid value '{value}'").into(),
        };
        cx.host().write_out(
            rcmake_host::Stream::Stderr,
            &rcmake_diag::render(&diagnostic, &RenderOptions::default()),
        );
        let error = probe_error(cx, "Failed to generate test project build system.");
        cx.set_fatal();
        return Err(error);
    }
    Ok(probe_languages)
}

fn compile_flags(cx: &mut Cx<'_>, options: &Options<'_>) -> Vec<BString> {
    options
        .many(b"COMPILE_DEFINITIONS")
        .iter()
        .flat_map(|arg| list::iter(arg, list::Empty::Drop))
        .flat_map(|arg| compile_definition_words(cx, &arg))
        .collect()
}

#[cfg(all(feature = "native", any(unix, windows)))]
pub(super) fn render_literal_probe(
    cx: &Cx<'_>,
    file: &Path,
    target: &str,
) -> Option<(BString, BString)> {
    let enabled = enabled_languages_peek(cx);
    let language = source_language_peek(cx, file, &enabled)?;
    let languages = vec![language];
    let options = Options::default();
    let properties = language_properties_peek(cx, &options, &languages);
    let project = render_project(
        ProjectInputs {
            archive: cx
                .peek_var(b"CMAKE_TRY_COMPILE_TARGET_TYPE")
                .is_some_and(|value| value == "STATIC_LIBRARY"),
            files: vec![path::from_native_cmake(file)],
            languages,
            compile_flags: Vec::new(),
            language: None,
            policies: outer_policies(cx),
            properties: &properties,
        },
        &options,
        target,
    );
    Some((project, language.into()))
}

fn valid_standard(language: &[u8], value: &[u8]) -> bool {
    let Ok(value) = std::str::from_utf8(value) else {
        return false;
    };
    let Ok(level) = value.parse::<u32>() else {
        return false;
    };
    match language {
        b"C" | b"OBJC" => matches!(level, 90 | 99 | 11 | 17 | 23),
        b"CUDA" => matches!(level, 3 | 98 | 11 | 14 | 17 | 20 | 23 | 26),
        b"CXX" | b"OBJCXX" | b"HIP" | b"SYCL_CXX" => {
            matches!(level, 98 | 11 | 14 | 17 | 20 | 23 | 26)
        }
        _ => true,
    }
}

fn source_copy_error(cx: &mut Cx<'_>, file: &[u8], error: &rcmake_host::HostError) -> CmdError {
    probe_error(
        cx,
        [
            b"SOURCE_FROM_FILE failed to copy \"".as_slice(),
            file,
            b"\": ",
            error.os_message.as_bytes(),
        ]
        .concat(),
    )
}

/// Policies that the test project takes from the calling project. They can affect the
/// toolchain file (`CMP0126`), language extensions (`CMP0128`), and the linker flags
/// (`CMP0181`, `CMP0210`).
pub(super) fn outer_policies(cx: &Cx<'_>) -> [(u16, PolicyStatus); 4] {
    [126, 128, 181, 210].map(|id| (id, cx.policy(PolicyId::new(id))))
}

struct ProjectInputs<'a> {
    archive: bool,
    files: Vec<BString>,
    languages: Vec<&'a [u8]>,
    compile_flags: Vec<BString>,
    language: Option<&'a [u8]>,
    policies: [(u16, PolicyStatus); 4],
    properties: &'a [(BString, BString)],
}

fn render_project(inputs: ProjectInputs<'_>, options: &Options<'_>, target: &str) -> BString {
    let ProjectInputs {
        archive,
        files,
        languages,
        compile_flags,
        language,
        policies,
        properties,
    } = inputs;
    let mut code = BString::from("cmake_minimum_required(VERSION 4.5)\n");
    for (id, status) in policies {
        if status != PolicyStatus::New {
            code.extend_from_slice(format!("cmake_policy(SET CMP{id:04} OLD)\n").as_bytes());
        }
    }
    code.extend_from_slice(b"project(CMAKE_TRY_COMPILE LANGUAGES");
    for &lang in &languages {
        code.push(b' ');
        code.extend_from_slice(lang);
    }
    code.extend_from_slice(b")\n");
    if options.one(b"__CMAKE_INTERNAL") == Some(b"ABI") {
        // Erase include_directories() calls from a toolchain file so the ABI
        // probe does not record them as implicit compiler include directories.
        code.extend_from_slice(b"set_property(DIRECTORY PROPERTY INCLUDE_DIRECTORIES \"\")\n");
    }
    code.extend_from_slice(
        b"include_directories(${INCLUDE_DIRECTORIES})\nlink_directories(${LINK_DIRECTORIES})\n",
    );
    if languages.contains(&b"CUDA".as_slice()) {
        code.extend_from_slice(
            b"string(REPLACE \"<CUDA_COMPILE_MODE>\" \"${_CMAKE_CUDA_WHOLE_FLAG}\" CMAKE_CUDA_COMPILE_OBJECT \"${CMAKE_CUDA_COMPILE_OBJECT}\")\n",
        );
    }
    code.extend_from_slice(b"if(NOT \"${COMPILE_DEFINITIONS}\" STREQUAL \"\")\n");
    for lang in languages {
        let flags = BString::from([b"CMAKE_", lang, b"_FLAGS"].concat());
        code.extend_from_slice(
            format!("set({flags} \"${{{flags}}} ${{COMPILE_DEFINITIONS}}\")\n").as_bytes(),
        );
    }
    code.extend_from_slice(b"endif()\n");
    let prefix = if archive {
        format!("add_library({target} STATIC")
    } else {
        format!("add_executable({target}")
    };
    cmake_command(
        &mut code,
        &prefix,
        files.iter().map(|value| value.as_slice()),
    );
    for (name, value) in properties {
        cmake_command(
            &mut code,
            &format!("set_property(TARGET {target} PROPERTY {name}"),
            [value.as_slice()],
        );
    }
    for (command, definitions) in [
        ("target_compile_definitions", true),
        ("target_compile_options", false),
    ] {
        let values: Vec<_> = compile_flags
            .iter()
            .filter_map(|arg| {
                let arg = arg.as_slice();
                let definition = arg.strip_prefix(b"-D").or_else(|| arg.strip_prefix(b"/D"));
                if definitions {
                    definition
                } else {
                    definition.is_none().then_some(arg)
                }
            })
            .map(escape_definition_list_item)
            .collect();
        if !values.is_empty() {
            cmake_command(
                &mut code,
                &format!("{command}({target} PRIVATE"),
                values.iter().map(|value| value.as_slice()),
            );
        }
    }
    if !archive {
        code.extend_from_slice(
            b"separate_arguments(_probe_link_flags UNIX_COMMAND \"${EXE_LINKER_FLAGS}\")\n",
        );
        code.extend_from_slice(
            format!("target_link_options({target} PRIVATE ${{_probe_link_flags}})\n").as_bytes(),
        );
    }
    let link_options = options.many(b"LINK_OPTIONS");
    if !link_options.is_empty() {
        if archive {
            let flags = list::join(link_options);
            cmake_command(
                &mut code,
                &format!("set_property(TARGET {target} PROPERTY STATIC_LIBRARY_OPTIONS"),
                [flags.as_slice()],
            );
        } else {
            cmake_command(
                &mut code,
                &format!("target_link_options({target} PRIVATE"),
                link_options.iter().copied(),
            );
        }
    }
    if options.has(b"LINK_LIBRARIES") {
        code.extend_from_slice(format!("target_link_libraries({target}").as_bytes());
        for item in options.many(b"LINK_LIBRARIES") {
            code.push(b' ');
            write_quoted_arg(&mut code, item.trim_ascii());
        }
        code.extend_from_slice(b")\n");
    } else {
        code.extend_from_slice(
            format!("target_link_libraries({target} ${{LINK_LIBRARIES}})\n").as_bytes(),
        );
    }
    if let Some(language) = language {
        cmake_command(
            &mut code,
            &format!("set_property(TARGET {target} PROPERTY LINKER_LANGUAGE"),
            [language],
        );
    }
    code
}

fn write_source(
    host: &dyn Host,
    directory: &Path,
    name: &[u8],
    value: &[u8],
) -> Result<PathBuf, CmdError> {
    let file = directory.join(path::to_native(name));
    host.write(&file, value, WriteMode::CreateNew)
        .map_err(error_text)?;
    Ok(file)
}

fn cmake_command<'a>(code: &mut BString, prefix: &str, args: impl IntoIterator<Item = &'a [u8]>) {
    code.extend_from_slice(prefix.as_bytes());
    for arg in args {
        code.push(b' ');
        write_bracket_arg(code, arg);
    }
    code.extend_from_slice(b")\n");
}

pub(super) fn write_quoted_arg(code: &mut BString, arg: &[u8]) {
    code.push(b'"');
    for &byte in arg {
        if matches!(byte, b'\\' | b'"' | b'$' | b'@') {
            code.push(b'\\');
        }
        code.push(byte);
    }
    code.push(b'"');
}

pub(super) fn write_bracket_arg(code: &mut BString, arg: &[u8]) {
    let mut delimiter = vec![b']', b']'];
    while arg
        .windows(delimiter.len())
        .any(|window| window == delimiter)
        || arg.ends_with(delimiter.get(..delimiter.len() - 1).unwrap_or_default())
    {
        delimiter.insert(1, b'=');
    }
    code.push(b'[');
    code.extend_from_slice(delimiter.get(1..delimiter.len() - 1).unwrap_or_default());
    code.extend_from_slice(b"[\n");
    code.extend_from_slice(arg);
    code.extend_from_slice(&delimiter);
}

fn source_language<'a>(cx: &mut Cx<'_>, file: &Path, enabled: &'a [BString]) -> Option<&'a [u8]> {
    let extension = file.extension()?.as_encoded_bytes();
    enabled
        .iter()
        .rev()
        .map(|language| language.as_slice())
        .find(|language| {
            let name = [b"CMAKE_".as_slice(), language, b"_SOURCE_FILE_EXTENSIONS"].concat();
            if let Some(configured) = cx.var(&name) {
                return list::split(&configured, list::Empty::Drop)
                    .iter()
                    .any(|item| item == extension);
            }
            default_source_language(language, extension)
        })
}

#[cfg(all(feature = "native", any(unix, windows)))]
fn source_language_peek<'a>(cx: &Cx<'_>, file: &Path, enabled: &'a [BString]) -> Option<&'a [u8]> {
    let extension = file.extension()?.as_encoded_bytes();
    enabled
        .iter()
        .rev()
        .map(|language| language.as_slice())
        .find(|language| {
            let name = [b"CMAKE_".as_slice(), language, b"_SOURCE_FILE_EXTENSIONS"].concat();
            if let Some(configured) = cx.peek_var(&name) {
                return list::split(configured, list::Empty::Drop)
                    .iter()
                    .any(|item| item == extension);
            }
            default_source_language(language, extension)
        })
}

fn default_source_language(language: &[u8], extension: &[u8]) -> bool {
    match language {
        b"C" => extension == b"c",
        b"CXX" => matches!(
            extension,
            b"C" | b"cc"
                | b"cpp"
                | b"cxx"
                | b"c++"
                | b"CPP"
                | b"ixx"
                | b"cppm"
                | b"ccm"
                | b"cxxm"
                | b"c++m"
                | b"M"
                | b"m"
                | b"mm"
                | b"mpp"
        ),
        b"OBJC" => extension == b"m",
        b"OBJCXX" => matches!(extension, b"M" | b"m" | b"mm"),
        _ => false,
    }
}

fn escape_definition_list_item(value: &[u8]) -> BString {
    let mut escaped = BString::default();
    for &byte in value {
        if byte == b';' {
            escaped.push(b'\\');
        }
        escaped.push(byte);
    }
    escaped
}

fn compile_definition_words(cx: &mut Cx<'_>, value: &[u8]) -> Vec<BString> {
    let mut words = command_words(cx, value);
    if let [word] = words.as_mut_slice()
        && (value.starts_with(b"-D") || value.starts_with(b"/D"))
        && let Some((_, definition)) = split_byte(value, b'=')
        && definition.starts_with(b"\"")
        && definition.ends_with(b"\"")
    {
        // Keep the quotes in a single macro definition for the preprocessor.
        *word = value.into();
    }
    words
}

fn command_words(cx: &mut Cx<'_>, value: &[u8]) -> Vec<BString> {
    if value.is_empty() {
        return Vec::new();
    }
    let var = b"__rcmake_try_compile_words";
    let previous = cx.var_normal_only(var);
    let args = [var.as_slice(), b"UNIX_COMMAND", value]
        .into_iter()
        .map(|value| ExpandedArg {
            value: value.into(),
            kind: rcmake_syntax::ArgKind::Quoted,
            line: 0,
        })
        .collect();
    let words = if cx.call_command(b"separate_arguments", args).is_ok() {
        list::split(&cx.var_or_empty(var), list::Empty::Keep)
    } else {
        Vec::new()
    };
    if let Some(previous) = previous {
        cx.set_var(var, previous);
    } else {
        cx.unset_var(var);
    }
    words
}
