use rcmake_base::{BString, path_lex::Lexical};
use rcmake_host::Host;
use rcmake_resolve::Invocation;

use crate::syntax::shell_word;

#[derive(Clone, Default)]
pub(crate) struct Commands {
    comspec: Option<BString>,
}

impl Commands {
    pub(crate) fn new(host: &dyn Host) -> Self {
        if host.system().sysname != b"Windows" {
            return Self::default();
        }
        Self {
            comspec: Some(
                host.env_get(b"COMSPEC")
                    .filter(|path| !path.is_empty())
                    .unwrap_or_else(|| "cmd.exe".into()),
            ),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_windows(comspec: &[u8]) -> Self {
        Self {
            comspec: Some(comspec.into()),
        }
    }

    pub(crate) fn windows(&self) -> bool {
        self.comspec.is_some()
    }

    pub(crate) fn is_cmd(&self, command: &[u8]) -> bool {
        self.comspec.as_ref().is_some_and(|comspec| {
            let mut prefix = Vec::new();
            self.word(comspec, &mut prefix);
            prefix.extend_from_slice(b" /D /S /V:OFF /C ");
            command.starts_with(&prefix)
        })
    }

    pub(crate) fn word(&self, word: &[u8], out: &mut Vec<u8>) {
        if self.windows() {
            windows_argument(word, out, false);
        } else {
            shell_word(word, out);
        }
    }

    pub(crate) fn argv(&self, argv: &[BString]) -> Vec<u8> {
        let mut out = Vec::new();
        for word in argv {
            if !out.is_empty() {
                out.push(b' ');
            }
            self.word(word, &mut out);
        }
        out
    }

    pub(crate) fn path(&self, path: &[u8]) -> BString {
        if !self.windows() {
            return Lexical::new(path).normalize();
        }
        let (prefix, path) = path
            .strip_prefix(br"\\?\")
            .map_or((b"".as_slice(), path), |path| {
                path.strip_prefix(br"UNC\")
                    .map_or((b"".as_slice(), path), |unc| (b"//".as_slice(), unc))
            });
        let path: BString = prefix
            .iter()
            .chain(path)
            .map(|&byte| if byte == b'\\' { b'/' } else { byte })
            .collect();
        if let Some(unc) = path.strip_prefix(b"//") {
            // Keep the share name when normalization removes a parent component.
            let Some((end, _)) = unc
                .iter()
                .enumerate()
                .filter(|(_, byte)| **byte == b'/')
                .nth(1)
            else {
                return path;
            };
            let (share, body) = path.split_at(end + 2);
            [share, &Lexical::windows(body).normalize()].concat().into()
        } else {
            Lexical::windows(&path).normalize()
        }
    }

    pub(crate) fn absolute(&self, path: &[u8]) -> bool {
        path.starts_with(b"/") || (self.windows() && drive(path))
    }

    pub(crate) fn beneath<'a>(&self, path: &'a [u8], base: &[u8]) -> Option<&'a [u8]> {
        let base = base
            .strip_suffix(b"/")
            .filter(|x| !x.is_empty())
            .unwrap_or(base);
        let (head, tail) = path.split_at_checked(base.len())?;
        let matched = if self.windows() {
            head.eq_ignore_ascii_case(base)
        } else {
            head == base
        };
        if !matched {
            return None;
        }
        if tail.is_empty() || tail == b"/" {
            return Some(b".");
        }
        if base == b"/" {
            return Some(tail);
        }
        tail.strip_prefix(b"/")
    }

    pub(crate) fn in_directory(&self, command: &[u8], directory: &[u8]) -> Vec<u8> {
        if self.windows() {
            let mut script = Vec::new();
            change_directory(&mut script, directory, false);
            script.extend_from_slice(b" && ");
            script.extend_from_slice(command);
            self.cmd(&script)
        } else {
            let mut out = b"cd -- ".to_vec();
            self.word(directory, &mut out);
            out.extend_from_slice(b" && ");
            out.extend_from_slice(command);
            out
        }
    }

    pub(crate) fn chain(&self, first: &[u8], second: &[u8]) -> Vec<u8> {
        let script = [first, b" && ", second].concat();
        if self.windows() {
            self.cmd(&script)
        } else {
            script
        }
    }

    pub(crate) fn raw_script<'a>(&self, invocation: &'a Invocation) -> Option<&'a [u8]> {
        if !self.windows() {
            return None;
        }
        let [program, d, s, v, c, script] = invocation.argv.as_slice() else {
            return None;
        };
        let name = program
            .rsplit(|&byte| matches!(byte, b'/' | b'\\'))
            .next()?;
        (name.eq_ignore_ascii_case(b"cmd.exe")
            && d == b"/D"
            && s == b"/S"
            && v == b"/V:OFF"
            && c == b"/C")
            .then_some(script.as_slice())
    }

    pub(crate) fn line(
        &self,
        invocations: &[Invocation],
        custom: bool,
        build_dir: &[u8],
        prefix: bool,
    ) -> Vec<u8> {
        if self.windows() {
            let lowered = lowered_chain(invocations);
            let invocations = lowered.as_deref().unwrap_or(invocations);
            if let [invocation] = invocations
                && !prefix
                && invocation
                    .working_dir
                    .as_ref()
                    .is_none_or(|dir| self.path(dir).eq_ignore_ascii_case(&self.path(build_dir)))
            {
                if let Some(script) = self.raw_script(invocation) {
                    return self.cmd(script);
                }
                if !needs_cmd(invocation, custom) {
                    return self.argv(&invocation.argv);
                }
            }
            self.cmd(&self.windows_script(invocations, custom, build_dir, prefix, false))
        } else {
            let command = posix_line(invocations, custom);
            if prefix {
                self.in_directory(&command, build_dir)
            } else {
                command
            }
        }
    }

    pub(crate) fn custom_script(
        &self,
        invocations: &[Invocation],
        build_dir: &[u8],
        prefix: bool,
    ) -> Option<Vec<u8>> {
        if !self.windows() {
            return None;
        }
        let command = self.line(invocations, true, build_dir, prefix);
        let shell = invocations.len() != 1
            || invocations.first().is_some_and(|invocation| {
                prefix
                    || invocation.working_dir.is_some()
                    || self.raw_script(invocation).is_some()
                    || needs_cmd(invocation, true)
            });
        let literal_percent = invocations.iter().any(|invocation| {
            invocation.argv.iter().any(|arg| arg.contains(&b'%'))
                || invocation
                    .working_dir
                    .as_ref()
                    .is_some_and(|path| path.contains(&b'%'))
        }) || (prefix && build_dir.contains(&b'%'));
        if !shell || (unicode_length(&command) < 4096 && !literal_percent) {
            return None;
        }
        let mut script = b"@echo off\r\nsetlocal DisableDelayedExpansion\r\n".to_vec();
        script.extend_from_slice(&self.windows_script(invocations, true, build_dir, prefix, true));
        script.extend_from_slice(b"exit /b 0\r\n:rcmake_fail\r\nexit /b %errorlevel%\r\n");
        Some(script)
    }

    pub(crate) fn cmd(&self, script: &[u8]) -> Vec<u8> {
        let Some(comspec) = &self.comspec else {
            return script.to_vec();
        };
        let mut out = Vec::new();
        windows_argument(comspec, &mut out, false);
        out.extend_from_slice(b" /D /S /V:OFF /C \"");
        out.extend_from_slice(script);
        out.push(b'"');
        out
    }

    fn windows_script(
        &self,
        invocations: &[Invocation],
        custom: bool,
        build_dir: &[u8],
        prefix: bool,
        batch: bool,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        let base = self.path(build_dir);
        let mut current = (!prefix).then_some(base.clone());
        for invocation in invocations {
            if !out.is_empty() && !batch {
                out.extend_from_slice(b" && ");
            }
            let dir = invocation.working_dir.as_ref().map_or_else(
                || base.clone(),
                |dir| {
                    let dir = self.path(dir);
                    if self.absolute(&dir) {
                        dir
                    } else {
                        self.path(&[base.as_slice(), b"/", &dir].concat())
                    }
                },
            );
            if current
                .as_ref()
                .is_none_or(|current| !current.eq_ignore_ascii_case(&dir))
            {
                change_directory(&mut out, &dir, batch);
                finish_command(&mut out, batch);
                current = Some(dir);
            }
            let raw = self.raw_script(invocation);
            let group = raw.is_some()
                || invocation.argv.is_empty()
                || custom && invocation.argv.iter().any(|word| word == b"||");
            if group {
                out.extend_from_slice(b"( ");
            }
            if let Some(raw) = raw {
                out.extend_from_slice(raw);
                current = None;
            } else if let Some((program, arguments)) = invocation.argv.split_first() {
                if is_batch(program) {
                    out.extend_from_slice(b"call ");
                }
                shell_argument(program, &mut out, batch, is_batch(program));
                for arg in arguments {
                    out.push(b' ');
                    if custom && operator(arg) {
                        out.extend_from_slice(arg);
                    } else {
                        shell_argument(arg, &mut out, batch, is_batch(program));
                    }
                }
                if builtin(program) {
                    current = None;
                }
            } else if invocation.shell.is_empty() {
                out.extend_from_slice(b"ver >NUL");
            } else {
                out.extend_from_slice(&invocation.shell);
                current = None;
            }
            if group {
                out.extend_from_slice(b" )");
            }
            if batch {
                finish_command(&mut out, true);
            }
        }
        if out.is_empty() {
            out.extend_from_slice(b"ver >NUL");
        }
        out
    }
}

fn lowered_chain(invocations: &[Invocation]) -> Option<Vec<Invocation>> {
    let chain = |invocation: &Invocation| match invocation.argv.as_slice() {
        [program, option, shell]
            if program == b"sh" && option == b"-c" && shell == &invocation.shell =>
        {
            rcmake_resolve::lower::rule_command_chain(shell)
        }
        _ => None,
    };
    if !invocations.iter().any(|invocation| {
        matches!(invocation.argv.as_slice(), [program, option, _] if program == b"sh" && option == b"-c")
    }) {
        return None;
    }
    Some(
        invocations
            .iter()
            .flat_map(|invocation| {
                chain(invocation).map_or_else(
                    || vec![invocation.clone()],
                    |commands| {
                        commands
                            .into_iter()
                            .map(|argv| Invocation {
                                argv,
                                working_dir: invocation.working_dir.clone(),
                                ..Default::default()
                            })
                            .collect()
                    },
                )
            })
            .collect(),
    )
}

fn unicode_length(command: &[u8]) -> usize {
    String::from_utf8_lossy(command).encode_utf16().count()
}

fn drive(path: &[u8]) -> bool {
    matches!(path, [letter, b':', b'/', ..] if letter.is_ascii_alphabetic())
}

fn needs_cmd(invocation: &Invocation, custom: bool) -> bool {
    invocation
        .argv
        .first()
        .is_none_or(|program| is_batch(program) || builtin(program))
        || custom && invocation.argv.iter().skip(1).any(|arg| operator(arg))
}

fn is_batch(program: &[u8]) -> bool {
    let mut parts = program.rsplitn(2, |&b| b == b'.');
    parts
        .next()
        .is_some_and(|ext| ext.eq_ignore_ascii_case(b"bat") || ext.eq_ignore_ascii_case(b"cmd"))
        && parts.next().is_some()
}

fn builtin(program: &[u8]) -> bool {
    [
        b"cd".as_slice(),
        b"chdir",
        b"copy",
        b"del",
        b"echo",
        b"exit",
        b"if",
        b"md",
        b"mkdir",
        b"move",
        b"popd",
        b"pushd",
        b"rd",
        b"ren",
        b"rmdir",
        b"set",
        b"type",
        b"ver",
    ]
    .iter()
    .any(|name| program.eq_ignore_ascii_case(name))
}

fn operator(arg: &[u8]) -> bool {
    matches!(
        arg,
        b"<" | b">"
            | b"<<"
            | b">>"
            | b"1>"
            | b"2>"
            | b"2>>"
            | b"2>&1"
            | b"1>&2"
            | b"|"
            | b"||"
            | b"&&"
    )
}

fn finish_command(out: &mut Vec<u8>, batch: bool) {
    if batch {
        out.extend_from_slice(b" || goto :rcmake_fail\r\n");
    } else {
        out.extend_from_slice(b" && ");
    }
}

fn change_directory(out: &mut Vec<u8>, dir: &[u8], batch: bool) {
    if dir.starts_with(b"//") || dir.starts_with(br"\\") {
        out.extend_from_slice(b"pushd \"");
    } else {
        out.extend_from_slice(b"cd /D \"");
    }
    for &byte in dir {
        if batch && byte == b'%' {
            out.push(byte);
        }
        out.push(if byte == b'/' { b'\\' } else { byte });
    }
    out.push(b'"');
}

fn shell_argument(word: &[u8], out: &mut Vec<u8>, batch: bool, called: bool) {
    let mut arg = Vec::new();
    windows_argument(word, &mut arg, word.iter().any(|b| b"&|<>()^".contains(b)));
    let mut quoted = false;
    for byte in arg {
        if !quoted && b"&|<>()^".contains(&byte) {
            out.push(b'^');
        }
        if byte == b'"' {
            quoted = !quoted;
        }
        if batch && byte == b'%' {
            out.push(byte);
            if called {
                out.extend_from_slice(b"%%");
            }
        }
        out.push(byte);
    }
}

pub(crate) fn windows_argument(word: &[u8], out: &mut Vec<u8>, force_quote: bool) {
    if !force_quote && !word.is_empty() && !word.iter().any(|b| matches!(b, b' ' | b'\t' | b'"')) {
        out.extend_from_slice(word);
        return;
    }
    out.push(b'"');
    let mut slashes = 0;
    for &byte in word {
        if byte == b'\\' {
            slashes += 1;
        } else {
            let count = if byte == b'"' {
                2 * slashes + 1
            } else {
                slashes
            };
            out.extend(std::iter::repeat_n(b'\\', count));
            slashes = 0;
            out.push(byte);
        }
    }
    out.extend(std::iter::repeat_n(b'\\', 2 * slashes));
    out.push(b'"');
}

fn posix_line(invocations: &[Invocation], custom: bool) -> Vec<u8> {
    let mut result = Vec::new();
    for invocation in invocations {
        if !result.is_empty() {
            result.extend_from_slice(b" && ");
        }
        let group = invocations.len() > 1 || invocation.working_dir.is_some();
        if group {
            result.push(b'(');
        }
        if let Some(directory) = &invocation.working_dir {
            result.extend_from_slice(b"cd -- ");
            shell_word(directory, &mut result);
            result.extend_from_slice(b" && ");
        }
        if !invocation.shell.is_empty() && (!custom || invocation.argv.is_empty()) {
            result.extend_from_slice(&invocation.shell);
        } else if let Some((first, rest)) = invocation.argv.split_first() {
            shell_word(first, &mut result);
            for arg in rest {
                result.push(b' ');
                if custom && operator(arg) {
                    result.extend_from_slice(arg);
                } else {
                    shell_word(arg, &mut result);
                }
            }
        } else {
            result.push(b':');
        }
        if group {
            result.push(b')');
        }
    }
    if result.is_empty() {
        result.push(b':');
    }
    result
}
