use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_eval::RunMode;
use rcmake_host::{Host, Stream, path};

use crate::build::{invalid_value, job_count};
use crate::options::{Options as ConfigureOptions, split_flag, take_value, unknown_mode_option};
use crate::{cache, discover_cmake_paths, new_interp, resolve_cli_path};

const USAGE: &str = "Usage: cmake --install <dir> [options]
Options:
  <dir>              = Project binary directory to install.
  --config <cfg>     = For multi-configuration tools, choose <cfg>.
  --component <comp> = Component-based install. Only install <comp>.
                       May be passed multiple components. t
  --default-directory-permissions <permission>\x20
     Default install permission. Use default permission <permission>.
  -j <jobs> --parallel <jobs>
     Build in parallel using the given number of jobs.\x20
     The CMAKE_INSTALL_PARALLEL_LEVEL environment variable
     specifies a default parallel level when this option is not given.
  --prefix <prefix>  = The installation prefix CMAKE_INSTALL_PREFIX.
  --strip            = Performing install/strip.
  -v --verbose       = Enable verbose output.
";

const FLAGS: &[&[u8]] = &[
    b"--config",
    b"--component",
    b"--default-directory-permissions",
    b"--prefix",
    b"--strip",
    b"--verbose",
    b"--parallel",
    b"-j",
    b"-v",
];

struct Options {
    dir: PathBuf,
    vars: BTreeMap<&'static [u8], BString>,
    components: Vec<BString>,
}

enum ParseError {
    Usage(String),
    Permissions,
}

fn parse(args: &[BString], host: &dyn Host) -> Result<Options, ParseError> {
    let mut args = args.iter().map(|arg| arg.as_slice()).peekable();
    let dir = take_value(&mut args)
        .filter(|dir| !dir.is_empty())
        .ok_or_else(|| ParseError::Usage(String::new()))?;
    let mut opts = Options {
        dir: resolve_cli_path(host, dir),
        vars: BTreeMap::new(),
        components: Vec::new(),
    };

    while let Some(arg) = args.next() {
        let (mut flag, mut inline) = split_flag(arg, b'=');
        if let Some(jobs) = flag.strip_prefix(b"-j").filter(|jobs| !jobs.is_empty()) {
            flag = b"-j";
            inline = Some(jobs);
        }
        let mut value = || {
            inline
                .or_else(|| take_value(&mut args))
                .filter(|value| !value.is_empty())
                .ok_or_else(|| ParseError::Usage(invalid_value(flag)))
        };
        let var: (&[u8], BString) = match flag {
            b"--config" => (b"CMAKE_INSTALL_CONFIG_NAME", value()?.into()),
            b"--prefix" => (b"CMAKE_INSTALL_PREFIX", value()?.into()),
            b"--strip" if inline.is_none() => (b"CMAKE_INSTALL_DO_STRIP", "1".into()),
            b"--default-directory-permissions" => (
                b"CMAKE_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS",
                permissions(value()?).ok_or(ParseError::Permissions)?,
            ),
            b"--component" => {
                opts.components.push(value()?.into());
                opts.components
                    .extend(std::iter::from_fn(|| take_value(&mut args)).map(BString::from));
                continue;
            }
            b"--parallel" | b"-j" => {
                let count = inline
                    .or_else(|| {
                        args.next_if(|arg| {
                            !arg.starts_with(b"-") || arg.get(1).is_some_and(u8::is_ascii_digit)
                        })
                    })
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| ParseError::Usage(invalid_value(flag)))?;
                job_count(count).map_err(|err| {
                    let message = if err == "invalid" {
                        format!(
                            "'{}' invalid number '{}' given.",
                            String::from_utf8_lossy(flag),
                            String::from_utf8_lossy(count)
                        )
                    } else {
                        err.to_owned()
                    };
                    ParseError::Usage(format!("{message}\n"))
                })?;
                continue;
            }
            b"-v" | b"--verbose" if inline.is_none() => {
                continue;
            }
            _ => return Err(ParseError::Usage(unknown_mode_option(arg, FLAGS))),
        };
        opts.vars.insert(var.0, var.1);
    }
    Ok(opts)
}

fn permissions(value: &[u8]) -> Option<BString> {
    let mut entries = Vec::new();
    for part in value.split(|&byte| byte == b',') {
        let (owner, bits) = split_flag(part, b'=');
        let owner = match owner {
            b"u" => "OWNER",
            b"g" => "GROUP",
            b"o" => "WORLD",
            _ => return None,
        };
        for bit in bits? {
            let access = match bit {
                b'r' => "READ",
                b'w' => "WRITE",
                b'x' => "EXECUTE",
                _ => return None,
            };
            let entry = format!("{owner}_{access}");
            if !entries.contains(&entry) {
                entries.push(entry);
            }
        }
    }
    Some(entries.join(";").into())
}

pub(crate) fn run(argv: &[BString], args: &[BString], host: Arc<dyn Host>) -> i32 {
    let opts = match parse(args, host.as_ref()) {
        Ok(opts) => opts,
        Err(ParseError::Usage(message)) => {
            if !message.is_empty() {
                host.write_out(Stream::Stderr, format!("{message}\n").as_bytes());
            }
            host.write_out(Stream::Stderr, USAGE.as_bytes());
            return 1;
        }
        Err(ParseError::Permissions) => {
            host.write_out(
                Stream::Stderr,
                b"--default-directory-permissions is in incorrect format\n",
            );
            return 1;
        }
    };
    let dir = opts.dir.clone();
    crate::instrument_lifecycle(
        host.as_ref(),
        &dir,
        argv,
        rcmake_instrument::SnippetRole::CMakeInstall,
        rcmake_instrument::Hook::PostCMakeInstall,
        || install(argv, opts, Arc::clone(&host)),
    )
}

fn install(argv: &[BString], opts: Options, host: Arc<dyn Host>) -> i32 {
    let script = opts.dir.join("cmake_install.cmake");
    let components = if opts.components.is_empty() {
        vec![None]
    } else {
        opts.components.into_iter().map(Some).collect()
    };
    let command = path::from_native(&discover_cmake_paths(host.as_ref(), argv.first()).0);
    for component in components {
        let defines = [
            (
                b"CMAKE_INSTALL_PREFIX".as_slice(),
                opts.vars.get(b"CMAKE_INSTALL_PREFIX".as_slice()),
            ),
            (
                b"CMAKE_INSTALL_DO_STRIP",
                opts.vars.get(b"CMAKE_INSTALL_DO_STRIP".as_slice()),
            ),
            (
                b"CMAKE_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS",
                opts.vars
                    .get(b"CMAKE_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS".as_slice()),
            ),
            (b"CMAKE_INSTALL_COMPONENT", component.as_ref()),
            (
                b"CMAKE_INSTALL_CONFIG_NAME",
                opts.vars.get(b"CMAKE_INSTALL_CONFIG_NAME".as_slice()),
            ),
        ];
        let definitions = defines
            .into_iter()
            .filter_map(|(key, value)| value.map(|value| (key, value)));
        let mut script_args = vec![command.clone()];
        for (key, value) in definitions.clone() {
            let mut arg = BString::from("-D");
            arg.extend_from_slice(key);
            arg.push(b'=');
            arg.extend_from_slice(value);
            script_args.push(arg);
        }
        script_args.extend(["-P".into(), path::from_native(&script)]);
        let mut interp = new_interp(
            &script_args,
            &ConfigureOptions::default(),
            RunMode::Script,
            Arc::clone(&host),
        );
        for (key, value) in definitions {
            cache::insert(interp.cx().cache(), key, value.clone(), None);
        }
        let outcome = interp.run_script(&script);
        if outcome.exit_code != 0 {
            return outcome.exit_code;
        }
    }
    0
}
