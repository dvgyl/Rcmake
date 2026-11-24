//! Load the file graph and merge preset inheritance.

use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_host::{FileKind, Follow, Host, ReadRange, path};
use rcmake_json::{ReadOptions, Value};

use crate::condition::Condition;
use crate::expand;
use crate::location::{self, Span};
use crate::schema::{self, Field, Fields, Origin, Reader};
use crate::settings::{
    BuildSettings, ConfigureSettings, PackageSettings, TestSettings, WorkflowSettings,
};
use crate::{
    CmakeVersion, EnvSnapshot, PresetFile, PresetKind, PresetSettings, PresetSummary, Presets,
    PresetsError,
};

const PROJECT: &str = "CMakePresets.json";
const USER: &str = "CMakeUserPresets.json";
const MAX_FILES: usize = 256;
const MAX_PRESETS: usize = 4096;

#[derive(Clone, Debug)]
pub(crate) struct Record {
    pub(crate) summary: PresetSummary,
    pub(crate) hidden: bool,
    pub(crate) source: Arc<Origin>,
    pub(crate) fields: Fields,
    pub(crate) condition: Option<Arc<(Condition, Arc<Value>)>>,
    pub(crate) vendor: Option<BTreeMap<BString, Value>>,
    offset: Option<usize>,
    inherits: Vec<BString>,
}

impl Record {
    fn parse(kind: PresetKind, field: &Field) -> Result<Self, PresetsError> {
        let mut reader = Reader::object(field, "Preset")?;
        let name_field = reader.required("name")?;
        let name =
            schema::literal(&name_field).map_err(|_| name_field.invalid("Invalid Preset Name"))?;
        if name.is_empty() {
            return Err(name_field.invalid("Invalid Preset Name"));
        }
        let display_name = reader.optional("displayName", 1, 13, &mut schema::literal)?;
        let description = reader.optional("description", 1, 13, &mut schema::literal)?;
        let vendor = reader
            .take("vendor", 1, 13)?
            .as_ref()
            .map(|field| {
                schema::object(field)
                    .cloned()
                    .map_err(|_| field.invalid("Invalid preset"))
            })
            .transpose()?;
        let mut condition = None;
        let (hidden, inherits) = if kind == PresetKind::Workflow {
            (false, Vec::new())
        } else {
            if let Some(field) = reader.take("condition", 3, 13)? {
                condition = Some(Arc::new((Condition::parse(&field, &name, 0)?, field.value)));
            }
            let hidden = reader
                .optional("hidden", 1, 13, &mut schema::literal)?
                .unwrap_or(false);
            let inherits = reader
                .take("inherits", 1, 13)?
                .as_ref()
                .map(|field| {
                    schema::string_or_array(field, &mut schema::literal)
                        .map_err(|_| field.invalid("Invalid preset"))
                })
                .transpose()?
                .unwrap_or_default();
            (hidden, inherits)
        };
        let mut fields = reader.into_fields();
        if kind == PresetKind::Configure {
            for key in [b"architecture".as_slice(), b"toolset"] {
                if let Some(field) = fields.get_mut(key)
                    && matches!(field.value.as_ref(), Value::String(_))
                {
                    let members = Fields::from([("value".into(), field.clone())]);
                    field.value = Arc::new(Value::Object(BTreeMap::new()));
                    field.members = Some(Arc::new(members));
                }
            }
        }
        let record = Self {
            summary: PresetSummary {
                kind,
                name,
                display_name,
                description,
            },
            hidden,
            source: Arc::clone(&field.source),
            fields,
            condition,
            vendor,
            offset: field.offset(),
            inherits,
        };
        record.read(&mut schema::literal)?;
        if let Some(environment) = record.fields.get(b"environment".as_slice()) {
            for (key, field) in
                schema::fields(environment).map_err(|_| environment.invalid("Invalid preset"))?
            {
                if key.is_empty() {
                    return Err(record.invalid("Empty environment variable names are not allowed"));
                }
                if !matches!(field.value.as_ref(), Value::Null) {
                    schema::text(&field).map_err(|_| field.invalid("Invalid preset"))?;
                }
            }
        }
        Ok(record)
    }

    pub(crate) fn read(
        &self,
        expand: &mut schema::Expand<'_>,
    ) -> Result<PresetSettings, PresetsError> {
        let mut reader = Reader::new(self.fields.clone(), &self.source, "Preset", self.offset)
            .with_preset(&self.summary.name);
        if self.summary.kind != PresetKind::Workflow {
            reader.take("environment", 1, 13)?;
        }
        if matches!(
            self.summary.kind,
            PresetKind::Build | PresetKind::Test | PresetKind::Package
        ) {
            reader.optional::<BString>("configurePreset", 1, 13, expand)?;
            reader.optional::<bool>("inheritConfigureEnvironment", 1, 13, expand)?;
        }
        let settings = match self.summary.kind {
            PresetKind::Configure => {
                PresetSettings::Configure(Box::new(ConfigureSettings::read(&mut reader, expand)?))
            }
            PresetKind::Build => PresetSettings::Build(BuildSettings::read(&mut reader, expand)?),
            PresetKind::Test => {
                PresetSettings::Test(Box::new(TestSettings::read(&mut reader, expand)?))
            }
            PresetKind::Package => {
                PresetSettings::Package(PackageSettings::read(&mut reader, expand)?)
            }
            PresetKind::Workflow => {
                PresetSettings::Workflow(WorkflowSettings::read(&mut reader, expand)?)
            }
        };
        reader.finish()?;
        Ok(settings)
    }

    pub(crate) fn invalid(&self, message: &str) -> PresetsError {
        let kind = self.summary.kind.label();
        let name = &self.summary.name;
        let base = format!("Invalid {kind} preset: \"{name}\"");
        PresetsError::new(if message.is_empty() {
            base
        } else {
            format!("{base}: {message}")
        })
    }
}

struct FileGraph<'a> {
    host: &'a dyn Host,
    values: Option<BTreeMap<PathBuf, &'a Value>>,
    env: EnvSnapshot,
    file_paths: BTreeMap<PathBuf, PathBuf>,
    reachable: BTreeMap<PathBuf, BTreeSet<PathBuf>>,
    presets: Presets,
    count: usize,
}

struct Document {
    value: Value,
    text: Option<Arc<[u8]>>,
}

pub(crate) fn load(
    source_dir: &Path,
    host: &dyn Host,
    values: Option<&BTreeMap<PathBuf, Value>>,
    entry: Option<&Path>,
) -> Result<Presets, PresetsError> {
    let source_dir = if source_dir.is_absolute() {
        source_dir.to_path_buf()
    } else {
        host.initial_cwd().join(source_dir)
    };
    let source_dir = normalize(&source_dir);
    load_graph(&source_dir, host, values, entry).map_err(|error| {
        if values.is_some() {
            error
        } else {
            PresetsError::new(format!(
                "Could not read presets from {}:\n{error}",
                source_dir.display()
            ))
        }
    })
}

fn load_graph(
    source_dir: &Path,
    host: &dyn Host,
    values: Option<&BTreeMap<PathBuf, Value>>,
    entry: Option<&Path>,
) -> Result<Presets, PresetsError> {
    let mut graph = FileGraph {
        host,
        values: values.map(|values| {
            values
                .iter()
                .map(|(path, value)| (normalize(&source_dir.join(path)), value))
                .collect()
        }),
        env: EnvSnapshot::from_host(host),
        file_paths: BTreeMap::new(),
        reachable: BTreeMap::new(),
        presets: Presets {
            source_dir: source_dir.to_path_buf(),
            host_system: host.system().sysname,
            ..Presets::default()
        },
        count: 0,
    };
    let project = graph.presets.source_dir.join(PROJECT);
    let user = graph.presets.source_dir.join(USER);
    let roots = if let Some(entry) = entry {
        let entry = normalize(&host.initial_cwd().join(entry));
        vec![graph.visit(&entry, None, None, 0)?]
    } else {
        let user_value = graph.read(&user)?;
        if let Some(user_value) = user_value {
            graph.visit(&user, Some(user_value), Some(project.clone()), 0)?;
            vec![project, user]
        } else {
            vec![graph.visit(&project, None, None, 0)?]
        }
    };
    graph.presets.version = roots
        .iter()
        .filter_map(|path| graph.file_paths.get(&graph.identity(path)))
        .filter_map(|path| graph.presets.files.get(path).map(|file| file.version))
        .max()
        .unwrap_or_default();
    graph.merge()?;
    graph.presets.validate_references(&graph.reachable)?;
    graph.presets.reachable = graph.reachable;
    if values.is_none() {
        graph.presets.validate_expansions(&graph.env)?;
    }
    Ok(graph.presets)
}

impl FileGraph<'_> {
    fn read(&self, path: &Path) -> Result<Option<Document>, PresetsError> {
        if let Some(values) = &self.values {
            return values
                .get(path)
                .map(|value| {
                    schema::check_tree(value)?;
                    Ok(Document {
                        value: (*value).clone(),
                        text: None,
                    })
                })
                .transpose();
        }
        let metadata = match self.host.metadata(path, Follow::Yes) {
            Ok(metadata) => metadata,
            Err(error) if error.kind == ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(PresetsError::new(format!(
                    "Could not read {}: {error}",
                    path.display()
                )));
            }
        };
        if metadata.kind != FileKind::File {
            return Err(PresetsError::new(format!(
                "Could not read {}: Expected a regular file",
                path.display()
            )));
        }
        let bytes = self
            .host
            .read(
                path,
                ReadRange::All {
                    limit: schema::MAX_FILE_SIZE,
                },
            )
            .map_err(|error| {
                PresetsError::new(format!("Could not read {}: {error}", path.display()))
            })?;
        let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
        if bytes.is_empty() {
            return Err(PresetsError::new("A JSON document cannot be empty"));
        }
        let options = ReadOptions {
            allow_comments: false,
            dup_keys_last_wins: false,
        };
        let value = rcmake_json::parse(bytes, &options)
            .map_err(|error| location::syntax_error(path, bytes, &error))?;
        if !matches!(value, Value::Object(_) | Value::Array(_)) {
            return Err(location::diagnostic(
                path,
                bytes,
                0,
                location::EXPECT_CONTAINER,
            ));
        }
        schema::check_tree(&value)?;
        Ok(Some(Document {
            value,
            text: Some(Arc::from(bytes)),
        }))
    }

    fn identity(&self, path: &Path) -> PathBuf {
        if self.values.is_some() {
            path.to_path_buf()
        } else {
            self.host
                .canonicalize(path)
                .unwrap_or_else(|_| path.to_path_buf())
        }
    }

    fn visit(
        &mut self,
        path: &Path,
        value: Option<Document>,
        implicit: Option<PathBuf>,
        depth: usize,
    ) -> Result<PathBuf, PresetsError> {
        schema::depth(depth, "Preset include graph")?;
        let identity = self.identity(path);
        if let Some(known_path) = self.file_paths.get(&identity) {
            if self.reachable.contains_key(known_path) {
                return Ok(known_path.clone());
            }
            return Err(PresetsError::new(format!(
                "Cyclic include among presets files: {}",
                path.display()
            )));
        }
        if self.file_paths.len() >= MAX_FILES {
            return Err(PresetsError::new("Too many presets files"));
        }
        self.file_paths.insert(identity, path.to_path_buf());
        let value = match value {
            Some(value) => value,
            None => self.read(path)?.ok_or_else(|| file_not_found(path))?,
        };
        let root = read_root(path, value)?;
        let source = &root.source;
        let mut reader = Reader::object(&root, "root object")?;
        let minimum = reader
            .take("cmakeMinimumRequired", 1, 13)?
            .as_ref()
            .map(minimum_version)
            .transpose()?;
        let schema_uri = reader.optional("$schema", 8, 13, &mut schema::literal)?;
        let vendor = reader
            .take("vendor", 1, 13)?
            .as_ref()
            .map(|field| {
                schema::object(field)
                    .cloned()
                    .map_err(|_| field.invalid("Invalid root object"))
            })
            .transpose()?
            .unwrap_or_default();
        let includes = reader.take("include", 4, 13)?;
        for (kind, key) in PresetKind::ARRAYS {
            let version = match kind {
                PresetKind::Configure => 1,
                PresetKind::Build | PresetKind::Test => 2,
                PresetKind::Package | PresetKind::Workflow => 6,
            };
            if let Some(field) = reader.take(key, version, 13)? {
                for item in schema::array(&field)
                    .map_err(|_| field.invalid(format_args!("Invalid \"{key}\" field")))?
                {
                    self.insert(kind, &item)?;
                }
            }
        }
        reader.finish()?;

        let mut reachable = BTreeSet::from([path.to_path_buf()]);
        if let Some(includes) = &includes {
            for include in schema::array(includes)
                .map_err(|_| includes.invalid("Invalid \"include\" field"))?
            {
                let text = expand::include_path(&self.presets, &include, &self.env)
                    .map_err(|_| include.invalid("Invalid \"include\" field"))?;
                let dir = path.parent().unwrap_or(&self.presets.source_dir);
                let included = normalize(&dir.join(path::to_native(text.as_slice())));
                let included = self.visit(&included, None, None, depth + 1)?;
                reachable.extend(self.reachable.get(&included).into_iter().flatten().cloned());
            }
        }
        if let Some(project) = implicit
            && let Some(value) = self.read(&project)?
        {
            let project = self.visit(&project, Some(value), None, depth + 1)?;
            reachable.extend(self.reachable.get(&project).into_iter().flatten().cloned());
        }
        self.presets.files.insert(
            path.to_path_buf(),
            PresetFile {
                version: source.version,
                cmake_minimum_required: minimum,
                schema_uri,
                vendor,
            },
        );
        self.reachable.insert(path.to_path_buf(), reachable);
        Ok(path.to_path_buf())
    }

    fn insert(&mut self, kind: PresetKind, field: &Field) -> Result<(), PresetsError> {
        self.count += 1;
        if self.count > MAX_PRESETS {
            return Err(PresetsError::new("Too many presets"));
        }
        let record = Record::parse(kind, field)?;
        let name = &record.summary.name;
        let records = self.presets.records.entry(kind).or_default();
        if records.contains_key(name) {
            return Err(PresetsError::new(format!("Duplicate preset: \"{name}\"")));
        }
        records.insert(name.clone(), record);
        Ok(())
    }

    fn merge(&mut self) -> Result<(), PresetsError> {
        for (kind, records) in &mut self.presets.records {
            let mut merger = Inheritance {
                kind: *kind,
                records,
                reachable: &self.reachable,
                merged: BTreeMap::new(),
                visiting: BTreeSet::new(),
            };
            // CMake resolves inheritance in name order and lists presets in file order.
            let names: BTreeSet<_> = records.keys().cloned().collect();
            for name in names {
                merger.merge(&name, 0)?;
            }
            let merged = merger.merged;
            for (name, record) in records {
                if let Some(value) = merged.get(name) {
                    *record = value.clone();
                }
            }
        }
        Ok(())
    }
}

struct Inheritance<'a> {
    kind: PresetKind,
    records: &'a indexmap::IndexMap<BString, Record>,
    reachable: &'a BTreeMap<PathBuf, BTreeSet<PathBuf>>,
    merged: BTreeMap<BString, Record>,
    visiting: BTreeSet<BString>,
}

impl Inheritance<'_> {
    fn merge(&mut self, name: &BString, depth: usize) -> Result<(), PresetsError> {
        schema::depth(depth, "Preset inheritance graph")?;
        if self.merged.contains_key(name) {
            return Ok(());
        }
        if !self.visiting.insert(name.clone()) {
            let kind = self.kind.label();
            return Err(PresetsError::new(format!(
                "Cyclic preset inheritance for {kind} preset \"{name}\""
            )));
        }
        let mut record = self
            .records
            .get(name)
            .cloned()
            .ok_or_else(|| PresetsError::new("Invalid Preset"))?;
        for parent in &record.inherits {
            let inherited = self.records.get(parent).ok_or_else(|| {
                record.invalid(&format!("Could not find inherited preset \"{parent}\""))
            })?;
            if !reachable(self.reachable, &record.source, &inherited.source) {
                let kind = self.kind.title();
                return Err(PresetsError::new(format!(
                    "{kind} preset \"{name}\" inherits preset \"{parent}\", but \"{parent}\" is not reachable from the preset's file"
                )));
            }
            self.merge(parent, depth + 1)?;
            if let Some(inherited) = self.merged.get(parent) {
                merge_fields(&mut record.fields, &inherited.fields, "")?;
                if record.condition.is_none()
                    && let Some(condition) = &inherited.condition
                    && !matches!(condition.0, Condition::Null)
                {
                    record.condition = Some(Arc::clone(condition));
                }
            }
        }
        self.visiting.remove(name);
        self.merged.insert(name.clone(), record);
        Ok(())
    }
}

fn merge_fields(child: &mut Fields, parent: &Fields, prefix: &str) -> Result<(), PresetsError> {
    for (key, value) in parent {
        if key.as_slice() == b"$comment"
            && !matches!(prefix, "cacheVariables" | "environment" | "variables")
        {
            continue;
        }
        let alias: Option<&[u8]> = match (prefix, key.as_slice()) {
            ("warnings" | "errors", b"dev") => Some(b"author"),
            ("warnings" | "errors", b"author") => Some(b"dev"),
            _ => None,
        };
        if alias.is_some_and(|alias| child.contains_key(alias)) {
            continue;
        }
        let Some(own) = child.get_mut(key) else {
            child.insert(key.clone(), value.clone());
            continue;
        };
        let nested = matches!(
            (prefix, key.as_slice()),
            (
                "",
                b"environment"
                    | b"cacheVariables"
                    | b"variables"
                    | b"architecture"
                    | b"toolset"
                    | b"warnings"
                    | b"errors"
                    | b"debug"
                    | b"trace"
                    | b"output"
                    | b"filter"
                    | b"execution"
            ) | ("filter", b"include" | b"exclude")
                | ("exclude", b"fixtures")
        );
        let is_map = matches!(prefix, "environment" | "cacheVariables" | "variables");
        let empty = match own.value.as_ref() {
            Value::String(value) => value.is_empty(),
            Value::Array(value) => value.is_empty(),
            _ => false,
        };
        if !is_map && empty && !matches!((prefix, key.as_slice()), ("execution", b"jobs")) {
            *own = value.clone();
        } else if nested {
            let mut fields = schema::fields(own)?;
            merge_fields(&mut fields, &schema::fields(value)?, &key.to_string())?;
            own.members = Some(Arc::new(fields));
        }
    }
    Ok(())
}

pub(crate) fn reachable(
    graph: &BTreeMap<PathBuf, BTreeSet<PathBuf>>,
    from: &Origin,
    to: &Origin,
) -> bool {
    graph
        .get(&from.path)
        .is_some_and(|files| files.contains(&to.path))
}

fn read_root(path: &Path, document: Document) -> Result<Field, PresetsError> {
    let span = document.text.as_deref().and_then(Span::scan);
    let mut source = Arc::new(Origin {
        path: path.to_path_buf(),
        text: document.text,
        version: 0,
    });
    let Value::Object(mut root) = document.value else {
        return Err(source.invalid(span.as_ref().map(|span| span.start), "Invalid root object"));
    };
    let version_offset = span
        .as_ref()
        .and_then(|span| span.member(b"version"))
        .map(|span| span.start);
    let version = match root.remove(b"version".as_slice()) {
        Some(Value::Int(version)) if i32::try_from(version).is_ok() => version as i32,
        Some(Value::UInt(version)) if i32::try_from(version).is_ok() => version as i32,
        Some(Value::Real(version))
            if version.fract() == 0.0
                && version >= f64::from(i32::MIN)
                && version <= f64::from(i32::MAX) =>
        {
            version as i32
        }
        Some(Value::Null) => return Err(PresetsError::new("Invalid \"version\" field")),
        Some(_) => return Err(source.invalid(version_offset, "Invalid \"version\" field")),
        None => return Err(PresetsError::new("No \"version\" field")),
    };
    let version = u32::try_from(version)
        .ok()
        .filter(|version| (1..=13).contains(version))
        .ok_or_else(|| {
            source.invalid(
                version_offset,
                format_args!("Unrecognized \"version\" {version}: must be >=1 and <=13"),
            )
        })?;
    Arc::make_mut(&mut source).version = version;
    Ok(Field::new(&source, Value::Object(root), span))
}

fn minimum_version(field: &Field) -> Result<CmakeVersion, PresetsError> {
    let mut reader = Reader::object(field, "cmakeMinimumRequired")
        .map_err(|_| field.invalid("Invalid \"cmakeMinimumRequired\""))?;
    let components = schema::fields(field)?;
    let read_component = |key: &str, reader: &mut Reader<'_>| -> Result<u32, PresetsError> {
        let Some(field) = reader.take(key, 1, 13)? else {
            return Ok(0);
        };
        use crate::schema::Decode;
        u32::decode(&field, &mut schema::literal)
            .map_err(|_| field.invalid("Invalid \"version\" field"))
    };
    let version = CmakeVersion {
        major: read_component("major", &mut reader)?,
        minor: read_component("minor", &mut reader)?,
        patch: read_component("patch", &mut reader)?,
    };
    reader.finish()?;
    let minimum = [
        (version.major, 4, "major"),
        (version.minor, 5, "minor"),
        (version.patch, 1, "patch"),
    ];
    for (actual, available, component) in minimum {
        match actual.cmp(&available) {
            std::cmp::Ordering::Greater => {
                let location = components.get(component.as_bytes()).unwrap_or(field);
                return Err(location.invalid(format_args!(
                    "\"cmakeMinimumRequired\" {component} version {actual} must be less than {available}"
                )));
            }
            std::cmp::Ordering::Less => break,
            std::cmp::Ordering::Equal => {}
        }
    }
    Ok(version)
}

fn file_not_found(path: &Path) -> PresetsError {
    PresetsError::new(format!("File not found: {}", path.display()))
}

pub(crate) fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            _ => result.push(component),
        }
    }
    result
}
