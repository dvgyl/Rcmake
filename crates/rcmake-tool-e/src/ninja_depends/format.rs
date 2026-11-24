use std::collections::{BTreeMap, BTreeSet};

use rcmake_base::{BString, path_lex::Lexical};
use rcmake_fortran::{FortranCompiler, FortranSourceInfo, ModuleMap, ObjectScan, SourceForm};
use rcmake_json::{ReadOptions, Value};

use super::Request;

type Object = BTreeMap<BString, Value>;

#[derive(Default)]
pub(super) struct ScanMetadata {
    pub(super) module_dir: BString,
    pub(super) top_build: BString,
    pub(super) include_dirs: Vec<BString>,
    pub(super) compiler_id: BString,
    pub(super) submodule_sep: Option<BString>,
    pub(super) submodule_ext: Option<BString>,
    pub(super) building_intrinsic_modules: bool,
    pub(super) source_form: Option<SourceForm>,
}

impl ScanMetadata {
    pub(super) fn compiler(&self) -> FortranCompiler<'_> {
        compiler(
            &self.compiler_id,
            self.submodule_sep.as_ref().map(|value| value.as_slice()),
            self.submodule_ext.as_ref().map(|value| value.as_slice()),
        )
    }
}

pub(crate) fn compiler<'a>(
    id: &'a [u8],
    separator: Option<&'a [u8]>,
    extension: Option<&'a [u8]>,
) -> FortranCompiler<'a> {
    let defaults = FortranCompiler::from_id(id);
    FortranCompiler {
        submodule_sep: separator.unwrap_or(defaults.submodule_sep),
        submodule_ext: extension.unwrap_or(defaults.submodule_ext),
        ..defaults
    }
}

pub(super) fn read_metadata(input: &[u8], source: &[u8]) -> Result<ScanMetadata, BString> {
    let document = parse(input)?;
    let root = object(&document)?;
    let source_form = match root.get(b"_rcmake_source_formats".as_slice()) {
        None | Some(Value::Null) => None,
        Some(formats) => field(object(formats)?, source)?.and_then(SourceForm::from_property),
    };
    Ok(ScanMetadata {
        source_form,
        top_build: field(root, b"dir-top-bld")?.unwrap_or_default().into(),
        module_dir: field(root, b"module-dir")?.unwrap_or_default().into(),
        include_dirs: array(root, b"include-dirs")?
            .iter()
            .map(|directory| string(directory).map(BString::from))
            .collect::<Result<_, _>>()?,
        compiler_id: field(root, b"compiler-id")?.unwrap_or_default().into(),
        submodule_sep: field(root, b"submodule-sep")?.map(BString::from),
        submodule_ext: field(root, b"submodule-ext")?.map(BString::from),
        building_intrinsic_modules: matches!(
            root.get(b"building-intrinsic-modules".as_slice()),
            Some(Value::Bool(true))
        ),
    })
}

pub(crate) fn read_compiler(input: &[u8]) -> Result<(Option<BString>, Option<BString>), BString> {
    let document = parse(input)?;
    let root = object(&document)?;
    Ok((
        field(root, b"submodule-sep")?.map(BString::from),
        field(root, b"submodule-ext")?.map(BString::from),
    ))
}

pub(super) fn write_ddi(
    request: &Request<'_>,
    scan: &FortranSourceInfo,
    metadata: &ScanMetadata,
) -> Result<BString, BString> {
    let compiler = metadata.compiler();
    let mut provided = BTreeMap::new();
    for logical in &scan.provides {
        let name = physical_name(&compiler, logical)?;
        provided.insert(name, scan.conditional_provides.contains(logical));
    }
    let required: BTreeSet<_> = scan
        .requires
        .iter()
        .map(|logical| physical_name(&compiler, logical))
        .collect::<Result<_, _>>()?;
    let requires = required
        .into_iter()
        .filter(|name| !provided.contains_key(name))
        .map(|name| record([("logical-name", Value::string(name))]))
        .collect();
    let provides = provided
        .into_iter()
        .map(|(name, conditional)| {
            let mut fields = Object::from([
                ("is-interface".into(), Value::Bool(true)),
                ("logical-name".into(), Value::string(name.clone())),
            ]);
            if conditional {
                fields.insert("rcmake-conditional".into(), Value::Bool(true));
            }
            if !metadata.module_dir.is_empty() {
                let path = Lexical::new(&metadata.module_dir).append(&name);
                let path = relative_to_top(&path, &metadata.top_build);
                fields.insert("compiled-module-path".into(), Value::string(path));
            }
            Value::Object(fields)
        })
        .collect();
    let rule = record([
        ("outputs", Value::Array(Vec::new())),
        ("primary-output", Value::string(request.obj)),
        ("provides", Value::Array(provides)),
        ("requires", Value::Array(requires)),
    ]);
    Ok(rcmake_json::write_styled(
        &record([
            ("revision", Value::UInt(0)),
            ("rules", Value::Array(vec![rule])),
            ("version", Value::UInt(0)),
        ]),
        "\t",
    ))
}

pub(crate) struct FortranDdi {
    pub(crate) scan: ObjectScan,
    pub(crate) compiled: ModuleMap,
}

pub(crate) fn read_ddi(
    input: &[u8],
    compiler: &FortranCompiler<'_>,
) -> Result<FortranDdi, BString> {
    let document = parse(input)?;
    let root = object(&document)?;
    if !matches!(
        root.get(b"version".as_slice()),
        Some(Value::UInt(0 | 1) | Value::Int(0 | 1))
    ) {
        return Err("Invalid P1689 version.".into());
    }
    let [rule] = array(root, b"rules")? else {
        return Err("Expected one source rule in the dependency scan.".into());
    };
    let rule = object(rule)?;
    let primary = required_field(rule, b"primary-output")?;
    let directory = field(rule, b"work-directory")?.unwrap_or_default();
    let mut scan = ObjectScan {
        object: Lexical::new(directory).append(primary),
        ..ObjectScan::default()
    };
    let mut compiled = ModuleMap::new();
    for entry in array(rule, b"provides")? {
        let entry = object(entry)?;
        let name = logical_name(compiler, required_field(entry, b"logical-name")?)?;
        if matches!(
            entry.get(b"rcmake-conditional".as_slice()),
            Some(Value::Bool(true))
        ) {
            scan.info.conditional_provides.insert(name.clone());
        }
        if let Some(path) = field(entry, b"compiled-module-path")?.filter(|path| !path.is_empty()) {
            compiled.insert(name.clone(), path.into());
        }
        scan.info.provides.insert(name);
    }
    for entry in array(rule, b"requires")? {
        let name = required_field(object(entry)?, b"logical-name")?;
        scan.info.requires.insert(logical_name(compiler, name)?);
    }
    Ok(FortranDdi { scan, compiled })
}

pub(crate) fn physical_name(
    compiler: &FortranCompiler<'_>,
    logical: &[u8],
) -> Result<BString, BString> {
    compiler
        .module_filename(logical)
        .filter(|name| {
            !name
                .iter()
                .any(|byte| matches!(byte, 0 | b'\n' | b'\r' | b'/' | b'\\' | b':'))
        })
        .ok_or_else(|| format!("Invalid Fortran module name {:?}.", BString::from(logical)).into())
}

pub(crate) fn logical_name(
    compiler: &FortranCompiler<'_>,
    physical: &[u8],
) -> Result<BString, BString> {
    let extension = if compiler.submodule_ext.is_empty() {
        b".mod"
    } else {
        compiler.submodule_ext
    };
    if let Some(stem) = physical.strip_suffix(extension) {
        let separator = compiler.submodule_sep;
        if separator.is_empty() {
            if extension != b".mod" {
                return checked_logical_name(
                    compiler,
                    physical,
                    [stem, b"@", stem].concat().into(),
                );
            }
        } else if let Some(index) = stem
            .windows(separator.len())
            .position(|part| part == separator)
            && let (Some(parent), Some(child)) = (
                stem.get(..index),
                stem.get(index.saturating_add(separator.len())..),
            )
        {
            return checked_logical_name(compiler, physical, [parent, b"@", child].concat().into());
        }
    }
    let logical = physical.strip_suffix(b".mod").unwrap_or(physical).into();
    checked_logical_name(compiler, physical, logical)
}

fn checked_logical_name(
    compiler: &FortranCompiler<'_>,
    physical: &[u8],
    mut logical: BString,
) -> Result<BString, BString> {
    logical.make_ascii_lowercase();
    let name = physical_name(compiler, &logical)?;
    if physical.eq_ignore_ascii_case(&name) || physical.eq_ignore_ascii_case(&logical) {
        Ok(logical)
    } else {
        Err(format!(
            "Invalid Fortran module file name {:?}.",
            BString::from(physical)
        )
        .into())
    }
}

pub(crate) fn relative_to_top(path: &[u8], top: &[u8]) -> BString {
    if top.is_empty() {
        return path.into();
    }
    let directory = top.strip_suffix(b"/").unwrap_or(top);
    path.strip_prefix(directory)
        .and_then(|tail| tail.strip_prefix(b"/"))
        .unwrap_or(path)
        .into()
}

fn record<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
}

fn parse(input: &[u8]) -> Result<Value, BString> {
    rcmake_json::parse(input, &ReadOptions::default()).map_err(|error| error.to_string().into())
}

fn object(value: &Value) -> Result<&Object, BString> {
    match value {
        Value::Object(object) => Ok(object),
        _ => Err("Expected a JSON object.".into()),
    }
}

fn array<'a>(object: &'a Object, key: &[u8]) -> Result<&'a [Value], BString> {
    match object.get(key) {
        Some(Value::Array(array)) => Ok(array),
        None | Some(Value::Null) => Ok(&[]),
        _ => Err(format!("Expected an array for {:?}.", BString::from(key)).into()),
    }
}

fn field<'a>(object: &'a Object, key: &[u8]) -> Result<Option<&'a [u8]>, BString> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => string(value).map(Some),
    }
}

fn required_field<'a>(object: &'a Object, key: &[u8]) -> Result<&'a [u8], BString> {
    field(object, key)?
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("Missing {:?}.", BString::from(key)).into())
}

fn string(value: &Value) -> Result<&[u8], BString> {
    match value {
        Value::String(value) => Ok(value),
        _ => Err("Expected a JSON string.".into()),
    }
}
