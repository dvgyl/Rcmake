// SPDX-License-Identifier: MIT OR Apache-2.0

//! Evaluate and write `file(GENERATE ...)` outputs inside a `try_compile` child build.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use jiff::{Timestamp, fmt::strtime, tz::TimeZone};
use rcmake_base::{BString, Limits, truth};
use rcmake_diag::{Backtrace, Frame, FrameKind, PolicyStatus};
use rcmake_genex::{ConfigId, EvalContext, EvalSite, GenexError, GenexHost, evaluate, parse};
use rcmake_host::{
    FileKind, Follow, Host, ReadRange, WriteMode,
    path::{from_native_cmake, to_native},
};
use rcmake_model::{DirId, ProjectModel, PropertyMap};
use rcmake_resolve::{
    GenerationResult,
    file_sets::{file_generate_contexts, file_generate_policy},
    usage::{ObjectPaths, UsageResolver},
};

#[derive(Clone, Copy)]
enum NewlineStyle {
    Native,
    Lf,
    CrLf,
}

#[derive(Clone, Copy)]
enum Permissions {
    Default,
    NoSource,
    Explicit(u32),
}

enum InputSource<'a> {
    File(&'a [u8]),
    Content(&'a [u8]),
}

struct GenerateRequest<'a> {
    dir: DirId,
    backtrace: Backtrace,
    cmp0189: Option<bool>,
    input_base: PathBuf,
    output_base: PathBuf,
    output: &'a [u8],
    input: InputSource<'a>,
    condition: Option<&'a [u8]>,
    target: Option<&'a [u8]>,
    permissions: Permissions,
    newline: NewlineStyle,
}

/// Evaluate all `file(GENERATE)` entries recorded in `model` and write their outputs.
pub(super) fn write(
    model: &ProjectModel,
    result: &GenerationResult,
    host: &dyn Host,
) -> Result<(), String> {
    let props = &model.global.props;
    let Some(count) = props.get(b"_RCMAKE_FILE_GENERATE_COUNT") else {
        return Ok(());
    };
    let count = parse_index(count).ok_or_else(|| "Invalid file(GENERATE) count.".to_owned())?;
    let dirs: Vec<_> = model
        .dirs()
        .map(|(id, dir)| (id, to_native(&dir.source_dir), to_native(&dir.binary_dir)))
        .collect();
    let cwd = host.initial_cwd();
    let mut requests = Vec::with_capacity(count.min(4096));

    for request_idx in 0..count {
        let section = format!("_RCMAKE_FILE_GENERATE_{request_idx}");
        let number = |key: &str| -> Result<usize, String> {
            props
                .get(format!("{section}_{key}").as_bytes())
                .and_then(|value| parse_index(value))
                .ok_or_else(|| "Invalid file(GENERATE) metadata.".to_owned())
        };
        let dir_idx = number("DIRECTORY")?;
        let (dir_id, source, binary) = dirs
            .get(dir_idx)
            .ok_or_else(|| "Invalid file(GENERATE) directory.".to_owned())?;
        let argc = number("ARGC")?;
        let mut args = Vec::with_capacity(argc.min(64));
        for arg in 0..argc {
            let value = props
                .get(format!("{section}_ARG{arg}").as_bytes())
                .map(|value| value.as_slice())
                .ok_or_else(|| "Invalid file(GENERATE) argument.".to_owned())?;
            args.push(value);
        }
        let [b"OUTPUT", output, input_kind, input, options @ ..] = args.as_slice() else {
            return Err("Invalid file(GENERATE) request.".to_owned());
        };
        let policy = file_generate_policy(model, *dir_id, request_idx);
        let use_directory = policy == PolicyStatus::New;
        let mut req = GenerateRequest {
            dir: *dir_id,
            backtrace: origin(props, &section),
            cmp0189: props
                .get(format!("{section}_CMP0189").as_bytes())
                .map(|value| value.as_slice() == b"New"),
            input_base: if use_directory {
                source.clone()
            } else {
                cwd.clone()
            },
            output_base: if use_directory {
                binary.clone()
            } else {
                cwd.clone()
            },
            output,
            input: match *input_kind {
                b"INPUT" => InputSource::File(input),
                b"CONTENT" => InputSource::Content(input),
                _ => return Err("Invalid file(GENERATE) input kind.".to_owned()),
            },
            condition: None,
            target: None,
            permissions: Permissions::Default,
            newline: NewlineStyle::Native,
        };
        apply_options(&mut req, options)?;
        requests.push(req);
    }

    let eval_contexts = file_generate_contexts(model, &result.configs);
    let mut usage = UsageResolver::new(model).with_traced_sources(&result.traced_sources);
    let objects = ObjectPaths::new(result.targets.iter());
    let timestamp = |format: Option<&[u8]>, utc| format_timestamp(host, format, utc);
    let mut outputs: BTreeMap<PathBuf, (usize, Vec<u8>, Option<u32>)> = BTreeMap::new();

    for (req_index, req) in requests.iter().enumerate() {
        let (raw_input, source_mode) = match req.input {
            InputSource::Content(bytes) => (bytes.to_vec(), None),
            InputSource::File(path) => {
                let path = resolve_path(&req.input_base, path);
                let mode = match req.permissions {
                    Permissions::Default => host
                        .metadata(&path, Follow::Yes)
                        .ok()
                        .filter(|meta| meta.kind == FileKind::File)
                        .map(|meta| meta.mode & 0o7777),
                    Permissions::NoSource | Permissions::Explicit(_) => None,
                };
                let bytes = host
                    .read(
                        &path,
                        ReadRange::All {
                            limit: Limits::default().max_file_read,
                        },
                    )
                    .map_err(|err| {
                        format!(
                            "file(GENERATE) failed to read \"{}\": {err}",
                            from_native_cmake(&path)
                        )
                    })?;
                (bytes, mode)
            }
        };
        let target_mode = match req.permissions {
            Permissions::Default => source_mode,
            Permissions::NoSource => None,
            Permissions::Explicit(mode) => Some(mode),
        };

        for eval_ctx in &eval_contexts {
            let mut genex_host = usage
                .host(req.dir)
                .with_object_paths(&objects)
                .with_clock(host, &timestamp);
            let target = req
                .target
                .map(|name| {
                    genex_host.find_target(name).ok_or_else(|| {
                        format!("Unknown target \"{}\".", String::from_utf8_lossy(name))
                    })
                })
                .transpose()?;
            let policies: Vec<(&'static [u8], bool)> = req
                .cmp0189
                .map(|new| (b"CMP0189".as_slice(), new))
                .into_iter()
                .collect();
            let mut policy_host = CallSitePolicyHost {
                inner: &mut genex_host,
                policies: &policies,
            };
            let ctx = EvalContext {
                config: eval_ctx.config.clone(),
                language: eval_ctx.language.clone(),
                current: target,
                head: target,
                site: EvalSite {
                    backtrace: req.backtrace.clone(),
                    property: None,
                },
                ..EvalContext::default()
            };
            if let Some(condition) = req.condition {
                let value = eval_genex(condition, &ctx, &mut policy_host)?;
                match value.as_slice() {
                    b"0" => continue,
                    b"1" => {}
                    _ => {
                        return Err(format!(
                            "Evaluation file condition \"{}\" did not evaluate to valid content. Got \"{value}\".",
                            BString::from(condition),
                        ));
                    }
                }
            }
            let output_str = eval_genex(req.output, &ctx, &mut policy_host)?;
            if output_str.is_empty() {
                continue;
            }
            let output_path = resolve_path(&req.output_base, &output_str);
            let mut content = eval_genex(&raw_input, &ctx, &mut policy_host)?;
            if matches!(req.input, InputSource::Content(_))
                && !matches!(req.newline, NewlineStyle::Native)
                && !content.is_empty()
                && !content.ends_with(b"\n")
            {
                content.push(b'\n');
            }
            let normalized = apply_newlines(content.as_slice(), req.newline);
            if let Some((prev_req, prev_bytes, _)) = outputs.get(&output_path) {
                if *prev_req != req_index {
                    return Err(format!(
                        "Files to be generated by multiple different commands: \"{}\"",
                        from_native_cmake(&output_path)
                    ));
                }
                if *prev_bytes != normalized {
                    return Err(format!(
                        "Evaluation file to be written multiple times with different content: \"{}\"",
                        from_native_cmake(&output_path)
                    ));
                }
            } else {
                outputs.insert(output_path, (req_index, normalized, target_mode));
            }
        }
    }

    if let Some((_, path)) = result.file_generate_cycles.first() {
        return Err(format!(
            "Evaluation output file \"{path}\" depends on the sources of a target it is used in."
        ));
    }

    for (path, (_, bytes, mode)) in outputs {
        let unchanged = host
            .read(
                &path,
                ReadRange::All {
                    limit: (bytes.len() as u64).saturating_add(1),
                },
            )
            .is_ok_and(|existing| existing == bytes);
        if !unchanged {
            if let Some(parent) = path.parent() {
                host.create_dir_all(parent).map_err(|err| err.to_string())?;
            }
            host.write(&path, &bytes, WriteMode::Truncate)
                .map_err(|err| err.to_string())?;
        }
        if let Some(mode) = mode {
            let _ = host.set_permissions(&path, mode);
        }
    }

    let _ = truth::is_on;
    Ok(())
}

fn resolve_path(base: &Path, raw: &[u8]) -> PathBuf {
    let path = to_native(raw);
    let joined = if path.is_absolute() {
        path
    } else {
        base.join(path)
    };
    let cmake = from_native_cmake(&joined);
    to_native(&rcmake_base::path_lex::Lexical::new(&cmake).normalize())
}

fn eval_genex(
    value: &[u8],
    ctx: &EvalContext,
    host: &mut dyn GenexHost,
) -> Result<BString, String> {
    parse(value)
        .and_then(|ast| evaluate(&ast, ctx, host))
        .map(|(out, _)| out)
        .map_err(|err| match err {
            GenexError::Generation { message } => message.to_string(),
            other => other.to_string(),
        })
}

fn apply_newlines(bytes: &[u8], style: NewlineStyle) -> Vec<u8> {
    match style {
        NewlineStyle::Native => bytes.to_vec(),
        NewlineStyle::Lf => {
            let mut out = Vec::with_capacity(bytes.len());
            let mut i = 0;
            while let Some(&b) = bytes.get(i) {
                if b == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
                    out.push(b'\n');
                    i += 2;
                } else {
                    out.push(b);
                    i += 1;
                }
            }
            out
        }
        NewlineStyle::CrLf => {
            let mut out = Vec::with_capacity(bytes.len());
            let mut i = 0;
            while let Some(&b) = bytes.get(i) {
                if b == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
                    out.extend_from_slice(b"\r\n");
                    i += 2;
                } else if b == b'\n' {
                    out.extend_from_slice(b"\r\n");
                    i += 1;
                } else {
                    out.push(b);
                    i += 1;
                }
            }
            out
        }
    }
}

struct CallSitePolicyHost<'a> {
    inner: &'a mut dyn GenexHost,
    policies: &'a [(&'static [u8], bool)],
}

impl GenexHost for CallSitePolicyHost<'_> {
    fn find_target(&self, name: &[u8]) -> Option<rcmake_genex::TargetHandle> {
        self.inner.find_target(name)
    }

    fn find_target_in_context(
        &self,
        name: &[u8],
        ctx: &EvalContext,
    ) -> Option<rcmake_genex::TargetHandle> {
        self.inner.find_target_in_context(name, ctx)
    }

    fn target_name(&self, target: rcmake_genex::TargetHandle) -> BString {
        self.inner.target_name(target)
    }

    fn alias_property(&self, name: &[u8], property: &[u8], ctx: &EvalContext) -> Option<BString> {
        self.inner.alias_property(name, property, ctx)
    }

    fn source_property(
        &self,
        source: &[u8],
        scope: Option<(&[u8], &[u8])>,
        property: &[u8],
        ctx: &EvalContext,
    ) -> Result<BString, BString> {
        self.inner.source_property(source, scope, property, ctx)
    }

    fn source_exists(
        &self,
        source: &[u8],
        scope: Option<(&[u8], &[u8])>,
        ctx: &EvalContext,
    ) -> Result<bool, BString> {
        self.inner.source_exists(source, scope, ctx)
    }

    fn file_set_exists(&self, target: rcmake_genex::TargetHandle, name: &[u8]) -> bool {
        self.inner.file_set_exists(target, name)
    }

    fn file_set_property(
        &self,
        target: rcmake_genex::TargetHandle,
        name: &[u8],
        property: &[u8],
    ) -> Option<BString> {
        self.inner.file_set_property(target, name, property)
    }

    fn global_property(&self, name: &[u8]) -> Option<BString> {
        self.inner.global_property(name)
    }

    fn directory_property(
        &self,
        directory: &[u8],
        property: &[u8],
        ctx: &EvalContext,
    ) -> Result<Option<BString>, BString> {
        self.inner.directory_property(directory, property, ctx)
    }

    fn directory_policy(&self, policy: &[u8], ctx: &EvalContext) -> Option<bool> {
        self.policies
            .iter()
            .find(|(name, _)| *name == policy)
            .map(|(_, new)| *new)
            .or_else(|| self.inner.directory_policy(policy, ctx))
    }

    fn directory_policy_status(
        &self,
        policy: rcmake_diag::PolicyId,
        ctx: &EvalContext,
    ) -> Option<PolicyStatus> {
        self.inner.directory_policy_status(policy, ctx)
    }

    fn raw_property(&self, target: rcmake_genex::TargetHandle, name: &[u8]) -> Option<BString> {
        self.inner.raw_property(target, name)
    }

    fn scalar_property(
        &mut self,
        target: rcmake_genex::TargetHandle,
        name: &[u8],
        ctx: &EvalContext,
    ) -> Result<rcmake_genex::InterfaceView, GenexError> {
        self.inner.scalar_property(target, name, ctx)
    }

    fn transitive_property(
        &mut self,
        target: rcmake_genex::TargetHandle,
        name: &[u8],
        ctx: &EvalContext,
    ) -> Result<rcmake_genex::InterfaceView, GenexError> {
        self.inner.transitive_property(target, name, ctx)
    }

    fn artifact(
        &self,
        target: rcmake_genex::TargetHandle,
        kind: rcmake_genex::ArtifactKind,
        part: rcmake_genex::ArtifactPart,
        config: &ConfigId,
    ) -> Option<BString> {
        self.inner.artifact(target, kind, part, config)
    }

    fn checked_artifact(
        &self,
        target: rcmake_genex::TargetHandle,
        kind: rcmake_genex::ArtifactKind,
        part: rcmake_genex::ArtifactPart,
        config: &ConfigId,
    ) -> Result<Option<BString>, GenexError> {
        self.inner.checked_artifact(target, kind, part, config)
    }

    fn link_closure(
        &mut self,
        target: rcmake_genex::TargetHandle,
        ctx: &EvalContext,
    ) -> Result<Vec<rcmake_genex::TargetHandle>, GenexError> {
        self.inner.link_closure(target, ctx)
    }

    fn definition(&self, name: &[u8]) -> Option<BString> {
        self.inner.definition(name)
    }

    fn language_enabled(&self, language: &[u8]) -> Option<bool> {
        self.inner.language_enabled(language)
    }

    fn imported_config_matches(
        &self,
        target: rcmake_genex::TargetHandle,
        head: Option<rcmake_genex::TargetHandle>,
        consumer: &ConfigId,
    ) -> Option<Vec<BString>> {
        self.inner.imported_config_matches(target, head, consumer)
    }

    fn target_policy(&self, target: rcmake_genex::TargetHandle, policy: &[u8]) -> Option<bool> {
        self.inner.target_policy(target, policy)
    }

    fn target_objects(
        &self,
        target: rcmake_genex::TargetHandle,
        config: &ConfigId,
        sources: Option<&[u8]>,
    ) -> Option<BString> {
        self.inner.target_objects(target, config, sources)
    }

    fn target_intermediate_directory(
        &self,
        target: rcmake_genex::TargetHandle,
        config: &ConfigId,
    ) -> Option<BString> {
        self.inner.target_intermediate_directory(target, config)
    }

    fn artifact_base_name_without_postfix(
        &self,
        target: rcmake_genex::TargetHandle,
        kind: rcmake_genex::ArtifactKind,
        config: &ConfigId,
    ) -> Option<BString> {
        self.inner
            .artifact_base_name_without_postfix(target, kind, config)
    }

    fn is_device_link(&self, ctx: &EvalContext) -> bool {
        self.inner.is_device_link(ctx)
    }

    fn install_prefix(&self, ctx: &EvalContext) -> Option<BString> {
        self.inner.install_prefix(ctx)
    }

    fn timestamp(&self, format: Option<&[u8]>, utc: bool) -> Option<BString> {
        self.inner.timestamp(format, utc)
    }

    fn random_seed(&self) -> Option<u64> {
        self.inner.random_seed()
    }

    fn limits(&self) -> Limits {
        self.inner.limits()
    }
}

fn format_timestamp(host: &dyn Host, format: Option<&[u8]>, utc: bool) -> Option<BString> {
    let format = format
        .filter(|format| !format.is_empty())
        .unwrap_or(if utc {
            b"%Y-%m-%dT%H:%M:%SZ"
        } else {
            b"%Y-%m-%dT%H:%M:%S"
        });
    let (secs, nanos) = if let Some(epoch) = host
        .env_get(b"SOURCE_DATE_EPOCH")
        .filter(|epoch| !epoch.is_empty())
    {
        let secs = rcmake_base::parse_source_date_epoch(&epoch)?;
        if secs == -1 {
            return Some(BString::default());
        }
        (secs, 0)
    } else {
        let now = host.now();
        (now.secs, i32::try_from(now.nanos).ok()?)
    };
    let instant = Timestamp::new(secs, nanos).ok()?;
    let zone = if utc { TimeZone::UTC } else { host.time_zone() };
    let zoned = instant.to_zoned(zone);
    let mut result = BString::default();
    let mut bytes = format.iter().copied();
    while let Some(byte) = bytes.next() {
        if byte != b'%' {
            result.push(byte);
            continue;
        }
        match bytes.next() {
            Some(b'%') | None => result.push(b'%'),
            Some(b'f') => result.extend_from_slice(format!("{:06}", nanos / 1000).as_bytes()),
            Some(b's') => result.extend_from_slice(secs.to_string().as_bytes()),
            Some(b'Z') if utc => result.extend_from_slice(b"GMT"),
            Some(b'Z') => result.extend_from_slice(
                zoned
                    .time_zone()
                    .to_offset_info(instant)
                    .abbreviation()
                    .as_bytes(),
            ),
            Some(
                spec @ (b'd' | b'H' | b'I' | b'j' | b'm' | b'b' | b'B' | b'M' | b'S' | b'U' | b'V'
                | b'w' | b'a' | b'A' | b'y' | b'Y' | b'z'),
            ) => result.extend_from_slice(strtime::format([b'%', spec], &zoned).ok()?.as_bytes()),
            Some(spec) => result.extend_from_slice(&[b'%', spec]),
        }
    }
    Some(result)
}

fn origin(props: &PropertyMap, section: &str) -> Backtrace {
    let frames: Vec<_> = (0..)
        .map_while(|frame| {
            let key = format!("{section}_FRAME_{frame}");
            let get = |field: &str| props.get(format!("{key}_{field}").as_bytes());
            Some(Frame {
                file: get("FILE")?.as_slice().into(),
                line: u32::try_from(parse_index(get("LINE")?)?).ok()?,
                name: get("NAME").map(|name| name.as_slice().into()),
                kind: if get("DEFERRED").is_some() {
                    FrameKind::Deferred
                } else {
                    FrameKind::Command
                },
            })
        })
        .collect();
    frames
        .into_iter()
        .rev()
        .fold(Backtrace::default(), |trace, frame| trace.push(frame))
}

fn parse_index(value: &[u8]) -> Option<usize> {
    std::str::from_utf8(value).ok()?.parse().ok()
}

fn apply_options<'a>(
    request: &mut GenerateRequest<'a>,
    options: &[&'a [u8]],
) -> Result<(), String> {
    let mut iter = options.iter().copied().peekable();
    while let Some(option) = iter.next() {
        match option {
            b"CONDITION" => {
                request.condition = Some(
                    iter.next()
                        .ok_or_else(|| "Missing CONDITION value.".to_owned())?,
                );
            }
            b"TARGET" => {
                request.target = Some(
                    iter.next()
                        .ok_or_else(|| "Missing TARGET value.".to_owned())?,
                );
            }
            b"NEWLINE_STYLE" => {
                request.newline = match iter.next() {
                    Some(b"LF" | b"UNIX") => NewlineStyle::Lf,
                    Some(b"CRLF" | b"DOS" | b"WIN32") => NewlineStyle::CrLf,
                    _ => return Err("Invalid NEWLINE_STYLE value.".to_owned()),
                };
            }
            b"NO_SOURCE_PERMISSIONS" => request.permissions = Permissions::NoSource,
            b"USE_SOURCE_PERMISSIONS" => request.permissions = Permissions::Default,
            b"FILE_PERMISSIONS" => {
                let mut mode = 0;
                while let Some(bits) = iter.peek().and_then(|value| permission(value)) {
                    mode |= bits;
                    iter.next();
                }
                request.permissions = Permissions::Explicit(mode);
            }
            _ => return Err("Invalid file(GENERATE) option.".to_owned()),
        }
    }
    Ok(())
}

fn permission(value: &[u8]) -> Option<u32> {
    Some(match value {
        b"SETUID" => 0o4000,
        b"SETGID" => 0o2000,
        b"OWNER_READ" => 0o400,
        b"OWNER_WRITE" => 0o200,
        b"OWNER_EXECUTE" => 0o100,
        b"GROUP_READ" => 0o040,
        b"GROUP_WRITE" => 0o020,
        b"GROUP_EXECUTE" => 0o010,
        b"WORLD_READ" => 0o004,
        b"WORLD_WRITE" => 0o002,
        b"WORLD_EXECUTE" => 0o001,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcmake_host::PureHost;

    #[test]
    fn resolve_path_normalizes_dot_dot_segments() {
        let base = Path::new(if cfg!(windows) {
            "C:/work/build"
        } else {
            "/work/build"
        });
        assert_eq!(
            resolve_path(base, b"out.c"),
            resolve_path(base, b"sub/../out.c")
        );
        assert_eq!(
            resolve_path(base, b"./a/b/../out.c"),
            resolve_path(base, b"a/out.c")
        );
    }

    #[test]
    fn oversized_file_generate_count_errors_without_oom() -> Result<(), Box<dyn std::error::Error>>
    {
        let host = PureHost::default();
        let mut model = ProjectModel::default();
        model.global.props.set(
            BString::from("_RCMAKE_FILE_GENERATE_COUNT"),
            BString::from("999999999999"),
        );
        let result = GenerationResult::default();
        let Err(err) = write(&model, &result, &host) else {
            return Err("oversized count must fail cleanly".into());
        };
        assert!(err.contains("Invalid file(GENERATE)"));
        Ok(())
    }
}
