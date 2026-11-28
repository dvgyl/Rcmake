use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use rcmake_base::{BString, list};
use rcmake_eval::RunMode;
use rcmake_host::{Host, Stream, discover_cmake_root, path};

use crate::build::{invalid_value, job_count};
use crate::options::{Options as ConfigureOptions, split_flag, take_value, unknown_mode_option};
use crate::{cache, new_interp_with_paths, resolve_cli_path, resolve_cmake_command};

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

mod parallel;

struct Options {
    dir: PathBuf,
    vars: BTreeMap<&'static [u8], BString>,
    components: Vec<BString>,
    jobs: Option<u32>,
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
        jobs: None,
    };

    while let Some(arg) = args.next() {
        let (mut flag, mut inline) = split_flag(arg, b'=');
        if let Some(component) = flag.strip_prefix(b"--component ")
            && inline.is_none()
        {
            flag = b"--component";
            inline = Some(component.trim_ascii());
        }
        if let Some(jobs) = flag.strip_prefix(b"-j").filter(|jobs| !jobs.is_empty()) {
            flag = b"-j";
            inline = Some(jobs.trim_ascii());
        }
        let mut value = || {
            inline
                .or_else(|| take_value(&mut args))
                .filter(|value| !value.is_empty())
                .ok_or_else(|| ParseError::Usage(invalid_value(flag)))
        };
        let var: (&[u8], BString) = match flag {
            b"--config" => (b"CMAKE_INSTALL_CONFIG_NAME", value()?.into()),
            b"--prefix" => {
                let raw = value()?;
                let prefix = if raw.starts_with(b"/") {
                    rcmake_base::path_lex::Lexical::new(raw).normalize()
                } else {
                    path::from_native_cmake(&resolve_cli_path(host, raw))
                };
                (b"CMAKE_INSTALL_PREFIX", prefix)
            }
            b"--strip" if inline.is_none() => (b"CMAKE_INSTALL_DO_STRIP", "1".into()),
            b"--default-directory-permissions" => (
                b"CMAKE_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS",
                permissions(value()?).ok_or(ParseError::Permissions)?,
            ),
            b"--component" => {
                // An attached value is one argument. Separate values join
                // with `;` into one CMake list, as in `cmCommandLineArgument`.
                let attached = inline.is_some();
                let mut components = BString::from(value()?);
                while !attached && let Some(next) = take_value(&mut args) {
                    components.push(b';');
                    components.extend_from_slice(next);
                }
                opts.components
                    .extend(list::split(&components, list::Empty::Drop));
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
                opts.jobs = Some(job_count(count).map_err(|err| {
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
                })?);
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

pub(crate) fn run(
    argv: &[BString],
    args: &[BString],
    persona: crate::persona::Persona,
    shim_handoff: bool,
    host: Arc<dyn Host>,
) -> i32 {
    let opts = match parse(args, host.as_ref()) {
        Ok(opts) => opts,
        Err(ParseError::Usage(message)) => {
            if !message.is_empty() {
                host.write_out(Stream::Stderr, format!("{message}\n").as_bytes());
            }
            host.write_out(Stream::Stderr, persona.usage(USAGE).as_bytes());
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
        || {
            crate::CommandExit::Code(install(
                argv,
                opts,
                persona,
                shim_handoff,
                Arc::clone(&host),
            ))
        },
    )
}

impl Options {
    fn configurations<'a>(&'a self, defaults: &'a [BString]) -> Vec<Option<&'a BString>> {
        match self.vars.get(b"CMAKE_INSTALL_CONFIG_NAME".as_slice()) {
            Some(config) => vec![Some(config)],
            None if !defaults.is_empty() => defaults.iter().map(Some).collect(),
            None => vec![None],
        }
    }
}

fn install(
    argv: &[BString],
    opts: Options,
    persona: crate::persona::Persona,
    shim_handoff: bool,
    host: Arc<dyn Host>,
) -> i32 {
    let (discovered_command, cmake_root) = discover_cmake_root(host.as_ref(), argv.first());
    let cmake_command = resolve_cmake_command(
        host.as_ref(),
        persona,
        discovered_command,
        argv.first(),
        shim_handoff,
    );
    let command = path::from_native_cmake(&cmake_command);
    let index = match parallel::Index::load(host.as_ref(), &opts.dir) {
        Ok(Some(index)) if index.parallel => {
            return parallel::run(&opts, host.as_ref(), index, &command).unwrap_or_else(|error| {
                crate::emit_cli_error(host.as_ref(), error);
                1
            });
        }
        Ok(index) => index,
        Err(error) => {
            crate::emit_cli_error(host.as_ref(), error);
            return 1;
        }
    };
    let configs = opts.configurations(index.as_ref().map_or(&[], |index| &index.configs));
    let script = index
        .as_ref()
        .and_then(|index| index.scripts.first())
        .cloned()
        .unwrap_or_else(|| opts.dir.join("cmake_install.cmake"));
    let components = if opts.components.is_empty() {
        vec![None]
    } else {
        opts.components.iter().map(Some).collect()
    };
    let cmake_root = cmake_root.unwrap_or_default();
    let cfg_opts = ConfigureOptions {
        persona,
        shim_handoff,
        ..ConfigureOptions::default()
    };
    for (component, config) in components
        .into_iter()
        .flat_map(|component| configs.iter().map(move |config| (component, *config)))
    {
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
            (b"CMAKE_INSTALL_COMPONENT", component),
            (b"CMAKE_INSTALL_CONFIG_NAME", config),
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
        script_args.extend(["-P".into(), path::from_native_cmake(&script)]);
        let mut interp = new_interp_with_paths(
            &cfg_opts,
            RunMode::Script,
            Arc::clone(&host),
            cmake_command.clone(),
            cmake_root.clone(),
            &script_args,
            rcmake_eval::Services::default(),
        );
        for (key, value) in definitions {
            cache::insert(interp.cx().cache(), key, value.clone(), None);
        }
        let outcome = interp.run_script(&script);
        if outcome.exit_code != 0 {
            return 1;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use rcmake_base::BString;
    use rcmake_host::PureHost;

    use super::{ParseError, parse};

    fn components(args: &[&str]) -> Vec<String> {
        let args: Vec<BString> = args.iter().copied().map(BString::from).collect();
        match parse(&args, &PureHost::default()) {
            Ok(opts) => opts.components.iter().map(ToString::to_string).collect(),
            Err(ParseError::Usage(message)) => panic!("usage error: {message}"),
            Err(ParseError::Permissions) => panic!("permissions error"),
        }
    }

    #[test]
    fn install_component_arguments_split_cmake_lists() {
        assert_eq!(
            components(&["build", "--component", "Runtime;Development", "Docs"]),
            ["Runtime", "Development", "Docs"]
        );
        assert_eq!(
            components(&["build", "--component=A;;B", "--component", "C", "--strip"]),
            ["A", "B", "C"]
        );
        assert_eq!(
            components(&["build", "--component", r"a\;b", "[x;y]"]),
            ["a;b", "[x;y]"]
        );
        assert!(components(&["build", "--component", ";"]).is_empty());
        // Separate values join into one list, so a bracket can span them.
        assert_eq!(components(&["build", "--component", "[x", "y]"]), ["[x;y]"]);
        // An attached value consumes only its own argument.
        let args: Vec<BString> = ["build", "--component=a", "b"]
            .into_iter()
            .map(BString::from)
            .collect();
        assert!(matches!(
            parse(&args, &PureHost::default()),
            Err(ParseError::Usage(_))
        ));
    }

    #[test]
    fn install_prefix_preserves_cmake_root_style_paths() {
        let host = PureHost::default();
        let args: Vec<BString> = ["build", "--prefix", "/var/test"]
            .into_iter()
            .map(BString::from)
            .collect();
        let opts = match parse(&args, &host) {
            Ok(opts) => opts,
            Err(ParseError::Usage(message)) => panic!("usage error: {message}"),
            Err(ParseError::Permissions) => panic!("permissions error"),
        };
        assert_eq!(
            opts.vars.get(b"CMAKE_INSTALL_PREFIX".as_slice()),
            Some(&BString::from("/var/test"))
        );
    }
}
