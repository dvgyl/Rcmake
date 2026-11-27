//! HIP architecture selection and compiler/linker flags.
#![forbid(unsafe_code)]

use rcmake_base::{BString, list, truth};
use rcmake_diag::{Diagnostic, Effect, Location, MessageKind};
use rcmake_genex::ConfigId;
use rcmake_model::{ProjectModel, Target};

use super::cuda::{
    arch::{self, ArchPhase},
    in_try_compile,
};
use super::definition;

/// Emit HIP architecture arguments and diagnostics for one build phase.
///
/// Mirrors `cmGeneratorTarget::AddHIPArchitectureFlags`:
/// - Empty `HIP_ARCHITECTURES` is a fatal generation error (outside `try_compile`
///   when the property was not explicitly set).
/// - A false constant (`OFF`, `FALSE`, etc.) suppresses architecture flags.
/// - `CMAKE_HIP_PLATFORM == "nvidia"` reuses the CUDA architecture grammar with
///   HIP-derived compiler and architecture variables.
/// - `CMAKE_HIP_PLATFORM == "spirv"` relies on the chipStar package for targeting.
/// - Otherwise (AMD/ROCm), emits ordered `--offload-arch=<item>` options preserving
///   target feature suffixes such as `gfx90a:xnack+`.
pub(crate) fn architecture_flags(
    model: &ProjectModel,
    target: &Target,
    phase: ArchPhase,
    config: &ConfigId,
    in_try_compile: bool,
) -> (Vec<BString>, Vec<Diagnostic>) {
    let raw = target.props.get(b"HIP_ARCHITECTURES");
    let arch = raw.map_or(b"".as_slice(), |value| value.as_slice());
    if arch.is_empty() {
        if in_try_compile && raw.is_none() {
            return (Vec::new(), Vec::new());
        }
        return (
            Vec::new(),
            vec![Diagnostic {
                annotations: None,
                kind: MessageKind::Error,
                effect: Effect::Fatal,
                category: None,
                policy: None,
                location: Location::Backtrace(target.backtrace.clone()),
                body: format!("HIP_ARCHITECTURES is empty for target \"{}\".", target.name).into(),
            }],
        );
    }
    if truth::is_off(arch) {
        return (Vec::new(), Vec::new());
    }
    let platform = definition(model, target.dir, b"CMAKE_HIP_PLATFORM")
        .map_or(b"".as_slice(), |v| v.as_slice());
    if platform == b"nvidia" {
        return arch::architecture_flags_for(model, target, b"HIP", phase, config, in_try_compile);
    }
    if platform == b"spirv" {
        return (Vec::new(), Vec::new());
    }
    let flags = list::split(arch, list::Empty::Drop)
        .into_iter()
        .map(|option| {
            [b"--offload-arch=".as_slice(), option.as_slice()]
                .concat()
                .into()
        })
        .collect();
    (flags, Vec::new())
}

pub(crate) fn compile_flags(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
) -> Vec<BString> {
    architecture_flags(
        model,
        target,
        ArchPhase::Compile,
        config,
        in_try_compile(model),
    )
    .0
}

pub(crate) fn link_flags(model: &ProjectModel, target: &Target, config: &ConfigId) -> Vec<BString> {
    architecture_flags(
        model,
        target,
        ArchPhase::Link,
        config,
        in_try_compile(model),
    )
    .0
}

#[cfg(test)]
mod tests {
    use rcmake_diag::{Backtrace, PolicyMap};
    use rcmake_genex::{EvalContext, UsageTraversal};
    use rcmake_model::{
        BT, DirId, ImportedVisibility, LinkItem, TargetKind, TargetOrigin, UsageField, UsageScope,
    };

    use super::*;
    use crate::usage::{ResolvedLinkKind, UsageResolver};

    fn hip_model(platform: &str) -> ProjectModel {
        let mut model = ProjectModel::default();
        let root = model.dir_mut(DirId::ROOT);
        root.source_dir = "/src".into();
        root.binary_dir = "/build".into();
        root.vars
            .insert("CMAKE_HIP_PLATFORM".into(), platform.into());
        root.vars
            .insert("CMAKE_HIP_SOURCE_FILE_EXTENSIONS".into(), "hip".into());
        model
            .global
            .props
            .set("ENABLED_LANGUAGES".into(), "CXX;HIP".into());
        model
    }

    fn make_target(name: &str, kind: TargetKind, origin: TargetOrigin) -> Target {
        Target::new(
            name.into(),
            kind,
            origin,
            DirId::ROOT,
            Backtrace::default(),
            PolicyMap::default(),
        )
    }

    #[test]
    fn amd_ordered_architectures_and_feature_suffixes() {
        let model = hip_model("amd");
        let mut target = make_target("kernel", TargetKind::StaticLibrary, TargetOrigin::Normal);
        target
            .props
            .set("HIP_ARCHITECTURES".into(), "gfx90a:xnack+;gfx1100".into());
        let (flags, diags) = architecture_flags(
            &model,
            &target,
            ArchPhase::Compile,
            &ConfigId::default(),
            false,
        );
        assert!(diags.is_empty());
        assert_eq!(
            flags,
            vec![
                BString::from("--offload-arch=gfx90a:xnack+"),
                BString::from("--offload-arch=gfx1100"),
            ]
        );
    }

    #[test]
    fn empty_hip_architectures_is_fatal_and_off_suppresses() {
        let model = hip_model("amd");
        let mut target = make_target("kernel", TargetKind::StaticLibrary, TargetOrigin::Normal);
        let (flags, diags) = architecture_flags(
            &model,
            &target,
            ArchPhase::Compile,
            &ConfigId::default(),
            false,
        );
        assert!(flags.is_empty());
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].kind, MessageKind::Error);
        assert!(diags[0].body.starts_with(b"HIP_ARCHITECTURES is empty"));

        target.props.set("HIP_ARCHITECTURES".into(), "OFF".into());
        let (flags, diags) = architecture_flags(
            &model,
            &target,
            ArchPhase::Compile,
            &ConfigId::default(),
            false,
        );
        assert!(flags.is_empty());
        assert!(diags.is_empty());
    }

    #[test]
    fn spirv_uses_package_targeting_and_nvidia_reuses_cuda_grammar() {
        let spirv_model = hip_model("spirv");
        let mut target = make_target("kernel", TargetKind::StaticLibrary, TargetOrigin::Normal);
        target
            .props
            .set("HIP_ARCHITECTURES".into(), "generic".into());
        let (flags, diags) = architecture_flags(
            &spirv_model,
            &target,
            ArchPhase::Compile,
            &ConfigId::default(),
            false,
        );
        assert!(flags.is_empty());
        assert!(diags.is_empty());

        let mut nv_model = hip_model("nvidia");
        {
            let vars = &mut nv_model.dir_mut(DirId::ROOT).vars;
            vars.insert("CMAKE_HIP_COMPILER_ID".into(), "NVIDIA".into());
            vars.insert("CMAKE_HIP_COMPILER_VERSION".into(), "12.6".into());
        }
        target
            .props
            .set("HIP_ARCHITECTURES".into(), "75-real;80-virtual".into());
        let (flags, diags) = architecture_flags(
            &nv_model,
            &target,
            ArchPhase::Compile,
            &ConfigId::default(),
            false,
        );
        assert!(diags.is_empty());
        assert_eq!(
            flags,
            vec![
                BString::from("--generate-code=arch=compute_75,code=[sm_75]"),
                BString::from("--generate-code=arch=compute_80,code=[compute_80]"),
            ]
        );
    }

    #[test]
    fn audit_12_hip_runtime_targets_propagate_usage_and_transitive_libs()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut model = hip_model("amd");
        {
            let vars = &mut model.dir_mut(DirId::ROOT).vars;
            vars.insert("CMAKE_HIP_RUNTIME_LIBRARY_DEFAULT".into(), "SHARED".into());
            vars.insert(
                "CMAKE_HIP_RUNTIME_LIBRARIES_SHARED".into(),
                "hip::device".into(),
            );
        }

        // Imported transitive library hip::host
        let mut hip_host = make_target(
            "hip::host",
            TargetKind::SharedLibrary,
            TargetOrigin::Imported(ImportedVisibility::Global),
        );
        hip_host.props.set(
            "IMPORTED_LOCATION".into(),
            "/fake/rocm/lib/libamdhip64.so".into(),
        );
        model.add_target(hip_host)?;

        // Imported runtime target hip::device injecting include dir, define, and transitive hip::host
        let mut hip_device = make_target(
            "hip::device",
            TargetKind::InterfaceLibrary,
            TargetOrigin::Imported(ImportedVisibility::Global),
        );
        hip_device.usage.push(
            UsageField::InterfaceIncludeDirectories,
            BT::new("/fake/rocm/include".into(), Backtrace::default()),
        );
        hip_device.usage.push(
            UsageField::InterfaceCompileDefinitions,
            BT::new("HIP_RUNTIME_INJECTED=1".into(), Backtrace::default()),
        );
        hip_device
            .usage
            .interface_link_libraries
            .get_or_insert_with(Vec::new)
            .push(LinkItem {
                opens_lookup_scope: false,
                closes_lookup_scope: false,
                scope: Some(UsageScope::Interface),
                text: BT::new("hip::host".into(), Backtrace::default()),
                lookup_dir: DirId::ROOT,
            });
        model.add_target(hip_device)?;

        // Static HIP library compiling kernel.hip without explicitly linking hip::device
        let mut hip_static = make_target(
            "hip_static",
            TargetKind::StaticLibrary,
            TargetOrigin::Normal,
        );
        hip_static
            .sources
            .push(BT::new("kernel.hip".into(), Backtrace::default()));
        let hip_static_id = model.add_target(hip_static)?;

        // C++ executable linking hip_static
        let mut cpp_app = make_target("cpp_app", TargetKind::Executable, TargetOrigin::Normal);
        cpp_app
            .sources
            .push(BT::new("main.cpp".into(), Backtrace::default()));
        cpp_app
            .usage
            .link_libraries
            .get_or_insert_with(Vec::new)
            .push(LinkItem {
                opens_lookup_scope: false,
                closes_lookup_scope: false,
                scope: Some(UsageScope::Private),
                text: BT::new("hip_static".into(), Backtrace::default()),
                lookup_dir: DirId::ROOT,
            });
        let cpp_app_id = model.add_target(cpp_app)?;

        let mut usage = UsageResolver::new(&model);

        // 1. HIP static library receives hip::device include dirs and compile defs during HIP compile.
        let hip_compile_ctx = EvalContext {
            language: Some("HIP".into()),
            traversal: UsageTraversal::Compile,
            ..EvalContext::default()
        };
        let includes = usage.collect_includes(hip_static_id, &hip_compile_ctx)?;
        assert!(
            includes
                .iter()
                .any(|inc| inc.path.value == "/fake/rocm/include"),
            "expected hip::device include dir on hip_static, got {includes:?}"
        );
        let defs = usage.collect(
            hip_static_id,
            UsageField::CompileDefinitions,
            &hip_compile_ctx,
        )?;
        assert!(
            defs.entries
                .iter()
                .any(|def| def.value == "HIP_RUNTIME_INJECTED=1"),
            "expected hip::device compile define on hip_static, got {:?}",
            defs.entries
        );

        // 2. C++ consumer linking static HIP library inherits hip::device -> hip::host in link order.
        let link_ctx = EvalContext {
            link_language: Some("CXX".into()),
            traversal: UsageTraversal::Link,
            ..EvalContext::default()
        };
        let link_items = usage.collect_link_items(cpp_app_id, &link_ctx)?;
        let linked_names: Vec<BString> = link_items
            .iter()
            .filter_map(|item| match item.kind {
                ResolvedLinkKind::Library(id) | ResolvedLinkKind::Interface(id) => {
                    model.target(id).map(|t| t.name.clone())
                }
                _ => None,
            })
            .collect();
        assert!(
            linked_names.iter().any(|name| name == "hip::host"),
            "expected transitive hip::host in cpp_app link items, got {linked_names:?}"
        );
        Ok(())
    }

    #[test]
    fn host_link_launcher_paths_with_spaces_are_quoted() {
        let mut model = hip_model("nvidia");
        {
            let vars = &mut model.dir_mut(DirId::ROOT).vars;
            vars.insert(
                "CMAKE_HIP_HOST_LINK_LAUNCHER".into(),
                "/opt/my tools/hip-host-link".into(),
            );
            vars.insert(
                "CMAKE_CUDA_HOST_LINK_LAUNCHER".into(),
                "/opt/my tools/cuda-host-link".into(),
            );
        }
        let hip_inv = crate::lower::shell::rule_invocations(
            &model,
            DirId::ROOT,
            b"<CMAKE_HIP_HOST_LINK_LAUNCHER> -o out",
            |_| None,
        );
        assert_eq!(hip_inv.len(), 1);
        assert_eq!(
            hip_inv[0].argv.first().map(|a| a.as_slice()),
            Some(b"/opt/my tools/hip-host-link".as_slice())
        );

        let cuda_inv = crate::lower::shell::rule_invocations(
            &model,
            DirId::ROOT,
            b"<CMAKE_CUDA_HOST_LINK_LAUNCHER> -o out",
            |_| None,
        );
        assert_eq!(cuda_inv.len(), 1);
        assert_eq!(
            cuda_inv[0].argv.first().map(|a| a.as_slice()),
            Some(b"/opt/my tools/cuda-host-link".as_slice())
        );
    }

    #[test]
    fn genex_selected_hip_source_scopes_runtime_to_hip_group_and_propagates_to_cpp_consumer()
    -> Result<(), Box<dyn std::error::Error>> {
        use rcmake_host::{Host, PureHost, WriteMode};
        use rcmake_model::GeneratorKind;

        let host = PureHost::default();
        host.create_dir_all(std::path::Path::new("/src"))?;
        host.create_dir_all(std::path::Path::new("/fake/rocm/runtime_inc"))?;
        for file in ["kernel.hip", "host.cpp", "consumer.cpp"] {
            host.write(
                &std::path::Path::new("/src").join(file),
                b"",
                WriteMode::Truncate,
            )?;
        }

        let mut model = hip_model("amd");
        model.generator = Some(GeneratorKind::NinjaMultiConfig);
        {
            let vars = &mut model.dir_mut(DirId::ROOT).vars;
            vars.insert("CMAKE_CXX_COMPILER".into(), "/usr/bin/c++".into());
            vars.insert("CMAKE_CXX_SOURCE_FILE_EXTENSIONS".into(), "cpp".into());
            vars.insert("CMAKE_HIP_COMPILER".into(), "/rocm/bin/clang++".into());
            vars.insert("CMAKE_HIP_RUNTIME_LIBRARY_DEFAULT".into(), "SHARED".into());
            vars.insert(
                "CMAKE_HIP_RUNTIME_LIBRARIES_SHARED".into(),
                "hip::device".into(),
            );
        }

        let mut hip_host = make_target(
            "hip::host",
            TargetKind::SharedLibrary,
            TargetOrigin::Imported(ImportedVisibility::Global),
        );
        hip_host.props.set(
            "IMPORTED_LOCATION".into(),
            "/fake/rocm/lib/libamdhip64.so".into(),
        );
        model.add_target(hip_host)?;

        let mut hip_device = make_target(
            "hip::device",
            TargetKind::InterfaceLibrary,
            TargetOrigin::Imported(ImportedVisibility::Global),
        );
        hip_device.usage.push(
            UsageField::InterfaceIncludeDirectories,
            BT::new("/fake/rocm/runtime_inc".into(), Backtrace::default()),
        );
        hip_device.usage.push(
            UsageField::InterfaceCompileDefinitions,
            BT::new("HIP_RUNTIME_GENEX=1".into(), Backtrace::default()),
        );
        hip_device
            .usage
            .interface_link_libraries
            .get_or_insert_with(Vec::new)
            .push(LinkItem {
                opens_lookup_scope: false,
                closes_lookup_scope: false,
                scope: Some(UsageScope::Interface),
                text: BT::new("hip::host".into(), Backtrace::default()),
                lookup_dir: DirId::ROOT,
            });
        model.add_target(hip_device)?;

        let mut hiplib = make_target("hiplib", TargetKind::StaticLibrary, TargetOrigin::Normal);
        hiplib
            .props
            .set("HIP_ARCHITECTURES".into(), "gfx90a".into());
        hiplib
            .sources
            .push(BT::new("host.cpp".into(), Backtrace::default()));
        hiplib.sources.push(BT::new(
            "$<$<CONFIG:Debug>:kernel.hip>".into(),
            Backtrace::default(),
        ));
        let hiplib_id = model.add_target(hiplib)?;

        let mut consumer = make_target("consumer", TargetKind::Executable, TargetOrigin::Normal);
        consumer
            .sources
            .push(BT::new("consumer.cpp".into(), Backtrace::default()));
        consumer
            .usage
            .link_libraries
            .get_or_insert_with(Vec::new)
            .push(LinkItem {
                opens_lookup_scope: false,
                closes_lookup_scope: false,
                scope: Some(UsageScope::Private),
                text: BT::new("hiplib".into(), Backtrace::default()),
                lookup_dir: DirId::ROOT,
            });
        let consumer_id = model.add_target(consumer)?;

        let result = crate::resolve(
            &model,
            &host,
            &[ConfigId("Debug".into()), ConfigId("Release".into())],
        )?;

        // In Debug: hiplib's HIP compile group receives hip::device includes/defines,
        // its CXX compile group does NOT, and consumer links libamdhip64.so.
        let dbg_lib = result
            .targets
            .iter()
            .find(|t| t.id == hiplib_id && t.config == ConfigId("Debug".into()))
            .ok_or("missing Debug hiplib")?;
        let dbg_hip_group = dbg_lib
            .compile_groups
            .iter()
            .find(|g| g.language == "HIP")
            .ok_or("missing Debug HIP compile group")?;
        assert!(
            dbg_hip_group
                .includes
                .iter()
                .any(|inc| inc.path.value == "/fake/rocm/runtime_inc"),
            "HIP compile group missing runtime include: {:?}",
            dbg_hip_group.includes
        );
        assert!(
            dbg_hip_group
                .defines
                .iter()
                .any(|def| def.value == "HIP_RUNTIME_GENEX=1"),
            "HIP compile group missing runtime define: {:?}",
            dbg_hip_group.defines
        );
        let dbg_cxx_group = dbg_lib
            .compile_groups
            .iter()
            .find(|g| g.language == "CXX")
            .ok_or("missing Debug CXX compile group")?;
        assert!(
            !dbg_cxx_group
                .includes
                .iter()
                .any(|inc| inc.path.value == "/fake/rocm/runtime_inc"),
            "CXX compile group should not receive HIP runtime include: {:?}",
            dbg_cxx_group.includes
        );
        assert!(
            !dbg_cxx_group
                .defines
                .iter()
                .any(|def| def.value == "HIP_RUNTIME_GENEX=1"),
            "CXX compile group should not receive HIP runtime define: {:?}",
            dbg_cxx_group.defines
        );
        let dbg_consumer = result
            .targets
            .iter()
            .find(|t| t.id == consumer_id && t.config == ConfigId("Debug".into()))
            .ok_or("missing Debug consumer")?;
        let dbg_link = dbg_consumer.link.as_ref().ok_or("missing Debug link")?;
        assert!(
            dbg_link
                .fragments
                .iter()
                .any(|frag| frag.text.value == "/fake/rocm/lib/libamdhip64.so"),
            "Debug consumer missing transitive hip::host: {:?}",
            dbg_link.fragments
        );

        // In Release: kernel.hip is not selected, so no HIP group and no hip::host on consumer.
        let rel_lib = result
            .targets
            .iter()
            .find(|t| t.id == hiplib_id && t.config == ConfigId("Release".into()))
            .ok_or("missing Release hiplib")?;
        assert!(
            !rel_lib.compile_groups.iter().any(|g| g.language == "HIP"),
            "Release should have no HIP compile group"
        );
        let rel_consumer = result
            .targets
            .iter()
            .find(|t| t.id == consumer_id && t.config == ConfigId("Release".into()))
            .ok_or("missing Release consumer")?;
        let rel_link = rel_consumer.link.as_ref().ok_or("missing Release link")?;
        assert!(
            !rel_link
                .fragments
                .iter()
                .any(|frag| frag.text.value == "/fake/rocm/lib/libamdhip64.so"),
            "Release consumer should not link hip::host: {:?}",
            rel_link.fragments
        );
        Ok(())
    }
}
