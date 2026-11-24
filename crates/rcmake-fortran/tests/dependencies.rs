//! Tests for Fortran source scans and build dependency files.

use std::collections::{BTreeMap, BTreeSet};

use rcmake_base::{BString, Limits};
use rcmake_fortran::{
    CollateError, DependInfo, FortranCompiler, FortranSourceInfo, FortranTdi, IncludeResolver,
    ModuleCase, ObjectScan, ScanError, ScanInput, SourceForm, collate_make_depend,
    collate_ninja_dyndep, scan,
};

fn names(words: &[&str]) -> BTreeSet<BString> {
    words.iter().map(|&word| word.into()).collect()
}

fn scan_text(text: &[u8], form: SourceForm) -> Result<FortranSourceInfo, ScanError> {
    scan(
        &ScanInput::new(b"source", text, form),
        None,
        &Limits::default(),
    )
}

#[test]
fn free_form_joins_split_tokens_and_ignores_comments_and_strings() -> Result<(), ScanError> {
    let info = scan_text(
        br#"
! module comment
mod&
 &ule MiXeD; use First
  u&
! this comment has an ampersand &

  &se SPLIT_& ! comment
   &Name
  use, non_intrinsic :&
   &: EXTERNAL
  print *, "module fake; ""!"" use hidden"
  print *, 'another fake ''quote''; &
            &module hidden'
  print *, "fake"; use Real
  10 USE Last
  module procedure foo
  module function bar()
  module real function num()
  module subroutine example()
  end module
"#,
        SourceForm::Free,
    )?;
    assert_eq!(info.provides, names(&["mixed"]));
    assert_eq!(
        info.requires,
        names(&["first", "split_name", "external", "real", "last"])
    );
    Ok(())
}

#[test]
fn fixed_form_uses_statement_columns_and_column_six_continuation() -> Result<(), ScanError> {
    let mut source = b"C     USE NOT_THIS\r\nc     MODULE COMMENT\n*     USE COMMENT\n!     USE HIDDEN\n12345 MODULE THING\n      USE SECOND_\n     !HALF; USE THIRD ! use ignored\n      USE FOURTH".to_vec();
    source.resize(source.len() + 72 - b"      USE FOURTH".len(), b' ');
    source.extend_from_slice(b"; USE PAST_CUTOFF\n      INCLUDE 'a;!''b.inc'\n");
    let info = scan_text(&source, SourceForm::Fixed)?;
    assert_eq!(info.provides, names(&["thing"]));
    assert_eq!(
        info.requires,
        names(&["second_half", "third", "fourth", "past_cutoff"])
    );
    assert_eq!(info.includes, names(&["a;!'b.inc"]));
    Ok(())
}

#[test]
fn submodule_and_intrinsic_dependencies() -> Result<(), ScanError> {
    let text = b"
MODULE Parent
SUBMODULE (PARENT) Child
SUBMODULE ( Parent : CHILD ) GrandChild
USE ISO_C_BINDING
USE omp_lib
USE, INTRINSIC :: Vendor
USE, NON_INTRINSIC :: IEEE_ARITHMETIC
USE :: Actual
";
    let info = scan_text(text, SourceForm::Free)?;
    assert_eq!(
        info.provides,
        names(&["parent", "parent@child", "parent@grandchild"])
    );
    assert_eq!(
        info.requires,
        names(&[
            "parent",
            "parent@child",
            "ieee_arithmetic",
            "actual",
            "iso_c_binding",
            "omp_lib"
        ])
    );
    assert_eq!(info.intrinsic, names(&["vendor"]));

    let mut input = ScanInput::new(b"sub.f90", text, SourceForm::Free);
    input.building_intrinsic_modules = true;
    assert!(
        scan(&input, None, &Limits::default())?
            .requires
            .contains(&BString::from("vendor"))
    );
    Ok(())
}

#[test]
fn conditionals_collect_dependencies_and_preserve_split_statements() -> Result<(), ScanError> {
    let info = scan_text(
        b"
#if 0
use zero
# elif defined(FLAG)
use middle
#else
use default
# endif
use &
#ifdef FLAG
 &one
#else
 &two
#endif
use part_&
#if FLAG
&one&
#else
&two&
#endif
&_end
# include <system.inc>
#include \\\n \"quoted.inc\"
include 'file;name.inc'
",
        SourceForm::Free,
    )?;
    assert_eq!(
        info.requires,
        names(&["default", "two", "part_one_end", "part_two_end"])
    );
    assert_eq!(
        info.includes,
        names(&["system.inc", "quoted.inc", "file;name.inc"])
    );
    Ok(())
}

struct MemoryIncludes {
    files: BTreeMap<BString, BString>,
    lookups: Vec<(BString, BString, bool)>,
}

impl IncludeResolver for MemoryIncludes {
    fn read(&mut self, from: &[u8], name: &[u8], quoted: bool) -> Option<(BString, BString)> {
        self.lookups.push((from.into(), name.into(), quoted));
        self.files
            .get(name)
            .cloned()
            .map(|content| (name.into(), content))
    }
}

#[test]
fn include_cycles_are_bounded_and_invalid_bytes_are_safe() -> Result<(), ScanError> {
    let mut resolver = MemoryIncludes {
        files: BTreeMap::from([
            ("next.inc".into(), BString::from(b"use Found\n#include <main.F90>\ninclude '\xff.inc'\nsubmodule ((bad) broken\n".as_slice())),
            ("main.F90".into(), "use should_not_rescan".into()),
        ]),
        lookups: Vec::new(),
    };
    let text = b"include 'next.inc'\nmodule \xff\nuse, unknown :: ignored\ninclude 'unfinished";
    let input = ScanInput::new(b"main.F90", text, SourceForm::Free);
    let info = scan(&input, Some(&mut resolver), &Limits::default())?;
    assert_eq!(info.provides, BTreeSet::new());
    assert_eq!(info.requires, names(&["found"]));
    assert_eq!(info.resolved_includes, names(&["next.inc"]));
    assert!(
        info.includes
            .contains(&BString::from(b"\xff.inc".as_slice()))
    );
    assert!(
        resolver
            .lookups
            .iter()
            .any(|(_, name, quoted)| name == "main.F90" && !quoted)
    );
    let limits = Limits {
        max_recursion: 1,
        ..Limits::default()
    };
    assert_eq!(
        scan(&input, Some(&mut resolver), &limits),
        Err(ScanError::IncludeDepth)
    );
    let limits = Limits {
        max_file_read: 4,
        ..Limits::default()
    };
    assert_eq!(scan(&input, None, &limits), Err(ScanError::SourceBytes));
    Ok(())
}

#[test]
fn selects_source_forms_and_compiler_file_names() {
    for path in [
        b"dir/model.f".as_slice(),
        b"x.FOR",
        b"x.FTN",
        b"x.F77",
        b"x.fpp",
    ] {
        assert_eq!(SourceForm::from_path(path), Some(SourceForm::Fixed));
    }
    for path in [b"dir/model.F90".as_slice(), b"x.F95", b"x.f03", b"x.F08"] {
        assert_eq!(SourceForm::from_path(path), Some(SourceForm::Free));
    }
    for compiler in [
        b"GNU".as_slice(),
        b"Flang",
        b"LLVMFlang",
        b"NVHPC",
        b"Intel",
    ] {
        assert_eq!(
            FortranCompiler::from_id(compiler).module_filename(b"MiXeD"),
            Some("mixed.mod".into())
        );
    }
    for compiler in [b"Cray".as_slice(), b"PGI", b"Absoft"] {
        assert_eq!(
            FortranCompiler::from_id(compiler).module_filename(b"MiXeD"),
            Some("mixed.mod".into())
        );
    }
    assert_eq!(
        FortranCompiler::from_id(b"GNU").module_filename(b"Parent@Child"),
        Some("parent@child.smod".into())
    );
    assert_eq!(
        FortranCompiler::from_id(b"Intel").module_filename(b"Parent@Child"),
        Some("parent@child.smod".into())
    );
    for (id, filename) in [
        ("IntelLLVM", "parent@child.smod"),
        ("Flang", "parent-child.mod"),
        ("LLVMFlang", "parent-child.mod"),
        ("NVHPC", "parent-child.mod"),
        ("Cray", "child.mod"),
    ] {
        assert_eq!(
            FortranCompiler::from_id(id.as_bytes()).module_filename(b"Parent@Child"),
            Some(filename.into())
        );
    }
    let owned = FortranTdi {
        dir_cur_bld: "/build".into(),
        dir_top_bld: "/build".into(),
        include_dirs: vec!["/includes".into()],
        module_dir: Some("mod".into()),
        linked_target_dirs: vec![],
        compiler_id: "Custom".into(),
        submodule_sep: "__".into(),
        submodule_ext: "".into(),
        module_case: ModuleCase::Upper,
        sources: vec![],
    };
    let info = owned.as_depend_info();
    assert_eq!(info.include_dirs, owned.include_dirs);
    assert_eq!(
        info.compiler.module_filename(b"Parent@Child"),
        Some("parent__child.mod".into())
    );
    assert_eq!(info.compiler.module_filename(b"invalid@@module"), None);
}

fn object(path: &str, provides: &[&str], requires: &[&str]) -> ObjectScan {
    ObjectScan {
        object: path.into(),
        info: FortranSourceInfo {
            provides: names(provides),
            requires: names(requires),
            ..FortranSourceInfo::default()
        },
    }
}

#[test]
fn ninja_dyndep_is_sorted_and_uses_linked_target_module_paths() -> Result<(), CollateError> {
    let mut info = DependInfo::new(FortranCompiler::from_id(b"GNU"));
    info.dir_top_bld = "/build".into();
    info.dir_cur_bld = "/build/sub".into();
    info.module_dir = Some("local mods".into());
    let mut objects = [
        object(
            "/build/obj/z.o",
            &["Root@Child"],
            &["root", "Linked", "unresolved"],
        ),
        object("obj/a.o", &["Root"], &["Root"]),
        object("obj/b$: x.o", &[], &[]),
    ];
    let linked = [BTreeMap::from([(
        "linked".into(),
        "/build/custom mods/linked.mod".into(),
    )])];
    let dyndep = collate_ninja_dyndep(&objects, &info, &linked)?;
    assert_eq!(
        dyndep.text,
        concat!(
            "ninja_dyndep_version = 1.0\n",
            "build obj/a.o | sub/local$ mods/root.mod: dyndep\n  restat = 1\n\n",
            "build obj/b$$$:$ x.o: dyndep\n\n",
            "build obj/z.o | sub/local$ mods/root@child.smod: dyndep | custom$ mods/linked.mod sub/local$ mods/root.mod\n  restat = 1\n\n",
        )
    );
    assert_eq!(
        dyndep.modules,
        BTreeMap::from([
            ("root".into(), "sub/local mods/root.mod".into()),
            ("root@child".into(), "sub/local mods/root@child.smod".into()),
        ])
    );
    objects.reverse();
    assert_eq!(dyndep, collate_ninja_dyndep(&objects, &info, &linked)?);
    Ok(())
}

#[test]
fn make_depend_emits_module_producers_and_sorted_prerequisites() -> Result<(), CollateError> {
    let mut info = DependInfo::new(FortranCompiler::from_id(b"GNU"));
    info.module_dir = Some("mod dir".into());
    info.include_dirs = vec!["first".into(), "second".into()];
    let mut consumer = object("dependent $.o", &[], &["local", "remote", "vendor"]);
    consumer.info.resolved_includes = names(&["includes/foo#bar.inc"]);
    let mut objects = [object("producer.o", &["Local"], &[]), consumer];
    let linked = [BTreeMap::from([(
        "remote".into(),
        "remote/remote.mod.stamp".into(),
    )])];
    let exists = |path: &[u8]| matches!(path, b"first/VENDOR.mod" | b"second/vendor.mod");
    let stamp_dir = b"CMakeFiles/app.dir";
    let depend = collate_make_depend(&objects, &info, &linked, stamp_dir, Some(&exists))?;
    assert_eq!(
        depend.text,
        concat!(
            "dependent\\ $$.o: \\\n includes/foo\\#bar.inc\n",
            "dependent\\ $$.o: CMakeFiles/app.dir/local.mod.stamp\n",
            "dependent\\ $$.o: remote/remote.mod.stamp\n",
            "dependent\\ $$.o: first/VENDOR.mod\n",
            "producer.o.provides.build: CMakeFiles/app.dir/local.mod.stamp\n",
            "CMakeFiles/app.dir/local.mod.stamp: producer.o\n",
            "\t$(CMAKE_COMMAND) -E cmake_copy_f90_mod \"mod dir/local.mod\" CMakeFiles/app.dir/local.mod.stamp GNU\n",
            "producer.o.provides.build:\n",
            "\t$(CMAKE_COMMAND) -E touch producer.o.provides.build\n",
            "CMakeFiles/app.dir/build: producer.o.provides.build\n",
        )
    );
    assert_eq!(
        depend.modules.get(b"local".as_slice()),
        Some(&"CMakeFiles/app.dir/local.mod.stamp".into())
    );
    objects.reverse();
    assert_eq!(
        depend,
        collate_make_depend(&objects, &info, &linked, stamp_dir, Some(&exists))?
    );
    Ok(())
}

#[test]
fn collators_report_cycles_duplicates_and_invalid_paths() {
    let info = DependInfo::new(FortranCompiler::from_id(b"GNU"));
    let cycle = [
        object("a.o", &["ma"], &["mb"]),
        object("b.o", &["mb"], &["ma"]),
    ];
    assert_eq!(
        collate_ninja_dyndep(&cycle, &info, &[]),
        Err(CollateError::Cycle)
    );
    assert_eq!(
        collate_make_depend(&cycle, &info, &[], b"target.dir", None),
        Err(CollateError::Cycle)
    );
    let duplicate = [object("b.o", &["m"], &[]), object("a.o", &["M"], &[])];
    assert_eq!(
        collate_ninja_dyndep(&duplicate, &info, &[]),
        Err(CollateError::DuplicateProvider {
            module: "m".into(),
            objects: ["a.o".into(), "b.o".into()],
        })
    );
    let invalid = [object("a\n.o", &[], &[])];
    assert!(matches!(
        collate_ninja_dyndep(&invalid, &info, &[]),
        Err(CollateError::InvalidPath(_))
    ));
}

#[test]
fn fixed_form_tabs_start_statements_and_continue_tokens() -> Result<(), ScanError> {
    let info = scan_text(
        b"\tuse foo\n123\tuse bar\n\tuse long_\n\t1name\n",
        SourceForm::Fixed,
    )?;
    assert_eq!(info.requires, names(&["foo", "bar", "long_name"]));
    Ok(())
}

#[test]
fn complementary_and_unknown_conditional_providers_collate()
-> Result<(), Box<dyn std::error::Error>> {
    let defines = ["USE_MPI=1".into()];
    let mut mpi = ScanInput::new(
        b"mpi.F90",
        b"#ifdef USE_MPI\nmodule comm\n#endif",
        SourceForm::Free,
    );
    mpi.defines = &defines;
    let mut serial = ScanInput::new(
        b"serial.F90",
        b"#ifndef USE_MPI\nmodule comm\n#endif",
        SourceForm::Free,
    );
    serial.defines = &defines;
    let info = DependInfo::new(FortranCompiler::from_id(b"GNU"));
    let objects = [
        ObjectScan {
            object: "mpi.o".into(),
            info: scan(&mpi, None, &Limits::default())?,
        },
        ObjectScan {
            object: "serial.o".into(),
            info: scan(&serial, None, &Limits::default())?,
        },
        object("user.o", &[], &["comm"]),
    ];
    assert_eq!(objects[0].info.provides, names(&["comm"]));
    assert!(objects[1].info.provides.is_empty());
    assert!(
        collate_ninja_dyndep(&objects, &info, &[])?
            .text
            .windows(b"build mpi.o | comm.mod".len())
            .any(|bytes| bytes == b"build mpi.o | comm.mod")
    );

    let possible = scan_text(b"#if UNKNOWN > 1\nmodule comm\n#endif", SourceForm::Free)?;
    assert_eq!(possible.conditional_provides, names(&["comm"]));
    let objects = [
        ObjectScan {
            object: "a.o".into(),
            info: possible.clone(),
        },
        ObjectScan {
            object: "b.o".into(),
            info: possible,
        },
        object("user.o", &[], &["comm"]),
    ];
    let result = collate_ninja_dyndep(&objects, &info, &[])?;
    let text = std::str::from_utf8(&result.text)?;
    assert!(text.contains("build a.o | comm.mod"));
    assert!(text.contains("build b.o: dyndep"));
    assert!(text.contains("build user.o: dyndep | b.o comm.mod"));
    let result = collate_make_depend(&objects, &info, &[], b"target.dir", None)?;
    assert!(std::str::from_utf8(&result.text)?.contains("target.dir/comm.mod.stamp: a.o b.o"));
    Ok(())
}

#[test]
fn define_undef_and_malformed_conditionals() -> Result<(), ScanError> {
    let info = scan_text(b"#define FLAG 9\n#if defined(FLAG)\nuse yes\n#endif\n#undef FLAG\n#ifdef FLAG\nuse no\n#else\nuse fallback\n#endif", SourceForm::Free)?;
    assert_eq!(info.requires, names(&["yes", "fallback"]));
    let info = scan_text(b"#if 0\nuse a\n#else\nuse b\n", SourceForm::Free)?;
    assert_eq!(info.requires, names(&["a", "b"]));
    Ok(())
}

#[test]
fn preprocessor_definitions_follow_include_order() -> Result<(), ScanError> {
    let mut resolver = MemoryIncludes {
        files: BTreeMap::from([(
            "flags.inc".into(),
            "#ifdef BEFORE\n#define AFTER\nuse from_include\n#endif\n".into(),
        )]),
        lookups: Vec::new(),
    };
    let input = ScanInput::new(
        b"main.F90",
        b"#define BEFORE\n#include \"flags.inc\"\n#ifdef AFTER\nuse after_include\n#endif",
        SourceForm::Free,
    );
    let info = scan(&input, Some(&mut resolver), &Limits::default())?;
    assert_eq!(info.requires, names(&["from_include", "after_include"]));
    Ok(())
}
