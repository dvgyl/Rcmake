//! `CXXDependInfo.json` reader and writer.

use std::collections::BTreeMap;

use rcmake_base::BString;
use rcmake_json::Value;

use crate::json::{
    self, Object, array, boolean, entries, invalid, object, optional_string, parse_json, record,
    string, text, text_array, write_file,
};
use crate::{
    BmiInstallInfo, DatabaseInfo, InterfaceObjectInfo, ModuleExportInfo, ModuleSourceInfo,
    ModuleVisibility, ModulesError, ScannedSourceInfo, TargetDependInfo,
};

struct Fields<'a>(&'a Object);

impl Fields<'_> {
    fn value(&self, key: &str) -> Option<&Value> {
        self.0.get(key.as_bytes())
    }

    fn optional(&self, key: &str) -> Result<Option<BString>, ModulesError> {
        optional_string(self.value(key))
    }
    fn string(&self, key: &str) -> Result<BString, ModulesError> {
        Ok(self.optional(key)?.unwrap_or_default())
    }
    fn boolean(&self, key: &str) -> Result<bool, ModulesError> {
        boolean(self.value(key), false)
    }
    fn strings(&self, key: &str) -> Result<Vec<BString>, ModulesError> {
        json::strings(self.value(key))
    }
}

/// Parse the target metadata used for collation.
///
/// # Errors
/// Returns an error for invalid JSON or invalid field types.
pub fn read_tdi(input: &[u8]) -> Result<TargetDependInfo, ModulesError> {
    let document = parse_json(input)?;
    let fields = Fields(object(&document)?);
    Ok(TargetDependInfo {
        compiler_id: fields.string("compiler-id")?,
        compiler_frontend_variant: fields.string("compiler-frontend-variant")?,
        compiler_simulate_id: fields.string("compiler-simulate-id")?,
        language: fields.string("language")?,
        config: fields.string("config")?,
        module_dir: fields.string("module-dir")?,
        dir_cur_bld: fields.string("dir-cur-bld")?,
        dir_cur_src: fields.string("dir-cur-src")?,
        dir_top_bld: fields.string("dir-top-bld")?,
        dir_top_src: fields.string("dir-top-src")?,
        include_dirs: fields.strings("include-dirs")?,
        linked_target_dirs: fields.strings("linked-target-dirs")?,
        forward_modules_from_target_dirs: fields.strings("forward-modules-from-target-dirs")?,
        cxx_modules: read_map(fields.value("cxx-modules"), read_module_source)?,
        exports: array(fields.value("exports"))?
            .iter()
            .map(read_export)
            .collect::<Result<_, _>>()?,
        bmi_installation: read_optional(fields.value("bmi-installation"), read_bmi_install)?,
        database_info: read_optional(fields.value("database-info"), |fields| {
            Ok(DatabaseInfo {
                template_path: fields.string("template-path")?,
                output: fields.string("output")?,
            })
        })?,
        sources: read_map(fields.value("sources"), |value| {
            let fields = Fields(object(value)?);
            Ok(ScannedSourceInfo {
                source: fields.string("source")?,
                language: fields.string("language")?,
            })
        })?,
        native_target_dir: fields.optional("native-target-dir")?,
        interface_objects: read_optional(fields.value("cxx-interface-objects"), |fields| {
            Ok(InterfaceObjectInfo {
                rsp_file: fields.optional("rsp-file")?,
                module_objects: read_map(fields.value("module-objects"), string)?,
            })
        })?,
    })
}

fn read_map<T>(
    value: Option<&Value>,
    read: impl Fn(&Value) -> Result<T, ModulesError>,
) -> Result<BTreeMap<BString, T>, ModulesError> {
    entries(value)?
        .map(|(key, value)| Ok((key.clone(), read(value)?)))
        .collect()
}

fn read_optional<T>(
    value: Option<&Value>,
    read: impl FnOnce(Fields<'_>) -> Result<T, ModulesError>,
) -> Result<Option<T>, ModulesError> {
    value
        .filter(|v| **v != Value::Null)
        .map(|v| read(Fields(object(v)?)))
        .transpose()
}

fn read_module_source(value: &Value) -> Result<ModuleSourceInfo, ModulesError> {
    let fields = Fields(object(value)?);
    let visibility = match fields.string("visibility")?.as_slice() {
        b"" | b"PRIVATE" => ModuleVisibility::Private,
        b"PUBLIC" => ModuleVisibility::Public,
        b"INTERFACE" => ModuleVisibility::Interface,
        _ => return Err(invalid("Invalid module file set visibility")),
    };
    Ok(ModuleSourceInfo {
        source: fields.string("source")?,
        file_set: fields.string("name")?,
        relative_directory: fields.string("relative-directory")?,
        visibility,
        file_set_type: fields.optional("type")?,
        bmi_only: fields.boolean("bmi-only")?,
        destination: fields.optional("destination")?,
        compile_features: fields.strings("compile-features")?,
        compile_options: fields.strings("compile-options")?,
        definitions: fields.strings("definitions")?,
        include_directories: fields.strings("include-directories")?,
    })
}

fn read_export(value: &Value) -> Result<ModuleExportInfo, ModulesError> {
    let fields = Fields(object(value)?);
    Ok(ModuleExportInfo {
        export_name: fields.string("export-name")?,
        filesystem_export_name: fields.string("filesystem-export-name")?,
        namespace: fields.string("namespace")?,
        destination: fields.string("destination")?,
        cxx_module_info_dir: fields.string("cxx-module-info-dir")?,
        export_prefix: fields.string("export-prefix")?,
        install: fields.boolean("install")?,
    })
}

fn read_bmi_install(fields: Fields<'_>) -> Result<BmiInstallInfo, ModulesError> {
    Ok(BmiInstallInfo {
        destination: fields.string("destination")?,
        component: fields.string("component")?,
        permissions: fields.string("permissions")?,
        exclude_from_all: fields.boolean("exclude-from-all")?,
        optional: fields.boolean("optional")?,
        message_level: fields.string("message-level")?,
        script_location: fields.string("script-location")?,
    })
}

/// Write the target metadata in CMake's JSON file format.
///
/// # Errors
/// The writer currently accepts all values of `TargetDependInfo`.
pub fn write_tdi(info: &TargetDependInfo) -> Result<BString, ModulesError> {
    let mut fields = Object::new();
    for (key, value) in [
        ("compiler-id", &info.compiler_id),
        ("compiler-frontend-variant", &info.compiler_frontend_variant),
        ("compiler-simulate-id", &info.compiler_simulate_id),
        ("language", &info.language),
        ("config", &info.config),
        ("module-dir", &info.module_dir),
        ("dir-cur-bld", &info.dir_cur_bld),
        ("dir-cur-src", &info.dir_cur_src),
        ("dir-top-bld", &info.dir_top_bld),
        ("dir-top-src", &info.dir_top_src),
    ] {
        fields.insert(key.into(), text(value));
    }
    for (key, values) in [
        ("include-dirs", &info.include_dirs),
        ("linked-target-dirs", &info.linked_target_dirs),
        (
            "forward-modules-from-target-dirs",
            &info.forward_modules_from_target_dirs,
        ),
    ] {
        fields.insert(key.into(), text_array(values));
    }
    fields.insert(
        "cxx-modules".into(),
        write_map(&info.cxx_modules, write_module_source),
    );
    fields.insert(
        "exports".into(),
        Value::Array(info.exports.iter().map(write_export).collect()),
    );
    fields.insert(
        "bmi-installation".into(),
        info.bmi_installation
            .as_ref()
            .map_or(Value::Null, write_bmi_install),
    );
    fields.insert(
        "database-info".into(),
        info.database_info.as_ref().map_or(Value::Null, |db| {
            record([
                ("template-path", text(&db.template_path)),
                ("output", text(&db.output)),
            ])
        }),
    );
    fields.insert(
        "sources".into(),
        write_map(&info.sources, |source| {
            record([
                ("source", text(&source.source)),
                ("language", text(&source.language)),
            ])
        }),
    );
    if let Some(directory) = &info.native_target_dir {
        fields.insert("native-target-dir".into(), text(directory));
    }
    if let Some(objects) = &info.interface_objects {
        fields.insert(
            "cxx-interface-objects".into(),
            record([
                (
                    "rsp-file",
                    objects.rsp_file.as_ref().map_or(Value::Null, text),
                ),
                ("module-objects", write_map(&objects.module_objects, text)),
            ]),
        );
    }
    Ok(write_file(&Value::Object(fields)))
}

pub(crate) fn write_map<T>(map: &BTreeMap<BString, T>, write: impl Fn(&T) -> Value) -> Value {
    Value::Object(
        map.iter()
            .map(|(key, value)| (key.clone(), write(value)))
            .collect(),
    )
}

fn write_module_source(source: &ModuleSourceInfo) -> Value {
    record([
        ("source", text(&source.source)),
        ("name", text(&source.file_set)),
        ("relative-directory", text(&source.relative_directory)),
        (
            "visibility",
            Value::string(match source.visibility {
                ModuleVisibility::Private => "PRIVATE",
                ModuleVisibility::Public => "PUBLIC",
                ModuleVisibility::Interface => "INTERFACE",
            }),
        ),
        (
            "type",
            source.file_set_type.as_ref().map_or(Value::Null, text),
        ),
        ("bmi-only", Value::Bool(source.bmi_only)),
        (
            "destination",
            source.destination.as_ref().map_or(Value::Null, text),
        ),
        ("compile-features", text_array(&source.compile_features)),
        ("compile-options", text_array(&source.compile_options)),
        ("definitions", text_array(&source.definitions)),
        (
            "include-directories",
            text_array(&source.include_directories),
        ),
    ])
}

fn write_export(info: &ModuleExportInfo) -> Value {
    record([
        ("export-name", text(&info.export_name)),
        ("filesystem-export-name", text(&info.filesystem_export_name)),
        ("namespace", text(&info.namespace)),
        ("destination", text(&info.destination)),
        ("cxx-module-info-dir", text(&info.cxx_module_info_dir)),
        ("export-prefix", text(&info.export_prefix)),
        ("install", Value::Bool(info.install)),
    ])
}

fn write_bmi_install(info: &BmiInstallInfo) -> Value {
    let mut fields = Object::from([
        ("permissions".into(), text(&info.permissions)),
        ("destination".into(), text(&info.destination)),
        ("message-level".into(), text(&info.message_level)),
        ("script-location".into(), text(&info.script_location)),
    ]);
    if !info.component.is_empty() {
        fields.insert("component".into(), text(&info.component));
    }
    if info.optional {
        fields.insert("optional".into(), Value::Bool(true));
    }
    if info.exclude_from_all {
        fields.insert("exclude-from-all".into(), Value::Bool(true));
    }
    Value::Object(fields)
}

/// A translation unit in a build database template.
pub struct DatabaseUnit<'a> {
    /// The absolute source path.
    pub source: &'a [u8],
    /// The object file path.
    pub object: &'a [u8],
    /// The compiler invocation.
    pub arguments: &'a [BString],
    /// The compiler's working directory.
    pub working_directory: &'a [u8],
    /// Whether the unit is private to its target.
    pub private: bool,
}

/// Serialize a build database template for one target and configuration.
#[must_use]
pub fn write_database_template<'a>(
    target: &[u8],
    config: &[u8],
    units: impl IntoIterator<Item = DatabaseUnit<'a>>,
) -> BString {
    let units = units
        .into_iter()
        .map(|unit| {
            Value::Object(
                [
                    ("source".into(), Value::string(unit.source)),
                    ("object".into(), Value::string(unit.object)),
                    (
                        "work-directory".into(),
                        Value::string(unit.working_directory),
                    ),
                    (
                        "arguments".into(),
                        Value::Array(unit.arguments.iter().cloned().map(Value::string).collect()),
                    ),
                    ("baseline-arguments".into(), Value::Array(Vec::new())),
                    ("local-arguments".into(), Value::Array(Vec::new())),
                    ("private".into(), Value::Bool(unit.private)),
                    ("provides".into(), Value::Object(Default::default())),
                    ("requires".into(), Value::Array(Vec::new())),
                ]
                .into_iter()
                .collect(),
            )
        })
        .collect();
    let set = Value::Object(
        [
            ("family-name".into(), Value::string(target)),
            (
                "name".into(),
                Value::string([target, b"@", config].concat()),
            ),
            ("translation-units".into(), Value::Array(units)),
        ]
        .into_iter()
        .collect(),
    );
    rcmake_json::write_compact(&Value::Object(
        [
            ("revision".into(), Value::UInt(0)),
            ("sets".into(), Value::Array(vec![set])),
        ]
        .into_iter()
        .collect(),
    ))
}
