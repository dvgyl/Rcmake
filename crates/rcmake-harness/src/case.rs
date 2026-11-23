use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

#[derive(Default)]
pub(crate) enum Tool {
    #[default]
    Cmake,
    Ctest,
    Cpack,
}

impl Tool {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Cmake => "cmake",
            Self::Ctest => "ctest",
            Self::Cpack => "cpack",
        }
    }
}

#[derive(Default)]
pub(crate) struct Case {
    pub(crate) name: String,
    pub(crate) tool: Tool,
    pub(crate) args: Vec<String>,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) stdin: String,
    pub(crate) check_files: bool,
    pub(crate) expect_fail_allowed: bool,
    pub(crate) files: BTreeMap<PathBuf, String>,
}

pub(crate) fn parse(source: &str) -> Result<Vec<Case>, String> {
    let mut cases: Vec<Case> = Vec::new();
    let mut names = BTreeSet::new();
    let mut file = None;
    for (index, raw) in source.split_inclusive('\n').enumerate() {
        let line = raw.trim_end_matches(['\r', '\n']);
        let fail = |message: &str| format!("line {}: {message}", index + 1);
        if let Some(name) = line.strip_prefix("=== CASE ") {
            let name = name
                .strip_suffix(" ===")
                .filter(|name| !name.trim().is_empty())
                .ok_or_else(|| fail("expected a case name and closing ==="))?;
            if !names.insert(name) {
                return Err(fail(&format!("duplicate case name {name:?}")));
            }
            cases.push(Case {
                name: name.to_owned(),
                ..Case::default()
            });
            file = None;
            continue;
        }
        let Some(case) = cases.last_mut() else {
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            return Err(fail("expected a case header"));
        };
        if let Some(path) = line
            .strip_prefix("-- ")
            .and_then(|line| line.strip_suffix(" --"))
        {
            let path = relative_path(path).map_err(|message| fail(&message))?;
            if case.files.contains_key(&path) {
                return Err(fail(&format!("duplicate file path {}", path.display())));
            }
            case.files.insert(path.clone(), String::new());
            file = Some(path);
        } else if let Some(path) = &file {
            case.files.entry(path.clone()).or_default().push_str(raw);
        } else if let Some(directive) = line.trim_start().strip_prefix('#') {
            if let Some((key, value)) = directive.split_once(':') {
                case.directive(key.trim(), value.strip_prefix(' ').unwrap_or(value))
                    .map_err(|message| fail(&message))?;
            }
        } else if !line.trim().is_empty() {
            return Err(fail("expected a directive or file header"));
        }
    }
    if cases.is_empty() {
        return Err("no case headers found".to_owned());
    }
    Ok(cases)
}

impl Case {
    fn directive(&mut self, key: &str, value: &str) -> Result<(), String> {
        match key {
            "tool" => {
                self.tool = match value.trim() {
                    "cmake" => Tool::Cmake,
                    "ctest" => Tool::Ctest,
                    "cpack" => Tool::Cpack,
                    _ => return Err(format!("unknown tool {value:?}")),
                };
            }
            "args" => self.args = split_args(value)?,
            "env" => {
                let (key, value) = value
                    .split_once('=')
                    .filter(|(key, _)| !key.is_empty() && !key.contains(['\0', ' ', '\t']))
                    .ok_or_else(|| "expected env: KEY=VALUE".to_owned())?;
                self.env.insert(key.to_owned(), value.to_owned());
            }
            "stdin" => self.stdin = value.to_owned(),
            "check_files" | "expect_fail_allowed" => {
                let enabled = value
                    .trim()
                    .parse()
                    .map_err(|_| format!("expected true or false for {key}"))?;
                if key == "check_files" {
                    self.check_files = enabled;
                } else {
                    self.expect_fail_allowed = enabled;
                }
            }
            _ => return Err(format!("unknown directive {key:?}")),
        }
        Ok(())
    }
}

fn relative_path(path: &str) -> Result<PathBuf, String> {
    let mut relative = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(name) => relative.push(name),
            Component::CurDir => (),
            _ => return Err(format!("invalid relative file path {path:?}")),
        }
    }
    if relative.as_os_str().is_empty() || path.contains('\0') {
        return Err(format!("invalid relative file path {path:?}"));
    }
    Ok(relative)
}

fn split_args(input: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut word = None;
    let mut quote = None;
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (Some(open), ch) if ch == open => quote = None,
            (None, '\'' | '"') => {
                quote = Some(ch);
                word.get_or_insert_with(String::new);
            }
            (None, ch) if ch.is_whitespace() => args.extend(word.take()),
            (None, '\\') => {
                let escaped = chars
                    .next()
                    .ok_or_else(|| "trailing backslash in args".to_owned())?;
                word.get_or_insert_with(String::new).push(escaped);
            }
            (Some('"'), '\\') if chars.peek().is_some_and(|ch| "\\\"$`".contains(*ch)) => {
                word.get_or_insert_with(String::new).extend(chars.next());
            }
            _ => word.get_or_insert_with(String::new).push(ch),
        }
    }
    if quote.is_some() {
        return Err("unclosed quote in args".to_owned());
    }
    args.extend(word);
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cases_directives_and_file_bytes() -> Result<(), String> {
        let source = concat!(
            "=== CASE configure ===\r\n",
            "# tool: ctest\r\n",
            "# args: -S '<TMP>/a b.cmake' \"\" -Dname=\"two words\" escaped\\ space\r\n",
            "# env: KEY=a=b\r\n",
            "# env: SIDE=both\r\n",
            "# stdin:  input  \r\n",
            "# check_files: true\r\n",
            "# expect_fail_allowed: true\r\n",
            "-- nested/a b.cmake --\r\n",
            "# args: this is file content\r\n",
            "message(STATUS hi)\r\n",
            "=== CASE second ===\n",
            "-- empty --\n",
            "-- final --\n",
            "no final newline",
        );
        let cases = parse(source)?;
        let [first, second] = cases.as_slice() else {
            return Err("expected two cases".to_owned());
        };
        assert_eq!(first.name, "configure");
        assert_eq!(first.tool.name(), "ctest");
        assert_eq!(
            first.args,
            [
                "-S",
                "<TMP>/a b.cmake",
                "",
                "-Dname=two words",
                "escaped space"
            ]
        );
        assert_eq!(first.env.get("KEY").map(String::as_str), Some("a=b"));
        assert_eq!(first.env.get("SIDE").map(String::as_str), Some("both"));
        assert_eq!(first.stdin, " input  ");
        assert!(first.check_files && first.expect_fail_allowed);
        assert_eq!(
            first
                .files
                .get(Path::new("nested/a b.cmake"))
                .map(String::as_str),
            Some("# args: this is file content\r\nmessage(STATUS hi)\r\n")
        );
        assert_eq!(second.tool.name(), "cmake");
        assert!(!second.check_files && !second.expect_fail_allowed);
        assert_eq!(
            second.files.get(Path::new("empty")).map(String::as_str),
            Some("")
        );
        assert_eq!(
            second.files.get(Path::new("final")).map(String::as_str),
            Some("no final newline")
        );
        Ok(())
    }

    #[test]
    fn rejects_bad_headers_and_escaping_paths() {
        for invalid in [
            "# args: --version",
            "=== CASE test ===\n# args: 'unterminated",
            "=== CASE test ===\n# check_files: maybe",
            "=== CASE test ===\n=== CASE test ===",
            "=== CASE test ===\n-- nested/../../outside --",
            "=== CASE test ===\n-- /absolute --",
            "=== CASE test ===\n-- a --\n-- ./a --",
        ] {
            assert!(parse(invalid).is_err(), "accepted {invalid:?}");
        }
    }
}
