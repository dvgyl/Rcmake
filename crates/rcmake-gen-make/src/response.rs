use rcmake_base::path_lex::Lexical;
use rcmake_gen_common::link_response;
use rcmake_resolve::ResponseRole;

use super::{BuildStep, GenError, Host, Path, Target};

pub(super) fn prepare(
    targets: &mut [Target<'_>],
    build: &Path,
    host: &dyn Host,
) -> Result<(), GenError> {
    let limit = link_response::command_line_limit(&host.system().sysname);
    for target in targets {
        let directory = Lexical::new(&target.file).parent();
        let mut ordinal = 0;
        for step in &mut target.rules {
            let invocations = match step.action {
                BuildStep::Link { invocations, .. } => invocations,
                BuildStep::Custom(custom) => &custom.invocations,
                _ => continue,
            };
            let mut revised = None;
            for (index, invocation) in invocations.iter().enumerate() {
                let rewritten = link_response::rewrite_named(
                    invocation,
                    build,
                    false,
                    |role, length, setting| use_response(role, length, setting, limit),
                    |role, span| {
                        let number = ordinal + span + 1;
                        let file = match role {
                            ResponseRole::Objects => format!("objects{number}.rsp"),
                            ResponseRole::Libraries if number == 1 => "linkLibs.rsp".to_owned(),
                            ResponseRole::Libraries => format!("linkLibs{number}.rsp"),
                        };
                        Lexical::new(directory).append(&file.into_bytes())
                    },
                );
                if let Some((invocation, files)) = rewritten {
                    ordinal += files.len();
                    for file in files {
                        file.write(host, build)?;
                        step.inputs.push(file.path);
                    }
                    let revised = revised.get_or_insert_with(|| invocations.to_vec());
                    if let Some(effective) = revised.get_mut(index) {
                        *effective = invocation;
                    }
                }
            }
            step.revised_invocations = revised;
        }
    }
    Ok(())
}

fn use_response(role: ResponseRole, length: usize, setting: Option<bool>, limit: usize) -> bool {
    setting.unwrap_or(role == ResponseRole::Objects && length > limit / 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_budget_depends_on_host_and_libraries_require_an_explicit_setting() {
        let length = 140 * 1024;
        let darwin = link_response::command_line_limit(b"Darwin");
        let linux = link_response::command_line_limit(b"Linux");
        assert!(!use_response(ResponseRole::Objects, length, None, darwin));
        assert!(use_response(ResponseRole::Objects, length, None, linux));
        assert!(!use_response(ResponseRole::Libraries, length, None, linux));
        assert!(!use_response(
            ResponseRole::Objects,
            length,
            Some(false),
            linux
        ));
        assert!(use_response(ResponseRole::Libraries, 1, Some(true), darwin));
    }
}
