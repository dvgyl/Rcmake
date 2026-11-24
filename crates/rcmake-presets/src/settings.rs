//! Typed settings for each preset category.

use std::collections::BTreeMap;

use rcmake_base::BString;
use rcmake_json::Value;

use crate::schema::{self, Decode, Expand, Field, Reader};
use crate::{PresetKind, PresetsError};

macro_rules! choices {
    (
        $(#[$doc:meta])* $name:ident {
            $($(#[$variant_doc:meta])* $variant:ident = $value:literal),+ $(,)?
        }
    ) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum $name {
            $($(#[$variant_doc])* $variant),+
        }

        impl Decode for $name {
            fn decode(field: &Field, _: &mut Expand<'_>) -> Result<Self, PresetsError> {
                schema::choice(field, &[$(($value, Self::$variant)),+])
            }
        }
    };
}

macro_rules! settings {
    (
        $(#[$doc:meta])* $name:ident {
            $($(#[$field_doc:meta])* $field:ident : $ty:ty = $key:literal
                $( @ $min:literal )? $(=> $decode:expr)?),* $(,)?
        }
    ) => {
        $(#[$doc])*
        #[derive(Clone, Debug, Default, PartialEq, Eq)]
        pub struct $name {
            $($(#[$field_doc])* pub $field: Option<$ty>),*
        }

        impl $name {
            pub(crate) fn read(reader: &mut Reader<'_>, expand: &mut Expand<'_>) -> Result<Self, PresetsError> {
                Ok(Self {$(
                    $field: settings!(@read reader, expand, $key $(, $min)? $(=> $decode)? )?,
                )*})
            }
        }

        impl Decode for $name {
            fn decode(field: &Field, expand: &mut Expand<'_>) -> Result<Self, PresetsError> {
                let mut reader = Reader::object(field, stringify!($name))?;
                let value = Self::read(&mut reader, expand)?;
                reader.finish()?;
                Ok(value)
            }
        }
    };
    (@min $min:literal) => { $min };
    (@min) => { 1 };
    (@read $reader:ident, $expand:ident, $key:literal $(, $min:literal)? => $decode:expr) => {
        $reader.take($key, settings!(@min $($min)?), 13)?
            .map(|field| $decode(&field, $expand)).transpose()
    };
    (@read $reader:ident, $expand:ident, $key:literal $(, $min:literal)?) => {
        $reader.optional($key, settings!(@min $($min)?), 13, $expand)
    };
}

fn expanded(field: &Field, expand: &mut Expand<'_>) -> Result<BString, PresetsError> {
    expand(field)
}

choices! {
    /// How to apply a generator platform or toolset.
    Strategy {
        /// Pass the value to CMake.
        Set = "set",
        /// Leave the value for an external tool.
        External = "external",
    }
}

/// Generator platform or toolset selection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GeneratorSelection {
    /// Selection value.
    pub value: Option<BString>,
    /// How to apply the value. The default is `Set`.
    pub strategy: Option<Strategy>,
}

fn selection(field: &Field, expand: &mut Expand<'_>) -> Result<GeneratorSelection, PresetsError> {
    let mut reader = Reader::object(field, "preset")?;
    let value = reader.optional("value", 1, 13, &mut schema::literal)?;
    let strategy = reader
        .take("strategy", 1, 13)?
        .map(|field| Strategy::decode(&field, expand).map_err(|_| field.invalid("Invalid preset")))
        .transpose()?;
    reader.finish()?;
    Ok(GeneratorSelection { value, strategy })
}

/// Cache variable value and optional type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheVariable {
    /// Type to use for the cache entry.
    pub kind: Option<BString>,
    /// Value to use for the cache entry.
    pub value: BString,
}

impl CacheVariable {
    fn read(field: &Field, label: &str, expand: &mut Expand<'_>) -> Result<Self, PresetsError> {
        match field.value.as_ref() {
            Value::String(_) => Ok(Self {
                kind: None,
                value: expand(field)?,
            }),
            Value::Bool(value) => Ok(Self {
                kind: Some(BString::from("BOOL")),
                value: bool_text(*value),
            }),
            _ => {
                let mut reader = Reader::object(field, label)
                    .map_err(|_| field.invalid(format_args!("Invalid CMake {label}")))?;
                let kind = reader.optional("type", 1, 13, expand)?;
                let field = reader.required("value")?;
                let value = match field.value.as_ref() {
                    Value::Bool(value) => bool_text(*value),
                    _ => expand(&field)?,
                };
                reader.finish()?;
                Ok(Self { kind, value })
            }
        }
    }
}

fn bool_text(value: bool) -> BString {
    BString::from(if value { "TRUE" } else { "FALSE" })
}

fn cache(
    field: &Field,
    expand: &mut Expand<'_>,
) -> Result<BTreeMap<BString, CacheVariable>, PresetsError> {
    let mut values = BTreeMap::new();
    let name = field
        .preset_name
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_default();
    for (key, entry) in schema::fields(field).map_err(|_| field.invalid("Invalid preset"))? {
        if key.is_empty() {
            return Err(PresetsError::new(format!(
                "Invalid configure preset: \"{name}\": Empty cache variable names are not allowed"
            )));
        }
        if !matches!(entry.value.as_ref(), Value::Null) {
            let label = format!("variable \"{key}\" for preset \"{name}\"");
            values.insert(key, CacheVariable::read(&entry, &label, expand)?);
        }
    }
    Ok(values)
}

settings! {
    /// Configure preset settings.
    ConfigureSettings {
        /// Generator name.
        generator: BString = "generator",
        /// Generator platform.
        architecture: GeneratorSelection = "architecture" => selection,
        /// Generator toolset.
        toolset: GeneratorSelection = "toolset" => selection,
        /// Output directory for the build system.
        binary_dir: BString = "binaryDir" => expanded,
        /// Installation prefix.
        install_dir: BString = "installDir" @ 3 => expanded,
        /// Toolchain file path.
        toolchain_file: BString = "toolchainFile" @ 3 => expanded,
        /// Graphviz output file path.
        graphviz: BString = "graphviz" @ 10 => expanded,
        /// CMake executable path for IDEs.
        cmake_executable: BString = "cmakeExecutable",
        /// Cache variables to set.
        cache_variables: BTreeMap<BString, CacheVariable> = "cacheVariables" => cache,
        /// Warning flags.
        warnings: ConfigureWarnings = "warnings" => warnings,
        /// Error flags.
        errors: ConfigureErrors = "errors" => errors,
        /// Debug flags.
        debug: ConfigureDebug = "debug",
        /// Trace settings.
        trace: TraceSettings = "trace" @ 7,
    }
}

settings! {
    /// Warning flags for configuration.
    ConfigureWarnings {
        /// Enable author warnings.
        author: bool = "author" @ 12,
        /// Enable deprecated usage warnings.
        deprecated: bool = "deprecated",
        /// Enable experimental feature warnings.
        experimental: bool = "experimental" @ 12,
        /// Enable absolute installation destination warnings.
        install_absolute_destination: bool = "installAbsoluteDestination" @ 12,
        /// Enable non-target directive warnings.
        non_target_directive: bool = "nonTargetDirective" @ 13,
        /// Enable policy warnings.
        policy: bool = "policy" @ 12,
        /// Enable strict warnings.
        strict: bool = "strict" @ 13,
        /// Warn about uninitialized variables.
        uninitialized: bool = "uninitialized",
        /// Warn about unused command-line variables.
        unused_cli: bool = "unusedCli",
        /// Check system files for unused and uninitialized variables.
        system_vars: bool = "systemVars",
    }
}

settings! {
    /// Error flags for configuration.
    ConfigureErrors {
        /// Treat author warnings as errors.
        author: bool = "author" @ 12,
        /// Treat deprecated usage warnings as errors.
        deprecated: bool = "deprecated",
        /// Treat experimental feature warnings as errors.
        experimental: bool = "experimental" @ 12,
        /// Treat absolute installation destination warnings as errors.
        install_absolute_destination: bool = "installAbsoluteDestination" @ 12,
        /// Treat non-target directive warnings as errors.
        non_target_directive: bool = "nonTargetDirective" @ 13,
        /// Treat policy warnings as errors.
        policy: bool = "policy" @ 12,
        /// Treat strict warnings as errors.
        strict: bool = "strict" @ 13,
        /// Treat uninitialized variable warnings as errors.
        uninitialized: bool = "uninitialized" @ 12,
        /// Treat unused command-line variable warnings as errors.
        unused_cli: bool = "unusedCli" @ 12,
    }
}

fn warnings(field: &Field, expand: &mut Expand<'_>) -> Result<ConfigureWarnings, PresetsError> {
    let mut reader = Reader::object(field, "warnings")?;
    let mut result = ConfigureWarnings::read(&mut reader, expand)?;
    result.author = result.author.or(reader.optional("dev", 1, 11, expand)?);
    reader.finish()?;
    Ok(result)
}

fn errors(field: &Field, expand: &mut Expand<'_>) -> Result<ConfigureErrors, PresetsError> {
    let mut reader = Reader::object(field, "errors")?;
    let mut result = ConfigureErrors::read(&mut reader, expand)?;
    result.author = result.author.or(reader.optional("dev", 1, 11, expand)?);
    reader.finish()?;
    Ok(result)
}

settings! {
    /// Debug flags for configuration.
    ConfigureDebug {
        /// Enable debug output and stack traces.
        output: bool = "output",
        /// Retain files from try-compile checks.
        try_compile: bool = "tryCompile",
        /// Log search paths from find commands.
        find: bool = "find",
    }
}

choices! {
    /// Trace mode.
    TraceMode {
        /// Trace all calls.
        On = "on",
        /// Disable tracing.
        Off = "off",
        /// Trace calls with expanded variables.
        Expand = "expand",
    }
}

choices! {
    /// Format of trace records.
    TraceFormat {
        /// Human-readable text.
        Human = "human",
        /// Version 1 JSON trace.
        JsonV1 = "json-v1",
    }
}

settings! {
    /// Configuration trace settings.
    TraceSettings {
        /// Trace mode.
        mode: TraceMode = "mode",
        /// Trace output format.
        format: TraceFormat = "format",
        /// Files to trace.
        source: Vec<BString> = "source" => schema::string_or_array,
        /// File to receive trace output.
        redirect: BString = "redirect" => expanded,
    }
}

fn nonnegative(field: &Field, expand: &mut Expand<'_>) -> Result<u32, PresetsError> {
    let value = i32::decode(field, expand)?;
    u32::try_from(value).map_err(|_| field.invalid("Expected a non-negative integer"))
}

choices! {
    /// Package reference resolution before a build.
    PackageReferences {
        /// Resolve package references before building.
        On = "on",
        /// Skip package reference resolution.
        Off = "off",
        /// Resolve package references.
        Only = "only",
    }
}

settings! {
    /// Build preset settings.
    BuildSettings {
        /// Number of parallel jobs. Zero uses the native tool's default.
        jobs: u32 = "jobs",
        /// Targets to build.
        targets: Vec<BString> = "targets" => schema::string_or_array,
        /// Build configuration for a multi-configuration generator.
        configuration: BString = "configuration",
        /// Clean before the build.
        clean_first: bool = "cleanFirst",
        /// Package reference resolution mode.
        resolve_package_references: PackageReferences = "resolvePackageReferences" @ 4,
        /// Show the build commands.
        verbose: bool = "verbose",
        /// Arguments for the native build tool.
        native_tool_options: Vec<BString> = "nativeToolOptions" => schema::strings,
    }
}

settings! {
    /// Test preset settings.
    TestSettings {
        /// Build configuration to test.
        configuration: BString = "configuration",
        /// Overrides for the CTest configuration file.
        overwrite_configuration_file: Vec<BString> = "overwriteConfigurationFile" => schema::strings,
        /// Output settings.
        output: TestOutput = "output",
        /// Filters for test selection.
        filter: TestFilter = "filter",
        /// Execution settings.
        execution: TestExecution = "execution",
    }
}

choices! {
    /// Test output verbosity.
    TestVerbosity {
        /// Use the default verbosity.
        Default = "default",
        /// Show test output.
        Verbose = "verbose",
        /// Show additional test details.
        Extra = "extra",
    }
}

choices! {
    /// Part of the test output to remove when it exceeds its size limit.
    OutputTruncation {
        /// Remove output at the end.
        Tail = "tail",
        /// Remove output in the middle.
        Middle = "middle",
        /// Remove output at the start.
        Head = "head",
    }
}

settings! {
    /// Output settings for tests.
    TestOutput {
        /// Use a single line for progress.
        short_progress: bool = "shortProgress",
        /// Test output verbosity.
        verbosity: TestVerbosity = "verbosity",
        /// Enable debug output.
        debug: bool = "debug",
        /// Show output for failed tests.
        output_on_failure: bool = "outputOnFailure",
        /// Suppress normal output.
        quiet: bool = "quiet",
        /// File to receive the test log.
        output_log_file: BString = "outputLogFile" => expanded,
        /// File to receive JUnit XML results.
        output_junit_file: BString = "outputJUnitFile" @ 6 => expanded,
        /// Include the label summary.
        label_summary: bool = "labelSummary",
        /// Include the subproject summary.
        subproject_summary: bool = "subprojectSummary",
        /// Maximum output size for passed tests in bytes.
        max_passed_test_output_size: i32 = "maxPassedTestOutputSize",
        /// Maximum output size for failed tests in bytes.
        max_failed_test_output_size: i32 = "maxFailedTestOutputSize",
        /// Part of test output to remove.
        test_output_truncation: OutputTruncation = "testOutputTruncation" @ 5,
        /// Maximum width of a test name in output.
        max_test_name_width: i32 = "maxTestNameWidth",
    }
}

settings! {
    /// Test selection filters.
    TestFilter {
        /// Select tests that match these criteria.
        include: TestInclude = "include",
        /// Exclude tests that match these criteria.
        exclude: TestExclude = "exclude",
    }
}

settings! {
    /// Test inclusion criteria.
    TestInclude {
        /// Regular expression for test names.
        name: BString = "name" => expanded,
        /// Regular expression for test labels.
        label: BString = "label" => expanded,
        /// Select the union of the name and index filters.
        use_union: bool = "useUnion",
        /// Test index settings.
        index: TestIndex = "index" => test_index,
    }
}

/// Selection of tests by index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TestIndex {
    /// Read index settings from this file.
    File(BString),
    /// Use these index settings directly.
    Range(TestIndexRange),
}

fn test_index(field: &Field, expand: &mut Expand<'_>) -> Result<TestIndex, PresetsError> {
    if matches!(field.value.as_ref(), Value::String(_)) {
        return Ok(TestIndex::File(expand(field)?));
    }
    Ok(TestIndex::Range(TestIndexRange::decode(field, expand)?))
}

settings! {
    /// Range and explicit indices to select.
    TestIndexRange {
        /// First index in the range.
        start: i32 = "start",
        /// Last index in the range.
        end: i32 = "end",
        /// Increment between indices.
        stride: i32 = "stride",
        /// List of test indices to select.
        specific_tests: Vec<i32> = "specificTests",
    }
}

settings! {
    /// Test exclusion criteria.
    TestExclude {
        /// Regular expression for test names.
        name: BString = "name" => expanded,
        /// Regular expression for test labels.
        label: BString = "label" => expanded,
        /// Regular expressions for fixture names.
        fixtures: TestFixtures = "fixtures",
    }
}

settings! {
    /// Fixtures for which CTest must omit automatic tests.
    TestFixtures {
        /// Exclude setup and cleanup tests.
        any: BString = "any" => expanded,
        /// Exclude setup tests.
        setup: BString = "setup" => expanded,
        /// Exclude cleanup tests.
        cleanup: BString = "cleanup" => expanded,
    }
}

/// Parallel job setting for tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestJobs {
    /// Use the default number of parallel jobs.
    Automatic,
    /// Use this job count. Zero means no limit.
    Count(u32),
}

impl Decode for TestJobs {
    fn decode(field: &Field, expand: &mut Expand<'_>) -> Result<Self, PresetsError> {
        if let Value::String(value) = field.value.as_ref()
            && value.is_empty()
            && field.source.version >= 11
        {
            return Ok(Self::Automatic);
        }
        Ok(Self::Count(nonnegative(field, expand)?))
    }
}

choices! {
    /// Format for listing tests without running them.
    ShowOnly {
        /// Human-readable text.
        Human = "human",
        /// Version 1 JSON.
        JsonV1 = "json-v1",
        /// Version 1 compact JSON.
        JsonV1Raw = "json-v1-raw",
    }
}

choices! {
    /// Action when the test selection is empty.
    NoTestsAction {
        /// Use the default behavior.
        Default = "default",
        /// Report an error.
        Error = "error",
        /// Ignore the empty selection.
        Ignore = "ignore",
    }
}

choices! {
    /// Rule for repeating tests.
    RepeatMode {
        /// Repeat until the test fails.
        UntilFail = "until-fail",
        /// Repeat until the test passes.
        UntilPass = "until-pass",
        /// Repeat a test if it times out.
        AfterTimeout = "after-timeout",
    }
}

/// Test repetition settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TestRepeat {
    /// Rule for repeating the test.
    pub mode: RepeatMode,
    /// Maximum number of runs.
    pub count: i32,
}

impl Decode for TestRepeat {
    fn decode(field: &Field, expand: &mut Expand<'_>) -> Result<Self, PresetsError> {
        let mut reader = Reader::object(field, "repeat")?;
        let mode = RepeatMode::decode(&reader.required("mode")?, expand)?;
        let count = i32::decode(&reader.required("count")?, expand)?;
        reader.finish()?;
        Ok(Self { mode, count })
    }
}

settings! {
    /// Execution settings for tests.
    TestExecution {
        /// Stop after a test fails.
        stop_on_failure: bool = "stopOnFailure",
        /// Resume a previous test run.
        enable_failover: bool = "enableFailover",
        /// Parallel job setting.
        jobs: TestJobs = "jobs",
        /// File that describes resources for tests.
        resource_spec_file: BString = "resourceSpecFile" => expanded,
        /// Load limit for starting tests.
        test_load: i32 = "testLoad",
        /// List tests in this format.
        show_only: ShowOnly = "showOnly",
        /// Test repetition rule.
        repeat: TestRepeat = "repeat",
        /// Enable interactive debugging.
        interactive_debugging: bool = "interactiveDebugging",
        /// Run tests in random order.
        schedule_random: bool = "scheduleRandom",
        /// Default test timeout in seconds.
        timeout: i32 = "timeout",
        /// Action when no tests match.
        no_tests_action: NoTestsAction = "noTestsAction",
        /// Arguments to pass to test executables.
        test_passthrough_arguments: Vec<BString> = "testPassthroughArguments" @ 12,
    }
}

fn string_map(
    field: &Field,
    expand: &mut Expand<'_>,
) -> Result<BTreeMap<BString, BString>, PresetsError> {
    schema::fields(field)?
        .into_iter()
        .map(|(key, field)| Ok((key, BString::decode(&field, expand)?)))
        .collect()
}

settings! {
    /// Package preset settings.
    PackageSettings {
        /// CPack generator names.
        generators: Vec<BString> = "generators",
        /// Build configurations to package.
        configurations: Vec<BString> = "configurations",
        /// Variables to pass to CPack.
        variables: BTreeMap<BString, BString> = "variables" => string_map,
        /// CPack configuration file.
        config_file: BString = "configFile",
        /// Package output settings.
        output: PackageOutput = "output",
        /// Package name.
        package_name: BString = "packageName",
        /// Package version.
        package_version: BString = "packageVersion",
        /// Output directory for packages.
        package_directory: BString = "packageDirectory",
        /// Package vendor name.
        vendor_name: BString = "vendorName",
    }
}

settings! {
    /// Output settings for CPack.
    PackageOutput {
        /// Enable debug output.
        debug: bool = "debug",
        /// Enable verbose output.
        verbose: bool = "verbose",
    }
}

/// Workflow preset settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowSettings {
    /// Steps in execution order.
    pub steps: Vec<WorkflowStep>,
}

impl WorkflowSettings {
    pub(crate) fn read(
        reader: &mut Reader<'_>,
        expand: &mut Expand<'_>,
    ) -> Result<Self, PresetsError> {
        let steps = Vec::decode(&reader.required("steps")?, expand)?;
        Ok(Self { steps })
    }
}

/// A reference to a preset in a workflow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowStep {
    /// Preset category.
    pub kind: PresetKind,
    /// Preset name.
    pub name: BString,
}

impl Decode for WorkflowStep {
    fn decode(field: &Field, expand: &mut Expand<'_>) -> Result<Self, PresetsError> {
        let mut reader = Reader::object(field, "Workflow Step")?;
        let kind = schema::choice(
            &reader.required("type")?,
            &[
                ("configure", PresetKind::Configure),
                ("build", PresetKind::Build),
                ("test", PresetKind::Test),
                ("package", PresetKind::Package),
            ],
        )?;
        let name = BString::decode(&reader.required("name")?, expand)?;
        reader.finish()?;
        Ok(Self { kind, name })
    }
}
