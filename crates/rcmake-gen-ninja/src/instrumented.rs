use rcmake_base::BString;
use rcmake_resolve::{BuildStep, GenerationResult, Invocation};

use crate::syntax::{Ninja, shell_word};

type Rule = (&'static [u8], &'static [u8], bool);

const RULES: [Rule; 4] = [
    (b"COMPILE", b"COMPILE_INSTRUMENTED", false),
    (b"COMPILE_DEPFILE", b"COMPILE_DEPFILE_INSTRUMENTED", true),
    (
        b"CXX_COMPILE_SCANNED",
        b"CXX_COMPILE_SCANNED_INSTRUMENTED",
        false,
    ),
    (
        b"CXX_COMPILE_SCANNED_DEPFILE",
        b"CXX_COMPILE_SCANNED_DEPFILE_INSTRUMENTED",
        true,
    ),
];

fn arguments(invocation: &Invocation) -> Option<(&BString, &[BString])> {
    let [program, flag, args @ ..] = invocation.argv.as_slice() else {
        return None;
    };
    (invocation.working_dir.is_none()
        && flag == b"--instrument"
        && args.first().is_some_and(|arg| arg == b"--command-type"))
    .then_some((program, args))
}

pub(crate) fn ordinary_rule(scanned: bool, depfile: bool) -> &'static [u8] {
    match (scanned, depfile) {
        (false, false) => b"COMPILE",
        (false, true) => b"COMPILE_DEPFILE",
        (true, false) => b"CXX_COMPILE_SCANNED",
        (true, true) => b"CXX_COMPILE_SCANNED_DEPFILE",
    }
}

pub(crate) fn compile(
    invocation: &Invocation,
    scanned: bool,
    depfile: bool,
) -> (&'static [u8], Option<Vec<u8>>) {
    let (ordinary, instrumented, _) = match (scanned, depfile) {
        (false, false) => RULES[0],
        (false, true) => RULES[1],
        (true, false) => RULES[2],
        (true, true) => RULES[3],
    };
    let Some((_, args)) = arguments(invocation) else {
        return (ordinary, None);
    };
    let mut command = Vec::new();
    for (index, arg) in args.iter().enumerate() {
        if index != 0 {
            command.push(b' ');
        }
        shell_word(arg, &mut command);
    }
    (instrumented, Some(command))
}

pub(crate) fn emit_rules(ninja: &mut Ninja, result: &GenerationResult) {
    let Some(program) = result
        .graphs
        .iter()
        .flat_map(|graph| &graph.steps)
        .find_map(|step| {
            let BuildStep::Compile { invocation, .. } = step else {
                return None;
            };
            arguments(invocation).map(|(program, _)| program)
        })
    else {
        return;
    };
    for (_, rule, depfile) in RULES {
        ninja.0.extend_from_slice(b"\nrule ");
        ninja.0.extend_from_slice(rule);
        ninja.0.extend_from_slice(b"\n  command = \"");
        for &byte in program.as_slice() {
            if matches!(byte, b'"' | b'\\' | b'$' | b'`') {
                ninja.0.push(b'\\');
            }
            if byte == b'$' {
                ninja.0.push(b'$');
            }
            ninja.0.push(byte);
        }
        ninja
            .0
            .extend_from_slice(b"\" --instrument $COMMAND\n  description = $DESC\n");
        if depfile {
            ninja
                .0
                .extend_from_slice(b"  depfile = $DEP_FILE\n  deps = gcc\n");
        }
    }
}
