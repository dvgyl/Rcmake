use rcmake_base::{BString, path_lex::Lexical};
use rcmake_eval::{CmdError, Cx, RunMode};
use rcmake_host::{FileKind, Follow, path};
use rcmake_model::{DirId, PropScope, TargetBinding, TargetId};

use crate::custom::{
    SOURCE_BINARY_ALTERNATIVE, SOURCE_ORDER_PROP, bind_source, record_source_order,
};
use crate::dirs::{find_directory, normalized_path, scoped_test_directory};

use super::quoted;

const DIRECTORY_ERROR: &str = "DIRECTORY scope provided but requested directory was not found.  This could be because the directory argument was invalid or, it is valid but has not been processed yet.";
pub(super) const RESOLVED_SOURCE_LOCATION: &[u8] = b"\0rcmake-source-location";
const ABSOLUTE_SOURCE_PATH: &[u8] = b"\0rcmake-source-absolute";

pub(super) fn source_scope(cx: &mut Cx<'_>, dir: DirId, source: &[u8]) -> PropScope {
    select_source(cx, dir, source, false, true).unwrap_or_else(|| PropScope::Source {
        dir,
        path: source.into(),
    })
}

pub(super) fn select_source(
    cx: &mut Cx<'_>,
    dir: DirId,
    source: &[u8],
    scoped: bool,
    create: bool,
) -> Option<PropScope> {
    let [source_path, binary_path] = source_paths(cx, source);
    let relative = !scoped && !path::to_native(source).is_absolute();
    let existing = relative
        .then(|| {
            [&source_path, &binary_path]
                .into_iter()
                .find(|path| source_file_resolves_path(cx, path))
        })
        .flatten();
    let first = existing.unwrap_or(&source_path);
    let alternative =
        (relative && existing.is_none() && source_path != binary_path).then_some(&binary_path);
    let paths = std::iter::once(first).chain(alternative);
    let exact_extension = [115, 187].into_iter().all(|id| {
        super::generated::policy(cx, dir, rcmake_diag::PolicyId::new(id))
            == rcmake_diag::PolicyStatus::New
    });
    let cmp0115_new = super::generated::policy(cx, dir, rcmake_diag::PolicyId::new(115))
        == rcmake_diag::PolicyStatus::New;
    let exact_match = |candidate: &[u8]| paths.clone().any(|prefix| candidate == prefix.as_slice());
    let known = cx
        .project()
        .dir(dir)
        .source_props
        .iter()
        .find_map(|(key, props)| {
            if let Some(resolved) = props.get(RESOLVED_SOURCE_LOCATION) {
                return exact_match(resolved).then(|| (key.clone(), resolved.clone(), None));
            }
            if exact_match(key) {
                return Some((key.clone(), key.clone(), None));
            }
            props
                .get(SOURCE_BINARY_ALTERNATIVE)
                .filter(|alternate| exact_match(alternate))
                .map(|alternate| (key.clone(), alternate.clone(), None))
        })
        .or_else(|| {
            if exact_extension {
                return None;
            }
            find_ambiguous_source_prop(cx, dir, first, alternative, relative)
        });
    let key = match known {
        Some((key, matched, updated_alt)) => {
            if key != matched {
                bind_source(cx.project().dir_mut(dir), &key, &matched);
                if let Some(alternate) = updated_alt {
                    cx.project()
                        .dir_mut(dir)
                        .source_props
                        .entry(matched.clone())
                        .or_default()
                        .set(SOURCE_BINARY_ALTERNATIVE.into(), alternate);
                }
            }
            matched
        }
        None if !create => return None,
        None => {
            let (key, created_alt, already_ordered) = if !exact_extension
                && existing.is_none()
                && !has_known_source_extension(source)
                && !source_file_resolves_path(cx, first)
            {
                if let Some((bound, bound_alt)) =
                    find_ordered_source(cx, dir, first, alternative, relative)
                {
                    (bound, bound_alt, true)
                } else if !cmp0115_new
                    && let Some((bound, bound_alt)) =
                        unique_disk_source_with_extension(cx, first, alternative)
                {
                    (bound, bound_alt, false)
                } else {
                    (first.clone(), alternative.cloned(), false)
                }
            } else {
                (first.clone(), alternative.cloned(), false)
            };
            let directory = cx.project().dir_mut(dir);
            let props = directory.source_props.entry(key.clone()).or_default();
            if let Some(alternate) = created_alt {
                // An absolute lookup can bind this record to either directory.
                props.set(SOURCE_BINARY_ALTERNATIVE.into(), alternate);
                if !already_ordered {
                    record_source_order(directory, source, true);
                }
            } else if !already_ordered {
                record_source_order(directory, &key, false);
            }
            key
        }
    };
    if !relative || existing.is_some() {
        let props = cx
            .project()
            .dir_mut(dir)
            .source_props
            .entry(key.clone())
            .or_default();
        props.unset(SOURCE_BINARY_ALTERNATIVE);
        if !relative {
            props.set(ABSOLUTE_SOURCE_PATH.into(), "1".into());
        }
    }
    Some(PropScope::Source { dir, path: key })
}

fn find_ambiguous_source_prop(
    cx: &mut Cx<'_>,
    dir: DirId,
    first: &BString,
    alternative: Option<&BString>,
    relative: bool,
) -> Option<(BString, BString, Option<BString>)> {
    let first_can_extend =
        !has_known_source_extension(first) && !source_file_resolves_path(cx, first);
    let alt_can_extend = alternative
        .is_some_and(|alt| !has_known_source_extension(alt) && !source_file_resolves_path(cx, alt));
    let lookup_extends = |candidate: &[u8]| {
        (first_can_extend && matches_with_extension(first, candidate))
            || (alt_can_extend
                && alternative.is_some_and(|alt| matches_with_extension(alt, candidate)))
    };
    let paths = std::iter::once(first).chain(alternative);
    let candidates: Vec<(BString, BString, Option<BString>, Option<BString>)> = cx
        .project()
        .dir(dir)
        .source_props
        .iter()
        .filter_map(|(key, props)| {
            if let Some(resolved) = props.get(RESOLVED_SOURCE_LOCATION) {
                return lookup_extends(resolved)
                    .then(|| (key.clone(), resolved.clone(), None, None));
            }
            if lookup_extends(key) {
                return Some((key.clone(), key.clone(), None, None));
            }
            if let Some(alternate) = props.get(SOURCE_BINARY_ALTERNATIVE)
                && lookup_extends(alternate)
            {
                return Some((key.clone(), alternate.clone(), None, None));
            }
            if !has_known_source_extension(key)
                && let Some(prefix) = paths
                    .clone()
                    .find(|prefix| matches_with_extension(key, prefix))
            {
                let updated_alt = (relative && props.get(SOURCE_BINARY_ALTERNATIVE).is_some())
                    .then_some(alternative)
                    .flatten()
                    .filter(|alt| *alt != prefix)
                    .cloned();
                return Some((key.clone(), prefix.clone(), updated_alt, Some(key.clone())));
            }
            if let Some(alternate) = props.get(SOURCE_BINARY_ALTERNATIVE)
                && !has_known_source_extension(alternate)
                && let Some(prefix) = paths
                    .clone()
                    .find(|prefix| matches_with_extension(alternate, prefix))
            {
                return Some((key.clone(), prefix.clone(), None, Some(alternate.clone())));
            }
            None
        })
        .collect();
    candidates
        .into_iter()
        .find(|(_, _, _, check)| {
            check
                .as_ref()
                .is_none_or(|path| !source_file_resolves_path(cx, path))
        })
        .map(|(key, matched, updated_alt, _)| (key, matched, updated_alt))
}

fn find_ordered_source(
    cx: &mut Cx<'_>,
    dir: DirId,
    first: &BString,
    alternative: Option<&BString>,
    relative: bool,
) -> Option<(BString, Option<BString>)> {
    let directory = cx.project().dir(dir);
    let order = directory.props.get(SOURCE_ORDER_PROP)?;
    for entry in order.split(|&byte| byte == 0) {
        let Some((&tag, raw)) = entry.split_first() else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }
        match tag {
            b'A' => {
                if matches_with_extension(first, raw)
                    || alternative.is_some_and(|alt| matches_with_extension(alt, raw))
                {
                    return Some((raw.into(), None));
                }
            }
            b'R' => {
                let reg_src =
                    Lexical::new(&Lexical::new(&directory.source_dir).append(raw)).normalize();
                let reg_bin =
                    Lexical::new(&Lexical::new(&directory.binary_dir).append(raw)).normalize();
                if matches_with_extension(first, &reg_src) {
                    let alt = (relative && reg_src != reg_bin).then_some(reg_bin);
                    return Some((reg_src, alt));
                }
                if alternative.is_some_and(|alt| matches_with_extension(alt, &reg_bin))
                    || matches_with_extension(first, &reg_bin)
                {
                    return Some((reg_bin, None));
                }
            }
            _ => {}
        }
    }
    None
}

fn unique_disk_source_with_extension(
    cx: &Cx<'_>,
    first: &BString,
    alternative: Option<&BString>,
) -> Option<(BString, Option<BString>)> {
    let mut found: Option<(&[u8], BString, Option<BString>)> = None;
    for (is_first, base) in
        std::iter::once((true, first)).chain(alternative.into_iter().map(|alt| (false, alt)))
    {
        for extension in SOURCE_EXTENSIONS.split_ascii_whitespace() {
            let ext = extension.as_bytes();
            let candidate: BString = [base.as_slice(), b".", ext].concat().into();
            if cx
                .host()
                .metadata(&path::to_native(&candidate), Follow::Yes)
                .is_ok_and(|meta| meta.kind == FileKind::File)
            {
                if let Some((prev_ext, _, _)) = &found {
                    if *prev_ext != ext {
                        return None;
                    }
                    continue;
                }
                let alt = is_first
                    .then_some(alternative)
                    .flatten()
                    .map(|alt_base| [alt_base.as_slice(), b".", ext].concat().into());
                found = Some((ext, candidate, alt));
            }
        }
    }
    found.map(|(_, path, alt)| (path, alt))
}

fn has_known_source_extension(source: &[u8]) -> bool {
    path::to_native(source)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| known_source_extension(ext.as_bytes()))
}

fn matches_with_extension(base: &[u8], with_ext: &[u8]) -> bool {
    with_ext
        .strip_prefix(base)
        .and_then(|suffix| suffix.strip_prefix(b"."))
        .is_some_and(known_source_extension)
}

pub(super) fn source_file_resolves_path(cx: &Cx<'_>, source: &[u8]) -> bool {
    let file = path::to_native(source);
    // A recognized extension has a definite spelling even if its directory is ambiguous.
    !has_known_source_extension(source) && cx.host().metadata(&file, Follow::Yes).is_ok()
}

fn known_source_extension(extension: &[u8]) -> bool {
    SOURCE_EXTENSIONS
        .split_ascii_whitespace()
        .any(|known| extension == known.as_bytes())
}

pub(super) const SOURCE_EXTENSIONS: &str = "c C c++ cc cpp cxx cu mpp m M mm ixx cppm ccm cxxm c++m h hh h++ hm hpp hxx in txx f F for f77 f90 f95 f03 hip ispc";

pub(super) fn source_paths(cx: &Cx<'_>, source: &[u8]) -> [BString; 2] {
    let file = path::to_native(source);
    [cx.current_source_dir(), cx.current_binary_dir()]
        .map(|base| path::from_native_cmake(&normalized_path(&base.join(&file))))
}

pub(super) fn single_name<'a>(scope: &[u8], names: &[&'a [u8]]) -> Result<&'a [u8], CmdError> {
    match names {
        [name] => Ok(name),
        [] => Err(CmdError::status(quoted(
            "not given name for ",
            scope,
            " scope.",
        ))),
        [_, extra, ..] => Err(CmdError::status(quoted(
            "given invalid argument \"",
            extra,
            "\".",
        ))),
    }
}

pub(super) fn test_names<'a, 'b>(
    cx: &mut Cx<'_>,
    names: &'a [&'b [u8]],
) -> Result<(&'a [&'b [u8]], DirId), CmdError> {
    match names {
        [tests @ .., b"DIRECTORY", directory] => Ok((tests, scoped_test_directory(cx, directory)?)),
        [.., b"DIRECTORY"] => Err(CmdError::status(
            "called with incorrect number of arguments no value provided to the DIRECTORY option",
        )),
        _ => Ok((names, cx.current_dir_id())),
    }
}

pub(super) struct SourceSelection<'a> {
    pub dir: DirId,
    pub source: &'a [u8],
    pub scoped: bool,
}

pub(super) fn source_scopes<'a>(
    cx: &mut Cx<'_>,
    mut names: &[&'a [u8]],
    query: bool,
) -> Result<Vec<SourceSelection<'a>>, CmdError> {
    let is_option = |word: &[u8]| matches!(word, b"DIRECTORY" | b"TARGET_DIRECTORY");
    let section_end = |words: &[&[u8]]| {
        words
            .iter()
            .position(|word| is_option(word))
            .unwrap_or(words.len())
    };
    let (sources, rest) = names.split_at(section_end(names));
    names = rest;
    let scoped = !rest.is_empty();
    if query {
        single_name(b"SOURCE", sources)?;
    }
    let mut dirs = Vec::new();
    while let Some((option, rest)) = names.split_first() {
        let (selected, tail) = rest.split_at(section_end(rest));
        if selected.is_empty() {
            return Err(CmdError::status(quoted(
                "called with incorrect number of arguments no value provided to the ",
                option,
                " option",
            )));
        }
        if query {
            single_name(option, selected)?;
        }
        for name in selected {
            let dir = source_directory(cx, option, name)?;
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
        names = tail;
    }
    if dirs.is_empty() {
        dirs.push(cx.current_dir_id());
    }
    let mut scopes = Vec::new();
    for dir in dirs {
        scopes.extend(sources.iter().map(|&source| SourceSelection {
            dir,
            source,
            scoped,
        }));
    }
    Ok(scopes)
}

pub(super) fn source_directory(
    cx: &mut Cx<'_>,
    option: &[u8],
    value: &[u8],
) -> Result<DirId, CmdError> {
    if option == b"DIRECTORY" {
        if cx.mode() != RunMode::Script
            && let Some(dir) = find_directory(cx, value)
        {
            return Ok(dir);
        }
        return Err(CmdError::status(quoted(
            "given non-existent DIRECTORY ",
            value,
            "",
        )));
    }
    let target = find_target(cx, value)
        .ok()
        .and_then(|id| cx.project().target(id));
    target.map(|target| target.dir).ok_or_else(|| {
        CmdError::status(quoted(
            "given non-existent target for TARGET_DIRECTORY ",
            value,
            "",
        ))
    })
}

pub(super) fn find_target(cx: &mut Cx<'_>, target: &[u8]) -> Result<TargetId, CmdError> {
    let dir = cx.current_dir_id();
    cx.project().find_target(dir, target).ok_or_else(|| {
        CmdError::status(quoted(
            "could not find TARGET ",
            target,
            ".  Perhaps it has not yet been created.",
        ))
    })
}

pub(super) fn writable_target(
    cx: &mut Cx<'_>,
    name: &[u8],
    property: &[u8],
) -> Result<TargetId, CmdError> {
    let dir = cx.current_dir_id();
    if matches!(property, b"ALIASED_TARGET" | b"ALIAS_GLOBAL")
        && matches!(
            cx.project().lookup_target(dir, name),
            Some(TargetBinding::Alias(_))
        )
    {
        return Err(CmdError::status(format!(
            "can not set property {} on an ALIAS target: {}",
            BString::from(property),
            BString::from(name),
        )));
    }
    let id = find_target(cx, name)?;
    if cx.project().target(id).is_some_and(|target| {
        target
            .props
            .get(b"SYMBOLIC")
            .is_some_and(|value| rcmake_base::truth::is_on(value))
    }) {
        return Err(CmdError::status("can not be used on a SYMBOLIC target."));
    }
    Ok(id)
}

pub(super) fn alias_value(cx: &mut Cx<'_>, target: &[u8], name: &[u8]) -> Option<BString> {
    let dir = cx.current_dir_id();
    let model = cx.project();
    let TargetBinding::Alias(id) = model.lookup_target(dir, target)? else {
        return None;
    };
    match name {
        b"ALIASED_TARGET" => Some(model.target(id)?.name.clone()),
        b"ALIAS_GLOBAL" => Some(
            if model.dir(dir).local_targets.contains_key(target) {
                "FALSE"
            } else {
                "TRUE"
            }
            .into(),
        ),
        _ => None,
    }
}

pub(super) fn check_directory(cx: &mut Cx<'_>, names: &[&[u8]]) -> Result<DirId, CmdError> {
    match names {
        [] => Ok(cx.current_dir_id()),
        [name] => find_directory(cx, name).ok_or_else(|| CmdError::status(DIRECTORY_ERROR)),
        _ => Err(CmdError::status(
            "given more than one name for DIRECTORY scope.",
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rcmake_base::BString;
    use rcmake_eval::{Interp, InterpConfig, RunMode};
    use rcmake_host::{PureHost, Stream, path::to_native};
    use rcmake_model::DirId;

    use crate::test_support::TestResult;

    fn run_configure(
        files: &[(&str, &[u8])],
        script: &str,
    ) -> Result<(Interp, Arc<PureHost>, i32), Box<dyn std::error::Error>> {
        let cwd = to_native(if cfg!(windows) { b"C:/work" } else { b"/work" });
        let exe = cwd.join("cmake");
        let src = cwd.join("src");
        let build = cwd.join("build");
        let host = Arc::new(PureHost::new(cwd.clone(), exe.clone()));
        for (rel, body) in files {
            host.seed_file(&src.join(rel), body)?;
        }
        host.seed_file(&src.join("CMakeLists.txt"), script.as_bytes())?;
        let mut interp = Interp::new(
            InterpConfig::new(host.clone(), RunMode::Configure, cwd.join("data"), exe, cwd),
            crate::register_all,
        );
        let code = interp.run_configure(&src, &build, &[]).exit_code;
        Ok((interp, host, code))
    }

    #[test]
    fn extensionless_source_properties_bind_under_cmp0115_and_cmp0187() -> TestResult {
        // 1. soxr-style CMP0115 WARN: setting COMPILE_FLAGS on extensionless `pffft64s`
        //    with unique `pffft64s.c` on disk binds the source_props record to `pffft64s.c`.
        let (mut interp, host, code) = run_configure(
            &[("pffft64s.c", b"int f(void) { return 0; }\n")],
            r#"
cmake_minimum_required(VERSION 3.10)
project(SoxrLike NONE)
set_source_files_properties(pffft64s PROPERTIES COMPILE_FLAGS -mavx)
get_source_file_property(v_c pffft64s.c COMPILE_FLAGS)
get_source_file_property(v_noext pffft64s COMPILE_FLAGS)
"#,
        )?;
        assert_eq!(code, 0, "{}", host.stream_bytes(Stream::Stderr));
        assert_eq!(interp.cx().var(b"v_c"), Some(BString::from("-mavx")));
        assert_eq!(interp.cx().var(b"v_noext"), Some(BString::from("-mavx")));
        let expected_key = if cfg!(windows) {
            b"C:/work/src/pffft64s.c".as_slice()
        } else {
            b"/work/src/pffft64s.c".as_slice()
        };
        assert_eq!(
            interp
                .cx()
                .project()
                .dir(DirId::ROOT)
                .source_props
                .get(expected_key)
                .and_then(|props| props.get(b"COMPILE_FLAGS"))
                .map(|value| value.as_slice()),
            Some(b"-mavx".as_slice())
        );

        // 2. Ambiguous disk candidates (`.c` and `.cpp` both exist): do not guess eagerly;
        //    bind to the first concrete extension queried/set and reject the other.
        let (mut interp, host, code) = run_configure(
            &[
                ("pffft64s.c", b"int f(void) { return 0; }\n"),
                ("pffft64s.cpp", b"int g() { return 0; }\n"),
            ],
            r#"
cmake_minimum_required(VERSION 3.10)
project(Ambiguous NONE)
set_source_files_properties(pffft64s PROPERTIES P1 one)
get_source_file_property(v_cpp pffft64s.cpp P1)
get_source_file_property(v_c pffft64s.c P1)
"#,
        )?;
        assert_eq!(code, 0, "{}", host.stream_bytes(Stream::Stderr));
        assert_eq!(interp.cx().var(b"v_cpp"), Some(BString::from("one")));
        assert_eq!(interp.cx().var(b"v_c"), Some(BString::from("NOTFOUND")));

        // 3. CMP0115 NEW + CMP0187 WARN (CMake 3.20): target-registered `pffft64s.c`
        //    binds later extensionless `set_source_files_properties(pffft64s ...)`,
        //    and extensionless-first preserves binary-dir alternative until absolute binding.
        let (mut interp, host, code) = run_configure(
            &[("pffft64s.c", b"int f(void) { return 0; }\n")],
            r#"
cmake_minimum_required(VERSION 3.20)
project(Cmp0187Warn NONE)
add_library(soxr OBJECT pffft64s.c)
set_source_files_properties(pffft64s PROPERTIES COMPILE_FLAGS -DSIMD)
get_source_file_property(v_c pffft64s.c COMPILE_FLAGS)

set_source_files_properties(gen_only PROPERTIES TAG built)
get_source_file_property(rel_c gen_only.c TAG)
get_source_file_property(bin_c "${CMAKE_CURRENT_BINARY_DIR}/gen_only.c" TAG)
get_source_file_property(src_c "${CMAKE_CURRENT_SOURCE_DIR}/gen_only.c" TAG)
"#,
        )?;
        assert_eq!(code, 0, "{}", host.stream_bytes(Stream::Stderr));
        assert_eq!(interp.cx().var(b"v_c"), Some(BString::from("-DSIMD")));
        assert_eq!(interp.cx().var(b"rel_c"), Some(BString::from("built")));
        assert_eq!(interp.cx().var(b"bin_c"), Some(BString::from("built")));
        assert_eq!(interp.cx().var(b"src_c"), Some(BString::from("NOTFOUND")));

        // 4. Exact-match precedence: when an exact extensionless file exists on disk
        //    alongside `.c`, or both records already exist, exact matches always win.
        let (mut interp, host, code) = run_configure(
            &[("pffft64s", b"exact\n"), ("pffft64s.c", b"c\n")],
            r#"
cmake_minimum_required(VERSION 3.20)
project(ExactPrecedence NONE)
set_source_files_properties(pffft64s.c PROPERTIES ORIGIN from_c)
set_source_files_properties(pffft64s PROPERTIES ORIGIN from_exact)
get_source_file_property(v_c pffft64s.c ORIGIN)
get_source_file_property(v_noext pffft64s ORIGIN)
"#,
        )?;
        assert_eq!(code, 0, "{}", host.stream_bytes(Stream::Stderr));
        assert_eq!(interp.cx().var(b"v_c"), Some(BString::from("from_c")));
        assert_eq!(
            interp.cx().var(b"v_noext"),
            Some(BString::from("from_exact"))
        );

        // 5. Exact-extension rules (CMP0115 NEW + CMP0187 NEW, CMake 4.5):
        //    ambiguous matches are rejected in both directions, and switching policy
        //    later still preserves exact-match precedence when both records exist.
        let (mut interp, host, code) = run_configure(
            &[("pffft64s.c", b"int f(void) { return 0; }\n")],
            r#"
cmake_minimum_required(VERSION 4.5)
project(ExactRules NONE)
set_source_files_properties(pffft64s PROPERTIES P1 noext)
get_source_file_property(v_c_from_noext pffft64s.c P1)
set_source_files_properties(pffft64s.c PROPERTIES P1 with_c)
get_source_file_property(v_noext_after pffft64s P1)
get_source_file_property(v_c_after pffft64s.c P1)
cmake_policy(SET CMP0187 OLD)
get_source_file_property(v_noext_old pffft64s P1)
get_source_file_property(v_c_old pffft64s.c P1)
"#,
        )?;
        assert_eq!(code, 0, "{}", host.stream_bytes(Stream::Stderr));
        assert_eq!(
            interp.cx().var(b"v_c_from_noext"),
            Some(BString::from("NOTFOUND"))
        );
        assert_eq!(
            interp.cx().var(b"v_noext_after"),
            Some(BString::from("noext"))
        );
        assert_eq!(interp.cx().var(b"v_c_after"), Some(BString::from("with_c")));
        assert_eq!(
            interp.cx().var(b"v_noext_old"),
            Some(BString::from("noext"))
        );
        assert_eq!(interp.cx().var(b"v_c_old"), Some(BString::from("with_c")));
        Ok(())
    }
}
