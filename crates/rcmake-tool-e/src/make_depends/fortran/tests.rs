use rcmake_host::PureHost;

use super::*;
use crate::make_depends::write_with_fortran;
use crate::read_all;

#[test]
fn writes_fortran_make_rules_for_local_and_linked_modules() -> HostResult<()> {
    let cwd = PathBuf::from(if cfg!(windows) { "C:/out" } else { "/out" });
    let host = PureHost::new(cwd.clone(), cwd.join("cmake"));
    let provider = cwd.join("CMakeFiles/provider.dir");
    let target_dir = cwd.join("CMakeFiles/demo.dir");
    host.seed_file(
        &provider.join("fortran.internal"),
        b"# A module name from an older CMake version.\nprovides\n external\n",
    )?;
    host.seed_file(&target_dir.join("DependInfo.cmake"), b"")?;
    let provider_source = cwd.join("local.f90");
    host.seed_file(&provider_source, b"module Local\nend module\n")?;
    let consumer_source = cwd.join("consumer.f90");
    host.seed_file(
        &consumer_source,
        b"include 'deps.fh'\nuse local\nuse, intrinsic :: iso_fortran_env\n",
    )?;
    host.seed_file(&cwd.join("headers/deps.fh"), b"use external\n")?;
    let target = FortranTarget {
        build_dir: cwd.clone(),
        compiler_id: "GNU".into(),
        module_dir: Some(PathBuf::from("mods")),
        linked_info_files: vec![provider.join("DependInfo.cmake")],
        sources: vec![
            FortranSource {
                source: Source {
                    input: provider_source,
                    object: target_dir.join("local.o"),
                    include_dirs: Vec::new(),
                },
                format: None,
            },
            FortranSource {
                source: Source {
                    input: consumer_source,
                    object: target_dir.join("consumer.o"),
                    include_dirs: vec![cwd.join("headers")],
                },
                format: Some("FREE".into()),
            },
        ],
        ..FortranTarget::default()
    };
    write_with_fortran(
        &host,
        &"Unix Makefiles".into(),
        &cwd,
        &target_dir,
        &[],
        &target,
    )?;
    let rules = host.read(&target_dir.join("depend.make"), read_all())?;
    let rules = String::from_utf8_lossy(&rules);
    assert!(
        rules.contains(
            "CMakeFiles/demo.dir/consumer.o: CMakeFiles/provider.dir/external.mod.stamp\n"
        )
    );
    assert!(
        rules.contains("CMakeFiles/demo.dir/consumer.o: CMakeFiles/demo.dir/local.mod.stamp\n")
    );
    assert!(
        rules.contains(
            "cmake_copy_f90_mod mods/local.mod CMakeFiles/demo.dir/local.mod.stamp GNU\n"
        )
    );
    assert!(
        rules.contains("CMakeFiles/demo.dir/build: CMakeFiles/demo.dir/local.o.provides.build\n")
    );
    assert!(
        String::from_utf8_lossy(&host.read(&target_dir.join("depend.internal"), read_all())?)
            .contains("headers/deps.fh")
    );
    assert_eq!(
        host.read(&target_dir.join("fortran.internal"), read_all())?,
        b"# The fortran modules provided by this target.\nprovides\n local.mod\n"
    );
    assert!(
        String::from_utf8_lossy(
            &host.read(&target_dir.join("cmake_clean_Fortran.cmake"), read_all())?
        )
        .contains("\"mods/LOCAL.mod\"")
    );
    Ok(())
}
