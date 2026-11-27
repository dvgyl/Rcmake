//! Typed configure-only request protocol between `rcmake verify` and `rcmake-verify`.

use std::path::PathBuf;

use rcmake_base::BString;
use rcmake_host::path;
use rcmake_json::{ParseBudget, ReadOptions, Value};

const REQUEST_PARSE_LIMIT: usize = 4 * 1024 * 1024;

/// Typed configure-only argument forwarded by `rcmake verify`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ForwardArg {
    /// Verbatim configure argument.
    Literal(BString),
    /// Flag followed by a path relative to the staged source root.
    FlagSourceRel { flag: BString, rel: BString },
    /// Attached `<prefix><staged_src>/<rel>` argument.
    PrefixSourceRel { prefix: BString, rel: BString },
}

/// Typed configure-only request read from stdin by `rcmake-verify`.
#[derive(Clone, Debug)]
pub(crate) struct VerifyRequest {
    pub(crate) rcmake_exe: PathBuf,
    pub(crate) oracle_exe: PathBuf,
    pub(crate) source_dir: PathBuf,
    pub(crate) excluded_build_dir: Option<PathBuf>,
    pub(crate) cwd_rel_to_source: Option<BString>,
    pub(crate) args: Vec<ForwardArg>,
    pub(crate) json: bool,
    pub(crate) request_compile_commands: bool,
    pub(crate) compile_commands_user_disabled: bool,
    pub(crate) generator_hint: Option<BString>,
    pub(crate) external_inputs: Vec<BString>,
}

impl VerifyRequest {
    pub(crate) fn parse_json(bytes: &[u8]) -> Result<Self, String> {
        let budget = ParseBudget::artifact(REQUEST_PARSE_LIMIT);
        let root = rcmake_json::parse_bounded(bytes, &ReadOptions::default(), &budget)
            .map_err(|err| format!("Invalid verify request JSON: {}", err.message))?;
        let version = root
            .get("schema_version")
            .and_then(Value::as_u64)
            .ok_or_else(|| "Missing schema_version in verify request.".to_owned())?;
        if version != 1 {
            return Err(format!(
                "Unsupported verify request schema_version {version}."
            ));
        }
        let req_path = |key: &str| -> Result<PathBuf, String> {
            let raw = root
                .get(key)
                .and_then(Value::as_bytes)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| format!("Missing required field '{key}' in verify request."))?;
            let native = path::to_native(raw);
            if !native.is_absolute() {
                return Err(format!("Field '{key}' must be an absolute path."));
            }
            Ok(native)
        };
        let rcmake_exe = req_path("rcmake_exe")?;
        let oracle_exe = req_path("oracle_exe")?;
        let source_dir = req_path("source_dir")?;
        let excluded_build_dir = root
            .get("excluded_build_dir")
            .and_then(Value::as_bytes)
            .filter(|v| !v.is_empty())
            .map(path::to_native);
        let cwd_rel_to_source = root
            .get("cwd_rel_to_source")
            .and_then(Value::as_bytes)
            .map(BString::from);
        let json = root.get("json").and_then(Value::as_bool).unwrap_or(false);
        let request_compile_commands = root
            .get("request_compile_commands")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let compile_commands_user_disabled = root
            .get("compile_commands_user_disabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let generator_hint = root
            .get("generator_hint")
            .and_then(Value::as_bytes)
            .filter(|v| !v.is_empty())
            .map(BString::from);
        let external_inputs = root
            .get("external_inputs")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_bytes)
                    .map(BString::from)
                    .collect()
            })
            .unwrap_or_default();

        let mut args = Vec::new();
        if let Some(items) = root.get("args").and_then(Value::as_array) {
            for item in items {
                let kind = item
                    .get("kind")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "Missing arg kind in verify request.".to_owned())?;
                match kind {
                    "literal" => {
                        let val = item
                            .get("value")
                            .and_then(Value::as_bytes)
                            .ok_or_else(|| "Missing literal arg value.".to_owned())?;
                        args.push(ForwardArg::Literal(BString::from(val)));
                    }
                    "flag_source_rel" => {
                        let flag = item
                            .get("flag")
                            .and_then(Value::as_bytes)
                            .ok_or_else(|| "Missing flag_source_rel flag.".to_owned())?;
                        let rel = item
                            .get("rel")
                            .and_then(Value::as_bytes)
                            .ok_or_else(|| "Missing flag_source_rel rel.".to_owned())?;
                        validate_relative(rel)?;
                        args.push(ForwardArg::FlagSourceRel {
                            flag: BString::from(flag),
                            rel: BString::from(rel),
                        });
                    }
                    "prefix_source_rel" => {
                        let prefix = item
                            .get("prefix")
                            .and_then(Value::as_bytes)
                            .ok_or_else(|| "Missing prefix_source_rel prefix.".to_owned())?;
                        let rel = item
                            .get("rel")
                            .and_then(Value::as_bytes)
                            .ok_or_else(|| "Missing prefix_source_rel rel.".to_owned())?;
                        validate_relative(rel)?;
                        args.push(ForwardArg::PrefixSourceRel {
                            prefix: BString::from(prefix),
                            rel: BString::from(rel),
                        });
                    }
                    other => {
                        return Err(format!("Unknown verify arg kind '{other}'."));
                    }
                }
            }
        }

        Ok(Self {
            rcmake_exe,
            oracle_exe,
            source_dir,
            excluded_build_dir,
            cwd_rel_to_source,
            args,
            json,
            request_compile_commands,
            compile_commands_user_disabled,
            generator_hint,
            external_inputs,
        })
    }

    pub(crate) fn materialize_args(
        &self,
        staged_src: &std::path::Path,
        staged_bin: &std::path::Path,
        inject_compile_commands: bool,
    ) -> Vec<BString> {
        let mut out = Vec::with_capacity(self.args.len() + 6);
        out.push(BString::from("-S"));
        out.push(path::from_native_cmake(staged_src));
        out.push(BString::from("-B"));
        out.push(path::from_native_cmake(staged_bin));
        for arg in &self.args {
            match arg {
                ForwardArg::Literal(v) => out.push(v.clone()),
                ForwardArg::FlagSourceRel { flag, rel } => {
                    let joined = staged_src.join(path::to_native(rel));
                    out.push(flag.clone());
                    out.push(path::from_native_cmake(&joined));
                }
                ForwardArg::PrefixSourceRel { prefix, rel } => {
                    let joined = staged_src.join(path::to_native(rel));
                    let mut combined = prefix.clone();
                    combined.extend_from_slice(&path::from_native_cmake(&joined));
                    out.push(combined);
                }
            }
        }
        if inject_compile_commands {
            out.push(BString::from("-DCMAKE_EXPORT_COMPILE_COMMANDS:BOOL=ON"));
        }
        out
    }
}

fn validate_relative(rel: &[u8]) -> Result<(), String> {
    let native = path::to_native(rel);
    if native.is_absolute()
        || native.components().any(|c| {
            !matches!(
                c,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
    {
        return Err(format!(
            "Source-relative path '{}' must stay inside the staged source directory.",
            String::from_utf8_lossy(rel)
        ));
    }
    Ok(())
}
