//! Collate Ninja dependencies for Fortran modules.

use std::collections::BTreeSet;

use rcmake_base::{BString, Limits, path_lex::Lexical};
use rcmake_cxxmodules::collate::{CollationInput, collate, write_modules_json};
use rcmake_cxxmodules::dyndep::DyndepRequest;
use rcmake_cxxmodules::{
    CxxModulesJson, ModuleBmi, ModuleProvide, ModuleRequire, P1689File, P1689Rule, TargetDependInfo,
};
use rcmake_fortran::{DependInfo, ModuleMap, ObjectScan, collate_ninja_dyndep};

use crate::ninja_depends::{
    compiler, logical_name, physical_name, read_compiler, read_ddi, relative_to_top,
};
use crate::{ToolCtx, path};

use super::{COMMAND, read_input, read_table_file, write_output};

const MODULES_FILE: &[u8] = b"FortranModules.json";

pub(super) fn execute(
    request: &DyndepRequest,
    cx: &ToolCtx<'_>,
    tdi: &TargetDependInfo,
    tdi_bytes: &[u8],
) -> Result<(), BString> {
    let (separator, extension) = read_compiler(tdi_bytes)
        .map_err(|error| format!("{COMMAND} failed to parse {}{error}", request.tdi))?;
    let mut info = DependInfo::new(compiler(
        &tdi.compiler_id,
        separator.as_ref().map(|value| value.as_slice()),
        extension.as_ref().map(|value| value.as_slice()),
    ));
    info.dir_cur_bld = tdi.dir_cur_bld.clone();
    info.dir_top_bld = tdi.dir_top_bld.clone();
    info.module_dir = (!tdi.module_dir.is_empty()).then(|| tdi.module_dir.clone());
    let mut scans = Vec::with_capacity(request.ddis.len());
    let mut provided_paths = ModuleMap::new();
    let mut bytes_left = Limits::default().max_file_read;
    for ddi in &request.ddis {
        let content = read_input(cx, ddi, ddi)?;
        bytes_left = bytes_left
            .checked_sub(content.len() as u64)
            .ok_or_else(|| BString::from("Fortran dependency scans exceed the size limit."))?;
        let input = read_ddi(&content, &info.compiler)
            .map_err(|error| format!("{COMMAND} failed to parse ddi file {ddi}: {error}"))?;
        provided_paths.extend(input.compiled);
        scans.push(input.scan);
    }
    let linked = tdi
        .linked_target_dirs
        .iter()
        .rev()
        .map(|directory| {
            let table = read_table_file(cx, directory, MODULES_FILE)?;
            table
                .modules
                .into_iter()
                .map(|(name, module)| Ok((logical_name(&info.compiler, &name)?, module.bmi)))
                .collect::<Result<ModuleMap, BString>>()
        })
        .collect::<Result<Vec<_>, _>>()?;
    let result = collate_ninja_dyndep(&scans, &info, &linked).map_err(|error| error.to_string())?;

    let absolute_path =
        |name: &[u8]| Lexical::new(&Lexical::new(&info.dir_top_bld).append(name)).normalize();
    let mut overrides = false;
    let mut modules = CxxModulesJson::default();
    let mut paths = ModuleMap::new();
    for (logical, expected) in result.modules {
        let expected = absolute_path(&expected);
        let file = provided_paths
            .get(&logical)
            .map_or_else(|| expected.clone(), |file| absolute_path(file));
        overrides |= file != expected;
        let name = physical_name(&info.compiler, &logical)?;
        paths.insert(logical, file.clone());
        modules.modules.insert(
            name,
            ModuleBmi {
                bmi: file,
                is_private: false,
            },
        );
    }
    for directory in &tdi.forward_modules_from_target_dirs {
        let table = read_table_file(cx, directory, MODULES_FILE)?;
        for (name, module) in table.modules {
            modules.modules.entry(name).or_insert(module);
        }
    }
    let dyndep = if overrides {
        collate_with_paths(&scans, &info.dir_top_bld, &linked, &paths)?
    } else {
        result.text
    };
    let table = write_modules_json(&modules).map_err(|error| error.to_string())?;
    let dd = path(cx, &request.dd);
    write_output(cx, &dd.with_file_name("FortranModules.json"), &table, true)?;
    write_output(cx, &dd, &dyndep, false)
}

fn collate_with_paths(
    scans: &[ObjectScan],
    top: &[u8],
    linked: &[ModuleMap],
    paths: &ModuleMap,
) -> Result<BString, BString> {
    let relative = |file: &[u8]| relative_to_top(file, top);
    let mut outputs = BTreeSet::new();
    for scan in scans {
        outputs.insert(relative(&scan.object));
    }
    for file in paths.values() {
        let path = relative(file);
        if path.is_empty()
            || path
                .iter()
                .any(|byte| matches!(byte, 0 | b'\n' | b'\r' | b'\t' | b'|'))
        {
            return Err(format!("Invalid Fortran build path {file:?}.").into());
        }
        if !outputs.insert(path) {
            return Err(format!("Multiple rules generate {file:?}.").into());
        }
    }
    let mut input = CollationInput::default();
    for scan in scans {
        let provides = scan
            .info
            .provides
            .iter()
            .map(|name| ModuleProvide {
                logical_name: name.clone(),
                compiled_module_path: paths.get(name).map(|file| relative(file)),
                ..ModuleProvide::default()
            })
            .collect();
        let requires = scan
            .info
            .requires
            .difference(&scan.info.provides)
            .map(|name| ModuleRequire {
                logical_name: name.clone(),
                ..ModuleRequire::default()
            })
            .collect();
        input.ddis.push(P1689File {
            rules: vec![P1689Rule {
                primary_output: Some(relative(&scan.object)),
                provides,
                requires,
                ..P1689Rule::default()
            }],
            ..P1689File::default()
        });
    }
    for table in linked.iter().rev() {
        input.linked.push(CxxModulesJson {
            modules: table
                .iter()
                .map(|(name, file)| {
                    (
                        name.clone(),
                        ModuleBmi {
                            bmi: relative(file),
                            is_private: false,
                        },
                    )
                })
                .collect(),
            ..CxxModulesJson::default()
        });
    }
    collate(&input)
        .map(|result| result.dyndep)
        .map_err(|error| error.to_string().into())
}
