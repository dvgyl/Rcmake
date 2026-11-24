use std::collections::BTreeSet;

use rcmake_base::{BString, list};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_host::path;
use rcmake_syntax::ArgKind;

use crate::policy_misc::files::configure_file;
use crate::string_math::make_c_identifier;

use super::{status, values};

pub(super) fn create(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let (output, driver, options) = match args {
        [output, driver, options @ ..] if !options.is_empty() => (output, driver, options),
        _ => return Err(status("called with wrong number of arguments.")),
    };
    let line = driver.line;
    let driver = driver.value.as_slice();
    if extension_index(driver).is_none_or(|index| index + 1 == driver.len()) {
        return Err(status(
            "You must specify a file extension for the test driver file.",
        ));
    }
    let options = values(options);
    let spec = TestDriver::parse(&options)?;
    let binary_dir = path::from_native(&cx.current_binary_dir());
    let driver: BString = [binary_dir.as_slice(), b"/", driver].concat().into();
    let root = path::to_native(&cx.var_or_empty(b"CMAKE_ROOT"));
    let template = path::from_native(&root.join("Templates/TestDriver.cxx.in"));

    spec.set_template_vars(cx)?;
    let arguments = [template, driver.clone(), "@ONLY".into()].map(|value| ExpandedArg {
        value,
        kind: ArgKind::Quoted,
        line,
    });
    configure_file(cx, &arguments)?;
    cx.set_var(
        &output.value,
        list::join(std::iter::once(driver.as_slice()).chain(spec.tests)),
    );
    Ok(Flow::Normal)
}

#[derive(Default)]
struct TestDriver<'a> {
    tests: Vec<&'a [u8]>,
    extra_include: Option<&'a [u8]>,
    function: Option<&'a [u8]>,
}

impl<'a> TestDriver<'a> {
    fn parse(mut args: &[&'a [u8]]) -> Result<Self, CmdError> {
        let mut spec = Self::default();
        while let Some((&arg, tail)) = args.split_first() {
            args = tail;
            let destination = match arg {
                b"EXTRA_INCLUDE" => &mut spec.extra_include,
                b"FUNCTION" => &mut spec.function,
                _ => {
                    spec.tests.push(arg);
                    continue;
                }
            };
            let Some((&value, tail)) = args.split_first() else {
                return Err(status(format!(
                    "incorrect arguments to {}",
                    BString::from(arg)
                )));
            };
            *destination = Some(value);
            args = tail;
        }
        Ok(spec)
    }

    fn set_template_vars(&self, cx: &mut Cx<'_>) -> Result<(), CmdError> {
        let mut declarations = BString::default();
        let mut table = BString::default();
        let mut functions = BTreeSet::new();
        for source in &self.tests {
            let name = extension_index(source)
                .and_then(|index| source.get(..index))
                .unwrap_or(source);
            let identifier = make_c_identifier(name);
            if functions.insert(identifier.clone()) {
                declarations.extend_from_slice(b"extern int ");
                declarations.extend_from_slice(&identifier);
                declarations.extend_from_slice(b"(int, char*[]);\n");
            }
            table.extend_from_slice(b"  {\n    \"");
            table.extend_from_slice(name);
            table.extend_from_slice(b"\",\n    ");
            table.extend_from_slice(&identifier);
            table.extend_from_slice(b"\n  },\n");
            if table.len().saturating_add(declarations.len()) > cx.limits().max_value_bytes {
                return Err(status("test driver content exceeds the value size limit"));
            }
        }
        if let Some(header) = self.extra_include {
            cx.set_var(
                b"CMAKE_TESTDRIVER_EXTRA_INCLUDES",
                [b"#include \"".as_slice(), header, b"\"\n"].concat().into(),
            );
        }
        if let Some(function) = self.function {
            cx.set_var(
                b"CMAKE_TESTDRIVER_ARGVC_FUNCTION",
                [function, b"(&ac, &av);\n"].concat().into(),
            );
        }
        cx.set_var(b"CMAKE_FORWARD_DECLARE_TESTS", declarations);
        cx.set_var(b"CMAKE_FUNCTION_TABLE_ENTRIES", table);
        Ok(())
    }
}

fn extension_index(path: &[u8]) -> Option<usize> {
    let index = path
        .iter()
        .rposition(|&byte| matches!(byte, b'.' | b'/' | b'\\'))?;
    (path.get(index) == Some(&b'.')).then_some(index)
}
