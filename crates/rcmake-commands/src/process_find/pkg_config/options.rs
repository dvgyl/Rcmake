use std::collections::BTreeMap;

use rcmake_base::{BString, list, truth};
use rcmake_eval::{CmdError, Cx};
use rcmake_expr::ExpandedArg;

use super::{pc::Strictness, status, text};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    Extract,
    Import,
    Populate,
}

pub(super) struct Request<'a> {
    pub mode: Mode,
    pub package: &'a [u8],
    pub version: &'a [u8],
    options: BTreeMap<&'a [u8], Vec<&'a [u8]>>,
}

impl<'a> Request<'a> {
    pub(super) fn parse(args: &'a [ExpandedArg]) -> Result<Self, CmdError> {
        let [mode, package, tail @ ..] = args else {
            return Err(status("requires a subcommand and a package name."));
        };
        let mode = match mode.value.as_slice() {
            b"EXTRACT" => Mode::Extract,
            b"IMPORT" => Mode::Import,
            b"POPULATE" => Mode::Populate,
            other => {
                return Err(status(format!(
                    "given unknown subcommand {:?}.",
                    BString::from(other)
                )));
            }
        };
        let mut tail = tail.iter().map(|arg| arg.value.as_slice()).peekable();
        let version = tail.next_if(|arg| arity(arg).is_none()).unwrap_or_default();
        let mut options = BTreeMap::new();
        while let Some(key) = tail.next() {
            let Some(arity) = arity(key) else {
                return Err(status(format!(
                    "given unknown argument {:?}.",
                    BString::from(key)
                )));
            };
            let mut values = Vec::new();
            match arity {
                Arity::Flag => {}
                Arity::One => values.extend(tail.next_if(|arg| self::arity(arg).is_none())),
                Arity::Many => {
                    while let Some(value) = tail.next_if(|arg| self::arity(arg).is_none()) {
                        values.push(value);
                    }
                }
            }
            if matches!(arity, Arity::One) && values.is_empty() {
                return Err(status(format!(
                    "missing value for {:?}.",
                    BString::from(key)
                )));
            }
            options.insert(key, values);
        }
        Ok(Self {
            mode,
            package: &package.value,
            version,
            options,
        })
    }

    pub(super) fn flag(&self, key: &[u8]) -> bool {
        self.options.contains_key(key)
    }

    pub(super) fn value(&self, key: &[u8]) -> Option<&'a [u8]> {
        self.options.get(key)?.first().copied()
    }

    pub(super) fn values(&self, key: &[u8]) -> Option<&[&'a [u8]]> {
        self.options.get(key).map(Vec::as_slice)
    }
}

enum Arity {
    Flag,
    One,
    Many,
}

fn arity(key: &[u8]) -> Option<Arity> {
    match key {
        b"REQUIRED" | b"QUIET" | b"EXACT" | b"NO_STRICTNESS" => Some(Arity::Flag),
        b"STRICTNESS"
        | b"ENV_MODE"
        | b"DISABLE_UNINSTALLED"
        | b"PC_SYSROOT_DIR"
        | b"TOP_BUILD_DIR"
        | b"NAME"
        | b"PREFIX"
        | b"ALLOW_SYSTEM_INCLUDES"
        | b"ALLOW_SYSTEM_LIBS" => Some(Arity::One),
        b"PC_LIBDIR"
        | b"PC_PATH"
        | b"SYSTEM_INCLUDE_DIRS"
        | b"SYSTEM_LIBRARY_DIRS"
        | b"BIND_PC_REQUIRES" => Some(Arity::Many),
        _ => None,
    }
}

pub(super) struct Settings {
    pub strictness: Strictness,
    pub search: Vec<BString>,
    pub sysroot: BString,
    pub top_build: Option<BString>,
    pub disable_uninstalled: bool,
    pub filter_includes: Vec<BString>,
    pub filter_libs: Vec<BString>,
}

impl Settings {
    pub(super) fn load(cx: &mut Cx<'_>, req: &Request<'_>) -> Result<Self, CmdError> {
        let strictness = match req.value(b"STRICTNESS") {
            Some(b"STRICT") => Strictness::Strict,
            Some(b"PERMISSIVE") => Strictness::Permissive,
            Some(b"BEST_EFFORT") => Strictness::BestEffort,
            Some(value) => {
                return Err(status(format!(
                    "given invalid STRICTNESS {:?}.",
                    BString::from(value)
                )));
            }
            None if req.flag(b"NO_STRICTNESS") => Strictness::BestEffort,
            None => Strictness::default(),
        };
        let env_mode = req.value(b"ENV_MODE").unwrap_or(b"PKGCONF");
        if !matches!(env_mode, b"IGNORE" | b"FDO" | b"PKGCONF") {
            return Err(status(format!(
                "given invalid ENV_MODE {:?}.",
                BString::from(env_mode)
            )));
        }
        let mut config = Config { cx, req, env_mode };
        let mut search = config.paths(b"PC_PATH", b"PC_PATH", &[b"PKG_CONFIG_PATH"], b"");
        search.extend(config.paths(
            b"PC_LIBDIR",
            b"PC_LIB_DIRS",
            &[b"PKG_CONFIG_LIBDIR"],
            default_search(),
        ));
        let sysroot = config
            .scalar(
                b"PC_SYSROOT_DIR",
                &[b"SYSROOT_DIR", b"PC_SYSROOT_DIR"],
                b"PKG_CONFIG_SYSROOT_DIR",
            )
            .unwrap_or_default();
        let top_build = config.scalar(
            b"TOP_BUILD_DIR",
            &[b"TOP_BUILD_DIR"],
            b"PKG_CONFIG_TOP_BUILD_DIR",
        );
        let disable_uninstalled = config.boolean(
            b"DISABLE_UNINSTALLED",
            b"DISABLE_UNINSTALLED",
            b"PKG_CONFIG_DISABLE_UNINSTALLED",
        );
        let mut filter_includes = Vec::new();
        let mut filter_libs = Vec::new();
        if req.mode == Mode::Extract {
            let includes = config.paths(
                b"SYSTEM_INCLUDE_DIRS",
                b"SYS_INCLUDE_DIRS",
                &[b"PKG_CONFIG_SYSTEM_INCLUDE_PATH"],
                b"/usr/include",
            );
            let libs = config.paths(
                b"SYSTEM_LIBRARY_DIRS",
                b"SYS_LIB_DIRS",
                &[b"PKG_CONFIG_SYSTEM_LIBRARY_PATH"],
                b"/usr/lib",
            );
            if !config.boolean(
                b"ALLOW_SYSTEM_INCLUDES",
                b"ALLOW_SYS_INCLUDES",
                b"PKG_CONFIG_ALLOW_SYSTEM_CFLAGS",
            ) {
                filter_includes = includes;
            }
            if !config.boolean(
                b"ALLOW_SYSTEM_LIBS",
                b"ALLOW_SYS_LIBS",
                b"PKG_CONFIG_ALLOW_SYSTEM_LIBS",
            ) {
                filter_libs = libs;
            }
            if env_mode == b"PKGCONF" {
                let includes = config.paths(
                    b"",
                    b"PKGCONF_INCLUDES",
                    &[
                        b"CPATH",
                        b"C_INCLUDE_PATH",
                        b"CPLUS_INCLUDE_PATH",
                        b"OBJC_INCLUDE_PATH",
                        #[cfg(windows)]
                        b"INCLUDE",
                    ],
                    b"",
                );
                let libs = config.paths(b"", b"PKGCONF_LIB_DIRS", &[b"LIBRARY_PATH"], b"");
                if !config.boolean(
                    b"ALLOW_SYSTEM_INCLUDES",
                    b"ALLOW_SYS_INCLUDES",
                    b"PKG_CONFIG_ALLOW_SYSTEM_CFLAGS",
                ) {
                    filter_includes.extend(includes);
                }
                if !config.boolean(
                    b"ALLOW_SYSTEM_LIBS",
                    b"ALLOW_SYS_LIBS",
                    b"PKG_CONFIG_ALLOW_SYSTEM_LIBS",
                ) {
                    filter_libs.extend(libs);
                }
            }
        }
        Ok(Self {
            strictness,
            search,
            sysroot,
            top_build,
            disable_uninstalled,
            filter_includes,
            filter_libs,
        })
    }
}

struct Config<'a, 'cx, 'req> {
    cx: &'a mut Cx<'cx>,
    req: &'a Request<'req>,
    env_mode: &'a [u8],
}

impl Config<'_, '_, '_> {
    fn env(&self, name: &[u8]) -> Option<BString> {
        if self.env_mode == b"IGNORE" {
            None
        } else {
            self.cx.host().env_get(name)
        }
    }

    fn paths(&mut self, key: &[u8], var: &[u8], env: &[&[u8]], default: &[u8]) -> Vec<BString> {
        if let Some(values) = self.req.values(key) {
            return values.iter().copied().map(BString::from).collect();
        }
        let var = text(&[b"CMAKE_PKG_CONFIG_", var]);
        if let Some(value) = self.cx.var(&var) {
            return list::split(&value, list::Empty::Drop);
        }
        if self.env_mode == b"IGNORE" {
            return Vec::new();
        }
        let delimiter = if cfg!(windows) { b';' } else { b':' };
        let mut values = Vec::new();
        let mut defined = false;
        for name in env {
            if let Some(value) = self.env(name) {
                defined = true;
                values.extend(
                    value
                        .split(|&byte| byte == delimiter)
                        .filter(|part| !part.is_empty())
                        .map(BString::from),
                );
            }
        }
        if !defined {
            values = list::split(default, list::Empty::Drop);
        }
        self.cx.set_var(&var, list::join(&values));
        values
    }

    fn scalar(&mut self, key: &[u8], vars: &[&[u8]], env: &[u8]) -> Option<BString> {
        if let Some(value) = self.req.value(key) {
            return Some(value.into());
        }
        for var in vars {
            if let Some(value) = self.cx.var(&text(&[b"CMAKE_PKG_CONFIG_", var])) {
                return Some(value);
            }
        }
        let value = self.env(env)?;
        if let Some(var) = vars.first() {
            self.cx
                .set_var(&text(&[b"CMAKE_PKG_CONFIG_", var]), value.clone());
        }
        Some(value)
    }

    fn boolean(&mut self, key: &[u8], var: &[u8], env: &[u8]) -> bool {
        if let Some(value) = self.req.value(key) {
            return truth::is_on(value);
        }
        let var = text(&[b"CMAKE_PKG_CONFIG_", var]);
        if let Some(value) = self.cx.var(&var) {
            return truth::is_on(&value);
        }
        if self.env_mode == b"IGNORE" {
            return true;
        }
        let defined = self.env(env).is_some();
        if defined {
            self.cx.set_var(&var, "ON".into());
        }
        defined
    }
}

fn default_search() -> &'static [u8] {
    if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
        b"/opt/homebrew/lib/pkgconfig;/opt/homebrew/share/pkgconfig;/usr/local/lib/pkgconfig;/usr/local/share/pkgconfig;/usr/lib/pkgconfig;/usr/share/pkgconfig"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        b"/usr/local/lib/pkgconfig;/usr/local/share/pkgconfig;/usr/lib/x86_64-linux-gnu/pkgconfig;/usr/lib/pkgconfig;/usr/share/pkgconfig"
    } else {
        b"/usr/local/lib/pkgconfig;/usr/local/share/pkgconfig;/usr/lib/pkgconfig;/usr/share/pkgconfig"
    }
}
