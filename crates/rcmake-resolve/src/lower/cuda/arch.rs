//! CUDA architecture selection and compiler flags.

use rcmake_base::{BString, list, truth, version};
use rcmake_diag::{
    DiagCategory, Diagnostic, Effect, Location, MessageKind, PolicyId, PolicyStatus,
};
use rcmake_genex::ConfigId;
use rcmake_model::{ProjectModel, Target};

use crate::lower::{definition, policy_flags::Flags};
use crate::usage::link_options::shell_arguments;

/// Architecture selection before compiler-specific expansion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ArchSpec {
    /// The property is absent or empty.
    Empty,
    /// A CMake false constant disables architecture flags.
    Off,
    /// Select all supported architectures.
    All,
    /// Select the supported major architectures.
    AllMajor,
    /// Select the architectures detected on the build host.
    Native,
    /// Select these architectures in the given order.
    List(Vec<CudaArch>),
}

/// A CUDA architecture and the code to generate for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CudaArch {
    /// Architecture name, including an optional architecture family suffix.
    pub(crate) name: BString,
    /// Generate real GPU code.
    pub(crate) real: bool,
    /// Generate virtual PTX code.
    pub(crate) virtual_: bool,
}

/// The phase that consumes CUDA architecture flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArchPhase {
    /// Compile a translation unit.
    Compile,
    /// Link host or device code.
    Link,
}

/// Read the architecture property that target creation initialized.
///
/// Validate the directory variable separately before compiler detection. A later
/// assignment to that variable cannot change this target. Rcmake checks the syntax
/// of an explicit target property before it passes architecture names to the compiler.
pub(crate) fn target_architectures(target: &Target) -> Result<ArchSpec, BString> {
    let value = target.props.get(b"CUDA_ARCHITECTURES");
    parse_architectures(value.map_or(b"".as_slice(), |value| value.as_slice())).map_err(|error| {
        format!(
            "Invalid CUDA_ARCHITECTURES property for target \"{}\". {error}",
            target.name
        )
        .into()
    })
}

/// Parse a CUDA architecture property.
pub(crate) fn parse_architectures(value: &[u8]) -> Result<ArchSpec, BString> {
    Ok(match value {
        b"" => ArchSpec::Empty,
        b"all" => ArchSpec::All,
        b"all-major" => ArchSpec::AllMajor,
        b"native" => ArchSpec::Native,
        value if truth::is_off(value) => ArchSpec::Off,
        value => ArchSpec::List(parse_list(value)?),
    })
}

/// Validate the variable form before compiler detection.
pub(crate) fn validate_architecture_variable(value: &[u8]) -> Result<(), BString> {
    if value.is_empty() {
        return Err("CMAKE_CUDA_ARCHITECTURES must be non-empty if set.".into());
    }
    if value.contains(&b'"') {
        return Err(format!(
            "CMAKE_CUDA_ARCHITECTURES contains literal quotes:\n  {}\n",
            BString::from(value)
        )
        .into());
    }
    let valid = match parse_architectures(value) {
        Ok(ArchSpec::List(archs)) => {
            value.first().is_some_and(u8::is_ascii_digit) && !archs.is_empty()
        }
        Ok(_) => true,
        Err(_) => false,
    };
    if valid {
        return Ok(());
    }
    Err(format!(
        "CMAKE_CUDA_ARCHITECTURES:\n  {}\nis not one of the following:\n  * a semicolon-separated list of architecture numbers, such as 75, 90a, or 100f\n    Each number can end in 'a' or 'f' and can use '-real' or '-virtual'.\n  * a special value: all, all-major, native, or a false value such as OFF\n",
        BString::from(value)
    )
    .into())
}

fn parse_list(value: &[u8]) -> Result<Vec<CudaArch>, BString> {
    list::split(value, list::Empty::Drop)
        .into_iter()
        .map(|entry| {
            let mut parts = entry.splitn(2, |&byte| byte == b'-');
            let name = parts.next().unwrap_or_default();
            let suffix = parts.next();
            let (real, virtual_) = match suffix {
                None => (true, true),
                Some(b"real") => (true, false),
                Some(b"virtual") => (false, true),
                Some(suffix) => {
                    return Err(format!(
                        "Unknown CUDA architecture specifier \"{}\".",
                        BString::from(suffix)
                    )
                    .into());
                }
            };
            let digits = match name.split_last() {
                Some((b'a' | b'f', digits)) => digits,
                _ => name,
            };
            if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
                return Err(
                    format!("Invalid CUDA architecture \"{}\".", BString::from(name)).into(),
                );
            }
            Ok(CudaArch {
                name: name.into(),
                real,
                virtual_,
            })
        })
        .collect()
}

/// Expand a named selection to individual architectures for an explicit device link.
///
/// For Clang 19, use the same architectures as compilation. CMake 4.5.1 can
/// pass a selection name such as `all` to nvlink and request `sm_all.cubin`.
/// nvlink requires a GPU architecture such as `sm_75`.
pub(crate) fn expand_architectures(
    model: &ProjectModel,
    target: &Target,
    spec: &ArchSpec,
) -> Result<Vec<CudaArch>, BString> {
    let key: &[u8] = match spec {
        ArchSpec::Empty | ArchSpec::Off => return Ok(Vec::new()),
        ArchSpec::List(list) => return Ok(list.clone()),
        ArchSpec::All => b"CMAKE_CUDA_ARCHITECTURES_ALL",
        ArchSpec::AllMajor => b"CMAKE_CUDA_ARCHITECTURES_ALL_MAJOR",
        ArchSpec::Native => b"CMAKE_CUDA_ARCHITECTURES_NATIVE",
    };
    let values =
        definition(model, target.dir, key).map_or(b"".as_slice(), |value| value.as_slice());
    if matches!(spec, ArchSpec::Native) && values.is_empty() {
        return Err(
            "CUDA_ARCHITECTURES is set to \"native\", but no NVIDIA GPU was detected.".into(),
        );
    }
    parse_list(values)
}

/// Emit CUDA architecture arguments and diagnostics for one phase.
///
/// A fatal diagnostic prevents the caller from generating the build graph.
pub(crate) fn architecture_flags(
    model: &ProjectModel,
    target: &Target,
    phase: ArchPhase,
    config: &ConfigId,
    in_try_compile: bool,
) -> (Vec<BString>, Vec<Diagnostic>) {
    let spec = match target_architectures(target) {
        Ok(spec) => spec,
        Err(body) => return (Vec::new(), vec![fatal(target, None, body)]),
    };
    if spec == ArchSpec::Empty {
        // Compiler checks can run before the parent selects an architecture.
        if in_try_compile && target.props.get(b"CUDA_ARCHITECTURES").is_none() {
            return (Vec::new(), Vec::new());
        }
        return (
            Vec::new(),
            empty_diagnostic(target, in_try_compile)
                .into_iter()
                .collect(),
        );
    }
    if spec == ArchSpec::Off {
        return (Vec::new(), Vec::new());
    }

    let get = |key: &[u8]| {
        definition(model, target.dir, key).map_or(b"".as_slice(), |value| value.as_slice())
    };
    let compiler = get(b"CMAKE_CUDA_COMPILER_ID");
    let version = get(b"CMAKE_CUDA_COMPILER_VERSION");
    let at_least = |minimum: &[u8]| version::compare(version, minimum).is_ge();
    if compiler == b"NVIDIA" {
        let mode = match spec {
            ArchSpec::All if at_least(b"11.5") => Some("-arch=all"),
            ArchSpec::AllMajor if at_least(b"11.5") => Some("-arch=all-major"),
            ArchSpec::Native
                if at_least(b"11.6") && !get(b"CMAKE_CUDA_ARCHITECTURES_NATIVE").is_empty() =>
            {
                Some("-arch=native")
            }
            _ => None,
        };
        if let Some(flag) = mode {
            return (vec![flag.into()], Vec::new());
        }
    }

    let architectures = match expand_architectures(model, target, &spec) {
        Ok(architectures) => architectures,
        Err(body) => return (Vec::new(), vec![fatal(target, None, body)]),
    };
    let mut flags = Vec::new();
    let mut diagnostics = Vec::new();
    if compiler == b"NVIDIA" {
        let ipo = ipo_enabled(model, target, config, b"CUDA");
        if ipo && phase == ArchPhase::Link {
            flags.extend(shell_arguments(get(b"CMAKE_CUDA_LINK_OPTIONS_IPO")));
        }
        flags.extend(
            architectures
                .iter()
                .map(|arch| nvidia_flag(arch, phase, ipo)),
        );
    } else if compiler == b"Clang" && phase == ArchPhase::Compile {
        let separable = target
            .props
            .get(b"CUDA_SEPARABLE_COMPILATION")
            .is_some_and(|value| truth::is_on(value));
        let modern = at_least(b"20.0");
        for arch in &architectures {
            clang_flags(
                &mut flags,
                &mut diagnostics,
                target,
                arch,
                modern,
                separable,
                in_try_compile,
            );
        }
    }
    (flags, diagnostics)
}

/// Check the target's interprocedural optimization property for this configuration.
pub(crate) fn ipo_enabled(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    lang: &[u8],
) -> bool {
    if lang == b"CUDA"
        && [
            b"CUDA_ARCHITECTURES".as_slice(),
            b"CUDA_SEPARABLE_COMPILATION",
        ]
        .iter()
        .any(|key| {
            target
                .props
                .get(key)
                .is_none_or(|value| truth::is_off(value))
        })
    {
        return false;
    }
    Flags::new(model, target, lang).ipo(config)
}

fn nvidia_flag(arch: &CudaArch, phase: ArchPhase, ipo: bool) -> BString {
    let name = &arch.name;
    let mut result = BString::from(format!("--generate-code=arch=compute_{name},code=["));
    if arch.virtual_ {
        result.extend_from_slice(format!("compute_{name}").as_bytes());
    }
    if arch.real || ipo {
        if arch.virtual_ {
            result.push(b',');
        }
        let prefix = if ipo && phase == ArchPhase::Compile {
            "lto"
        } else {
            "sm"
        };
        result.extend_from_slice(format!("{prefix}_{name}").as_bytes());
    }
    result.push(b']');
    result
}

fn clang_flags(
    flags: &mut Vec<BString>,
    diagnostics: &mut Vec<Diagnostic>,
    target: &Target,
    arch: &CudaArch,
    modern: bool,
    separable: bool,
    in_try_compile: bool,
) {
    let name = &arch.name;
    let option = if modern {
        "--offload-arch"
    } else {
        "--cuda-gpu-arch"
    };
    flags.push(format!("{option}=sm_{name}").into());
    if !arch.real {
        diagnostics.push(diagnostic(
            target,
            MessageKind::Warning,
            Effect::None,
            None,
            "Clang doesn't support disabling CUDA real code generation.".into(),
        ));
    }
    if modern && arch.virtual_ {
        if separable && !in_try_compile {
            diagnostics.push(diagnostic(
                target, MessageKind::Warning, Effect::None, None,
                "Clang 20+ CUDA offloading model doesn't support virtual archs when CUDA_SEPARABLE_COMPILATION is enabled.".into(),
            ));
        } else {
            flags.push(format!("--cuda-include-ptx=sm_{name}").into());
        }
    } else if !modern && !arch.virtual_ {
        flags.push(format!("--no-cuda-include-ptx=sm_{name}").into());
    }
}

fn empty_diagnostic(target: &Target, in_try_compile: bool) -> Option<Diagnostic> {
    let policy = PolicyId::new(104);
    let body = || {
        format!(
            "CUDA_ARCHITECTURES is empty for target \"{}\".",
            target.name
        )
        .into()
    };
    match target.policies.policies.get(policy) {
        PolicyStatus::Old => None,
        PolicyStatus::Warn if in_try_compile => None,
        PolicyStatus::Warn => {
            let body: BString = body();
            let mut warning = diagnostic(
                target, MessageKind::AuthorWarning, Effect::None, Some(policy),
                format!("Policy CMP0104 is not set: CMAKE_CUDA_ARCHITECTURES now detected for NVCC, empty CUDA_ARCHITECTURES not allowed.  Run \"cmake --help-policy CMP0104\" for policy details.  Use the cmake_policy command to set the policy and suppress this warning.\n\n{body}").into(),
            );
            warning.category = Some(DiagCategory::Policy);
            Some(warning)
        }
        PolicyStatus::New => Some(fatal(target, Some(policy), body())),
    }
}

fn fatal(target: &Target, policy: Option<PolicyId>, body: BString) -> Diagnostic {
    diagnostic(target, MessageKind::Error, Effect::Fatal, policy, body)
}

fn diagnostic(
    target: &Target,
    kind: MessageKind,
    effect: Effect,
    policy: Option<PolicyId>,
    body: BString,
) -> Diagnostic {
    Diagnostic {
        annotations: None,
        kind,
        effect,
        category: None,
        policy,
        location: Location::Backtrace(target.backtrace.clone()),
        body,
    }
}

#[cfg(test)]
mod tests {
    use rcmake_diag::{Backtrace, PolicyMap};
    use rcmake_model::{DirId, TargetKind, TargetOrigin};

    use super::*;

    fn fixture(id: &str, version: &str, value: &str) -> (ProjectModel, Target) {
        let mut model = ProjectModel::default();
        let vars = &mut model.dir_mut(DirId::ROOT).vars;
        for (key, value) in [
            ("CMAKE_CUDA_COMPILER_ID", id),
            ("CMAKE_CUDA_COMPILER_VERSION", version),
            ("CMAKE_CUDA_ARCHITECTURES_ALL", "52-real;75"),
            ("CMAKE_CUDA_ARCHITECTURES_ALL_MAJOR", "52-real;86"),
        ] {
            vars.insert(key.into(), value.into());
        }
        let mut target = Target::new(
            "app".into(),
            TargetKind::Executable,
            TargetOrigin::Normal,
            DirId::ROOT,
            Backtrace::default(),
            PolicyMap::default(),
        );
        target.props.set("CUDA_ARCHITECTURES".into(), value.into());
        (model, target)
    }

    #[test]
    fn architecture_validation_uses_the_target_snapshot() -> Result<(), BString> {
        let (mut model, mut target) = fixture("NVIDIA", "12.6", "75");
        model
            .dir_mut(DirId::ROOT)
            .vars
            .insert("CMAKE_CUDA_ARCHITECTURES".into(), "90a".into());
        assert_eq!(target_architectures(&target)?, parse_architectures(b"75")?);
        let unset = Target::new(
            "earlier".into(),
            TargetKind::Executable,
            TargetOrigin::Normal,
            DirId::ROOT,
            Backtrace::default(),
            PolicyMap::default(),
        );
        assert_eq!(target_architectures(&unset)?, ArchSpec::Empty);
        target
            .props
            .set("CUDA_ARCHITECTURES".into(), "75;abc".into());
        let error = target_architectures(&target).expect_err("Expected a target property error.");
        assert!(error.starts_with(b"Invalid CUDA_ARCHITECTURES property for target \"app\"."));
        let error =
            validate_architecture_variable(b"75;abc").expect_err("Expected a variable error.");
        assert!(error.starts_with(b"CMAKE_CUDA_ARCHITECTURES:"));
        assert!(error.as_slice().windows(4).any(|bytes| bytes == b"100f"));
        Ok(())
    }

    #[test]
    fn nvidia_real_virtual_and_family_architectures() {
        let (model, target) = fixture("NVIDIA", "12.6", "75;86-real;90a-virtual");
        let (flags, diagnostics) = architecture_flags(
            &model,
            &target,
            ArchPhase::Compile,
            &ConfigId::default(),
            false,
        );
        assert!(diagnostics.is_empty());
        assert_eq!(
            flags,
            [
                "--generate-code=arch=compute_75,code=[compute_75,sm_75]",
                "--generate-code=arch=compute_86,code=[sm_86]",
                "--generate-code=arch=compute_90a,code=[compute_90a]",
            ]
        );
        assert!(parse_architectures(b"75-unknown").is_err());
        assert!(validate_architecture_variable(b"").is_err());
        assert!(validate_architecture_variable(b"75;86-real;90a-virtual").is_ok());
    }

    #[test]
    fn named_modes_and_off() {
        for (mode, expected) in [
            ("all", "-arch=all"),
            ("all-major", "-arch=all-major"),
            ("native", "-arch=native"),
        ] {
            let (mut model, target) = fixture("NVIDIA", "12.6", mode);
            model
                .dir_mut(DirId::ROOT)
                .vars
                .insert("CMAKE_CUDA_ARCHITECTURES_NATIVE".into(), "90".into());
            let (flags, diagnostics) = architecture_flags(
                &model,
                &target,
                ArchPhase::Compile,
                &ConfigId::default(),
                false,
            );
            assert_eq!(flags, [expected]);
            assert!(diagnostics.is_empty());
        }
        let (model, target) = fixture("NVIDIA", "11.4", "all-major");
        let (flags, _) = architecture_flags(
            &model,
            &target,
            ArchPhase::Compile,
            &ConfigId::default(),
            false,
        );
        assert_eq!(flags.len(), 2);
        assert!(
            flags
                .iter()
                .any(|flag| flag.ends_with(b"[compute_86,sm_86]"))
        );
        let (model, target) = fixture("NVIDIA", "12.6", "OFF");
        let (flags, diagnostics) = architecture_flags(
            &model,
            &target,
            ArchPhase::Compile,
            &ConfigId::default(),
            false,
        );
        assert!(flags.is_empty() && diagnostics.is_empty());
        let (model, target) = fixture("NVIDIA", "12.6", "native");
        let (_, diagnostics) = architecture_flags(
            &model,
            &target,
            ArchPhase::Compile,
            &ConfigId::default(),
            false,
        );
        assert!(diagnostics.iter().any(|item| item.effect == Effect::Fatal));
    }

    #[test]
    fn cmp0104_uses_target_policy() -> Result<(), rcmake_diag::PolicyError> {
        let (model, mut target) = fixture("NVIDIA", "12.6", "");
        for (status, effect) in [
            (PolicyStatus::Old, None),
            (PolicyStatus::Warn, Some(Effect::None)),
            (PolicyStatus::New, Some(Effect::Fatal)),
        ] {
            target.policies.policies.set(PolicyId::new(104), status)?;
            let (flags, diagnostics) = architecture_flags(
                &model,
                &target,
                ArchPhase::Compile,
                &ConfigId::default(),
                false,
            );
            assert!(flags.is_empty());
            assert_eq!(diagnostics.first().map(|diag| diag.effect), effect);
            if let Some(diag) = diagnostics.first() {
                assert!(
                    diag.body
                        .ends_with(b"CUDA_ARCHITECTURES is empty for target \"app\".")
                );
                assert_eq!(diag.policy, Some(PolicyId::new(104)));
            }
        }
        target
            .policies
            .policies
            .set(PolicyId::new(104), PolicyStatus::Warn)?;
        assert!(
            architecture_flags(
                &model,
                &target,
                ArchPhase::Compile,
                &ConfigId::default(),
                true
            )
            .1
            .is_empty()
        );
        Ok(())
    }

    #[test]
    fn clang_offload_modes_and_nvidia_ipo() -> Result<(), Box<dyn std::error::Error>> {
        let (model, target) = fixture("Clang", "19.0", "75-real;90-virtual");
        let (flags, diagnostics) = architecture_flags(
            &model,
            &target,
            ArchPhase::Compile,
            &ConfigId::default(),
            false,
        );
        assert_eq!(
            flags,
            [
                "--cuda-gpu-arch=sm_75",
                "--no-cuda-include-ptx=sm_75",
                "--cuda-gpu-arch=sm_90"
            ]
        );
        assert_eq!(diagnostics.len(), 1);
        let (mut model, mut target) = fixture("Clang", "20.0", "75");
        let (flags, _) = architecture_flags(
            &model,
            &target,
            ArchPhase::Compile,
            &ConfigId::default(),
            false,
        );
        assert_eq!(flags, ["--offload-arch=sm_75", "--cuda-include-ptx=sm_75"]);
        target
            .props
            .set("CUDA_SEPARABLE_COMPILATION".into(), "ON".into());
        let (flags, diagnostics) = architecture_flags(
            &model,
            &target,
            ArchPhase::Compile,
            &ConfigId::default(),
            false,
        );
        assert_eq!(flags, ["--offload-arch=sm_75"]);
        assert_eq!(diagnostics.len(), 1);
        model
            .dir_mut(DirId::ROOT)
            .vars
            .insert("CMAKE_CUDA_COMPILER_ID".into(), "NVIDIA".into());
        target
            .policies
            .policies
            .set(PolicyId::new(69), PolicyStatus::New)?;
        target
            .props
            .set("INTERPROCEDURAL_OPTIMIZATION".into(), "ON".into());
        let config = ConfigId::default();
        let (flags, _) = architecture_flags(&model, &target, ArchPhase::Compile, &config, false);
        assert_eq!(
            flags,
            ["--generate-code=arch=compute_75,code=[compute_75,lto_75]"]
        );
        target
            .props
            .set("CUDA_SEPARABLE_COMPILATION".into(), "OFF".into());
        let (flags, _) = architecture_flags(&model, &target, ArchPhase::Compile, &config, false);
        assert_eq!(
            flags,
            ["--generate-code=arch=compute_75,code=[compute_75,sm_75]"]
        );
        assert!(!ipo_enabled(&model, &target, &config, b"CUDA"));
        target
            .props
            .set("CUDA_SEPARABLE_COMPILATION".into(), "ON".into());
        target.props.set("CUDA_ARCHITECTURES".into(), "OFF".into());
        assert!(!ipo_enabled(&model, &target, &config, b"CUDA"));
        Ok(())
    }
}
