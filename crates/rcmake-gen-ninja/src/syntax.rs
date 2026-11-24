//! Ninja file syntax and POSIX shell arguments.

use rcmake_base::BString;

#[derive(Default)]
pub(crate) struct Ninja(pub(crate) Vec<u8>);

#[derive(Default)]
pub(crate) struct Edge<'a> {
    pub(crate) outputs: &'a [BString],
    pub(crate) implicit_outputs: &'a [BString],
    pub(crate) inputs: &'a [BString],
    pub(crate) implicit_inputs: &'a [BString],
    pub(crate) order_only: &'a [&'a BString],
}

impl Ninja {
    pub(crate) fn edge(&mut self, rule: &[u8], edge: Edge<'_>) {
        self.0.extend_from_slice(b"build");
        self.paths(b"", edge.outputs);
        self.paths(b" |", edge.implicit_outputs);
        self.0.extend_from_slice(b": ");
        self.0.extend_from_slice(rule);
        self.paths(b"", edge.inputs);
        self.paths(b" |", edge.implicit_inputs);
        self.paths(b" ||", edge.order_only.iter().copied());
        self.0.push(b'\n');
    }

    pub(crate) fn phony(&mut self, output: &BString, inputs: &[BString]) {
        self.edge(
            b"phony",
            Edge {
                outputs: std::slice::from_ref(output),
                inputs,
                ..Edge::default()
            },
        );
    }

    pub(crate) fn include(&mut self, path: &[u8]) {
        self.0.extend_from_slice(b"include ");
        self.escape(path, true);
        self.0.push(b'\n');
    }

    pub(crate) fn binding(&mut self, key: &[u8], value: &[u8]) {
        self.0.extend_from_slice(b"  ");
        self.0.extend_from_slice(key);
        self.0.extend_from_slice(b" = ");
        self.escape(value, false);
        self.0.push(b'\n');
    }

    fn paths<'a>(&mut self, separator: &[u8], paths: impl IntoIterator<Item = &'a BString>) {
        let mut paths = paths.into_iter();
        let Some(first) = paths.next() else { return };
        self.0.extend_from_slice(separator);
        for path in std::iter::once(first).chain(paths) {
            self.0.push(b' ');
            self.escape(path, true);
        }
    }

    fn escape(&mut self, value: &[u8], path: bool) {
        for &byte in value {
            match byte {
                b'$' | b' ' | b':' | b'\n' => self.0.extend_from_slice(&[b'$', byte]),
                b'|' if path => self.0.extend_from_slice(b"${ninja_pipe}"),
                _ => self.0.push(byte),
            }
        }
    }
}

pub(crate) fn shell_word(word: &[u8], out: &mut Vec<u8>) {
    if !word.is_empty()
        && word
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_+-./:=@".contains(byte))
    {
        out.extend_from_slice(word);
        return;
    }

    out.push(b'\'');
    for &byte in word {
        if byte == b'\'' {
            out.extend_from_slice(b"'\\''");
        } else {
            out.push(byte);
        }
    }
    out.push(b'\'');
}
