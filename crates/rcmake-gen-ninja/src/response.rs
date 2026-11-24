use std::{collections::HashMap, ops::Range};

use rcmake_base::{BString, hash::HashAlgo, truth};
use rcmake_host::Host;
use rcmake_model::{DirId, ProjectModel};
use rcmake_resolve::{GenerationResult, Invocation, LinkKind, LinkStep};

use crate::commands::windows_argument;
use crate::graph::Graph;
use crate::syntax::Ninja;

pub(crate) const COMPILE_MSVC: &[u8] = b"COMPILE_MSVC";

#[derive(Default)]
pub(super) struct Settings<'a> {
    pub(super) model: Option<&'a ProjectModel>,
    pub(super) result: Option<&'a GenerationResult>,
    pub(super) force: bool,
    dirs: HashMap<&'a [u8], DirId>,
}

pub(super) struct Response {
    pub(super) rule: &'static [u8],
    pub(super) file: BString,
    pub(super) contents: BString,
    pub(super) command: Vec<u8>,
}

impl<'a> Graph<'a> {
    pub(crate) fn set_command_settings(
        &mut self,
        model: Option<&'a ProjectModel>,
        host: &dyn Host,
    ) {
        self.command_settings.model = model;
        self.command_settings.force = host.env_get(b"CMAKE_NINJA_FORCE_RESPONSE_FILE").is_some();
        if let (Some(model), Some(result)) = (model, self.command_settings.result) {
            for resolved in result
                .targets
                .iter()
                .filter(|target| target.config == self.input.config)
            {
                if let Some(target) = model.target(resolved.id) {
                    self.command_settings.dirs.extend(
                        resolved
                            .compile_units
                            .iter()
                            .map(|unit| unit.object.as_slice())
                            .chain(resolved.link.iter().map(|link| link.output.as_slice()))
                            .map(|output| (output, target.dir)),
                    );
                }
            }
        }
    }

    pub(super) fn msvc_deps(&self, object: &BString, invocation: &Invocation) -> bool {
        if self.commands.raw_script(invocation).is_some_and(|script| {
            script
                .split(|&byte| byte.is_ascii_whitespace())
                .any(|arg| arg.eq_ignore_ascii_case(b"/showIncludes"))
        }) {
            return true;
        }
        let (dir, language) = self.compile_location(object);
        self.language_setting(dir, language, b"_DEPFILE_FORMAT")
            .map_or_else(
                || {
                    invocation.argv.iter().any(|arg| {
                        arg.eq_ignore_ascii_case(b"/showIncludes")
                            || arg.eq_ignore_ascii_case(b"-showIncludes")
                    })
                },
                |format| format == b"msvc",
            )
    }

    pub(super) fn resource_dependency_scan(
        &self,
        object: &BString,
        source: &BString,
        invocation: &Invocation,
    ) -> Option<Invocation> {
        let (dir, language) = self.compile_location(object);
        let model = self.command_settings.model?;
        if !self.commands.windows()
            || language != b"RC"
            || !model
                .definition(dir, b"CMAKE_NINJA_CMCLDEPS_RC")
                .is_some_and(|value| truth::is_on(value))
        {
            return None;
        }
        let compiler = [b"CMAKE_C_COMPILER".as_slice(), b"CMAKE_CXX_COMPILER"]
            .into_iter()
            .find_map(|name| {
                model
                    .definition(dir, name)
                    .filter(|value| !truth::is_off(value))
            })?;
        let mut argv: Vec<BString> = [
            b"/nologo".as_slice(),
            b"/P",
            b"/FiNUL",
            b"/DRC_INVOKED",
            b"/showIncludes",
            b"/TC",
        ]
        .into_iter()
        .map(BString::from)
        .collect();
        argv.insert(0, compiler.clone());
        let mut flags = invocation.argv.iter().skip(1);
        while let Some(flag) = flags.next() {
            if let [
                b'/' | b'-',
                option @ (b'D' | b'd' | b'I' | b'i' | b'U' | b'u'),
                value @ ..,
            ] = flag.as_slice()
            {
                let option = option.to_ascii_uppercase();
                let value = if value.is_empty() {
                    flags.next()?.as_slice()
                } else {
                    value
                };
                argv.push([&[b'/', option], value].concat().into());
            }
        }
        argv.push(source.clone());
        Some(Invocation {
            argv,
            working_dir: invocation.working_dir.clone(),
            ..Default::default()
        })
    }

    pub(super) fn compile_response(
        &self,
        rule: &'static [u8],
        object: &BString,
        invocation: &Invocation,
        ordinary: &[u8],
    ) -> Option<Response> {
        let (dir, language) = self.compile_location(object);
        let argv = &invocation.argv;
        let msvc = argv.iter().any(|arg| msvc_compile_program(arg));
        if language == b"RC"
            || !self.use_response(dir, ordinary)
            || (!self.commands.windows() && !msvc)
            || invocation.working_dir.is_some()
        {
            return None;
        }
        let compiler = self
            .command_settings
            .result
            .and_then(|result| result.metadata.toolchains.get(language))
            .map(|toolchain| &toolchain.compiler_path);
        let program = argv.iter().position(|arg| {
            msvc_compile_program(arg) || compiler.is_some_and(|expected| same_tool(arg, expected))
        })?;
        let start = program + 1;
        let end = argv
            .iter()
            .enumerate()
            .skip(start)
            .find_map(|(index, arg)| {
                (arg.starts_with(b"/Fo") || matches!(arg.as_slice(), b"-o" | b"-c"))
                    .then_some(index)
            })?;
        let flag = self
            .language_setting(dir, language, b"_RESPONSE_FILE_FLAG")
            .filter(|flag| !flag.is_empty())
            .map_or(b"@".as_slice(), |value| value.as_slice());
        self.response(
            rule,
            object,
            std::slice::from_ref(invocation),
            (0, start..end, false),
            flag,
            msvc,
        )
    }

    pub(super) fn link_response(
        &self,
        step: &LinkStep,
        inputs: &[BString],
        invocations: &[Invocation],
        ordinary: &[u8],
    ) -> Option<Response> {
        let dir = self.target_dir_for_output(&step.output);
        let language = &step.language;
        if !self.use_response(dir, ordinary) || language == b"RC" {
            return None;
        }
        let archive = step.kind == LinkKind::StaticArchive;
        let msvc = invocations
            .iter()
            .any(|invocation| msvc_link_program(&invocation.argv).is_some());
        if !self.commands.windows() && !msvc {
            return None;
        }
        let flag = archive
            .then(|| self.language_setting(dir, language, b"_RESPONSE_FILE_ARCHIVE_FLAG"))
            .flatten()
            .or_else(|| self.language_setting(dir, language, b"_RESPONSE_FILE_LINK_FLAG"))
            .map_or(b"@".as_slice(), |value| value.as_slice());
        if flag.is_empty() {
            return None;
        }
        let library_response = !archive
            && self
                .language_setting(dir, language, b"_USE_RESPONSE_FILE_FOR_LIBRARIES")
                .is_none_or(|setting| truth::is_on(setting));
        let index_and_args = invocations
            .iter()
            .enumerate()
            .find_map(|(index, invocation)| {
                if invocation.working_dir.is_some() {
                    return None;
                }
                let argv = &invocation.argv;
                let native = msvc_link_program(argv);
                let wrapped = argv
                    .iter()
                    .any(|arg| matches!(arg.as_slice(), b"vs_link_exe" | b"vs_link_dll"));
                if let Some(program) = native.filter(|_| library_response) {
                    return Some((index, program + 1..argv.len(), wrapped));
                }
                let start = argv
                    .iter()
                    .enumerate()
                    .skip(native.map_or(1, |n| n + 1))
                    .find_map(|(index, arg)| inputs.contains(arg).then_some(index))?;
                let count = argv
                    .iter()
                    .skip(start)
                    .take_while(|arg| inputs.contains(arg))
                    .count();
                Some((index, start..start + count, wrapped))
            })?;
        self.response(
            step.kind.ninja_rule(),
            &step.output,
            invocations,
            index_and_args,
            flag,
            msvc,
        )
    }

    fn response(
        &self,
        rule: &[u8],
        output: &[u8],
        invocations: &[Invocation],
        (index, range, wrapped): (usize, Range<usize>, bool),
        flag: &[u8],
        msvc: bool,
    ) -> Option<Response> {
        if range.is_empty() {
            return None;
        }
        let file = if wrapped {
            // The link wrapper passes response files in CMakeFiles through to the linker.
            let mut hash = HashAlgo::Sha3_512.hex_digest(output);
            hash.truncate(16);
            format!("CMakeFiles/link-{hash}.rsp").into_bytes()
        } else {
            [output, b".rsp"].concat()
        };
        let command_file = self.commands.path(&file);
        let mut revised = invocations.to_vec();
        let invocation = revised.get_mut(index)?;
        let mut contents = Vec::new();
        let mut retained = Vec::new();
        let mut next = false;
        for argument in invocation.argv.get(range.clone())? {
            if wrapped && (next || wrapper_control_argument(argument)) {
                retained.push(argument.clone());
                next = !next
                    && matches!(
                        argument.to_ascii_uppercase().as_slice(),
                        b"/OUT:" | b"-OUT:" | b"/FE" | b"-FE"
                    );
                continue;
            }
            if !contents.is_empty() {
                contents.push(b' ');
            }
            if msvc {
                windows_argument(argument, &mut contents, false);
            } else {
                self.commands.word(argument, &mut contents);
            }
        }
        if contents.is_empty() {
            return None;
        }
        let trimmed = flag.trim_ascii_end();
        let reference = if trimmed.len() == flag.len() {
            vec![BString::from([flag, command_file.as_slice()].concat())]
        } else {
            vec![BString::from(trimmed), command_file]
        };
        invocation
            .argv
            .splice(range, retained.into_iter().chain(reference));
        if !self.commands.windows() {
            invocation.shell = self.commands.argv(&invocation.argv).into();
        }
        Some(Response {
            rule: RESPONSE_RULES.iter().find(|(base, _, _)| *base == rule)?.1,
            file: self.node(&file),
            contents: contents.into(),
            command: self.command_line(&revised, false),
        })
    }

    fn use_response(&self, dir: DirId, ordinary: &[u8]) -> bool {
        self.command_settings.force
            || self.command_settings.model.is_some_and(|model| {
                model
                    .definition(dir, b"CMAKE_NINJA_FORCE_RESPONSE_FILE")
                    .is_some()
            })
            || (self.commands.windows()
                && String::from_utf8_lossy(ordinary).encode_utf16().count()
                    >= if self.commands.is_cmd(ordinary) {
                        8191
                    } else {
                        32767
                    })
    }

    fn compile_location(&self, object: &BString) -> (DirId, &[u8]) {
        let language = self
            .compile_languages
            .get(object.as_slice())
            .copied()
            .unwrap_or_default();
        (self.target_dir_for_output(object), language)
    }

    fn target_dir_for_output(&self, output: &BString) -> DirId {
        self.command_settings
            .dirs
            .get(output.as_slice())
            .copied()
            .unwrap_or(DirId::ROOT)
    }

    fn language_setting(&self, dir: DirId, language: &[u8], suffix: &[u8]) -> Option<&BString> {
        self.command_settings
            .model?
            .definition(dir, &[b"CMAKE_", language, suffix].concat())
    }
}

fn wrapper_control_argument(argument: &[u8]) -> bool {
    let Some(option) = argument
        .strip_prefix(b"/")
        .or_else(|| argument.strip_prefix(b"-"))
    else {
        return false;
    };
    let upper = option.to_ascii_uppercase();
    upper.starts_with(b"OUT:")
        || option.starts_with(b"Fe")
        || matches!(
            upper.as_slice(),
            b"FE" | b"MANIFEST:NO" | b"INCREMENTAL" | b"INCREMENTAL:YES" | b"INCREMENTAL:NO"
        )
}

fn same_tool(actual: &[u8], expected: &[u8]) -> bool {
    !expected.is_empty()
        && actual
            .iter()
            .map(|&b| {
                if b == b'\\' {
                    b'/'
                } else {
                    b.to_ascii_lowercase()
                }
            })
            .eq(expected.iter().map(|&b| {
                if b == b'\\' {
                    b'/'
                } else {
                    b.to_ascii_lowercase()
                }
            }))
}

fn tool_name(arg: &[u8]) -> &[u8] {
    arg.rsplit(|&b| matches!(b, b'/' | b'\\'))
        .next()
        .unwrap_or_default()
}

fn msvc_compile_program(arg: &[u8]) -> bool {
    [b"cl".as_slice(), b"cl.exe", b"clang-cl", b"clang-cl.exe"]
        .iter()
        .any(|name| tool_name(arg).eq_ignore_ascii_case(name))
}

fn msvc_link_program(argv: &[BString]) -> Option<usize> {
    if argv
        .iter()
        .any(|arg| matches!(arg.as_slice(), b"vs_link_exe" | b"vs_link_dll"))
    {
        return argv
            .iter()
            .position(|arg| arg == b"--")
            .map(|index| index + 1);
    }
    argv.iter().position(|arg| {
        [
            b"link.exe".as_slice(),
            b"link",
            b"lld-link",
            b"lld-link.exe",
            b"lib.exe",
            b"lib",
            b"llvm-lib",
            b"llvm-lib.exe",
        ]
        .iter()
        .any(|name| tool_name(arg).eq_ignore_ascii_case(name))
    })
}

type Rule = (&'static [u8], &'static [u8], &'static [u8]);

const RESPONSE_RULES: &[Rule] = &[
    (b"COMPILE", b"COMPILE_RSP", b""),
    (
        b"COMPILE_DEPFILE",
        b"COMPILE_DEPFILE_RSP",
        b"  depfile = $DEP_FILE\n  deps = gcc\n",
    ),
    (COMPILE_MSVC, b"COMPILE_MSVC_RSP", b"  deps = msvc\n"),
    (b"CXX_COMPILE_SCANNED", b"CXX_COMPILE_SCANNED_RSP", b""),
    (
        b"CXX_COMPILE_SCANNED_DEPFILE",
        b"CXX_COMPILE_SCANNED_DEPFILE_RSP",
        b"  depfile = $DEP_FILE\n  deps = gcc\n",
    ),
    (b"LINK_EXECUTABLE", b"LINK_EXECUTABLE_RSP", b""),
    (b"LINK_SHARED_LIBRARY", b"LINK_SHARED_LIBRARY_RSP", b""),
    (b"LINK_MODULE_LIBRARY", b"LINK_MODULE_LIBRARY_RSP", b""),
    (b"STATIC_ARCHIVE", b"STATIC_ARCHIVE_RSP", b""),
];

pub(super) fn emit_rules(ninja: &mut Ninja) {
    ninja.0.extend_from_slice(
        b"\nrule COMPILE_MSVC\n  command = $COMMAND\n  description = $DESC\n  deps = msvc\n",
    );
    for (_, rule, deps) in RESPONSE_RULES {
        ninja.0.extend_from_slice(b"\nrule ");
        ninja.0.extend_from_slice(rule);
        ninja.0.extend_from_slice(b"\n  command = $COMMAND\n  description = $DESC\n  rspfile = $RSP_FILE\n  rspfile_content = $RSP_ARGS\n");
        ninja.0.extend_from_slice(deps);
    }
}
