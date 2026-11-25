//! Transform custom command dependency files for each generator.

use rcmake_base::{BString, hash::HashAlgo};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_model::{CustomCommand, DirId, GeneratorKind};

use super::{Commands, absolute_path, command_path, definition, direct};

impl Commands<'_> {
    pub(super) fn depfile(
        &self,
        command: &CustomCommand,
        path: &[u8],
        outputs: &[BString],
        invocations: &mut Vec<crate::Invocation>,
    ) -> BString {
        let root = self.model.dir(DirId::ROOT);
        let dir = self.model.dir(command.dir);
        let Some(generator) = self.model.generator else {
            return command_path(self.model, &dir.binary_dir, path);
        };
        let ninja = matches!(
            generator,
            GeneratorKind::Ninja | GeneratorKind::NinjaMultiConfig
        );
        if ninja && command.policies.get(PolicyId::new(116)) != PolicyStatus::New {
            return command_path(self.model, &root.binary_dir, path);
        }
        let original = absolute_path(&dir.binary_dir, path);
        let name = if ninja {
            let hash = HashAlgo::Sha256.hex_digest(&original);
            format!("CMakeFiles/d/{hash}.d").into()
        } else {
            legacy_depfile_name(generator, command, dir, path, &original, outputs)
        };
        let transformed = absolute_path(&root.binary_dir, &name);
        let cmake = definition(self.model, command.dir, b"CMAKE_COMMAND")
            .cloned()
            .unwrap_or_else(|| "cmake".into());
        if !invocations.is_empty()
            && matches!(
                generator,
                GeneratorKind::Ninja
                    | GeneratorKind::NinjaMultiConfig
                    | GeneratorKind::UnixMakefiles
                    | GeneratorKind::Xcode
            )
        {
            let argv = vec![
                cmake,
                "-E".into(),
                "cmake_transform_depfile".into(),
                generator.name().into(),
                if generator == GeneratorKind::UnixMakefiles {
                    "makedepfile"
                } else {
                    "gccdepfile"
                }
                .into(),
                root.source_dir.clone(),
                dir.source_dir.clone(),
                root.binary_dir.clone(),
                dir.binary_dir.clone(),
                original,
                transformed,
            ];
            let invocation = if ninja && !crate::lower::shell::windows_host(self.model, command.dir)
            {
                let directory = invocations
                    .last()
                    .and_then(|invocation| invocation.working_dir.clone());
                crate::lower::shell::ninja_custom(argv, directory, command.verbatim)
            } else {
                direct(argv, None)
            };
            invocations.push(invocation);
        }
        name
    }
}

fn legacy_depfile_name(
    generator: GeneratorKind,
    command: &CustomCommand,
    dir: &rcmake_model::Directory,
    path: &[u8],
    original: &[u8],
    outputs: &[BString],
) -> BString {
    let mut identity = HashAlgo::Sha256.hasher();
    let xcode = generator == GeneratorKind::Xcode;
    let declared = command
        .depfile
        .as_deref()
        .map_or(path, |value| value.as_ref());
    identity.update(if xcode { declared } else { original });
    identity.update(b"\0");
    if xcode {
        identity.update(&dir.binary_dir);
        identity.update(b"\0");
        for output in &command.outputs {
            identity.update(&output.value);
            identity.update(b"\0");
        }
    } else {
        for output in outputs {
            identity.update(output);
            identity.update(b"\0");
        }
    }
    let config = if xcode { ".$(CONFIGURATION)" } else { "" };
    format!(
        "CMakeFiles/rcmake-depfile-{}{config}.d",
        identity.finish_hex()
    )
    .into()
}
