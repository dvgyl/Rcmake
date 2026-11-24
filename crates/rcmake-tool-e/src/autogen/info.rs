use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use rcmake_base::BString;
use rcmake_json::{ReadOptions, Value};

use super::{DATA_LIMIT, ITEM_LIMIT, JobKind, Result, ToolCtx, output_path, read, resolve};

pub(super) struct Reader<'a> {
    fields: BTreeMap<BString, Value>,
    pub(super) path: PathBuf,
    config: &'a [u8],
    executable_config: &'a [u8],
    verbose: bool,
}

impl<'a> Reader<'a> {
    pub(super) fn read(
        cx: &ToolCtx<'_>,
        path: &Path,
        config: &'a [u8],
        executable_config: &'a [u8],
    ) -> Result<Self> {
        let data = read(cx, path, DATA_LIMIT)?;
        let Value::Object(fields) = rcmake_json::parse(&data, &ReadOptions::default())
            .map_err(|error| format!("Cannot read the info JSON. {error}"))?
        else {
            return Err("The info JSON must contain an object.".into());
        };
        Ok(Self {
            fields,
            path: path.to_path_buf(),
            config,
            executable_config: if executable_config.is_empty() {
                config
            } else {
                executable_config
            },
            verbose: cx.host.env_get(b"VERBOSE").is_some_and(|value| {
                std::str::from_utf8(&value)
                    .ok()
                    .and_then(|text| text.parse::<u32>().ok())
                    .map_or_else(
                        || rcmake_base::truth::is_on(&value),
                        |verbosity| verbosity > 0,
                    )
            }),
        })
    }

    fn select(&self, key: &str, configured: bool) -> BString {
        if configured {
            let qualified = config_key(key, self.config);
            if self
                .fields
                .get(&qualified)
                .is_some_and(|v| v != &Value::Null)
            {
                return qualified;
            }
        }
        key.into()
    }

    fn string(&self, key: &[u8], required: bool) -> Result<BString> {
        match self.fields.get(key) {
            Some(Value::String(value)) if !required || !value.is_empty() => {
                string_value(value, &String::from_utf8_lossy(key))
            }
            None | Some(Value::Null) if !required => Ok(BString::default()),
            _ => Err(invalid(&String::from_utf8_lossy(key), "a string")),
        }
    }

    fn text(&self, key: &str, configured: bool, required: bool) -> Result<BString> {
        self.string(&self.select(key, configured), required)
    }

    fn path(
        &self,
        key: &str,
        base: &Path,
        configured: bool,
        required: bool,
    ) -> Result<Option<PathBuf>> {
        let value = self.text(key, configured, required)?;
        self.optional_path(key, base, &value)
    }

    fn optional_path(&self, key: &str, base: &Path, value: &[u8]) -> Result<Option<PathBuf>> {
        if value.is_empty() {
            return Ok(None);
        }
        validate_path(key, value)?;
        Ok(Some(resolve(base, value)))
    }

    fn required_path(&self, key: &str, base: &Path, configured: bool) -> Result<PathBuf> {
        self.path(key, base, configured, true)?
            .ok_or_else(|| invalid(key, "a file path"))
    }

    fn tool(&self, key: &str, common: &Common, required: bool) -> Result<Option<PathBuf>> {
        let field = if common.better && common.cross {
            config_key(key, self.executable_config)
        } else {
            self.select(key, common.better)
        };
        let value = self.string(&field, required)?;
        self.optional_path(&String::from_utf8_lossy(&field), &common.binary, &value)
    }

    fn boolean(&self, key: &str, required: bool) -> Result<bool> {
        match self.fields.get(key.as_bytes()) {
            Some(Value::Bool(value)) => Ok(*value),
            None | Some(Value::Null) if !required => Ok(false),
            _ => Err(invalid(key, "a boolean")),
        }
    }

    fn unsigned(&self, key: &str, required: bool) -> Result<Option<u32>> {
        let value = match self.fields.get(key.as_bytes()) {
            Some(Value::Int(value)) => u32::try_from(*value).ok(),
            Some(Value::UInt(value)) => u32::try_from(*value).ok(),
            None | Some(Value::Null) if !required => return Ok(None),
            _ => None,
        };
        value
            .map(Some)
            .ok_or_else(|| invalid(key, "an unsigned integer"))
    }

    fn values(&self, key: &[u8], required: bool) -> Result<&[Value]> {
        match self.fields.get(key) {
            None | Some(Value::Null) if !required => Ok(&[]),
            Some(value) => array(value, &String::from_utf8_lossy(key)),
            None => Err(invalid(&String::from_utf8_lossy(key), "an array")),
        }
    }

    fn strings(&self, key: &str, configured: bool, required: bool) -> Result<Vec<BString>> {
        let key = self.select(key, configured);
        strings(self.values(&key, required)?, &String::from_utf8_lossy(&key))
    }

    fn paths(
        &self,
        key: &str,
        base: &Path,
        configured: bool,
        required: bool,
    ) -> Result<Vec<PathBuf>> {
        self.strings(key, configured, required)?
            .iter()
            .map(|path| absolute_path(base, path, key))
            .collect()
    }

    fn rows(
        &self,
        key: &str,
        kind: JobKind,
        common: &Common,
        moc_enabled: bool,
    ) -> Result<Vec<ScanRow>> {
        let mut rows = Vec::new();
        for (index, value) in self
            .values(key.as_bytes(), kind != JobKind::Module)?
            .iter()
            .enumerate()
        {
            let label = format!("{key}[{index}]");
            let parts = array(value, &label)?;
            let (path, flags, configs, output) = match (kind, parts) {
                (JobKind::Source, [path, flags, configs]) => (path, flags, configs, None),
                (JobKind::Header, [path, flags, output, configs])
                | (JobKind::Module, [path, flags, configs, output]) => {
                    (path, flags, configs, Some(output))
                }
                _ => return Err(invalid(&label, "a scan row")),
            };
            let path = absolute_path(&common.source, &string(path, &label)?, &label)?;
            let flags = string(flags, &label)?;
            let [moc @ (b'M' | b'm'), uic @ (b'U' | b'u')] = flags.as_slice() else {
                return Err(invalid(&label, "a pair of moc and uic scan flags"));
            };
            let output = output.map(|value| string(value, &label)).transpose()?;
            if let Some(output) = &output
                && ((!output.is_empty() && !super::safe_relative(output))
                    || (output.is_empty() && *moc == b'M' && moc_enabled))
            {
                return Err(invalid(&label, "a relative generated file path"));
            }
            let active = if configs == &Value::Null {
                true
            } else {
                strings(array(configs, &label)?, &label)?
                    .iter()
                    .any(|c| c == self.config)
            };
            if active {
                rows.push(ScanRow {
                    path,
                    kind,
                    moc: *moc == b'M',
                    uic: *uic == b'U',
                    output,
                });
            }
        }
        Ok(rows)
    }
}

fn config_key(key: &str, config: &[u8]) -> BString {
    [key.as_bytes(), b"_", config].concat().into()
}

fn invalid(field: &str, expected: &str) -> String {
    format!("The info field {field} must contain {expected}.")
}

fn array<'a>(value: &'a Value, field: &str) -> Result<&'a [Value]> {
    match value {
        Value::Array(values) if values.len() <= ITEM_LIMIT => Ok(values),
        _ => Err(invalid(field, "an array within the item limit")),
    }
}

fn strings(values: &[Value], field: &str) -> Result<Vec<BString>> {
    values.iter().map(|value| string(value, field)).collect()
}

fn string(value: &Value, field: &str) -> Result<BString> {
    match value {
        Value::String(value) => string_value(value, field),
        _ => Err(invalid(field, "a string")),
    }
}

fn string_value(value: &BString, field: &str) -> Result<BString> {
    if value.contains(&0) || value.len() > 128 * 1024 {
        return Err(invalid(
            field,
            "a string within the size limit and without null bytes",
        ));
    }
    Ok(value.clone())
}

fn validate_path(field: &str, value: &[u8]) -> Result<()> {
    if value.is_empty() || value.contains(&b'\n') || value.contains(&b'\r') {
        return Err(invalid(field, "a file path"));
    }
    Ok(())
}

fn absolute_path(base: &Path, value: &[u8], field: &str) -> Result<PathBuf> {
    validate_path(field, value)?;
    Ok(resolve(base, value))
}

pub(super) struct Common {
    pub(super) multi: bool,
    better: bool,
    cross: bool,
    pub(super) build: PathBuf,
    pub(super) include: PathBuf,
    pub(super) settings: PathBuf,
    pub(super) binary: PathBuf,
    source: PathBuf,
    pub(super) verbose: bool,
}

impl Common {
    fn read(reader: &Reader<'_>) -> Result<Self> {
        let base = reader
            .path
            .parent()
            .ok_or("The info file has no parent directory.")?;
        let source = reader.required_path("CMAKE_SOURCE_DIR", base, false)?;
        let binary = reader.required_path("CMAKE_BINARY_DIR", base, false)?;
        let source = reader.required_path("CMAKE_CURRENT_SOURCE_DIR", &source, false)?;
        let binary = reader.required_path("CMAKE_CURRENT_BINARY_DIR", &binary, false)?;
        Ok(Self {
            multi: reader.boolean("MULTI_CONFIG", true)?,
            better: reader.boolean("USE_BETTER_GRAPH", true)?,
            cross: reader.boolean("CROSS_CONFIG", true)?,
            build: reader.required_path("BUILD_DIR", &binary, false)?,
            include: reader.required_path("INCLUDE_DIR", &binary, true)?,
            settings: reader.required_path("SETTINGS_FILE", &binary, true)?,
            binary,
            source,
            verbose: reader.unsigned("VERBOSITY", false)?.unwrap_or(0) > 0 || reader.verbose,
        })
    }
}

pub(super) struct ScanRow {
    pub(super) path: PathBuf,
    pub(super) kind: JobKind,
    pub(super) moc: bool,
    pub(super) uic: bool,
    pub(super) output: Option<BString>,
}

pub(super) struct Autogen {
    pub(super) common: Common,
    pub(super) cmake: PathBuf,
    pub(super) info_file: PathBuf,
    pub(super) qt_version: (u32, u32),
    pub(super) parse_cache: PathBuf,
    pub(super) depfile: Option<(PathBuf, BString)>,
    pub(super) list_files: Vec<PathBuf>,
    pub(super) header_extensions: Vec<BString>,
    pub(super) rows: Vec<ScanRow>,
    pub(super) moc: Option<Moc>,
    pub(super) uic: Option<Uic>,
    #[cfg(windows)]
    pub(super) command_limit: usize,
}

impl Autogen {
    pub(super) fn read(reader: &Reader<'_>) -> Result<Self> {
        let common = Common::read(reader)?;
        let cmake = reader.required_path("CMAKE_EXECUTABLE", &common.binary, false)?;
        let parse_cache = reader.required_path("PARSE_CACHE_FILE", &common.binary, true)?;
        let qt_version = (
            reader.unsigned("QT_VERSION_MAJOR", true)?.unwrap_or(0),
            reader.unsigned("QT_VERSION_MINOR", true)?.unwrap_or(0),
        );
        reader.unsigned("PARALLEL", false)?;
        let depfile = reader.path("DEP_FILE", &common.binary, common.better, false)?;
        let rule = reader.text("DEP_FILE_RULE_NAME", common.better, depfile.is_some())?;
        let list_files = reader.paths("CMAKE_LIST_FILES", &common.source, false, true)?;
        let header_extensions = reader.strings("HEADER_EXTENSIONS", false, true)?;
        if header_extensions.iter().any(|extension| {
            extension.is_empty() || extension.contains(&b'/') || extension.contains(&b'\\')
        }) {
            return Err(invalid("HEADER_EXTENSIONS", "file extensions"));
        }
        let moc = reader
            .tool("QT_MOC_EXECUTABLE", &common, false)?
            .map(|executable| Moc::read(reader, &common, executable))
            .transpose()?;
        let mut rows = reader.rows("HEADERS", JobKind::Header, &common, moc.is_some())?;
        rows.extend(reader.rows("CXX_MODULE_UNITS", JobKind::Module, &common, moc.is_some())?);
        rows.extend(reader.rows("SOURCES", JobKind::Source, &common, moc.is_some())?);
        rows.sort_by(|a, b| a.path.cmp(&b.path));
        let uic = reader
            .tool("QT_UIC_EXECUTABLE", &common, false)?
            .map(|executable| Uic::read(reader, &common, executable))
            .transpose()?;
        Ok(Self {
            common,
            cmake,
            info_file: reader.path.clone(),
            qt_version,
            parse_cache,
            depfile: depfile.map(|path| (path, rule)),
            list_files,
            header_extensions,
            rows,
            moc,
            uic,
            #[cfg(windows)]
            command_limit: reader
                .unsigned("AUTOGEN_COMMAND_LINE_LENGTH_MAX", false)?
                .unwrap_or(32_000) as usize,
        })
    }
}

pub(super) struct Moc {
    pub(super) executable: PathBuf,
    pub(super) definitions: Vec<BString>,
    pub(super) includes: Vec<BString>,
    pub(super) include_paths: Vec<PathBuf>,
    pub(super) options: Vec<BString>,
    pub(super) predefs_command: Vec<BString>,
    pub(super) predefs_file: Option<PathBuf>,
    pub(super) compilation: PathBuf,
    pub(super) relaxed: bool,
    pub(super) path_prefix: bool,
    pub(super) macros: Vec<BString>,
    pub(super) filters: Vec<(BString, BString)>,
    pub(super) skip: BTreeSet<PathBuf>,
}

impl Moc {
    fn read(reader: &Reader<'_>, common: &Common, executable: PathBuf) -> Result<Self> {
        let predefs_command = reader.strings("MOC_PREDEFS_CMD", false, false)?;
        let predefs_file = reader.path(
            "MOC_PREDEFS_FILE",
            &common.binary,
            true,
            !predefs_command.is_empty(),
        )?;
        let filters = reader
            .values(b"MOC_DEPEND_FILTERS", false)?
            .iter()
            .map(|value| match array(value, "MOC_DEPEND_FILTERS")? {
                [keyword, pattern] => Ok((
                    string(keyword, "MOC_DEPEND_FILTERS")?,
                    string(pattern, "MOC_DEPEND_FILTERS")?,
                )),
                _ => Err(invalid(
                    "MOC_DEPEND_FILTERS",
                    "pairs of keywords and regular expressions",
                )),
            })
            .collect::<Result<Vec<_>>>()?;
        if filters
            .iter()
            .any(|(keyword, pattern)| keyword.is_empty() || pattern.is_empty())
        {
            return Err(invalid(
                "MOC_DEPEND_FILTERS",
                "non-empty keywords and regular expressions",
            ));
        }
        let macros = reader.strings("MOC_MACRO_NAMES", false, true)?;
        if macros.iter().any(|name| name.is_empty()) {
            return Err(invalid("MOC_MACRO_NAMES", "non-empty macro names"));
        }
        let definitions = reader.strings("MOC_DEFINITIONS", true, false)?;
        let include_values = reader.strings("MOC_INCLUDES", true, false)?;
        let include_paths = include_values
            .iter()
            .map(|path| absolute_path(&common.source, path, "MOC_INCLUDES"))
            .collect::<Result<Vec<_>>>()?;
        let mut includes: Vec<_> = include_values
            .iter()
            .map(|path| prefixed(b"-I", path))
            .collect();
        let frameworks: BTreeSet<_> = include_values
            .iter()
            .filter_map(|path| {
                let suffix = path.strip_suffix(b".framework/Headers")?;
                let index = suffix.iter().rposition(|&byte| byte == b'/')?;
                suffix.get(..index.max(1)).map(BString::from)
            })
            .collect();
        for path in frameworks {
            includes.extend([BString::from("-F"), path]);
        }
        Ok(Self {
            executable,
            definitions: definitions
                .into_iter()
                .map(|value| prefixed(b"-D", &value))
                .collect(),
            includes,
            include_paths,
            options: reader.strings("MOC_OPTIONS", false, false)?,
            predefs_command,
            predefs_file,
            compilation: reader.required_path("MOC_COMPILATION_FILE", &common.binary, true)?,
            relaxed: reader.boolean("MOC_RELAXED_MODE", false)?,
            path_prefix: reader.boolean("MOC_PATH_PREFIX", true)?,
            macros,
            filters,
            skip: reader
                .paths("MOC_SKIP", &common.source, false, false)?
                .into_iter()
                .collect(),
        })
    }
}

pub(super) fn prefixed(prefix: &[u8], value: &[u8]) -> BString {
    [prefix, value].concat().into()
}

pub(super) struct Uic {
    pub(super) executable: PathBuf,
    pub(super) options: Vec<BString>,
    pub(super) file_options: BTreeMap<PathBuf, Vec<BString>>,
    pub(super) search_paths: Vec<PathBuf>,
    pub(super) skip: BTreeSet<PathBuf>,
}

impl Uic {
    fn read(reader: &Reader<'_>, common: &Common, executable: PathBuf) -> Result<Self> {
        let mut file_options = BTreeMap::new();
        for value in reader.values(b"UIC_UI_FILES", true)? {
            let [path, options] = array(value, "UIC_UI_FILES")? else {
                return Err(invalid("UIC_UI_FILES", "a path and its options"));
            };
            let key = absolute_path(
                &common.source,
                &string(path, "UIC_UI_FILES")?,
                "UIC_UI_FILES",
            )?;
            let options = strings(array(options, "UIC_UI_FILES")?, "UIC_UI_FILES")?;
            file_options.insert(key, options);
        }
        Ok(Self {
            executable,
            options: reader.strings("UIC_OPTIONS", true, false)?,
            file_options,
            search_paths: reader.paths("UIC_SEARCH_PATHS", &common.source, false, false)?,
            skip: reader
                .paths("UIC_SKIP", &common.source, false, false)?
                .into_iter()
                .collect(),
        })
    }
}

pub(super) struct Rcc {
    pub(super) common: Common,
    pub(super) info: PathBuf,
    pub(super) source: PathBuf,
    pub(super) executable: PathBuf,
    pub(super) options: Vec<BString>,
    pub(super) list_options: Vec<BString>,
    pub(super) inputs: Vec<PathBuf>,
    pub(super) lock: PathBuf,
    pub(super) checksum: BString,
    pub(super) name: BString,
    pub(super) public: PathBuf,
}

impl Rcc {
    pub(super) fn read(reader: &Reader<'_>) -> Result<Self> {
        let common = Common::read(reader)?;
        reader.text("GENERATOR", false, true)?;
        let executable = reader
            .tool("RCC_EXECUTABLE", &common, true)?
            .ok_or_else(|| invalid("RCC_EXECUTABLE", "an executable path"))?;
        let source = reader.required_path("SOURCE", &common.source, false)?;
        let lock = reader.required_path("LOCK_FILE", &common.binary, false)?;
        let checksum = reader.text("OUTPUT_CHECKSUM", false, true)?;
        let mut name = reader.text("OUTPUT_NAME", false, true)?;
        if common.multi {
            name = replace_config(&name, reader.config);
        }
        for (field, value) in [("OUTPUT_CHECKSUM", &checksum), ("OUTPUT_NAME", &name)] {
            if !super::safe_relative(value) || value.contains(&b'/') {
                return Err(invalid(field, "one file name component"));
            }
        }
        let folder = output_path(&common.build, &checksum)?;
        let public = output_path(&folder, &name)?;
        Ok(Self {
            info: reader.path.clone(),
            options: reader.strings("OPTIONS", false, false)?,
            list_options: reader.strings("RCC_LIST_OPTIONS", common.better, false)?,
            inputs: reader.paths(
                "INPUTS",
                source.parent().unwrap_or(&common.source),
                common.better,
                false,
            )?,
            common,
            source,
            executable,
            lock,
            checksum,
            name,
            public,
        })
    }
}

fn replace_config(name: &[u8], config: &[u8]) -> BString {
    const PLACEHOLDER: &[u8] = b"$<CONFIG>";
    let mut result = Vec::with_capacity(name.len());
    let mut remaining = name;
    while let Some(index) = remaining
        .windows(PLACEHOLDER.len())
        .position(|bytes| bytes == PLACEHOLDER)
    {
        let (prefix, suffix) = remaining.split_at(index);
        result.extend_from_slice(prefix);
        result.extend_from_slice(config);
        remaining = suffix.strip_prefix(PLACEHOLDER).unwrap_or_default();
    }
    result.extend_from_slice(remaining);
    result.into()
}
