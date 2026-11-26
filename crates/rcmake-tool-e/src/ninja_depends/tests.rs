use std::{path::PathBuf, time::Duration};

use rcmake_host::{Follow, Host, PureHost, Stream, path::from_native_cmake};

use super::*;

#[test]
fn scans_free_form_f_sources_and_rewrites_identical_outputs()
-> Result<(), Box<dyn std::error::Error>> {
    let cwd = PathBuf::from(if cfg!(windows) { "C:/out" } else { "/out" });
    let host = PureHost::new(cwd.clone(), cwd.join("cmake"));
    let cx = ToolCtx::for_test(&host, &cwd);
    host.seed_file(
        &cwd.join("tdi.json"),
        br#"{"_rcmake_source_formats":{"original.f":"FREE"}}"#,
    )?;
    host.seed_file(&cwd.join("original.f"), b"")?;
    host.seed_file(&cwd.join("scan.f"), b"module first; module second\n")?;
    let args = [
        "--lang=Fortran",
        "--tdi=tdi.json",
        "--src=scan.f",
        "--src-orig=original.f",
        "--out=scan.f",
        "--dep=scan.d",
        "--obj=scan.o",
        "--ddi=scan.ddi",
    ]
    .map(BString::from);
    assert_eq!(run(&args, &cx), 0, "{}", host.stream_bytes(Stream::Stderr));
    let ddi = cwd.join("scan.ddi");
    let dep = cwd.join("scan.d");
    let result = read_ddi(
        &host.read(&ddi, read_all())?,
        &rcmake_fortran::FortranCompiler::from_id(b"GNU"),
    )
    .map_err(|error| error.to_string())?;
    assert_eq!(result.scan.info.provides.len(), 2);
    assert!(result.scan.info.provides.contains(b"first".as_slice()));
    assert!(result.scan.info.provides.contains(b"second".as_slice()));
    let ddi_mtime = host.metadata(&ddi, Follow::Yes)?.mtime;
    let dep_mtime = host.metadata(&dep, Follow::Yes)?.mtime;
    host.sleep(Duration::from_secs(1));
    assert_eq!(run(&args, &cx), 0, "{}", host.stream_bytes(Stream::Stderr));
    assert!(host.metadata(&ddi, Follow::Yes)?.mtime > ddi_mtime);
    assert!(host.metadata(&dep, Follow::Yes)?.mtime > dep_mtime);
    Ok(())
}

#[test]
fn scans_fortran_and_resolves_linked_module_dyndep() -> Result<(), Box<dyn std::error::Error>> {
    let cwd = PathBuf::from(if cfg!(windows) { "C:/out" } else { "/out" });
    let host = PureHost::new(cwd.clone(), cwd.join("cmake"));
    let cx = ToolCtx::for_test(&host, &cwd);
    let root = from_native_cmake(&cwd);
    let metadata = |module: &str, linked: &str| {
        format!(
            r#"{{
                "dir-cur-bld":"{root}","dir-cur-src":"{root}/src",
                "dir-top-bld":"{root}","dir-top-src":"{root}/src",
                "module-dir":"{root}/{module}","linked-target-dirs":[{linked}],
                "compiler-id":"GNU","submodule-sep":"@","submodule-ext":".smod"
            }}"#
        )
    };
    host.seed_file(&cwd.join("lib/tdi.json"), metadata("mods", "").as_bytes())?;
    host.seed_file(
        &cwd.join("app/tdi.json"),
        metadata("app", &format!(r#""{root}/lib""#)).as_bytes(),
    )?;
    host.seed_file(&cwd.join("src/parent.f90"), b"module Parent\nend module\n")?;
    host.seed_file(
        &cwd.join("src/body.inc"),
        b"use, intrinsic :: iso_fortran_env\n",
    )?;
    host.seed_file(&cwd.join("src/sub.F90"), b"")?;
    let preprocessed = format!(
        "# 1 \"{root}/src/sub.F90\"\nsubmodule (Parent) Child\ninclude 'body.inc'\nend submodule\n"
    );
    host.seed_file(&cwd.join("app/sub-pp.f90"), preprocessed.as_bytes())?;

    for args in [
        vec![
            "--lang=Fortran",
            "--tdi=lib/tdi.json",
            "--src=src/parent.f90",
            "--out=lib/parent.ddi",
            "--dep=lib/parent.d",
            "--obj=lib/parent.o",
            "--ddi=lib/parent.ddi",
        ],
        vec![
            "--lang=Fortran",
            "--tdi=app/tdi.json",
            "--src=app/sub-pp.f90",
            "--src-orig=src/sub.F90",
            "--out=app/sub-pp.f90",
            "--dep=app/sub.d",
            "--obj=app/sub.o",
            "--ddi=app/sub.ddi",
        ],
    ] {
        let args = args.into_iter().map(BString::from).collect::<Vec<_>>();
        assert_eq!(run(&args, &cx), 0, "{}", host.stream_bytes(Stream::Stderr));
    }
    let dep = host.read(&cwd.join("app/sub.d"), read_all())?;
    assert_eq!(
        dep,
        format!("app/sub-pp.f90: \\\n {root}/src/body.inc \\\n {root}/src/sub.F90\n").as_bytes()
    );
    let ddi_path = cwd.join("app/sub.ddi");
    let ddi = host.read(&ddi_path, read_all())?;
    let ddi = read_ddi(&ddi, &rcmake_fortran::FortranCompiler::from_id(b"GNU"))
        .map_err(|error| error.to_string())?;
    assert!(ddi.scan.info.provides.contains(b"parent@child".as_slice()));
    assert!(ddi.scan.info.requires.contains(b"parent".as_slice()));
    assert!(
        !ddi.scan
            .info
            .requires
            .contains(b"iso_fortran_env".as_slice())
    );

    for (dir, unit) in [("lib", "parent"), ("app", "sub")] {
        let args = [
            "--lang=Fortran".to_owned(),
            format!("--tdi={dir}/tdi.json"),
            format!("--dd={dir}/Fortran.dd"),
            format!("{dir}/{unit}.ddi"),
        ]
        .map(BString::from);
        assert_eq!(
            dyndep::run(&args, &cx),
            0,
            "{}",
            host.stream_bytes(Stream::Stderr)
        );
    }
    assert_eq!(
        host.read(&cwd.join("app/Fortran.dd"), read_all())?,
        b"ninja_dyndep_version = 1.0\nbuild app/sub.o | app/parent@child.smod: dyndep | mods/parent.mod\n  restat = 1\n\n"
    );

    host.seed_file(
        &ddi_path,
        br#"{
            "version":1,
            "rules":[{
                "primary-output":"app/sub.o",
                "provides":[{"logical-name":"parent@child.smod","compiled-module-path":"custom/child.smod"}],
                "requires":[{"logical-name":"parent.mod"}]
            }]
        }"#,
    )?;
    let args = [
        "--lang=Fortran",
        "--tdi=app/tdi.json",
        "--dd=app/Fortran.dd",
        "app/sub.ddi",
    ]
    .map(BString::from);
    assert_eq!(
        dyndep::run(&args, &cx),
        0,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    assert_eq!(
        host.read(&cwd.join("app/Fortran.dd"), read_all())?,
        b"ninja_dyndep_version = 1.0\nbuild app/sub.o | custom/child.smod: dyndep | mods/parent.mod\n  restat = 1\n\n"
    );
    Ok(())
}
