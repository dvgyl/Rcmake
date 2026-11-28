//! The project items of custom commands, as CMake names and attaches them.

use md5::{Digest, Md5};
use rcmake_base::{hash::HashAlgo, path_lex::Lexical, truth};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_model::{CommandId, CustomCommand};

use super::Context;
use crate::{absolute, join, string};

/// Where a custom command lives in a project file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Attachment {
    /// The `CustomBuild` item path.
    pub(crate) path: String,
    /// The generator writes the item file (a `.rule` file).
    pub(crate) synthetic: bool,
    /// `BuildInParallel` is on (policy `CMP0147`).
    pub(crate) parallel: bool,
}

impl Context<'_> {
    /// Find the model command of an IDE rule step from its first output.
    pub(super) fn rule_command(
        &self,
        config: &str,
        first_output: &str,
    ) -> Option<(CommandId, &CustomCommand)> {
        let id = *self
            .indices
            .command_by_output
            .get(&(config.to_owned(), first_output.to_owned()))?;
        Some((id, self.model.command(id)?))
    }

    /// Attach a custom command, as CMake attaches it.
    ///
    /// A command with a `MAIN_DEPENDENCY` is attached to that file, unless an
    /// earlier command with other command lines already uses the file (see
    /// [`Self::owns_main_dependency`]). Other commands get a `.rule` file
    /// under `CMakeFiles` of the top binary directory, named after the first
    /// output (see [`rule_file`]).
    pub(super) fn attach(&self, id: CommandId, command: &CustomCommand) -> Attachment {
        let dir = self.model.dir(command.dir);
        let main = command
            .main_dependency
            .as_ref()
            .filter(|_| !command.command_lines.is_empty())
            .map(|path| self.main_path(command, path))
            .filter(|path| self.owns_main_dependency(id, command, path));
        let generated = main.as_ref().is_some_and(|path| {
            dir.generated_sources.contains(path.as_bytes())
                || dir
                    .source_props
                    .get(path.as_bytes())
                    .and_then(|props| props.get(b"GENERATED"))
                    .is_some_and(|value| truth::is_on(value))
        });
        let disabled = main.as_ref().is_some_and(|path| {
            dir.source_props
                .get(path.as_bytes())
                .and_then(|props| props.get(b"VS_CUSTOM_COMMAND_DISABLE_PARALLEL_BUILD"))
                .is_some_and(|value| truth::is_on(value))
        });
        // A `.rule` file is a generated source, so a command with a main
        // dependency that gets one does not build in parallel.
        let rule_with_main = command.main_dependency.is_some() && main.is_none();
        let parallel = command.policies.get(PolicyId::new(147)) == PolicyStatus::New
            && !command.uses_terminal
            && !generated
            && !rule_with_main
            && !disabled;
        match main {
            Some(path) => Attachment {
                path,
                synthetic: false,
                parallel,
            },
            None => {
                let outputs = self
                    .indices
                    .command_outputs
                    .get(&id)
                    .map_or(&[][..], Vec::as_slice);
                let binary = absolute(&self.settings.bin, &string(&dir.binary_dir));
                let raw = command
                    .outputs
                    .first()
                    .map_or_else(String::new, |output| string(&output.value));
                Attachment {
                    path: rule_file(&self.settings.bin, &binary, &raw, outputs),
                    synthetic: true,
                    parallel,
                }
            }
        }
    }
}

impl Context<'_> {
    /// The absolute path of a `MAIN_DEPENDENCY`, relative to the source
    /// directory of the command.
    fn main_path(&self, command: &CustomCommand, main: &[u8]) -> String {
        let source = string(&self.model.dir(command.dir).source_dir);
        absolute(&absolute(&self.settings.bin, &source), &string(main))
    }

    /// Tell if command `id` can use its main dependency `path` as its item.
    ///
    /// As in CMake, the first command of the directory that names the file
    /// owns it. A later command with the same command lines is a duplicate
    /// of the first one and uses the same item. A later command with other
    /// command lines gets its own `.rule` file, so it does not replace the
    /// rule of the first command.
    fn owns_main_dependency(&self, id: CommandId, command: &CustomCommand, path: &str) -> bool {
        let Some(commands) = self.indices.output_commands_by_dir.get(&command.dir) else {
            return true;
        };
        commands
            .iter()
            .find(|(_, other)| {
                !other.command_lines.is_empty()
                    && other
                        .main_dependency
                        .as_ref()
                        .is_some_and(|main| self.main_path(other, main) == path)
            })
            .is_none_or(|(owner, other)| {
                *owner == id || other.command_lines == command.command_lines
            })
    }
}

/// Name the `.rule` file of a custom command without a main dependency, as
/// CMake names it.
///
/// The name starts from the first output, `raw`, as given:
///
/// 1. A path without generator expressions is used as it is.
/// 2. When `$<CONFIG>` is the only generator expression, it becomes
///    `(CONFIG)`. For example, `out-$<CONFIG>.txt` gives `out-(CONFIG).txt`.
/// 3. When the output is the same in all configurations (`evaluated`), that
///    path is used.
/// 4. Otherwise the name is the first 16 hexadecimal digits of the SHA-256
///    hash of `raw`, in `<binary>/CMakeFiles`.
///
/// The file is `<top>/CMakeFiles/<md5 of the directory>/<file name>.rule`.
pub(crate) fn rule_file(top: &str, binary: &str, raw: &str, evaluated: &[String]) -> String {
    let name = if !raw.contains("$<") {
        raw.to_owned()
    } else if let Some(simple) =
        Some(raw.replace("$<CONFIG>", "(CONFIG)")).filter(|simple| !simple.contains("$<"))
    {
        simple
    } else if let Some(first) = evaluated
        .first()
        .filter(|first| evaluated.iter().all(|output| output == *first))
    {
        first.clone()
    } else {
        let digest = HashAlgo::Sha256.hex_digest(raw.as_bytes());
        join(
            &join(binary, "CMakeFiles"),
            digest.get(..16).unwrap_or(&digest),
        )
    };
    let name = absolute(binary, &name);
    let path = Lexical::new(name.as_bytes());
    let parent = string(path.parent());
    let file = string(path.filename());
    let mut hash = Md5::new();
    hash.update(parent.as_bytes());
    let digest = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    join(top, &format!("CMakeFiles/{digest}/{file}.rule"))
}

#[cfg(test)]
mod tests {
    use super::rule_file;

    fn md5(text: &str) -> String {
        use md5::{Digest, Md5};
        Md5::digest(text.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    #[test]
    fn rule_files_follow_the_first_output() {
        let top = "/b";
        let dir = md5("/b/sub");
        assert_eq!(
            rule_file(top, "/b/sub", "/b/sub/out.txt", &[]),
            format!("/b/CMakeFiles/{dir}/out.txt.rule")
        );
        assert_eq!(
            rule_file(top, "/b/sub", "/b/sub/out-$<CONFIG>.txt", &[]),
            format!("/b/CMakeFiles/{dir}/out-(CONFIG).txt.rule")
        );
        let same = ["/b/sub/out.txt".to_owned(), "/b/sub/out.txt".to_owned()];
        assert_eq!(
            rule_file(top, "/b/sub", "$<1:out.txt>", &same),
            format!("/b/CMakeFiles/{dir}/out.txt.rule")
        );
    }

    #[test]
    fn rule_files_hash_outputs_that_differ_by_configuration() {
        let outputs = [
            "/b/a-Debug-1.txt".to_owned(),
            "/b/a-Release-0.txt".to_owned(),
        ];
        let rule = rule_file("/b", "/b", "/b/a-$<CONFIG>-$<CONFIG:Debug>.txt", &outputs);
        let dir = md5("/b/CMakeFiles");
        let name = rule
            .strip_prefix(&format!("/b/CMakeFiles/{dir}/"))
            .and_then(|name| name.strip_suffix(".rule"))
            .unwrap_or_default();
        assert_eq!(name.len(), 16);
        assert!(name.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
}
