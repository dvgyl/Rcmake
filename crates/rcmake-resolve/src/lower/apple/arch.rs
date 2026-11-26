use rcmake_base::{BString, list, shell::quote, truth};
use rcmake_genex::ConfigId;
use rcmake_model::{DirId, ProjectModel, Target};

use crate::ResolveError;
use crate::lower::{definition, is_darwin, target_property};

pub(in crate::lower) fn architectures(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    language: &[u8],
) -> Vec<BString> {
    if !is_darwin(model, target.dir) {
        return Vec::new();
    }
    let key = [&b"OSX_ARCHITECTURES_"[..], &config.0.to_ascii_uppercase()].concat();
    let mut architectures = (!config.0.is_empty())
        .then(|| target.props.get(&key))
        .flatten()
        .or_else(|| target_property(model, target, b"OSX_ARCHITECTURES"))
        .map_or_else(Vec::new, |value| list::split(value, list::Empty::Drop));
    if architectures.is_empty()
        && matches!(
            language,
            b"C" | b"CXX" | b"OBJC" | b"OBJCXX" | b"ASM" | b"SYCL" | b"Swift"
        )
        && compiler_target(model, target, language).is_none()
        && let Some(default) = definition(model, target.dir, b"_CMAKE_APPLE_ARCHS_DEFAULT")
    {
        architectures = list::split(default, list::Empty::Drop);
    }
    architectures
}

fn compiler_target<'a>(
    model: &'a ProjectModel,
    target: &Target,
    language: &[u8],
) -> Option<&'a BString> {
    definition(
        model,
        target.dir,
        &[b"CMAKE_", language, b"_COMPILER_TARGET"].concat(),
    )
    .filter(|value| !value.is_empty())
}

pub(in crate::lower) fn flags(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    language: &[u8],
    filter_arch: Option<&[u8]>,
) -> Result<Vec<BString>, ResolveError> {
    if !is_darwin(model, target.dir) {
        return Ok(Vec::new());
    }
    let architectures = architectures(model, target, config, language);
    let selected = |arch: &BString| filter_arch.is_none_or(|selected| arch == selected);
    let compiler_target = compiler_target(model, target, language);
    let system = super::system(model, target.dir);
    let variant = super::variant(model, target.dir);
    let mut flags = Vec::new();
    let version = definition(model, target.dir, b"CMAKE_OSX_DEPLOYMENT_TARGET")
        .map_or(b"".as_slice(), |value| value.as_slice());
    let triple = match (system, variant) {
        (b"iOS", Some(super::Variant::Catalyst)) => {
            Some((b"ios".as_slice(), b"-macabi".as_slice()))
        }
        (b"visionOS", Some(super::Variant::Simulator)) => {
            Some((b"xros".as_slice(), b"-simulator".as_slice()))
        }
        (b"visionOS", _) => Some((b"xros".as_slice(), b"".as_slice())),
        _ => None,
    };
    if matches!(
        language,
        b"C" | b"CXX" | b"OBJC" | b"OBJCXX" | b"ASM" | b"SYCL"
    ) {
        for arch in architectures.iter().filter(|arch| selected(arch)) {
            flags.push("-arch".into());
            flags.push(quote(arch));
        }
    }
    if language == b"Swift" && compiler_target.is_none() {
        let swift_platform = match (system, variant) {
            (b"iOS", Some(super::Variant::Simulator)) => {
                Some((b"ios".as_slice(), b"-simulator".as_slice()))
            }
            (b"iOS", Some(super::Variant::Catalyst)) => {
                Some((b"ios".as_slice(), b"-macabi".as_slice()))
            }
            (b"iOS", None) => Some((b"ios".as_slice(), b"".as_slice())),
            (b"tvOS", Some(super::Variant::Simulator)) => {
                Some((b"tvos".as_slice(), b"-simulator".as_slice()))
            }
            (b"tvOS", _) => Some((b"tvos".as_slice(), b"".as_slice())),
            (b"watchOS", Some(super::Variant::Simulator)) => {
                Some((b"watchos".as_slice(), b"-simulator".as_slice()))
            }
            (b"watchOS", _) => Some((b"watchos".as_slice(), b"".as_slice())),
            (b"visionOS", Some(super::Variant::Simulator)) => {
                Some((b"xros".as_slice(), b"-simulator".as_slice()))
            }
            (b"visionOS", _) => Some((b"xros".as_slice(), b"".as_slice())),
            _ if !version.is_empty() => Some((b"macosx".as_slice(), b"".as_slice())),
            _ => None,
        };
        if let Some((platform, suffix)) = swift_platform {
            let arch = selected_arch(model, target, &architectures, filter_arch, system, variant);
            flags.push("-target".into());
            flags.push(quote(
                &[arch, b"-apple-", platform, version, suffix].concat(),
            ));
        }
    } else if let Some((platform, suffix)) = triple
        && compiler_target.is_none()
        && !version.is_empty()
    {
        let arch = selected_arch(model, target, &architectures, filter_arch, system, variant);
        flags.push(quote(
            &[b"--target=", arch, b"-apple-", platform, version, suffix].concat(),
        ));
    }
    let sysroot = definition(model, target.dir, b"CMAKE_OSX_SYSROOT")
        .filter(|value| !value.is_empty())
        .or_else(|| {
            definition(
                model,
                target.dir,
                &[b"CMAKE_", language, b"_COMPILER_APPLE_SYSROOT_REQUIRED"].concat(),
            )
            .filter(|value| truth::is_on(value))
            .and_then(|_| definition(model, target.dir, b"_CMAKE_OSX_SYSROOT_PATH"))
        });
    let sysroot_flag = definition(
        model,
        target.dir,
        &[b"CMAKE_", language, b"_SYSROOT_FLAG"].concat(),
    )
    .map_or(b"-isysroot".as_slice(), |value| value.as_slice());
    let sysroot = sysroot.filter(|value| !value.is_empty() && value.as_slice() != b"/");
    let map = sdk_map(model, target.dir)?;
    if !sysroot_flag.is_empty() {
        let per_arch = architectures
            .iter()
            .filter_map(|arch| {
                let sdk = map
                    .get(arch)
                    .filter(|sdk| !truth::is_off(sdk) && sdk.as_slice() != b"/")?;
                Some((arch, sdk))
            })
            .collect::<Vec<_>>();
        if per_arch.iter().any(|(_, sdk)| Some(*sdk) != sysroot) {
            for (arch, sdk) in per_arch.into_iter().filter(|(arch, _)| selected(arch)) {
                flags.push(quote(&[b"-Xarch_", arch.as_slice()].concat()));
                flags.push(quote(&[sysroot_flag, sdk.as_slice()].concat()));
            }
        } else if let Some(sdk) = sysroot {
            flags.push(sysroot_flag.into());
            flags.push(quote(sdk));
        }
    }
    if !version.is_empty() && triple.is_none() && language != b"Swift" {
        let prefix: &[u8] = match (system, variant) {
            (b"iOS", Some(super::Variant::Simulator)) => b"-mios-simulator-version-min=",
            (b"iOS", _) => b"-miphoneos-version-min=",
            (b"tvOS", Some(super::Variant::Simulator)) => b"-mtvos-simulator-version-min=",
            (b"tvOS", _) => b"-mtvos-version-min=",
            (b"watchOS", Some(super::Variant::Simulator)) => b"-mwatchos-simulator-version-min=",
            (b"watchOS", _) => b"-mwatchos-version-min=",
            _ => b"-mmacosx-version-min=",
        };
        flags.push(quote(&[prefix, version].concat()));
    }
    Ok(flags)
}

fn selected_arch<'a>(
    model: &'a ProjectModel,
    target: &Target,
    architectures: &'a [BString],
    filter_arch: Option<&'a [u8]>,
    system: &[u8],
    variant: Option<super::Variant>,
) -> &'a [u8] {
    if let Some(arch) = filter_arch {
        return arch;
    }
    if let Some(arch) = architectures.first() {
        return arch.as_slice();
    }
    if let Some(processor) = definition(model, target.dir, b"CMAKE_SYSTEM_PROCESSOR")
        .or_else(|| definition(model, target.dir, b"CMAKE_HOST_SYSTEM_PROCESSOR"))
        .filter(|value| !value.is_empty())
    {
        return if processor == b"aarch64" {
            b"arm64"
        } else {
            processor.as_slice()
        };
    }
    if variant.is_none() && matches!(system, b"iOS" | b"tvOS" | b"watchOS" | b"visionOS") {
        b"arm64"
    } else if cfg!(target_arch = "x86_64") {
        b"x86_64"
    } else {
        b"arm64"
    }
}

pub(crate) fn sdk_map(
    model: &ProjectModel,
    dir: DirId,
) -> Result<std::collections::BTreeMap<BString, BString>, ResolveError> {
    let Some(value) = definition(model, dir, b"CMAKE_APPLE_ARCH_SYSROOTS") else {
        return Ok(Default::default());
    };
    let archs = definition(model, dir, b"CMAKE_OSX_ARCHITECTURES")
        .map_or(b"".as_slice(), |value| value.as_slice());
    let keys = list::split(archs, list::Empty::Drop);
    let values = list::split(value, list::Empty::Keep);
    if keys.len() != values.len() {
        let archs = BString::from(archs);
        return Err(ResolveError::Message(
            format!("CMAKE_APPLE_ARCH_SYSROOTS:\n  {value}\nis not the same length as CMAKE_OSX_ARCHITECTURES:\n  {archs}").into(),
        ));
    }
    Ok(keys.into_iter().zip(values).collect())
}
