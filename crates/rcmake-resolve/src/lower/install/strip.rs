use std::{collections::BTreeMap, time::Duration};

use rcmake_base::{BString, ByteSlice, truth};
use rcmake_host::{CommandSpec, Host, Input, Output, ProcessResult, RunRequest, path::to_native};
use rcmake_model::{ProjectModel, Target, TargetKind};

use super::{InstallAction, InstallActionKind, definition};

pub(super) struct Tools<'a> {
    host: Option<&'a dyn Host>,
    apple_syntax: BTreeMap<BString, bool>,
}

impl<'a> Tools<'a> {
    pub(super) fn new(host: Option<&'a dyn Host>) -> Self {
        Self {
            host: host.filter(|host| host.system().sysname == b"Darwin"),
            apple_syntax: BTreeMap::new(),
        }
    }

    pub(super) fn action(
        &mut self,
        model: &ProjectModel,
        target: &Target,
        action: &InstallAction,
        name: BString,
    ) -> Option<InstallAction> {
        if !matches!(
            target.kind,
            TargetKind::Executable | TargetKind::SharedLibrary | TargetKind::ModuleLibrary
        ) {
            return None;
        }
        let program = definition(model, target.dir, b"CMAKE_STRIP")
            .filter(|program| !truth::is_off(program))?;
        let mut flags = Vec::new();
        if super::super::is_darwin(model, target.dir) {
            if target.kind != TargetKind::Executable {
                flags.push("-x".into());
            } else if let Some(host) = self.host
                && *self.apple_syntax.entry(program.clone()).or_insert_with(|| {
                    apple_strip(host, program, &model.dir(target.dir).binary_dir)
                })
            {
                flags.extend(["-u".into(), "-r".into()]);
            }
        }
        Some(InstallAction {
            kind: InstallActionKind::Strip {
                program: program.clone(),
                flags,
                name,
            },
            ..action.clone()
        })
    }
}

fn apple_strip(host: &dyn Host, program: &BString, binary: &[u8]) -> bool {
    host.run_bounded(
        &RunRequest {
            pipeline: vec![CommandSpec {
                program: program.clone(),
                args: vec!["-u".into(), "-r".into()],
            }],
            cwd: to_native(binary),
            env: host.env_all(),
            stdin: Input::Null,
            stdout: Output::Capture,
            stderr: Output::Capture,
            timeout: Some(Duration::from_secs(5)),
            tee: false,
        },
        4096,
    )
    .is_ok_and(|output| {
        matches!(output.results.as_slice(), [ProcessResult::Exited(code)] if *code != 0)
            && output.stderr.starts_with(b"fatal error: /")
            && output
                .stderr
                .contains_str(b"/usr/bin/strip: no files specified")
    })
}
