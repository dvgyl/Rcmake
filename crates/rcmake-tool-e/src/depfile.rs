use std::io::ErrorKind;

use rcmake_base::{BString, path_lex::Lexical};
use rcmake_host::{Stream, WriteMode, path::from_native};

use super::{ToolCtx, error, path, read_all, write};

const CONTINUATION: &[u8] = b" \\\n  ";

pub(super) fn transform(args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    let [
        generator,
        format,
        _,
        _,
        top_binary,
        current_binary,
        input,
        output,
    ] = args
    else {
        return 1;
    };
    let relative = match generator.as_slice() {
        b"Ninja" | b"Ninja Multi-Config" | b"Unix Makefiles" | b"Watcom WMake" => true,
        b"Xcode" => false,
        _ => return 1,
    };
    let make = match format.as_slice() {
        b"gccdepfile" => false,
        b"makedepfile" => true,
        _ => return 1,
    };
    let contents = match cx.host.read(&path(cx, input), read_all()) {
        Ok(contents) => contents,
        Err(err) if err.kind == ErrorKind::NotFound => {
            write(
                cx,
                Stream::Stderr,
                &[
                    b"CMake Warning:\n  Expected depfile does not exist.\n\n    ",
                    input,
                    b"\n\n\n",
                ],
            );
            Vec::new()
        }
        Err(err) if err.kind == ErrorKind::IsADirectory => {
            error(cx, &[b"input in flex scanner failed\n"]);
            return 2;
        }
        Err(_) => return 2,
    };
    let Some(rules) = parse(&contents) else {
        return 2;
    };
    let top = from_native(&path(cx, top_binary));
    let current = from_native(&path(cx, current_binary));
    let top = normalized(&top);
    let convert = |word: &[u8]| {
        let absolute = normalized(&Lexical::new(&current).append(word));
        let path = Lexical::new(&absolute);
        let top = Lexical::new(&top);
        if relative && top.prefix(path) {
            path.relative(top).unwrap_or(absolute)
        } else {
            absolute
        }
    };

    let mut transformed = Vec::new();
    let mut phony = Vec::new();
    for rule in rules.into_iter().filter(|rule| !rule.targets.is_empty()) {
        for (index, target) in rule.targets.iter().enumerate() {
            if index > 0 {
                transformed.extend_from_slice(CONTINUATION);
            }
            escape(&convert(target), &mut transformed);
        }
        transformed.push(b':');
        for dependency in rule.dependencies {
            let dependency = convert(&dependency);
            transformed.extend_from_slice(CONTINUATION);
            escape(&dependency, &mut transformed);
            if make {
                phony.push(dependency);
            }
        }
        transformed.push(b'\n');
    }
    if make {
        transformed.push(b'\n');
        for dependency in phony {
            transformed.push(b'\n');
            escape(&dependency, &mut transformed);
            transformed.extend_from_slice(b":\n");
        }
    }

    match cx
        .host
        .write(&path(cx, output), &transformed, WriteMode::Truncate)
    {
        Ok(()) => 0,
        Err(err) if err.kind == ErrorKind::NotFound => 0,
        Err(_) => 2,
    }
}

fn normalized(path: &[u8]) -> BString {
    let mut result = Lexical::new(path).normalize();
    let root_len = Lexical::new(&result).root_path().len();
    if result.len() > root_len && result.ends_with(b"/") {
        result.pop();
    }
    result
}

#[derive(Default)]
struct Rule {
    targets: Vec<Vec<u8>>,
    dependencies: Vec<Vec<u8>>,
    colon: bool,
}

impl Rule {
    fn push(&mut self, word: &mut Vec<u8>) {
        if word.is_empty() {
            return;
        }
        let words = if self.colon {
            &mut self.dependencies
        } else {
            &mut self.targets
        };
        words.push(std::mem::take(word));
    }

    fn finish(mut self, rules: &mut Vec<Self>) -> Option<()> {
        if self.colon {
            self.dependencies.sort_unstable();
            self.dependencies.dedup();
            rules.push(self);
        } else if !self.targets.is_empty() {
            return None;
        }
        Some(())
    }
}

fn parse(contents: &[u8]) -> Option<Vec<Rule>> {
    let mut rules = Vec::new();
    let mut rule = Rule::default();
    let mut word = Vec::new();
    let mut bytes = contents.iter().copied().peekable();
    while let Some(byte) = bytes.next() {
        match byte {
            b'\\' => match bytes.peek() {
                Some(b'\n' | b'\r') => {
                    if bytes.next() == Some(b'\r') {
                        bytes.next_if_eq(&b'\n');
                    }
                    rule.push(&mut word);
                }
                Some(b'\\') => {
                    bytes.next();
                    word.extend_from_slice(b"//");
                }
                Some(b' ' | b'#' | b':') => word.extend(bytes.next()),
                Some(b'$') => {
                    bytes.next();
                    bytes.next_if_eq(&b'$');
                    word.push(b'$');
                }
                Some(b'\t') => {
                    bytes.next();
                    rule.push(&mut word);
                }
                _ => word.push(b'/'),
            },
            b'$' => {
                bytes.next_if_eq(&b'$');
                word.push(b'$');
            }
            b':' if bytes.peek().is_none_or(u8::is_ascii_whitespace) => {
                rule.push(&mut word);
                rule.colon = true;
            }
            b'\r' | b'\n' => {
                rule.push(&mut word);
                std::mem::take(&mut rule).finish(&mut rules)?;
            }
            b' ' | b'\t' => rule.push(&mut word),
            _ => word.push(byte),
        }
    }
    rule.push(&mut word);
    rule.finish(&mut rules)?;
    Some(rules)
}

pub(super) fn escape(path: &[u8], output: &mut Vec<u8>) {
    for &byte in path {
        match byte {
            b' ' | b'#' => output.push(b'\\'),
            b'$' => output.push(b'$'),
            _ => {}
        }
        output.push(byte);
    }
}
