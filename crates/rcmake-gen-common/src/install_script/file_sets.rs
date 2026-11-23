use std::collections::BTreeMap;

use rcmake_base::{BString, list, path_lex::Lexical};
use rcmake_genex::{ConfigId, EvalContext, PreprocessMode, TargetHandle};
use rcmake_model::{FileSet, InstallOptions, ProjectModel, Target};
use rcmake_resolve::usage::UsageResolver;

use crate::{GenError, genex};

pub(crate) fn options_for_set(
    model: &ProjectModel,
    index: usize,
    name: &[u8],
) -> Option<InstallOptions> {
    let prefix = format!("_RCMAKE_INSTALL_{index}_ARG");
    let mut args = (0..)
        .map_while(|i| model.global.props.get(format!("{prefix}{i}").as_bytes()))
        .map(|value| value.as_slice())
        .peekable();
    loop {
        args.find(|arg| *arg == b"FILE_SET")?;
        if args.next()? == name {
            break;
        }
    }
    let mut options = model.installs.get(index)?.options.clone();
    while let Some(key) = args.next() {
        match key {
            b"DESTINATION" => options.destination = Some(args.next()?.into()),
            b"COMPONENT" => options.component = Some(args.next()?.into()),
            b"OPTIONAL" => options.optional = true,
            b"EXCLUDE_FROM_ALL" => options.exclude_from_all = true,
            b"PERMISSIONS" => {
                options.permissions = std::iter::from_fn(|| args.next_if(|arg| !keyword(arg)))
                    .map(BString::from)
                    .collect();
            }
            b"CONFIGURATIONS" => {
                options.configurations = std::iter::from_fn(|| args.next_if(|arg| !keyword(arg)))
                    .map(BString::from)
                    .collect();
            }
            _ => break,
        }
    }
    Some(options)
}

fn keyword(value: &[u8]) -> bool {
    matches!(
        value,
        b"ARCHIVE"
            | b"LIBRARY"
            | b"RUNTIME"
            | b"FRAMEWORK"
            | b"BUNDLE"
            | b"OBJECTS"
            | b"PUBLIC_HEADER"
            | b"PRIVATE_HEADER"
            | b"RESOURCE"
            | b"FILE_SET"
            | b"CXX_MODULES_BMI"
            | b"INCLUDES"
            | b"DESTINATION"
            | b"COMPONENT"
            | b"PERMISSIONS"
            | b"OPTIONAL"
            | b"EXCLUDE_FROM_ALL"
            | b"CONFIGURATIONS"
    )
}

pub(crate) fn set_files(
    model: &ProjectModel,
    usage: &mut UsageResolver<'_>,
    target: &Target,
    set: &FileSet,
    config: &ConfigId,
) -> Result<BTreeMap<BString, Vec<(BString, BString)>>, GenError> {
    let handle = TargetHandle(set.target.index() as u32);
    let context = EvalContext {
        config: config.clone(),
        head: Some(handle),
        current: Some(handle),
        ..EvalContext::default()
    };
    let source = Lexical::new(&model.dir(target.dir).source_dir);
    let mut expand = |items: &[rcmake_model::BT<BString>]| {
        let mut paths = Vec::new();
        for item in items {
            let value = genex(
                usage,
                target.dir,
                &context,
                &item.value,
                Some(PreprocessMode::Build),
            )?;
            paths.extend(
                list::split(&value, list::Empty::Drop)
                    .into_iter()
                    .map(|path| Lexical::new(&source.append(&path)).normalize()),
            );
        }
        Ok::<_, GenError>(paths)
    };
    let mut bases = expand(&set.base_dirs)?;
    if set.base_dirs.is_empty() {
        bases.push(model.dir(target.dir).source_dir.clone());
    }
    let mut roots: BTreeMap<_, Vec<_>> = bases
        .iter()
        .cloned()
        .map(|base| (base, Vec::new()))
        .collect();
    for file in expand(&set.files)? {
        let (base, relative) = bases
            .iter()
            .find_map(|base| {
                let relative = Lexical::new(&file).relative(Lexical::new(base))?;
                (!matches!(relative.as_slice(), b"." | b"..") && !relative.starts_with(b"../"))
                    .then_some((base, relative))
            })
            .ok_or_else(|| {
                GenError::Invalid(
                    format!(
                        "File \"{file}\" is outside the base directories of file set \"{}\".",
                        set.name
                    )
                    .into(),
                )
            })?;
        let files = roots.entry(base.clone()).or_default();
        if !files.iter().any(|(known, _)| known == &file) {
            files.push((file, relative));
        }
    }
    Ok(roots)
}
