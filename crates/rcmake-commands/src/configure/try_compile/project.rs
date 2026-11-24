use std::path::{Path, PathBuf};

use rcmake_base::{BString, list};
use rcmake_eval::{CmdError, Cx};
use rcmake_expr::ExpandedArg;
use rcmake_host::{Host, WriteMode, path};

use super::args::{Options, Source};
use super::{absolute, error_text, split_byte};

pub(super) fn write_project(
    cx: &mut Cx<'_>,
    options: &Options<'_>,
    sources: &[Source<'_>],
    directory: &Path,
    run: bool,
) -> Result<(), CmdError> {
    let target_type = cx.var_or_empty(b"CMAKE_TRY_COMPILE_TARGET_TYPE");
    let archive = match (run, target_type.as_slice()) {
        (true, _) | (_, b"" | b"EXECUTABLE") => false,
        (_, b"STATIC_LIBRARY") => true,
        _ => {
            return Err(CmdError::status(
                "CMAKE_TRY_COMPILE_TARGET_TYPE must be EXECUTABLE or STATIC_LIBRARY.",
            ));
        }
    };
    let mut files = Vec::new();
    let mut languages = Vec::new();
    for &source in sources {
        let file = match source {
            Source::File(file) => absolute(directory, file),
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
                    .map_err(error_text)?;
                cx.host()
                    .write(&target, &contents, WriteMode::CreateNew)
                    .map_err(error_text)?;
                target
            }
        };
        if let Some(language) = source_language(&file)
            && !languages.contains(&language)
        {
            languages.push(language);
        }
        files.push(path::from_native(&file));
    }
    let language = options.one(b"LINKER_LANGUAGE");
    if let Some(known @ (b"C" | b"CXX")) = language
        && !languages.contains(&known)
    {
        languages.push(known);
    }
    if languages.is_empty() {
        return Err(CmdError::status(
            "The probe requires a C or C++ source file.",
        ));
    }

    let compile_flags: Vec<_> = options
        .many(b"COMPILE_DEFINITIONS")
        .iter()
        .flat_map(|arg| compile_definition_words(cx, arg))
        .collect();
    let code = render_project(
        ProjectInputs {
            archive,
            files,
            languages,
            compile_flags,
            language,
        },
        options,
    );
    cx.host()
        .write(
            &directory.join("CMakeLists.txt"),
            &code,
            WriteMode::CreateNew,
        )
        .map_err(error_text)
}

struct ProjectInputs<'a> {
    archive: bool,
    files: Vec<BString>,
    languages: Vec<&'a [u8]>,
    compile_flags: Vec<BString>,
    language: Option<&'a [u8]>,
}

fn render_project(inputs: ProjectInputs<'_>, options: &Options<'_>) -> BString {
    let ProjectInputs {
        archive,
        files,
        languages,
        compile_flags,
        language,
    } = inputs;
    let mut code = BString::from(
        b"cmake_minimum_required(VERSION 4.5)\nproject(CMAKE_TRY_COMPILE LANGUAGES".as_slice(),
    );
    for lang in languages {
        code.push(b' ');
        code.extend_from_slice(lang);
    }
    code.extend_from_slice(
        b")\ninclude_directories(${INCLUDE_DIRECTORIES})\nlink_directories(${LINK_DIRECTORIES})\n",
    );
    code.extend_from_slice(
        b"separate_arguments(_probe_compile_flags UNIX_COMMAND \"${COMPILE_DEFINITIONS}\")\n\
          add_compile_options(${_probe_compile_flags})\n",
    );
    let prefix = if archive {
        "add_library(cmTryCompile STATIC"
    } else {
        "add_executable(cmTryCompile"
    };
    cmake_command(
        &mut code,
        prefix,
        files.iter().map(|value| value.as_slice()),
    );
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
            .collect();
        if !values.is_empty() {
            cmake_command(
                &mut code,
                &format!("{command}(cmTryCompile PRIVATE"),
                values,
            );
        }
    }
    if !archive {
        code.extend_from_slice(
            b"separate_arguments(_probe_link_flags UNIX_COMMAND \"${EXE_LINKER_FLAGS}\")\n\
              target_link_options(cmTryCompile PRIVATE ${_probe_link_flags})\n",
        );
    }
    let link_options = options.many(b"LINK_OPTIONS");
    if !link_options.is_empty() {
        if archive {
            let flags = list::join(link_options);
            cmake_command(
                &mut code,
                "set_property(TARGET cmTryCompile PROPERTY STATIC_LIBRARY_OPTIONS",
                [flags.as_slice()],
            );
        } else {
            cmake_command(
                &mut code,
                "target_link_options(cmTryCompile PRIVATE",
                link_options.iter().copied(),
            );
        }
    }
    if options.has(b"LINK_LIBRARIES") {
        cmake_command(
            &mut code,
            "target_link_libraries(cmTryCompile PRIVATE",
            options.many(b"LINK_LIBRARIES").iter().copied(),
        );
    } else {
        code.extend_from_slice(b"target_link_libraries(cmTryCompile PRIVATE ${LINK_LIBRARIES})\n");
    }
    if let Some(language) = language {
        cmake_command(
            &mut code,
            "set_property(TARGET cmTryCompile PROPERTY LINKER_LANGUAGE",
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

fn write_bracket_arg(code: &mut BString, arg: &[u8]) {
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

fn source_language(file: &Path) -> Option<&'static [u8]> {
    match file.extension()?.as_encoded_bytes() {
        b"c" => Some(b"C"),
        b"C" | b"cc" | b"cpp" | b"cxx" | b"c++" | b"CPP" | b"ixx" | b"cppm" => Some(b"CXX"),
        _ => None,
    }
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
