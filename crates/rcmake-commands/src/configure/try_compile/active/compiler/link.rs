// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;

pub(super) fn link_dependencies(bytes: &[u8], cwd: &Path) -> Option<DependencyClosure> {
    let bytes = bytes.strip_suffix(&[0])?;
    let mut files = BTreeSet::new();
    let mut searches = Vec::new();
    // The version record begins with a zero opcode, which is also the record separator.
    let version = bytes.strip_prefix(&[0])?;
    let mut records = version.split(|&byte| byte == 0);
    records.next()?;
    for record in records {
        match record.split_first()? {
            (0x10, name) => {
                files.insert(cwd.join(path::to_native(name)));
            }
            (0x11, name) => {
                let path = cwd.join(path::to_native(name));
                let parent = path.parent()?.to_owned();
                let name = PathBuf::from(path.file_name()?);
                searches.push(SearchObservation {
                    root: parent,
                    relative: name,
                    found: false,
                });
            }
            (0x40, _) => {}
            _ => return None,
        }
    }
    (!files.is_empty()).then_some(DependencyClosure { files, searches })
}

impl CompilerEvidence {
    pub(super) fn failed_apple_link(
        &self,
        request: &RunRequest,
        flags: &Flags,
        output: &RunOutput,
    ) -> Option<DependencyClosure> {
        let name = [&output.stdout, &output.stderr]
            .into_iter()
            .flat_map(|stream| stream.split(|&byte| byte == b'\n'))
            .find_map(|line| {
                line.strip_prefix(b"ld: library '")?
                    .strip_suffix(b"' not found")
            })?;
        let library = [b"-l".as_slice(), name].concat();
        let command = request.pipeline.first()?;
        if !command.args.iter().any(|arg| arg.as_slice() == library) {
            return None;
        }
        let mut args = Vec::new();
        let mut original = command.args.iter();
        while let Some(arg) = original.next() {
            if arg == "-o" {
                original.next()?;
            } else if arg.as_slice() != library {
                args.push(arg.clone());
            }
        }
        args.push("-Wl,-undefined,dynamic_lookup".into());
        self.evidence_link(request, flags, args)
    }

    /// Read the dependencies of a verbose Apple link from a second link.
    ///
    /// The driver prints the linker command for `-v`, so the visible link must
    /// not carry the private trace option.
    pub(super) fn verbose_apple_link(
        &self,
        request: &RunRequest,
        flags: &Flags,
    ) -> Option<DependencyClosure> {
        let command = request.pipeline.first()?;
        let mut args = Vec::new();
        let mut original = command.args.iter();
        while let Some(arg) = original.next() {
            match arg.as_slice() {
                b"-o" => {
                    original.next()?;
                }
                b"-v" | b"-Wl,-v" => {}
                _ => args.push(arg.clone()),
            }
        }
        self.evidence_link(request, flags, args)
    }

    fn evidence_link(
        &self,
        request: &RunRequest,
        flags: &Flags,
        mut args: Vec<BString>,
    ) -> Option<DependencyClosure> {
        args.extend([
            "-o".into(),
            ".rcmake-link-evidence-output".into(),
            "-Xlinker".into(),
            "-dependency_info".into(),
            "-Xlinker".into(),
            LINK_TRACE.into(),
        ]);
        let trace = request.cwd.join(LINK_TRACE);
        let mut files = flags.inputs.iter().cloned().collect::<BTreeSet<_>>();
        let mut searches = Vec::new();
        for prefix in &self.toolchain.prefixes {
            let arguments = prefix.iter().chain(&args).cloned().collect::<Vec<_>>();
            if self.host.metadata(&trace, Follow::No).is_ok() {
                self.host.remove_file(&trace).ok()?;
            }
            query_in(
                self.host.as_ref(),
                &self.toolchain.driver,
                &arguments,
                &request.cwd,
                request.env.clone(),
            )?;
            let bytes = self
                .host
                .read(&trace, ReadRange::All { limit: 1 << 20 })
                .ok()?;
            let observed = link_dependencies(&bytes, &request.cwd)?;
            files.extend(observed.files);
            searches.extend(observed.searches);
        }
        Some(DependencyClosure { files, searches })
    }
}
