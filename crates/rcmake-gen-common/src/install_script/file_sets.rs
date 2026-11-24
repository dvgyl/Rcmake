use std::collections::BTreeMap;

use rcmake_base::BString;
use rcmake_genex::ConfigId;
use rcmake_model::{FileSet, ProjectModel, Target};
use rcmake_resolve::usage::UsageResolver;

use crate::GenError;

pub(crate) use rcmake_resolve::file_sets::options_for_set;

pub(crate) fn set_files(
    model: &ProjectModel,
    usage: &mut UsageResolver<'_>,
    target: &Target,
    set: &FileSet,
    config: &ConfigId,
) -> Result<BTreeMap<BString, Vec<(BString, BString)>>, GenError> {
    rcmake_resolve::file_sets::set_files(model, usage, target, set, config)
        .map_err(|error| GenError::Invalid(error.to_string().into()))
}
