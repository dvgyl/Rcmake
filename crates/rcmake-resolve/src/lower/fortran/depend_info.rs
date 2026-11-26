//! Write Fortran dependency data for Ninja and Make.

use indexmap::IndexSet;
use rcmake_base::BString;
use rcmake_fortran::FortranBuildTdi;
use rcmake_json::{Value, write_styled};
use rcmake_model::{ProjectModel, TargetId};

use super::FortranPlan;
use crate::{ResolveError, ResolvedTarget, model::DependInfo};

/// Write the scanner and collator settings for `FortranDependInfo.json`.
pub fn fortran_depend_info_json(tdi: &FortranBuildTdi) -> BString {
    let scan = &tdi.scan;
    let strings =
        |values: &[BString]| Value::Array(values.iter().cloned().map(Value::String).collect());
    let formats = tdi
        .source_formats
        .iter()
        .map(|(path, form)| (path.clone(), Value::string(form.property_value())))
        .collect();
    let fields = [
        ("language", Value::string("Fortran")),
        ("dir-top-bld", Value::string(scan.dir_top_bld.clone())),
        ("dir-cur-bld", Value::string(scan.dir_cur_bld.clone())),
        ("dir-top-src", Value::string(tdi.dir_top_src.clone())),
        ("dir-cur-src", Value::string(tdi.dir_cur_src.clone())),
        ("compiler-id", Value::string(scan.compiler_id.clone())),
        ("submodule-sep", Value::string(scan.submodule_sep.clone())),
        ("submodule-ext", Value::string(scan.submodule_ext.clone())),
        (
            "module-dir",
            Value::string(scan.module_dir.clone().unwrap_or_default()),
        ),
        ("include-dirs", strings(&scan.include_dirs)),
        ("linked-target-dirs", strings(&scan.linked_target_dirs)),
        (
            "forward-modules-from-target-dirs",
            strings(&tdi.forward_modules_from_target_dirs),
        ),
        (
            "building-intrinsic-modules",
            Value::Bool(tdi.building_intrinsic_modules),
        ),
        ("_rcmake_source_formats", Value::Object(formats)),
    ];
    write_styled(
        &Value::Object(
            fields
                .into_iter()
                .map(|(key, value)| (key.into(), value))
                .collect(),
        ),
        "  ",
    )
}

/// Collect Fortran dependency data and scan settings for a Make target.
///
/// # Errors
///
/// Returns an error if the model is missing a target or compile group.
pub fn fortran_make_depend_info<'a>(
    model: &ProjectModel,
    resolved: &ResolvedTarget,
    linked: impl IntoIterator<Item = &'a ResolvedTarget>,
    forwarded: &IndexSet<TargetId>,
    target_order: &IndexSet<TargetId>,
) -> Result<Option<DependInfo>, ResolveError> {
    Ok(
        FortranPlan::new(model, resolved, linked, forwarded, target_order)?
            .map(|plan| DependInfo::Fortran(plan.tdi)),
    )
}
