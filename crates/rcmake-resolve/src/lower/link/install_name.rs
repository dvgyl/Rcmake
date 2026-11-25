//! Mach-O shared-library install names.

use rcmake_base::{BString, path_lex::Lexical, truth};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_genex::{ConfigId, EvalContext};
use rcmake_host::Host;
use rcmake_model::{DirId, ProjectModel, Target, TargetKind};

use crate::{
    ArtifactSet, ResolveError,
    lower::{definition, is_darwin, target_property},
    usage::UsageResolver,
};

pub(crate) fn install_name_dir(
    model: &ProjectModel,
    target: &Target,
    artifacts: &ArtifactSet,
    config: &ConfigId,
    install_prefix: Option<&[u8]>,
) -> Result<BString, ResolveError> {
    let property = |key| target_property(model, target, key);
    let policy = target.policies.policies.get(PolicyId::new(68));
    let install = install_prefix.is_some() || build_with_install_name(model, target, policy);
    let skip = |key| definition(model, target.dir, key).is_some_and(|value| truth::is_on(value));
    if policy != PolicyStatus::New
        && (skip(b"CMAKE_SKIP_RPATH")
            || if install {
                skip(b"CMAKE_SKIP_INSTALL_RPATH")
            } else {
                property(b"SKIP_BUILD_RPATH").is_some_and(|value| truth::is_on(value))
            })
    {
        return Ok(BString::default());
    }
    let rpath = property(b"MACOSX_RPATH").map_or_else(
        || target.policies.policies.get(PolicyId::new(42)) != PolicyStatus::Old,
        |value| truth::is_on(value),
    );
    let mut dir = if install {
        if let Some(value) = property(b"INSTALL_NAME_DIR") {
            let prefix = install_prefix.unwrap_or_else(|| {
                definition(model, target.dir, b"CMAKE_INSTALL_PREFIX")
                    .map_or(b"/usr/local".as_slice(), |prefix| prefix.as_slice())
            });
            let expression = rcmake_genex::rewrite_install_prefix(value, prefix);
            if let Some(id) = model.find_target(target.dir, &target.name) {
                UsageResolver::new(model).eval_ide_property(
                    id,
                    b"INSTALL_NAME_DIR",
                    &expression,
                    &EvalContext {
                        config: config.clone(),
                        head: Some(crate::usage::handle(id)),
                        ..Default::default()
                    },
                )?
            } else {
                expression
            }
        } else if rpath {
            "@rpath".into()
        } else {
            BString::default()
        }
    } else if rpath {
        "@rpath".into()
    } else {
        artifacts
            .file
            .as_ref()
            .map(|file| Lexical::new(file).parent().into())
            .unwrap_or_else(|| model.dir(target.dir).binary_dir.clone())
    };
    if !dir.is_empty() && !dir.ends_with(b"/") {
        dir.push(b'/');
    }
    Ok(dir)
}

pub(crate) fn install_name_prefix(
    model: &ProjectModel,
    target: &Target,
    artifacts: &ArtifactSet,
    config: &ConfigId,
    install_prefix: Option<&[u8]>,
) -> Result<BString, ResolveError> {
    let mut dir = install_name_dir(model, target, artifacts, config, install_prefix)?;
    if let layout @ crate::ide::BundleKind::Framework { .. } = crate::ide::bundle_kind(target)
        && let Some(file) = &artifacts.file
        && let Some(root) = layout.root(file)
        && let Some(relative) = Lexical::new(Lexical::new(file).parent())
            .relative(Lexical::new(Lexical::new(root).parent()))
        && !dir
            .strip_suffix(b"/")
            .is_some_and(|dir| Lexical::new(dir).normalize() == Lexical::new(file).parent())
    {
        dir.extend_from_slice(&relative);
        dir.push(b'/');
    }
    Ok(dir)
}

fn build_with_install_name(model: &ProjectModel, target: &Target, policy: PolicyStatus) -> bool {
    target_property(model, target, b"BUILD_WITH_INSTALL_NAME_DIR").map_or_else(
        || {
            policy != PolicyStatus::New
                && target_property(model, target, b"BUILD_WITH_INSTALL_RPATH")
                    .is_some_and(|value| truth::is_on(value))
        },
        |value| truth::is_on(value),
    )
}

pub(crate) fn warn_install_name_policy(model: &ProjectModel, host: &dyn Host) {
    let policy = PolicyId::new(68);
    let mut names = BString::default();
    for (_, target) in model.targets() {
        if target.kind == TargetKind::SharedLibrary
            && !target.is_imported()
            && is_darwin(model, target.dir)
            && target.policies.policies.get(policy) == PolicyStatus::Warn
            && build_with_install_name(model, target, PolicyStatus::Old)
                != build_with_install_name(model, target, PolicyStatus::New)
        {
            names.extend_from_slice(b"   ");
            names.extend_from_slice(&target.name);
            names.push(b'\n');
        }
    }
    if !names.is_empty()
        && !definition(model, DirId::ROOT, b"CMAKE_SUPPRESS_DEVELOPER_WARNINGS")
            .is_some_and(|value| truth::is_on(value))
    {
        let body = format!(
            "CMake Warning (policy):\n  Policy CMP0068 is not set: RPATH settings on macOS do not affect\n  install_name.  Run \"cmake --help-policy CMP0068\" for policy details.  Use\n  the cmake_policy command to set the policy and suppress this warning.\n\n  For compatibility with older versions of CMake, the install_name fields for\n  the following targets are still affected by RPATH settings:\n\n{names}\nThis warning is for project developers.  Use -Wno-author or -Wno-policy to\nsuppress it.\n\n"
        );
        host.write_out(rcmake_host::Stream::Stderr, body.as_bytes());
    }
}
