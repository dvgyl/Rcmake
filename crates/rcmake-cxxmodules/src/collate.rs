//! Collate the module scan results for one target.

use std::collections::{BTreeMap, BTreeSet};

use rcmake_base::{BString, limits::Limits, path_lex::Lexical};

use crate::dyndep::{HEADER, edge, ninja_path, rule_path};
use crate::json::invalid;
use crate::modmap::write_modmap;
use crate::{
    CxxModulesJson, LookupMethod, ModmapFormat, ModuleBmi, ModuleReference, ModuleVisibility,
    ModulesError, P1689File, P1689Rule, TargetDependInfo,
};

pub use crate::modules_json::{read_modules_json, write_modules_json};

/// Input of [`collate`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CollationInput {
    /// The target's `CXXDependInfo.json`.
    pub tdi: TargetDependInfo,
    /// The target's `.ddi` files, in command-line order.
    pub ddis: Vec<P1689File>,
    /// `CXXModules.json` of each linked target directory, in TDI order.
    pub linked: Vec<CxxModulesJson>,
    /// Module tables to forward to consumers of this target.
    pub forwarded: Vec<CxxModulesJson>,
    /// Module table of the original target for a synthetic target.
    pub native: Option<CxxModulesJson>,
    /// `--modmapfmt=` value (`None` writes no module maps).
    pub modmap_format: Option<ModmapFormat>,
}

/// Output of [`collate`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CollationOutput {
    /// Content of the Ninja dyndep (`.dd`) file.
    pub dyndep: BString,
    /// Auxiliary files, with paths and contents. This includes module maps and the BMI install script.
    pub modmaps: Vec<(BString, BString)>,
    /// Module table for dependent targets.
    pub modules: CxxModulesJson,
    /// File set errors detected after Ninja and module metadata generation.
    pub diagnostics: Vec<BString>,
}

struct Scan<'a> {
    primary: BString,
    rule: &'a P1689Rule,
}

/// Collate the imports of one target.
///
/// # Errors
/// Returns an error for invalid scan results, cycles, or inaccessible imports.
pub fn collate(input: &CollationInput) -> Result<CollationOutput, ModulesError> {
    let mut scans = Vec::new();
    for ddi in &input.ddis {
        let [rule] = ddi.rules.as_slice() else {
            return Err(invalid("expected 1 source entry"));
        };
        let primary = rule
            .primary_output
            .as_ref()
            .filter(|p| !p.is_empty())
            .ok_or_else(|| invalid("Missing primary-output"))?;
        scans.push(Scan {
            primary: rule_path(primary, rule.work_directory.as_ref()),
            rule,
        });
    }
    let mut output = CollationOutput {
        dyndep: HEADER.into(),
        ..CollationOutput::default()
    };
    let local = collect_providers(input, &scans, &mut output)?;
    if let Some(install) = &input.tdi.bmi_installation
        && !install.script_location.is_empty()
    {
        output.modmaps.push((
            install.script_location.clone(),
            bmi_install_script(install, &output.modules),
        ));
    }
    let mut available = CxxModulesJson::default();
    for linked in &input.linked {
        available.modules.extend(linked.modules.clone());
        available.references.extend(linked.references.clone());
        merge_usages(&mut available, linked);
        merge_first(
            &mut output.modules.interface_objects,
            &linked.interface_objects,
        );
    }
    for (name, bmi) in &output.modules.modules {
        available.modules.insert(
            name.clone(),
            ModuleBmi {
                bmi: bmi.bmi.clone(),
                is_private: false,
            },
        );
    }

    check_imports(input, &scans, &available)?;
    check_local_visibility(input, &scans, &output.modules, &mut output.diagnostics);
    let order = dependency_order(&local)?;
    for bmi in available.modules.values_mut() {
        bmi.bmi = ninja_path(&bmi.bmi, &input.tdi.dir_top_bld);
    }
    if input.modmap_format.is_some() {
        seed_references(&mut available, &local, &scans);
        expand_usages(&mut available, &local, order)?;
    }
    for scan in &scans {
        let path_for = |name: &BString| {
            available
                .modules
                .get(name)
                .map(|module| module.bmi.as_slice())
        };
        edge(
            &mut output.dyndep,
            &ninja_path(&scan.primary, &input.tdi.dir_top_bld),
            scan.rule
                .provides
                .iter()
                .filter_map(|entry| path_for(&entry.logical_name)),
            scan.rule
                .requires
                .iter()
                .filter_map(|entry| path_for(&entry.logical_name)),
            !scan.rule.provides.is_empty(),
        )?;
        if let Some(format) = input.modmap_format {
            let map_path = [scan.primary.as_slice(), b".modmap"].concat().into();
            output
                .modmaps
                .push((map_path, write_modmap(format, scan.rule, &available)?));
        }
    }
    output.modules.references = available.references;
    output.modules.usages = available.usages;
    if let Some(native) = &input.native {
        output.modules.modules.extend(
            native
                .modules
                .iter()
                .filter(|(_, bmi)| bmi.is_private)
                .map(|(name, bmi)| (name.clone(), bmi.clone())),
        );
    }
    for forwarded in &input.forwarded {
        merge_first(&mut output.modules.modules, &forwarded.modules);
        merge_first(&mut output.modules.references, &forwarded.references);
        merge_first(&mut output.modules.usages, &forwarded.usages);
        merge_first(
            &mut output.modules.interface_objects,
            &forwarded.interface_objects,
        );
    }
    collect_interface_objects(input, &scans, &mut output.modules);
    Ok(output)
}

fn bmi_install_script(install: &crate::BmiInstallInfo, modules: &CxxModulesJson) -> BString {
    let mut script = BString::default();
    let mut files = modules
        .modules
        .values()
        .filter(|module| !module.is_private)
        .peekable();
    if files.peek().is_none() {
        return script;
    }
    script.extend_from_slice(b"file(INSTALL DESTINATION \"");
    if !Lexical::new(&install.destination).absolute() {
        script.extend_from_slice(b"${CMAKE_INSTALL_PREFIX}/");
    }
    escape_cmake(&mut script, &install.destination);
    script.extend_from_slice(b"\" TYPE FILE");
    if install.optional {
        script.extend_from_slice(b" OPTIONAL");
    }
    if matches!(
        install.message_level.as_slice(),
        b"MESSAGE_ALWAYS" | b"MESSAGE_LAZY" | b"MESSAGE_NEVER"
    ) {
        script.push(b' ');
        script.extend_from_slice(&install.message_level);
    }
    let mut permissions = install
        .permissions
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|word| {
            matches!(
                *word,
                b"OWNER_READ"
                    | b"OWNER_WRITE"
                    | b"OWNER_EXECUTE"
                    | b"GROUP_READ"
                    | b"GROUP_WRITE"
                    | b"GROUP_EXECUTE"
                    | b"WORLD_READ"
                    | b"WORLD_WRITE"
                    | b"WORLD_EXECUTE"
                    | b"SETUID"
                    | b"SETGID"
            )
        })
        .peekable();
    if permissions.peek().is_some() {
        script.extend_from_slice(b" PERMISSIONS");
        for permission in permissions {
            script.push(b' ');
            script.extend_from_slice(permission);
        }
    }
    script.extend_from_slice(b" FILES");
    for module in files {
        script.extend_from_slice(b" \"");
        escape_cmake(&mut script, &module.bmi);
        script.push(b'"');
    }
    script.extend_from_slice(b")\n");
    script
}

fn escape_cmake(out: &mut BString, value: &[u8]) {
    for &byte in value {
        if matches!(byte, b'\\' | b'"' | b'$') {
            out.push(b'\\');
        }
        out.push(byte);
    }
}

type Providers<'a> = BTreeMap<&'a BString, &'a P1689Rule>;

fn collect_providers<'a>(
    input: &CollationInput,
    scans: &[Scan<'a>],
    output: &mut CollationOutput,
) -> Result<Providers<'a>, ModulesError> {
    let mut local = BTreeMap::new();
    let top = &input.tdi.dir_top_bld;
    for scan in scans {
        let source = input.tdi.cxx_modules.get(&scan.primary);
        for provide in &scan.rule.provides {
            let name = &provide.logical_name;
            if local.insert(name, scan.rule).is_some() {
                return Err(invalid(format!(
                    "Multiple sources provide the C++ module `{name}`"
                )));
            }
            let bmi = if source.is_some_and(|info| info.bmi_only) {
                scan.primary.clone()
            } else if let Some(path) = provide
                .compiled_module_path
                .as_ref()
                .filter(|p| !p.is_empty())
            {
                let path = rule_path(path, scan.rule.work_directory.as_ref());
                if Lexical::new(&path).absolute() {
                    path
                } else {
                    Lexical::new(&Lexical::new(top).append(&path)).normalize()
                }
            } else {
                let mut file = name.clone();
                for byte in file.iter_mut() {
                    if *byte == b':' {
                        *byte = b'-';
                    }
                }
                file.extend_from_slice(match input.modmap_format {
                    Some(ModmapFormat::Clang) => b".pcm",
                    Some(ModmapFormat::Gcc) => b".gcm",
                    Some(ModmapFormat::Msvc) => b".ifc",
                    None => b".bmi",
                });
                Lexical::new(&input.tdi.module_dir).append(&file)
            };
            output.modules.modules.insert(
                name.clone(),
                ModuleBmi {
                    bmi,
                    is_private: source
                        .is_some_and(|info| info.visibility == ModuleVisibility::Private),
                },
            );
            if !top.is_empty() {
                match source {
                    None => output.diagnostics.push(format!(
                        "Output {} provides the `{name}` module but it is not found in a `FILE_SET` of type `CXX_MODULES`", scan.primary
                    ).into()),
                    Some(info) if info.file_set_type.as_ref().map(|kind| kind.as_slice()) != Some(b"CXX_MODULES") => {
                        let kind = info.file_set_type.as_ref().map_or("".into(), |kind| String::from_utf8_lossy(kind));
                        output.diagnostics.push(format!(
                            "Source {} provides the `{name}` C++ module but is of type `{kind}` module but must be of type `CXX_MODULES`", info.source
                        ).into());
                    }
                    Some(_) => {}
                }
            }
        }
    }
    Ok(local)
}

fn check_local_visibility(
    input: &CollationInput,
    scans: &[Scan<'_>],
    modules: &CxxModulesJson,
    diagnostics: &mut Vec<BString>,
) {
    for scan in scans {
        let Some(source) = input.tdi.cxx_modules.get(&scan.primary) else {
            continue;
        };
        if scan.rule.provides.is_empty() || source.visibility == ModuleVisibility::Private {
            continue;
        }
        for require in &scan.rule.requires {
            let name = &require.logical_name;
            if modules.modules.get(name).is_some_and(|bmi| bmi.is_private) {
                diagnostics.push(format!(
                    "Public C++ module source `{}` requires the `{name}` C++ module which is provided by a private source",
                    source.source
                ).into());
            }
        }
    }
}

fn check_imports(
    input: &CollationInput,
    scans: &[Scan<'_>],
    available: &CxxModulesJson,
) -> Result<(), ModulesError> {
    let errors: BTreeMap<_, _> = input
        .linked
        .iter()
        .rev()
        .filter_map(|linked| linked.import_error.as_ref())
        .flat_map(|error| error.modules.iter().map(|name| (name, &error.message)))
        .collect();
    for scan in scans {
        for require in &scan.rule.requires {
            let name = &require.logical_name;
            if let Some(message) = errors.get(name) {
                return Err(invalid((*message).clone()));
            }
            if available
                .modules
                .get(name)
                .is_some_and(|module| module.is_private)
            {
                return Err(ModulesError::PrivateImport(name.clone()));
            }
        }
    }
    Ok(())
}

fn dependency_order<'a>(local: &Providers<'a>) -> Result<Vec<&'a BString>, ModulesError> {
    let mut waiting = BTreeMap::new();
    let mut dependents: BTreeMap<_, Vec<_>> = BTreeMap::new();
    let mut ready = Vec::new();
    for (&name, rule) in local {
        let dependencies: BTreeSet<_> = rule
            .requires
            .iter()
            .map(|entry| &entry.logical_name)
            .filter(|required| local.contains_key(required))
            .collect();
        if dependencies.is_empty() {
            ready.push(name);
        } else {
            waiting.insert(name, dependencies.len());
            for required in dependencies {
                dependents.entry(required).or_default().push(name);
            }
        }
    }
    let mut order = Vec::with_capacity(local.len());
    while let Some(name) = ready.pop() {
        order.push(name);
        for dependent in dependents.remove(name).into_iter().flatten() {
            if let Some(count) = waiting.get_mut(dependent) {
                *count -= 1;
                if *count == 0 {
                    waiting.remove(dependent);
                    ready.push(dependent);
                }
            }
        }
    }
    if waiting.is_empty() {
        return Ok(order);
    }
    let names = waiting
        .keys()
        .map(|name| format!("\"{name}\""))
        .collect::<Vec<_>>();
    Err(ModulesError::Cycle(names.join(", ").into()))
}

fn seed_references(available: &mut CxxModulesJson, local: &Providers<'_>, scans: &[Scan<'_>]) {
    for name in local.keys() {
        if let Some(module) = available.modules.get(*name) {
            available.references.insert(
                (*name).clone(),
                ModuleReference {
                    path: module.bmi.clone(),
                    lookup_method: LookupMethod::ByName,
                },
            );
        }
    }
    for require in scans.iter().flat_map(|scan| &scan.rule.requires) {
        if let Some(module) = available.modules.get(&require.logical_name) {
            available
                .references
                .entry(require.logical_name.clone())
                .or_insert_with(|| ModuleReference {
                    path: module.bmi.clone(),
                    lookup_method: require.lookup_method,
                });
        }
    }
}

fn expand_usages(
    available: &mut CxxModulesJson,
    local: &Providers<'_>,
    order: Vec<&BString>,
) -> Result<(), ModulesError> {
    let mut bytes = 0_usize;
    let max_bytes = Limits::default().max_value_bytes;
    for name in order {
        let Some(rule) = local.get(name) else {
            continue;
        };
        let mut closure = BTreeSet::new();
        for require in &rule.requires {
            closure.insert(&require.logical_name);
            if let Some(usages) = available.usages.get(&require.logical_name) {
                closure.extend(usages);
            }
        }
        bytes = closure.iter().fold(bytes, |size, name| {
            size.saturating_add(name.len() + std::mem::size_of::<BString>())
        });
        if bytes > max_bytes {
            return Err(invalid("Module imports exceed the expansion limit"));
        }
        if !closure.is_empty() {
            let dependencies = closure.into_iter().cloned().collect();
            available.usages.insert(name.clone(), dependencies);
        }
    }
    Ok(())
}

fn merge_usages(target: &mut CxxModulesJson, linked: &CxxModulesJson) {
    for (name, usages) in &linked.usages {
        let entries = target.usages.entry(name.clone()).or_default();
        entries.extend(usages.iter().cloned());
        entries.sort_unstable();
        entries.dedup();
    }
}

fn merge_first<T: Clone>(target: &mut BTreeMap<BString, T>, linked: &BTreeMap<BString, T>) {
    for (name, value) in linked {
        target.entry(name.clone()).or_insert_with(|| value.clone());
    }
}

fn collect_interface_objects(
    input: &CollationInput,
    scans: &[Scan<'_>],
    modules: &mut CxxModulesJson,
) {
    let Some(info) = &input.tdi.interface_objects else {
        return;
    };
    let mut used = BTreeSet::new();
    for require in scans.iter().flat_map(|scan| &scan.rule.requires) {
        used.insert(&require.logical_name);
        if let Some(usages) = modules.usages.get(&require.logical_name) {
            used.extend(usages);
        }
    }
    for (name, object) in &info.module_objects {
        if used.contains(name) {
            modules
                .interface_objects
                .entry(name.clone())
                .or_insert_with(|| object.clone());
        }
    }
}
