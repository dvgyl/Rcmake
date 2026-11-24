use rcmake_base::BString;
use rcmake_diag::{DiagCategory, Effect, MessageKind};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};

use super::quoted;

struct Feature {
    name: &'static [u8],
    variable: &'static [u8],
    activation: &'static [u8],
    description: &'static str,
}

const FEATURES: &[Feature] = &[
    Feature {
        name: b"ExportPackageDependencies",
        variable: b"CMAKE_EXPERIMENTAL_EXPORT_PACKAGE_DEPENDENCIES",
        activation: b"1942b4fa-b2c5-4546-9385-83f254070067",
        description: "CMake's EXPORT_PACKAGE_DEPENDENCIES support is experimental.",
    },
    Feature {
        name: b"MappedPackageInfo",
        variable: b"CMAKE_EXPERIMENTAL_MAPPED_PACKAGE_INFO",
        activation: b"ababa1b5-7099-495f-a9cd-e22d38f274f2",
        description: "CMake's support for generating package information in the Common Package Specification format from CMake script exports is experimental.",
    },
    Feature {
        name: b"ExportBuildDatabase",
        variable: b"CMAKE_EXPERIMENTAL_EXPORT_BUILD_DATABASE",
        activation: b"70ef007e-b743-492d-9407-e35eeac03a40",
        description: "CMake's support for exporting build databases is experimental.",
    },
    Feature {
        name: b"Rust",
        variable: b"CMAKE_EXPERIMENTAL_RUST",
        activation: b"b6fdddce-bf66-41a5-bc5f-077f6fa4d2a1",
        description: "CMake's support for the Rust programming language is experimental.",
    },
    Feature {
        name: b"SYCL",
        variable: b"CMAKE_EXPERIMENTAL_SYCL",
        activation: b"c0d1fb10-2ece-420e-9d29-7d7f2b300f25",
        description: "CMake's support for the SYCL programming language is experimental.",
    },
];

pub(super) fn enabled(cx: &mut Cx<'_>, args: &[&[u8]]) -> CmdResult {
    let [name, out] = args else {
        return Err(CmdError::status(
            "sub-command GET_EXPERIMENTAL_FEATURE_ENABLED expects exactly two arguments",
        ));
    };
    let enabled = feature_enabled(cx, name)?;
    cx.set_var(out, if enabled { "TRUE" } else { "FALSE" }.into());
    Ok(Flow::Normal)
}

pub(crate) fn feature_enabled(cx: &mut Cx<'_>, name: &[u8]) -> Result<bool, CmdError> {
    let feature = FEATURES
        .iter()
        .find(|feature| feature.name == name)
        .ok_or_else(|| {
            CmdError::status(quoted(
                "Experimental feature name \"",
                name,
                "\" does not exist.",
            ))
        })?;
    let mut enabled = false;
    if let Some(value) = cx.var(feature.variable) {
        enabled = value == feature.activation;
        let key = [b"\0EXPERIMENTAL_WARNING", feature.name, b"\0", &value].concat();
        let props = &mut cx.global().props;
        if props.get(&key).is_none() {
            let message = if enabled {
                format!(
                    "{}  It is meant only for experimentation and feedback to CMake developers.",
                    feature.description
                )
            } else {
                format!(
                    "{} is set to incorrect value\n\n  {}\n\nSee 'Help/dev/experimental.rst' in the source tree of this version of CMake for documentation of the experimental feature and the corresponding activation value.  This project's code may require changes to work with this CMake's version of the feature.",
                    BString::from(feature.variable),
                    value
                )
            };
            props.set(key.into(), BString::default());
            cx.issue(
                MessageKind::AuthorWarning,
                Effect::None,
                Some(DiagCategory::Experimental),
                message.into(),
            );
        }
    }
    Ok(enabled)
}
