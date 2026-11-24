//! Check free-form and fixed-form scans and the build dependency files they produce.

use std::collections::BTreeSet;

use rcmake_base::{BString, Limits};
use rcmake_fortran::{
    CollateError, DependInfo, FortranCompiler, IncludeResolver, ModuleMap, ObjectScan, ScanError,
    ScanInput, SourceForm, collate_make_depend, collate_ninja_dyndep, scan,
};

use super::{Check, mutate, pick, random_bytes, require, setup};
use crate::Rng;

struct Includes;

impl IncludeResolver for Includes {
    fn read(&mut self, _: &[u8], name: &[u8], _: bool) -> Option<(BString, BString)> {
        match name {
            b"deps.inc" | b"alias.inc" => Some((
                "deps.inc".into(),
                "#include \"alias.inc\"\n      MoDuLe Included\r\n".into(),
            )),
            _ => None,
        }
    }
}

pub(super) fn check(rng: &mut Rng, limits: &Limits) -> Check {
    let mode = rng.below(16);
    let form = *pick(rng, &[SourceForm::Free, SourceForm::Fixed]);
    let mut text = match mode {
        0 => random_bytes(rng),
        4 => b"      module ALPHA\n      use beta\n".to_vec(),
        6 => b"#include \"deps.inc\"\n".to_vec(),
        7 => b"#if CALL(A)\n#if CALL(B)\n      module alpha\n#endif\n#endif\n".to_vec(),
        _ => b"\xef\xbb\xbf      module ALPHA\r\n#include \"deps.inc\"\n".to_vec(),
    };
    if mode >= 8 {
        let continuation = match form {
            SourceForm::Free => b"use, &\n &non_intrinsic :: external\n".as_slice(),
            SourceForm::Fixed => b"      use,\n     + non_intrinsic :: external\n",
        };
        for _ in 0..rng.below(24) {
            text.extend_from_slice(pick(
                rng,
                &[
                    continuation,
                    b"      submodule (alpha:parent) child\n",
                    b"      use, intrinsic :: ISO_C_BINDING\n",
                    b"      include 'missing''file'\n",
                    b"      module procedure argument\n",
                    b"#if FEATURE(X)\n      module alt\n#else\n      use other\n#endif\n",
                ],
            ));
        }
        mutate(rng, &mut text, b"\xff\x00'!&;\r\n");
    }
    let defines = [BString::from("FEATURE=1"), BString::from(random_bytes(rng))];
    let mut input = ScanInput::new(b"a.f", &text, form);
    input.defines = &defines;
    input.implicit_intrinsics = rng.below(2) == 0;
    input.building_intrinsic_modules = rng.below(2) == 0;

    let mut limits = limits.clone();
    if mode == 5 {
        limits.max_file_read = text.len().saturating_sub(1) as u64;
    }
    if matches!(mode, 6 | 7) {
        limits.max_recursion = 1;
    }
    let scanned = scan(&input, Some(&mut Includes), &limits);
    require(
        scanned == scan(&input, Some(&mut Includes), &limits),
        || "Repeated Fortran scans returned different results.".into(),
    )?;
    if let Some(error) = match mode {
        5 => Some(ScanError::SourceBytes),
        6 => Some(ScanError::IncludeDepth),
        7 => Some(ScanError::ConditionalLimit),
        _ => None,
    } {
        require(scanned == Err(error), || {
            format!("Expected {error:?} from the Fortran scan. Got {scanned:?}.")
        })?;
    }
    let Ok(scanned) = scanned else {
        return Ok(false);
    };
    require(
        scanned.conditional_provides.is_subset(&scanned.provides)
            && scanned
                .provides
                .iter()
                .chain(&scanned.requires)
                .chain(&scanned.intrinsic)
                .all(|name| !name.is_empty() && !name.iter().any(u8::is_ascii_uppercase)),
        || "Fortran scan returned invalid module names.".into(),
    )?;
    if mode == 1 {
        require(
            scanned.provides == BTreeSet::from(["alpha".into(), "included".into()])
                && scanned.resolved_includes == BTreeSet::from(["deps.inc".into()]),
            || "Fortran scan missed a module or counted an include alias twice.".into(),
        )?;
    }

    let other_text = if mode == 3 {
        b"      module alpha\n".as_slice()
    } else {
        b"      module beta\n      use alpha\n"
    };
    let other = setup(scan(
        &ScanInput::new(b"b.f", other_text, form),
        None,
        &limits,
    ))?;
    let mut objects = [
        ObjectScan {
            object: (*pick(rng, &["a.o", "a $file.o", "a:file.o"])).into(),
            info: scanned,
        },
        ObjectScan {
            object: "b.o".into(),
            info: other,
        },
    ];
    let compiler = FortranCompiler::from_id(pick(rng, &[b"GNU".as_slice(), b"NAG", b"Cray"]));
    let info = DependInfo::new(compiler);
    let linked = [ModuleMap::from([(
        "external".into(),
        "linked/external.mod".into(),
    )])];
    let ninja = collate_ninja_dyndep(&objects, &info, &linked);
    let make = collate_make_depend(&objects, &info, &linked, b"stamps", None);
    objects.reverse();
    require(
        ninja == collate_ninja_dyndep(&objects, &info, &linked)
            && make == collate_make_depend(&objects, &info, &linked, b"stamps", None),
        || "Changing the order of object scans changed the build dependency result.".into(),
    )?;
    match mode {
        3 => require(
            matches!(ninja, Err(CollateError::DuplicateProvider { .. }))
                && matches!(make, Err(CollateError::DuplicateProvider { .. })),
            || "Collation accepted duplicate module providers.".into(),
        )?,
        4 => require(
            ninja == Err(CollateError::Cycle) && make == Err(CollateError::Cycle),
            || "Collation accepted a dependency cycle.".into(),
        )?,
        _ => {}
    }
    let provided: BTreeSet<_> = objects
        .iter()
        .flat_map(|object| &object.info.provides)
        .collect();
    for output in [&ninja, &make]
        .into_iter()
        .filter_map(|result| result.as_ref().ok())
    {
        require(output.modules.keys().eq(provided.iter().copied()), || {
            "Collation changed the set or order of exported modules.".into()
        })?;
    }
    Ok(ninja.is_ok() && make.is_ok())
}
