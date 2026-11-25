// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use rcmake_base::BString;
use rcmake_host::{FileKind, Follow, Host, ReadRange, path};

use super::pure_flag;

const LIMIT: u64 = 16 * 1024;
const MAX_DEPTH: usize = 24;

pub(super) struct Wrapper {
    pub(super) interpreter: PathBuf,
    pub(super) env_program: Option<PathBuf>,
    pub(super) target: PathBuf,
    pub(super) telemetry: Vec<BString>,
    pub(super) prefixes: Vec<Vec<BString>>,
}

impl Wrapper {
    pub(super) fn read(host: &dyn Host, file: &Path) -> Option<Self> {
        if !host
            .metadata(file, Follow::Yes)
            .is_ok_and(|stat| stat.kind == FileKind::File && stat.len <= LIMIT)
        {
            return None;
        }
        let bytes = host.read(file, ReadRange::All { limit: LIMIT }).ok()?;
        let (shebang, body) = std::str::from_utf8(&bytes).ok()?.split_once('\n')?;
        let (interpreter, env_program) = match shebang {
            "#!/bin/sh" => ("/bin/sh", None),
            "#!/bin/bash" => ("/bin/bash", None),
            "#!/usr/bin/env sh" => ("/bin/sh", Some("/usr/bin/env")),
            "#!/usr/bin/env bash" => ("/bin/bash", Some("/usr/bin/env")),
            _ => return None,
        };
        if env_program.is_some() {
            let name = Path::new(interpreter).file_name()?;
            let search = path::to_native(&host.env_get(b"PATH")?);
            let candidates: Vec<_> = std::env::split_paths(&search).collect();
            if candidates.iter().any(|directory| !directory.is_absolute()) {
                return None;
            }
            let found = candidates
                .into_iter()
                .map(|directory| directory.join(name))
                .find(|candidate| {
                    host.metadata(candidate, Follow::Yes)
                        .is_ok_and(|stat| stat.kind == FileKind::File && stat.mode & 0o111 != 0)
                })?;
            host.canonicalize(&found)
                .ok()
                .filter(|found| host.canonicalize(Path::new(interpreter)).as_ref() == Ok(found))?;
        }

        let tokens = tokenize(body)?;
        let mut parser = Parser {
            remaining: &tokens,
            loop_variables: BTreeSet::new(),
            unstable_variables: BTreeSet::new(),
            telemetry: Vec::new(),
            prefixes: vec![Vec::new()],
        };
        parser.commands(&["exec"], true, 0)?;
        parser.take("exec")?;
        let target = PathBuf::from(literal(parser.next()?)?);
        if !target.is_absolute() {
            return None;
        }
        let prefix = parser.forward_arguments()?;
        parser.separator();
        if !parser.remaining.is_empty() {
            return None;
        }
        for candidate in &mut parser.prefixes {
            candidate.splice(..0, prefix.iter().cloned());
        }
        Some(Self {
            interpreter: interpreter.into(),
            env_program: env_program.map(PathBuf::from),
            target,
            telemetry: parser.telemetry,
            prefixes: parser.prefixes,
        })
    }
}

fn tokenize(body: &str) -> Option<Vec<&str>> {
    let mut tokens = Vec::new();
    let mut position = 0;
    let bytes = body.as_bytes();
    while let Some(&byte) = bytes.get(position) {
        let rest = body.get(position..)?;
        if matches!(byte, b' ' | b'\t') {
            position += 1;
            continue;
        }
        if byte == b'#' {
            position += rest.find('\n').unwrap_or(rest.len());
            continue;
        }
        if let Some(operator) = [";;", ">&2", ">>", "&&", ">", ";", "\n", "|", ")"]
            .into_iter()
            .find(|operator| rest.starts_with(operator))
        {
            tokens.push(body.get(position..position + operator.len())?);
            position += operator.len();
            continue;
        }
        let start = position;
        let mut quote = None;
        while let Some(&byte) = bytes.get(position) {
            match (quote, byte) {
                (Some(b'\''), b'\'') | (Some(b'"'), b'"') => quote = None,
                (Some(b'\''), _) => {}
                (_, b'`' | 0 | b'\r') => return None,
                (Some(b'"'), b'\\') => {
                    position += 1;
                    if bytes
                        .get(position)
                        .is_none_or(|byte| !b"\\\"".contains(byte))
                    {
                        return None;
                    }
                }
                (Some(b'"'), b'$') | (None, b'$') => {
                    if bytes.get(position + 1) == Some(&b'(') {
                        return None;
                    }
                }
                (Some(b'"'), _) => {}
                (None, b'\'' | b'"') => quote = Some(byte),
                (None, b' ' | b'\t' | b'\n' | b';' | b'>' | b'|' | b')' | b'&') => break,
                (None, b'<' | b'(' | b'\\') => return None,
                (None, _) => {}
                (Some(_), _) => return None,
            }
            position += 1;
        }
        if quote.is_some() || position == start {
            return None;
        }
        tokens.push(body.get(start..position)?);
    }
    Some(tokens)
}

struct Parser<'a> {
    remaining: &'a [&'a str],
    loop_variables: BTreeSet<&'a str>,
    unstable_variables: BTreeSet<&'a str>,
    telemetry: Vec<BString>,
    prefixes: Vec<Vec<BString>>,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&'a str> {
        self.remaining.first().copied()
    }

    fn next(&mut self) -> Option<&'a str> {
        let (&token, rest) = self.remaining.split_first()?;
        self.remaining = rest;
        Some(token)
    }

    fn take(&mut self, token: &str) -> Option<()> {
        (self.next()? == token).then_some(())
    }

    fn separator(&mut self) -> bool {
        let mut seen = false;
        while matches!(self.peek(), Some(";" | "\n")) {
            self.next();
            seen = true;
        }
        seen
    }

    fn commands(&mut self, stops: &[&str], stable: bool, depth: usize) -> Option<()> {
        if depth >= MAX_DEPTH {
            return None;
        }
        self.separator();
        loop {
            let token = self.peek()?;
            if stops.contains(&token) {
                return Some(());
            }
            self.statement(stable, depth)?;
            if !self.separator() && self.peek() != Some(";;") {
                return None;
            }
        }
    }

    fn statement(&mut self, stable: bool, depth: usize) -> Option<()> {
        match self.next()? {
            "if" => self.condition(stable, depth),
            "case" => self.case(stable, depth),
            "for" => self.for_loop(stable, depth),
            "set" if stable && self.loop_variables.is_empty() => {
                self.take("--")?;
                let additions = self.forward_arguments()?;
                if self.prefixes.len() >= 8 {
                    return None;
                }
                self.prefixes
                    .extend(self.prefixes.clone().into_iter().map(|mut candidate| {
                        candidate.splice(..0, additions.iter().cloned());
                        candidate
                    }));
                Some(())
            }
            "break" if !self.loop_variables.is_empty() => Some(()),
            ":" => Some(()),
            "printf" => self.printf(stable),
            assignment => {
                let (name, value) = assignment.split_once('=')?;
                if !variable_name(name)
                    || !name.as_bytes().first()?.is_ascii_lowercase()
                    || self.loop_variables.contains(name)
                {
                    return None;
                }
                let value_stable = self.word(value)?.1;
                if !stable || !value_stable {
                    self.unstable_variables.insert(name);
                }
                Some(())
            }
        }
    }

    fn condition(&mut self, stable: bool, depth: usize) -> Option<()> {
        let mut test_stable = stable;
        loop {
            self.take("[")?;
            let left = self.next()?;
            match left {
                "-n" | "-z" => {
                    let word = self.next()?;
                    test_stable &= self.word(word)?.1;
                }
                "-f" | "-d" | "-e" => {
                    let word = self.next()?;
                    let (name, _) = expansion(word)?;
                    if !self.loop_variables.contains(name) {
                        return None;
                    }
                    test_stable = false;
                }
                _ => {
                    let operator = self.next()?;
                    if !matches!(operator, "=" | "!=") {
                        return None;
                    }
                    let right = self.next()?;
                    let left_word = self.word(left)?;
                    let right_word = self.word(right)?;
                    let compares_flag = expansion(left).is_some_and(|(name, suffix)| {
                        suffix.is_empty() && self.loop_variables.contains(name)
                    }) && literal(right).is_some_and(|text| text == "-c");
                    test_stable &= (left_word.1 && right_word.1) || compares_flag;
                }
            }
            self.take("]")?;
            if self.peek() != Some("&&") {
                break;
            }
            self.next();
        }
        self.separator().then_some(())?;
        self.take("then")?;
        self.commands(&["else", "fi"], test_stable, depth + 1)?;
        if self.peek() == Some("else") {
            self.next();
            self.commands(&["fi"], test_stable, depth + 1)?;
        }
        self.take("fi")
    }

    fn case(&mut self, stable: bool, depth: usize) -> Option<()> {
        let subject = self.next()?;
        let subject_stable = self.word(subject)?.1;
        let argument = expansion(subject)
            .is_some_and(|(name, suffix)| suffix.is_empty() && self.loop_variables.contains(name));
        self.take("in")?;
        self.separator();
        while self.peek() != Some("esac") {
            let mut suffix_only = true;
            loop {
                let pattern = self.next()?;
                if !pattern
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-*/?".contains(&byte))
                {
                    return None;
                }
                let file = pattern.strip_prefix("*/").unwrap_or(pattern);
                suffix_only &= pattern == "-c"
                    || (!file.contains('/')
                        && [".c", ".C", ".cc", ".cpp", ".cxx", ".c++"]
                            .iter()
                            .any(|suffix| file.ends_with(suffix)));
                match self.next()? {
                    ")" => break,
                    "|" => {}
                    _ => return None,
                }
            }
            self.commands(
                &[";;"],
                stable && (subject_stable || argument && suffix_only),
                depth + 1,
            )?;
            self.take(";;")?;
            self.separator();
        }
        self.take("esac")
    }

    fn for_loop(&mut self, stable: bool, depth: usize) -> Option<()> {
        let name = self.next()?;
        if !variable_name(name) || !name.as_bytes().first()?.is_ascii_lowercase() {
            return None;
        }
        if self.peek() == Some("in") {
            self.next();
            self.take("\"$@\"")?;
            self.separator().then_some(())?;
        } else {
            self.separator();
        }
        self.take("do")?;
        let inserted = self.loop_variables.insert(name);
        self.commands(&["done"], stable, depth + 1)?;
        if inserted {
            self.loop_variables.remove(name);
        }
        self.unstable_variables.insert(name);
        self.take("done")
    }

    fn forward_arguments(&mut self) -> Option<Vec<BString>> {
        let mut arguments = Vec::new();
        loop {
            let word = self.next()?;
            if word == "\"$@\"" {
                return Some(arguments);
            }
            let word = literal(word)?;
            if !pure_flag(word.as_bytes()) {
                return None;
            }
            arguments.push(word.into());
        }
    }

    fn printf(&mut self, mut stable: bool) -> Option<()> {
        let format = literal(self.next()?)?;
        if format.starts_with('-') || format.contains("%n") {
            return None;
        }
        loop {
            match self.next()? {
                ">&2" => return stable.then_some(()),
                ">" | ">>" => {
                    let word = self.next()?;
                    if let Some(text) = literal(word).filter(|text| Path::new(text).is_absolute()) {
                        self.telemetry.push(text.into());
                    } else {
                        let (name, suffix) = expansion(word)?;
                        if !suffix.is_empty()
                            || !name.starts_with("RCMAKE_M6_")
                            || !word.starts_with('"')
                        {
                            return None;
                        }
                        self.telemetry.push(word.into());
                    }
                    return Some(());
                }
                word => {
                    stable &= self.word(word)?.1;
                }
            }
        }
    }

    fn word(&self, word: &'a str) -> Option<(&'a str, bool)> {
        if let Some(literal) = literal(word) {
            return Some((literal, true));
        }
        let (name, suffix) = expansion(word)?;
        let stable = if self.loop_variables.contains(name) {
            suffix == "##*/"
        } else {
            !shell_supplied(name) && !self.unstable_variables.contains(name)
        };
        Some((name, stable))
    }
}

fn shell_supplied(name: &str) -> bool {
    name == "BASH"
        || name.starts_with("BASH_")
        || matches!(
            name,
            "BASHPID"
                | "DIRSTACK"
                | "EPOCHREALTIME"
                | "EPOCHSECONDS"
                | "EUID"
                | "FUNCNAME"
                | "GROUPS"
                | "HISTCMD"
                | "LINENO"
                | "OLDPWD"
                | "PIPESTATUS"
                | "PPID"
                | "PWD"
                | "RANDOM"
                | "SECONDS"
                | "SHLVL"
                | "SRANDOM"
                | "UID"
                | "_"
        )
}

fn variable_name(name: &str) -> bool {
    name.as_bytes().split_first().is_some_and(|(&first, rest)| {
        (first.is_ascii_alphabetic() || first == b'_')
            && rest
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    })
}

fn literal(word: &str) -> Option<&str> {
    if let Some(quoted) = word
        .strip_prefix('\'')
        .and_then(|word| word.strip_suffix('\''))
    {
        return (!quoted.contains('\'')).then_some(quoted);
    }
    if let Some(quoted) = word
        .strip_prefix('"')
        .and_then(|word| word.strip_suffix('"'))
    {
        return (!quoted.contains(['"', '$', '`', '\\'])).then_some(quoted);
    }
    word.bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"._-/=+,:".contains(&byte))
        .then_some(word)
}

fn expansion(word: &str) -> Option<(&str, &str)> {
    let word = word
        .strip_prefix('"')
        .and_then(|word| word.strip_suffix('"'))
        .unwrap_or(word);
    let word = word.strip_prefix('$')?;
    let (name, suffix) = if let Some(word) = word
        .strip_prefix('{')
        .and_then(|word| word.strip_suffix('}'))
    {
        let boundary = word.find(['-', '#', '%']).unwrap_or(word.len());
        word.split_at_checked(boundary)?
    } else {
        (word, "")
    };
    (variable_name(name) && matches!(suffix, "" | "-" | "##*/")).then_some((name, suffix))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use rcmake_host::{HostResult, PureHost};

    #[test]
    fn accepts_tracing_and_rejects_shell_substitution() -> HostResult<()> {
        let host = PureHost::default();
        let script = Path::new("/work/compiler");
        host.seed_file(
            script,
            br#"#!/bin/sh
compile=no
for arg do
    case "$arg" in
        -c) compile=yes ;;
        */m6_probe_*.c|m6_probe_*.c) compile=yes ;;
    esac
done
if [ "$compile" = yes ] && [ -n "${LANG-}" ]; then
    printf '%s\n' 'compiled' >> /work/probes.log
    set -- -DMARKER=1 "$@"
fi
exec /usr/bin/cc "$@"
"#,
        )?;
        let wrapper = Wrapper::read(&host, script)
            .ok_or_else(|| rcmake_host::HostError::unsupported("Expected an admitted wrapper."))?;
        assert_eq!(wrapper.target, Path::new("/usr/bin/cc"));
        assert_eq!(
            wrapper.prefixes,
            vec![vec![], vec![BString::from("-DMARKER=1")]]
        );
        let original = rcmake_store::file_digest(&host, script)?;
        host.seed_file(
            script,
            b"#!/bin/sh\nvalue=$(cat f)\nexec /usr/bin/cc \"$@\"\n",
        )?;
        assert_ne!(rcmake_store::file_digest(&host, script)?, original);
        assert!(Wrapper::read(&host, script).is_none());
        Ok(())
    }

    #[test]
    fn rejects_shell_supplied_values_in_output_and_flags() -> HostResult<()> {
        let host = PureHost::default();
        let script = Path::new("/work/compiler");
        for body in [
            "if [ -n \"$UID\" ]; then set -- -DMARKER=1 \"$@\"; fi",
            "printf '%s\\n' \"$BASH_COMMAND\" >&2",
        ] {
            let source = format!("#!/bin/bash\n{body}\nexec /usr/bin/cc \"$@\"\n");
            host.seed_file(script, source.as_bytes())?;
            assert!(Wrapper::read(&host, script).is_none());
        }
        Ok(())
    }
}
