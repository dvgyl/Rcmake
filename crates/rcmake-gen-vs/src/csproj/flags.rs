//! MSBuild properties for C# compiler flags.
//!
//! The table holds the switch facts of the CMake `v143_CSharp.json` flag
//! table. For example, `/debug:full` sets `DebugType` to `full`, and each
//! `/define:NAME` adds `NAME` to `DefineConstants`.

use std::collections::BTreeMap;

use crate::msbuild_value;

/// How one table entry reads a flag.
#[derive(Clone, Copy)]
enum Rule {
    /// The flag must be the switch. The property gets the fixed value.
    Fixed(&'static str),
    /// The flag starts with the switch, and the rest is not empty. The rest
    /// replaces the property value.
    Required,
    /// The flag starts with the switch. The rest is one more list item.
    Semicolon,
    /// The rest must not be empty. It is appended to the last item with `,`.
    Comma,
}

/// One flag table entry: property name, switch without `/` or `-`, rule, and
/// `true` when the search continues after a match.
type Entry = (&'static str, &'static str, Rule, bool);

const TABLE: &[Entry] = &[
    ("ProjectName", "out:", Rule::Required, false),
    ("OutputType", "target:exe", Rule::Fixed("Exe"), false),
    ("OutputType", "target:winexe", Rule::Fixed("Winexe"), false),
    (
        "OutputType",
        "target:library",
        Rule::Fixed("Library"),
        false,
    ),
    ("OutputType", "target:module", Rule::Fixed("Module"), false),
    ("DocumentationFile", "doc", Rule::Required, false),
    ("Platform", "platform:x86", Rule::Fixed("x86"), false),
    (
        "Platform",
        "platform:Itanium",
        Rule::Fixed("Itanium"),
        false,
    ),
    ("Platform", "platform:x64", Rule::Fixed("x64"), false),
    ("Platform", "platform:arm", Rule::Fixed("arm"), false),
    (
        "Platform",
        "platform:anycpu32bitpreferred",
        Rule::Fixed("anycpu32bitpreferred"),
        false,
    ),
    ("Platform", "platform:anycpu", Rule::Fixed("anycpu"), false),
    ("References", "reference:", Rule::Fixed(""), false),
    ("AddModules", "addmodule:", Rule::Fixed(""), false),
    ("Win32Resource", "win32res:", Rule::Required, false),
    ("ApplicationIcon", "win32icon:", Rule::Required, false),
    (
        "ApplicationManifest",
        "win32manifest:",
        Rule::Required,
        false,
    ),
    (
        "NoWin32Manifest",
        "nowin32manifest",
        Rule::Fixed("true"),
        false,
    ),
    ("DefineDebug", "debug", Rule::Fixed("true"), true),
    ("DebugSymbols", "debug", Rule::Fixed("true"), false),
    ("DebugSymbols", "debug-", Rule::Fixed("false"), false),
    ("DebugSymbols", "debug+", Rule::Fixed("true"), false),
    ("DebugType", "debug:none", Rule::Fixed("none"), false),
    ("DebugType", "debug:full", Rule::Fixed("full"), false),
    ("DebugType", "debug:pdbonly", Rule::Fixed("pdbonly"), false),
    (
        "DebugType",
        "debug:portable",
        Rule::Fixed("portable"),
        false,
    ),
    (
        "DebugType",
        "debug:embedded",
        Rule::Fixed("embedded"),
        false,
    ),
    ("Optimize", "optimize", Rule::Fixed("true"), false),
    ("Optimize", "optimize-", Rule::Fixed("false"), false),
    ("Optimize", "optimize+", Rule::Fixed("true"), false),
    (
        "TreatWarningsAsErrors",
        "warnaserror",
        Rule::Fixed("true"),
        false,
    ),
    (
        "TreatWarningsAsErrors",
        "warnaserror-",
        Rule::Fixed("false"),
        false,
    ),
    (
        "TreatWarningsAsErrors",
        "warnaserror+",
        Rule::Fixed("true"),
        false,
    ),
    ("WarningsAsErrors", "warnaserror:", Rule::Comma, false),
    ("WarningLevel", "warn:0", Rule::Fixed("0"), false),
    ("WarningLevel", "warn:1", Rule::Fixed("1"), false),
    ("WarningLevel", "warn:2", Rule::Fixed("2"), false),
    ("WarningLevel", "warn:3", Rule::Fixed("3"), false),
    ("WarningLevel", "warn:4", Rule::Fixed("4"), false),
    ("NoWarn", "nowarn:", Rule::Comma, false),
    (
        "CheckForOverflowUnderflow",
        "checked",
        Rule::Fixed("true"),
        false,
    ),
    (
        "CheckForOverflowUnderflow",
        "checked-",
        Rule::Fixed("false"),
        false,
    ),
    (
        "CheckForOverflowUnderflow",
        "checked+",
        Rule::Fixed("true"),
        false,
    ),
    ("AllowUnsafeBlocks", "unsafe", Rule::Fixed("true"), false),
    ("AllowUnsafeBlocks", "unsafe-", Rule::Fixed("false"), false),
    ("AllowUnsafeBlocks", "unsafe+", Rule::Fixed("true"), false),
    ("DefineConstants", "define:", Rule::Semicolon, false),
    ("LangVersion", "langversion:", Rule::Required, false),
    ("DelaySign", "delaysign", Rule::Fixed("true"), false),
    ("DelaySign", "delaysign-", Rule::Fixed("false"), false),
    ("DelaySign", "delaysign+", Rule::Fixed("true"), false),
    (
        "AssemblyOriginatorKeyFile",
        "keyfile",
        Rule::Fixed(""),
        false,
    ),
    ("KeyContainerName", "keycontainer", Rule::Fixed(""), false),
    ("NoLogo", "nologo", Rule::Fixed(""), false),
    ("NoConfig", "noconfig", Rule::Fixed("true"), false),
    ("BaseAddress", "baseaddress:", Rule::Fixed(""), false),
    ("CodePage", "codepage", Rule::Fixed(""), false),
    ("Utf8Output", "utf8output", Rule::Fixed(""), false),
    ("MainEntryPoint", "main:", Rule::Fixed(""), false),
    ("GenerateFullPaths", "fullpaths", Rule::Fixed("true"), false),
    ("FileAlignment", "filealign", Rule::Fixed(""), false),
    ("PdbFile", "pdb:", Rule::Fixed(""), false),
    ("NoStdLib", "nostdlib", Rule::Fixed("true"), false),
    ("NoStdLib", "nostdlib-", Rule::Fixed("false"), false),
    ("NoStdLib", "nostdlib+", Rule::Fixed("true"), false),
    (
        "SubsystemVersion",
        "subsystemversion",
        Rule::Fixed(""),
        false,
    ),
    ("AdditionalLibPaths", "lib:", Rule::Fixed(""), false),
    (
        "ErrorReport",
        "errorreport:none",
        Rule::Fixed("none"),
        false,
    ),
    (
        "ErrorReport",
        "errorreport:prompt",
        Rule::Fixed("prompt"),
        false,
    ),
    (
        "ErrorReport",
        "errorreport:queue",
        Rule::Fixed("queue"),
        false,
    ),
    (
        "ErrorReport",
        "errorreport:send",
        Rule::Fixed("send"),
        false,
    ),
];

/// MSBuild properties of one C# configuration, in property name order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Flags {
    values: BTreeMap<&'static str, Vec<String>>,
}

impl Flags {
    /// Read the flags in command-line order. A flag that the table does not
    /// know has no property.
    pub(crate) fn parse(flags: impl IntoIterator<Item = String>) -> Self {
        let mut out = Self::default();
        for flag in flags {
            if let Some(body) = flag.strip_prefix(['/', '-']) {
                out.flag(body);
            }
        }
        out
    }

    fn flag(&mut self, body: &str) {
        for &(name, switch, rule, more) in TABLE {
            let found = match rule {
                Rule::Fixed(value) => {
                    let found = body == switch;
                    if found {
                        self.values.insert(name, vec![value.to_owned()]);
                    }
                    found
                }
                Rule::Required | Rule::Semicolon | Rule::Comma => match body.strip_prefix(switch) {
                    Some(rest) if !rest.is_empty() || matches!(rule, Rule::Semicolon) => {
                        self.update(name, rule, rest);
                        true
                    }
                    _ => false,
                },
            };
            if found && !more {
                return;
            }
        }
    }

    fn update(&mut self, name: &'static str, rule: Rule, value: &str) {
        let values = self.values.entry(name).or_default();
        match rule {
            Rule::Semicolon => values.push(value.to_owned()),
            Rule::Comma => match values.last_mut() {
                Some(last) if !last.is_empty() => {
                    last.push(',');
                    last.push_str(value);
                }
                Some(last) => value.clone_into(last),
                None => values.push(value.to_owned()),
            },
            Rule::Fixed(_) | Rule::Required => *values = vec![value.to_owned()],
        }
    }

    /// Add list items to a property. The property exists after the call, also
    /// when `items` is empty, as in CMake.
    pub(crate) fn append(&mut self, name: &'static str, items: impl IntoIterator<Item = String>) {
        self.values.entry(name).or_default().extend(items);
    }

    /// Remove a property and return its MSBuild value.
    pub(crate) fn take(&mut self, name: &str) -> Option<String> {
        self.values.remove(name).map(|values| join(&values))
    }

    /// Return `true` when `DebugType` has one value that is not `none`.
    pub(crate) fn debug_info(&self) -> bool {
        matches!(
            self.values.get("DebugType").map(Vec::as_slice),
            Some([value]) if value != "none"
        )
    }

    /// Return each property and its MSBuild value. Each list item is escaped
    /// for MSBuild, and the items are joined with `;`.
    pub(crate) fn properties(&self) -> impl Iterator<Item = (&'static str, String)> + '_ {
        self.values
            .iter()
            .map(|(&name, values)| (name, join(values)))
    }
}

fn join(values: &[String]) -> String {
    values
        .iter()
        .map(|value| msbuild_value(value))
        .collect::<Vec<_>>()
        .join(";")
}

#[cfg(test)]
mod tests {
    use super::Flags;

    fn parse(flags: &str) -> Flags {
        Flags::parse(flags.split_whitespace().map(str::to_owned))
    }

    fn properties(flags: &Flags) -> Vec<(&'static str, String)> {
        flags.properties().collect()
    }

    #[test]
    fn default_debug_flags_map_to_properties() {
        let mut flags = parse(
            "/platform:x64 /define:TRACE /debug:full /optimize- /warn:3 /errorreport:prompt /define:DEBUG",
        );
        flags.append("DefineConstants", ["MY_FOO".to_owned()]);
        assert!(flags.debug_info());
        assert_eq!(
            properties(&flags),
            [
                ("DebugType", "full".to_owned()),
                ("DefineConstants", "TRACE;DEBUG;MY_FOO".to_owned()),
                ("ErrorReport", "prompt".to_owned()),
                ("Optimize", "false".to_owned()),
                ("Platform", "x64".to_owned()),
                ("WarningLevel", "3".to_owned()),
            ]
        );
        assert_eq!(flags.take("Platform").as_deref(), Some("x64"));
        assert_eq!(flags.take("Platform"), None);
    }

    #[test]
    fn debug_switch_continues_to_the_next_entry() {
        let flags = parse("-debug /unknown /debug:none");
        assert!(!flags.debug_info());
        assert_eq!(
            properties(&flags),
            [
                ("DebugSymbols", "true".to_owned()),
                ("DebugType", "none".to_owned()),
                ("DefineDebug", "true".to_owned()),
            ]
        );
    }

    #[test]
    fn user_values_follow_the_table_rules() {
        let flags = parse(
            "/out: /langversion:latest /nowarn:1591 /nowarn:0168 /warnaserror /define:A;B /define:",
        );
        assert_eq!(
            properties(&flags),
            [
                ("DefineConstants", "A%3BB;".to_owned()),
                ("LangVersion", "latest".to_owned()),
                ("NoWarn", "1591,0168".to_owned()),
                ("TreatWarningsAsErrors", "true".to_owned()),
            ]
        );
    }

    #[test]
    fn appending_no_items_keeps_an_empty_property() {
        let mut flags = Flags::default();
        flags.append("DefineConstants", Vec::new());
        assert_eq!(properties(&flags), [("DefineConstants", String::new())]);
    }
}
