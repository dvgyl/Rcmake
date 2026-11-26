use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rcmake_base::BString;
use rcmake_cps::{CpsPackage, reader};
use rcmake_eval::{CmdError, Cx};
use rcmake_host::path;

use super::{
    Rejection, Request, internal_requirements, message, missing_dependency, read, target_kind,
};

const ACTIVE: &[u8] = b"_RCMAKE_ACTIVE_CPS_PACKAGES";

pub(in crate::process_find) struct Candidate {
    pub(in crate::process_find) root: CpsPackage,
    appendices: Vec<Appendix>,
    needed: BTreeSet<BString>,
}

pub(in crate::process_find) fn load(
    cx: &mut Cx<'_>,
    file: &Path,
    candidate: Candidate,
    request: &Request,
) -> Result<Result<CpsPackage, Rejection>, CmdError> {
    let active = cx.var_normal_only(ACTIVE);
    let files = active
        .iter()
        .flat_map(|value| value.split(|byte| *byte == 0));
    let identity =
        path::from_native_cmake(&cx.host().canonicalize(file).unwrap_or_else(|_| file.into()));
    if files.clone().any(|file| file == identity)
        || files.count() >= cx.limits().max_recursion.min(64) as usize
    {
        return Err(CmdError::status(
            "Circular or excessively deep CPS package dependency.",
        ));
    }
    let mut active_value = active.clone().unwrap_or_default();
    if !active_value.is_empty() {
        active_value.push(0);
    }
    active_value.extend_from_slice(&identity);
    cx.set_var(ACTIVE, active_value);
    let result = candidate.load(cx, file, request);
    if let Some(active) = active {
        cx.set_var(ACTIVE, active);
    } else {
        cx.unset_var(ACTIVE);
    }
    result
}
struct Appendix {
    file: PathBuf,
    contents: Vec<u8>,
    package: CpsPackage,
}

pub(in crate::process_find) fn inspect(
    cx: &Cx<'_>,
    file: &Path,
    mut package: CpsPackage,
    request: &Request,
) -> Result<Candidate, BString> {
    let (Some(dir), Some(stem)) = (
        file.parent(),
        file.file_stem()
            .map(|stem| path::from_native_cmake(Path::new(stem))),
    ) else {
        return Err("The CPS file has no package name or directory.".into());
    };
    let mut files: Vec<_> = cx
        .host()
        .read_dir(dir)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| {
            let suffix = entry
                .name
                .strip_prefix(stem.as_slice())?
                .strip_suffix(b".cps")?;
            matches!(suffix.first(), Some(b'-' | b'@'))
                .then(|| (BString::from(suffix), entry.name.clone()))
        })
        .collect();
    files.sort();
    let mut appendices = Vec::new();
    let mut supplements: BTreeMap<BString, Vec<PathBuf>> = BTreeMap::new();
    for (suffix, name) in files {
        let companion = dir.join(path::to_native(&name));
        if suffix.contains(&b'@') {
            let owner = suffix
                .split(|byte| *byte == b'@')
                .next()
                .unwrap_or_default();
            supplements.entry(owner.into()).or_default().push(companion);
        } else if let Ok(contents) = read(cx, &companion) {
            let mut appendix = CpsPackage {
                name: package.name.clone(),
                cps_path: package.cps_path.clone(),
                prefix: package.prefix.clone(),
                ..Default::default()
            };
            if reader::merge_appendix(
                &mut appendix,
                &contents,
                &path::from_native_cmake(&companion),
            )
            .is_ok()
            {
                appendices.push((
                    suffix,
                    Appendix {
                        file: companion,
                        contents,
                        package: appendix,
                    },
                ));
            }
        }
    }

    load_supplements(
        cx,
        &mut package,
        supplements.get(b"".as_slice()).map_or(&[], Vec::as_slice),
    )?;
    let mut wanted: BTreeSet<_> = request
        .components
        .iter()
        .map(|(name, _)| name.clone())
        .chain(package.default_components.iter().cloned())
        .chain(internal_requirements(&package))
        .collect();
    let mut selected = Vec::new();
    let mut merged = package.clone();
    while let Some(index) = appendices.iter().position(|(_, appendix)| {
        request.components.is_empty()
            || appendix
                .package
                .components
                .keys()
                .any(|name| wanted.contains(name))
    }) {
        let (suffix, mut appendix) = appendices.remove(index);
        load_supplements(
            cx,
            &mut appendix.package,
            supplements.get(&suffix).map_or(&[], Vec::as_slice),
        )?;
        merge(&mut merged, &appendix)?;
        wanted.extend(internal_requirements(&appendix.package));
        selected.push(appendix);
    }

    let mut needed: BTreeSet<_> = request
        .components
        .iter()
        .filter(|(_, required)| *required)
        .map(|(name, _)| name.clone())
        .chain(package.default_components.iter().cloned())
        .chain(internal_requirements(&package))
        .collect();
    let mut unchecked: Vec<_> = selected.iter().collect();
    while let Some(index) = unchecked.iter().position(|appendix| {
        appendix
            .package
            .components
            .keys()
            .any(|name| needed.contains(name))
    }) {
        let appendix = unchecked.remove(index);
        needed.extend(internal_requirements(&appendix.package));
    }
    for name in &needed {
        if !merged
            .components
            .get(name)
            .is_some_and(|entry| target_kind(&entry.ty).is_some())
        {
            return Err(format!("Required component was not found: {name}.").into());
        }
    }
    Ok(Candidate {
        root: package,
        appendices: selected,
        needed,
    })
}

impl Candidate {
    fn load(
        self,
        cx: &mut Cx<'_>,
        file: &Path,
        request: &Request,
    ) -> Result<Result<CpsPackage, Rejection>, CmdError> {
        let mut root = self.root;
        if let Some((dependency, diagnostics)) = missing_dependency(cx, &root, request, true)? {
            return Ok(Err(Rejection::dependency(
                &root,
                dependency,
                diagnostics,
                &[file],
            )));
        }
        for appendix in self.appendices {
            let needed = appendix
                .package
                .components
                .keys()
                .any(|name| self.needed.contains(name));
            if let Some((dependency, diagnostics)) =
                missing_dependency(cx, &appendix.package, request, needed)?
            {
                if needed {
                    return Ok(Err(Rejection::dependency(
                        &root,
                        dependency,
                        diagnostics,
                        &[&appendix.file, file],
                    )));
                }
                continue;
            }
            merge(&mut root, &appendix).map_err(CmdError::status)?;
        }
        Ok(Ok(root))
    }
}

fn merge(root: &mut CpsPackage, appendix: &Appendix) -> Result<(), BString> {
    reader::merge_appendix(
        root,
        &appendix.contents,
        &path::from_native_cmake(&appendix.file),
    )
    .map_err(message)?;
    root.components.extend(appendix.package.components.clone());
    Ok(())
}

fn load_supplements(
    cx: &Cx<'_>,
    package: &mut CpsPackage,
    files: &[PathBuf],
) -> Result<(), BString> {
    for file in files {
        let contents = read(cx, file)?;
        let path = path::from_native_cmake(file);
        let supplement = reader::read_supplement(&contents, &path).map_err(message)?;
        reader::merge_supplement(package, supplement, &path).map_err(message)?;
    }
    Ok(())
}
