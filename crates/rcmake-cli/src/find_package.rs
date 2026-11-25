// SPDX-License-Identifier: MIT OR Apache-2.0

use rcmake_base::{BString, list, path_lex::Lexical};
use rcmake_eval::{Cx, RunMode};
use rcmake_genex::{ArtifactKind, ArtifactPart, EvalContext};
use rcmake_host::Host;
use rcmake_model::{BT, LinkItem, Target, TargetKind, TargetOrigin};
use rcmake_resolve::usage::{ResolvedLinkKind, UsageResolver};
use std::sync::Arc;

use super::{Options, cache, emit_cli_error, new_interp, options};

/// `cmake --find-package` (`cmake::FindPackage`): run `CMakeFindPackageMode.cmake`
/// with the `FIND_PACKAGE` role and print the result for the requested `MODE`.
pub(super) fn run(argv: &[BString], opts: Options, host: Arc<dyn Host>) -> i32 {
    let (_, cmake_root) = rcmake_host::discover_cmake_root(host.as_ref(), argv.first());
    let script = cmake_root
        .unwrap_or_default()
        .join("Modules")
        .join("CMakeFindPackageMode.cmake");
    let mut interp = new_interp(argv, &opts, RunMode::Configure, Arc::clone(&host));
    for edit in opts.cache_edits {
        if let options::CacheEdit::Define(def) = edit {
            cache::apply(interp.cx().cache(), def);
        }
    }
    interp
        .cx()
        .global()
        .props
        .set("_RCMAKE_CMAKE_ROLE".into(), "FIND_PACKAGE".into());
    let outcome = interp.run_find_package_script(&script);
    if outcome.exit_code != 0 {
        return outcome.exit_code;
    }
    let mut cx = interp.cx();
    let name = cx.var_or_empty(b"NAME");
    let mode = cx.var_or_empty(b"MODE");
    let found = rcmake_base::truth::is_on(&cx.var_or_empty(b"PACKAGE_FOUND"));
    let quiet = rcmake_base::truth::is_on(&cx.var_or_empty(b"PACKAGE_QUIET"));
    let mut report = BString::default();
    if !found {
        if !quiet {
            report = [name.as_slice(), b" not found.\n"].concat().into();
        }
    } else if mode.as_slice() == b"EXIST" {
        if !quiet {
            report = [name.as_slice(), b" found.\n"].concat().into();
        }
    } else if mode.as_slice() == b"COMPILE" {
        report = compile_flags(&mut cx);
    } else if mode.as_slice() == b"LINK" {
        match link_flags(&mut cx) {
            Ok(flags) => report = flags,
            Err(error) => {
                emit_cli_error(host.as_ref(), error);
                return 1;
            }
        }
    }
    host.write_out(rcmake_host::Stream::Stdout, &report);
    if found {
        0
    } else {
        emit_cli_error(
            host.as_ref(),
            "Run 'cmake --help' for all supported options.",
        );
        1
    }
}

fn compile_flags(cx: &mut Cx<'_>) -> BString {
    let language = cx.var_or_empty(b"LANGUAGE");
    let flag = cx
        .var(&[b"CMAKE_INCLUDE_FLAG_", language.as_slice()].concat())
        .unwrap_or_else(|| "-I".into());
    let includes = cx.var_or_empty(b"PACKAGE_INCLUDE_DIRS");
    let definitions = cx.var_or_empty(b"PACKAGE_DEFINITIONS");
    let mut flags = BString::default();
    for directory in list::split(&includes, list::Empty::Drop) {
        flags.extend_from_slice(&flag);
        flags.extend_from_slice(&quote(&directory));
        flags.push(b' ');
    }
    flags.extend_from_slice(&definitions);
    flags.push(b'\n');
    flags
}

fn link_flags(cx: &mut Cx<'_>) -> Result<BString, String> {
    let language = cx.var_or_empty(b"LANGUAGE");
    let msvc = cx.var_or_empty(b"COMPILER_ID") == "MSVC";
    let mut flag = |key: &[u8], default: &str| {
        cx.var(&[b"CMAKE_".as_slice(), language.as_slice(), b"_", key].concat())
            .or_else(|| cx.var(&[b"CMAKE_", key].concat()))
            .unwrap_or_else(|| default.into())
    };
    let prefix = flag(b"LINK_LIBRARY_FLAG", if msvc { "" } else { "-l" });
    let suffix = flag(b"LINK_LIBRARY_SUFFIX", if msvc { ".lib" } else { "" });
    let config = cx.var_or_empty(b"CMAKE_BUILD_TYPE");
    let libraries = cx.var_or_empty(b"PACKAGE_LIBRARIES");
    let dir = cx.current_dir_id();
    let policies = *cx.policies_mut();
    let model = cx.project();
    let name = (0..=model.targets().count())
        .map(|index| BString::from(format!("_rcmake_find_package_{index}")))
        .find(|name| model.find_target(dir, name).is_none())
        .ok_or("Cannot create a target for the package link flags.")?;
    let mut target = Target::new(
        name,
        TargetKind::Executable,
        TargetOrigin::Normal,
        dir,
        Default::default(),
        policies,
    );
    target.usage.link_libraries = Some(
        list::split(&libraries, list::Empty::Drop)
            .into_iter()
            .map(|library| LinkItem {
                opens_lookup_scope: false,
                closes_lookup_scope: false,
                text: BT::new(library, Default::default()),
                lookup_dir: dir,
                scope: None,
            })
            .collect(),
    );
    let target = model
        .add_target(target)
        .map_err(|error| error.to_string())?;
    let mut resolver = UsageResolver::new(model);
    let context = EvalContext {
        config: rcmake_genex::ConfigId(config),
        language: Some(language),
        ..EvalContext::default()
    };
    let items = resolver
        .collect_link_items(target, &context)
        .map_err(|error| error.to_string())?;
    let mut flags = BString::default();
    for item in items {
        let word = match item.kind {
            ResolvedLinkKind::Library(id) => resolver
                .artifact(
                    id,
                    ArtifactKind::LinkerFile,
                    ArtifactPart::Full,
                    &context.config,
                )
                .or_else(|| {
                    model.target(id).and_then(|target| {
                        resolver.imported_runtime_location(target, &context.config)
                    })
                })
                .ok_or_else(|| format!("Cannot resolve the library {}.", item.text.value))?,
            ResolvedLinkKind::Raw => {
                let value = item.text.value;
                let (_, extension) = Lexical::new(&value).stem_extension(true);
                if value.starts_with(b"-")
                    || value.contains(&b'/')
                    || value.contains(&b'\\')
                    || (msvc && !extension.is_empty())
                    || extension.eq_ignore_ascii_case(b".lib")
                {
                    value
                } else {
                    [prefix.as_slice(), &value, &suffix].concat().into()
                }
            }
            ResolvedLinkKind::ObjectFile => item.text.value,
            _ => continue,
        };
        flags.extend_from_slice(&quote(&word));
        flags.push(b' ');
    }
    flags.push(b'\n');
    Ok(flags)
}

fn quote(value: &[u8]) -> BString {
    if value.iter().any(u8::is_ascii_whitespace)
        && !value
            .iter()
            .any(|byte| matches!(byte, b'"' | b'$' | b'`' | b'\\' | b'!'))
    {
        return [b"\"".as_slice(), value, b"\""].concat().into();
    }
    rcmake_base::shell::quote(value)
}
