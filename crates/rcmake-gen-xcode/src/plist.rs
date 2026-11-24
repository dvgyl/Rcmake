//! OpenStep property-list output for Xcode projects.

use std::fmt::Write as _;

use crate::object::{Attrs, Isa, Objects, Reference, Value};

fn string(out: &mut String, value: &str) {
    if !value.is_empty()
        && !value.contains("//")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"$_./".contains(&byte))
    {
        out.push_str(value);
        return;
    }
    quoted_string(out, value);
}

fn quoted_string(out: &mut String, value: &str) {
    out.push('"');
    for ch in value.chars() {
        if matches!(ch, '"' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
}

fn reference(out: &mut String, reference: &Reference) {
    out.push_str(&reference.id);
    if !reference.comment.is_empty() {
        let comment = reference
            .comment
            .replace("*/", "* /")
            .replace(['\n', '\r'], " ");
        let _ = write!(out, " /* {comment} */");
    }
}

fn indent(out: &mut String, level: usize) {
    out.extend(std::iter::repeat_n('\t', level));
}

fn value(out: &mut String, item: &Value, level: usize, inline: bool) {
    match item {
        Value::Text(text) => string(out, text),
        Value::QuotedText(text) => quoted_string(out, text),
        Value::Ref(item) => reference(out, item),
        Value::List(items) => {
            out.push('(');
            for (index, item) in items.iter().enumerate() {
                if !inline {
                    out.push('\n');
                    indent(out, level + 1);
                } else if index != 0 {
                    out.push(',');
                }
                value(out, item, level + 1, inline);
                if !inline {
                    out.push(',');
                }
            }
            if !inline {
                out.push('\n');
                indent(out, level);
            }
            out.push(')');
        }
        Value::ListRow(items) => {
            out.push_str("(\n");
            for (index, item) in items.iter().enumerate() {
                if index != 0 {
                    out.push(',');
                }
                string(out, item);
            }
            indent(out, level);
            out.push(')');
        }
        Value::Dict(items) => {
            if inline {
                out.push('{');
                fields(out, items, level + 1, true, true);
            } else {
                out.push_str("{\n");
                fields(out, items, level + 1, false, true);
                indent(out, level);
            }
            out.push('}');
        }
    }
}

fn fields(out: &mut String, items: &Attrs, level: usize, compact: bool, inline_lists: bool) {
    for (key, item) in items {
        if !compact {
            indent(out, level);
        }
        string(out, key);
        out.push_str(" = ");
        value(out, item, level, inline_lists);
        out.push_str(if compact { "; " } else { ";\n" });
    }
}

pub(crate) fn encode(objects: &Objects, root: &Reference) -> String {
    let mut out = String::from(
        "// !$*UTF8*$!\n{\n\tarchiveVersion = 1;\n\tclasses = {\n\t};\n\tobjectVersion = 46;\n\tobjects = {\n",
    );
    let mut section = None;
    for ((name, _), object) in &objects.entries {
        if section != Some(*name) {
            if let Some(previous) = section {
                let _ = writeln!(out, "/* End {previous} section */");
            }
            let _ = writeln!(out, "\n/* Begin {name} section */");
            section = Some(*name);
        }
        out.push_str("\t\t");
        reference(&mut out, &object.reference);
        if matches!(object.isa, Isa::PBXBuildFile | Isa::PBXFileReference) {
            let _ = write!(out, " = {{isa = {name}; ");
            fields(&mut out, &object.attrs, 3, true, true);
        } else {
            let _ = writeln!(out, " = {{\n\t\t\tisa = {name};");
            fields(&mut out, &object.attrs, 3, false, false);
            out.push_str("\t\t");
        }
        out.push_str("};\n");
    }
    if let Some(previous) = section {
        let _ = writeln!(out, "/* End {previous} section */");
    }
    out.push_str("\t};\n\trootObject = ");
    reference(&mut out, root);
    out.push_str(";\n}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_openstep_values() {
        for (input, expected) in [
            ("A/b.c_$0", "A/b.c_$0"),
            ("", "\"\""),
            ("a//b", "\"a//b\""),
            ("x \"a\"\\\nb", "\"x \\\"a\\\"\\\\\nb\""),
        ] {
            let mut actual = String::new();
            string(&mut actual, input);
            assert_eq!(actual, expected);
        }
    }
}
