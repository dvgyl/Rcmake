//! Compiler module map writers for Clang, GCC, and MSVC.

use std::collections::BTreeSet;

use rcmake_base::BString;

use crate::{CxxModulesJson, LookupMethod, ModmapFormat, ModuleReference, ModulesError, P1689Rule};

/// Write one compiler module map from the resolved module table.
///
/// # Errors
/// The writer currently accepts all values of `P1689Rule` and `CxxModulesJson`.
pub fn write_modmap(
    format: ModmapFormat,
    rule: &P1689Rule,
    modules: &CxxModulesJson,
) -> Result<BString, ModulesError> {
    let mut out = BString::default();
    if format == ModmapFormat::Gcc {
        out.extend_from_slice(b"$root .\n");
        for module in &rule.provides {
            if let Some(bmi) = modules.modules.get(&module.logical_name) {
                line(&mut out, &[&module.logical_name, b" ", &bmi.bmi, b"\n"]);
            }
        }
    } else if let [module] = rule.provides.as_slice()
        && let Some(bmi) = modules.modules.get(&module.logical_name)
    {
        if format == ModmapFormat::Clang {
            line(
                &mut out,
                &[b"-x c++-module\n-fmodule-output=\"", &bmi.bmi, b"\"\n"],
            );
        } else {
            let kind: &[u8] = if module.is_interface {
                b"-interface"
            } else {
                b"-internalPartition"
            };
            line(&mut out, &[kind, b"\n-ifcOutput \"", &bmi.bmi, b"\"\n"]);
        }
    }

    let mut indirect = BTreeSet::new();
    for require in &rule.requires {
        let name = &require.logical_name;
        if let Some(bmi) = modules.modules.get(name) {
            import(&mut out, format, name, &bmi.bmi, require.lookup_method);
            if format != ModmapFormat::Gcc
                && let Some(usages) = modules.usages.get(name)
            {
                indirect.extend(usages);
            }
        }
    }
    for require in &rule.requires {
        if modules.modules.contains_key(&require.logical_name) {
            indirect.remove(&require.logical_name);
        }
    }
    for name in indirect {
        if let Some(ModuleReference {
            path,
            lookup_method,
        }) = modules.references.get(name)
        {
            import(&mut out, format, name, path, *lookup_method);
        }
    }
    Ok(out)
}

fn import(out: &mut BString, format: ModmapFormat, name: &[u8], path: &[u8], method: LookupMethod) {
    match format {
        ModmapFormat::Gcc => line(out, &[name, b" ", path, b"\n"]),
        ModmapFormat::Clang => line(out, &[b"-fmodule-file=\"", name, b"=", path, b"\"\n"]),
        ModmapFormat::Msvc => {
            let flag: &[u8] = match method {
                LookupMethod::ByName => b"-reference",
                LookupMethod::IncludeAngle => b"-headerUnit:angle",
                LookupMethod::IncludeQuote => b"-headerUnit:quote",
            };
            line(out, &[flag, b" \"", name, b"=", path, b"\"\n"]);
        }
    }
}

fn line(out: &mut BString, parts: &[&[u8]]) {
    for part in parts {
        out.extend_from_slice(part);
    }
}
