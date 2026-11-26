use rcmake_diag::PolicyMap;
use rcmake_host::Stream;
use rcmake_model::GeneratorKind;

use super::*;

#[test]
fn parallel_resolution_matches_serial_across_64_target_dag() -> TestResult {
    let mut model = project();
    model.generator = Some(GeneratorKind::Ninja);
    let mut files = Vec::new();
    for i in 0..64usize {
        let name = format!("target_{i:02}");
        let src_a = format!("src_{i:02}_a.cpp");
        let src_b = format!("src_{i:02}_b.c");
        files.push(format!("/src/{src_a}"));
        files.push(format!("/src/{src_b}"));
        let kind = match i {
            62 | 63 => TargetKind::Executable,
            n if n % 5 == 0 => TargetKind::SharedLibrary,
            _ => TargetKind::StaticLibrary,
        };
        let mut raw = target(&name, kind, &[&src_a, &src_b]);
        raw.usage.push(
            UsageField::InterfaceCompileDefinitions,
            entry(&format!("DEF_{i:02}=$<CONFIG>")),
        );
        raw.usage.push(
            UsageField::InterfaceIncludeDirectories,
            entry(&format!("/src/include_{i:02}")),
        );
        raw.usage.push(
            UsageField::InterfaceCompileOptions,
            entry(&format!("$<$<COMPILE_LANGUAGE:CXX>:-DFOR_CXX_{i:02}>")),
        );
        if i > 0 {
            let mut links = vec![link_item(&format!("target_{:02}", i - 1))];
            if i >= 4 {
                links.push(link_item(&format!("target_{:02}", i / 2)));
            }
            if kind != TargetKind::Executable {
                raw.usage.interface_link_libraries = Some(links.clone());
            }
            raw.usage.link_libraries = Some(links);
        }
        model.add_target(raw)?;
    }

    let make_host = || -> Result<PureHost, Box<dyn std::error::Error>> {
        let host = PureHost::default();
        for file in &files {
            host.seed_file(Path::new(file), b"int f(void) { return 0; }\n")?;
        }
        Ok(host)
    };
    let serial_host = make_host()?;
    let parallel_host = make_host()?;
    let configs = [ConfigId("Release".into())];
    let options = ResolveOptions {
        provenance: true,
        ..ResolveOptions::default()
    };

    let serial =
        crate::pipeline::resolve_with_workers(&model, &serial_host, &configs, &options, true, 1)?;
    let parallel =
        crate::pipeline::resolve_with_workers(&model, &parallel_host, &configs, &options, true, 4)?;

    assert_eq!(serial.graphs, parallel.graphs);
    assert_eq!(serial.compile_inputs, parallel.compile_inputs);
    assert_eq!(serial.metadata, parallel.metadata);
    assert_eq!(
        format!("{:?}", serial.targets),
        format!("{:?}", parallel.targets)
    );
    assert_eq!(
        format!("{:?}", serial.install),
        format!("{:?}", parallel.install)
    );
    assert_eq!(
        format!("{:?}", serial.exports),
        format!("{:?}", parallel.exports)
    );
    assert_eq!(
        format!("{:?}", serial.provenance_entries),
        format!("{:?}", parallel.provenance_entries)
    );
    assert_eq!(
        format!("{:?}", serial.chains),
        format!("{:?}", parallel.chains)
    );
    assert_eq!(
        serial_host.stream_bytes(Stream::Stderr),
        parallel_host.stream_bytes(Stream::Stderr)
    );
    Ok(())
}

#[test]
fn parallel_fatal_middle_target_suppresses_later_diagnostics() -> TestResult {
    let mut model = project();
    model.generator = Some(GeneratorKind::Ninja);
    let mut policies = PolicyMap::default();
    policies.set(PolicyId::new(115), PolicyStatus::Warn)?;
    model.dir_mut(DirId::ROOT).final_policies = Some(policies);

    let mut files = Vec::new();
    for i in 0..24usize {
        let name = format!("t_{i:02}");
        let source = match i {
            5 => "warn_before".to_string(),
            10 => "fatal_missing_10.c".to_string(),
            15 => "warn_after".to_string(),
            _ => format!("ok_{i:02}.c"),
        };
        if i != 10 {
            let disk = match i {
                5 => "/src/warn_before.c".to_string(),
                15 => "/src/warn_after.c".to_string(),
                _ => format!("/src/ok_{i:02}.c"),
            };
            files.push(disk);
        }
        let mut raw = target(&name, TargetKind::StaticLibrary, &[]);
        raw.sources
            .push(BT::new(source.into(), backtrace(10 + i as u32)));
        model.add_target(raw)?;
    }

    let make_host = || -> Result<PureHost, Box<dyn std::error::Error>> {
        let host = PureHost::default();
        for file in &files {
            host.seed_file(Path::new(file), b"int x = 0;\n")?;
        }
        Ok(host)
    };
    let serial_host = make_host()?;
    let parallel_host = make_host()?;
    let configs = [ConfigId("Debug".into())];
    let options = ResolveOptions::default();

    let serial_err =
        crate::pipeline::resolve_with_workers(&model, &serial_host, &configs, &options, false, 1)
            .expect_err("Expected fatal missing source error in serial mode.");
    let parallel_err =
        crate::pipeline::resolve_with_workers(&model, &parallel_host, &configs, &options, false, 4)
            .expect_err("Expected fatal missing source error in parallel mode.");

    assert_eq!(serial_err, parallel_err);
    let serial_stderr = serial_host.stream_bytes(Stream::Stderr).to_string();
    let parallel_stderr = parallel_host.stream_bytes(Stream::Stderr).to_string();
    assert_eq!(serial_stderr, parallel_stderr);
    assert!(
        parallel_stderr.contains("warn_before.c"),
        "expected warning before fatal target: {parallel_stderr}"
    );
    assert!(
        !parallel_stderr.contains("warn_after.c"),
        "warning after fatal target must be suppressed: {parallel_stderr}"
    );
    Ok(())
}

#[test]
fn parallel_cross_target_once_only_diagnostics_deduplicate_deterministically() -> TestResult {
    let mut model = project();
    model.generator = Some(GeneratorKind::Ninja);
    let mut policies = PolicyMap::default();
    policies.set(PolicyId::new(115), PolicyStatus::Warn)?;
    model.dir_mut(DirId::ROOT).final_policies = Some(policies);

    let mut files = vec!["/src/shared_ext.c".to_string()];
    for i in 0..24usize {
        let name = format!("t_{i:02}");
        let mut raw = target(&name, TargetKind::StaticLibrary, &[]);
        let source = match i {
            3 | 12 | 20 => "shared_ext".to_string(),
            4 | 13 | 21 => "shared_missing.c".to_string(),
            18 => "unique_missing.c".to_string(),
            _ => {
                files.push(format!("/src/src_{i:02}.c"));
                format!("src_{i:02}.c")
            }
        };
        raw.sources
            .push(BT::new(source.into(), backtrace(20 + i as u32)));
        model.add_target(raw)?;
    }

    let make_host = || -> Result<PureHost, Box<dyn std::error::Error>> {
        let host = PureHost::default();
        for file in &files {
            host.seed_file(Path::new(file), b"int x = 0;\n")?;
        }
        Ok(host)
    };
    let serial_host = make_host()?;
    let parallel_host = make_host()?;
    let configs = [ConfigId("Debug".into())];
    let options = ResolveOptions::default();

    let serial_err =
        crate::pipeline::resolve_with_workers(&model, &serial_host, &configs, &options, true, 1)
            .expect_err("Expected collected missing source error in serial mode.");
    let parallel_err =
        crate::pipeline::resolve_with_workers(&model, &parallel_host, &configs, &options, true, 4)
            .expect_err("Expected collected missing source error in parallel mode.");

    assert_eq!(serial_err, parallel_err);
    let serial_stderr = serial_host.stream_bytes(Stream::Stderr).to_string();
    let parallel_stderr = parallel_host.stream_bytes(Stream::Stderr).to_string();
    assert_eq!(serial_stderr, parallel_stderr);
    assert_eq!(parallel_stderr.matches("shared_ext.c").count(), 1);
    assert_eq!(parallel_stderr.matches("shared_missing.c").count(), 1);
    let first = parallel_stderr
        .find("shared_missing.c")
        .ok_or("missing shared_missing.c diagnostic")?;
    let second = parallel_stderr
        .find("unique_missing.c")
        .ok_or("missing unique_missing.c diagnostic")?;
    assert!(first < second, "{parallel_stderr}");
    Ok(())
}
