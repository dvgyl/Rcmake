use rcmake_host::Host;

use super::try_compile::active::{self, ReuseWarningReason};
use crate::policy_misc::files::resolve_program_command;

/// Check a compiler selected by `CC` or `CXX` for probe cache admission.
/// This check reads the selection and wrapper files without running any process.
pub fn compiler_reuse_warning(host: &dyn Host, cxx: bool) -> Option<String> {
    let (variable, language) = if cxx { ("CXX", "C++") } else { ("CC", "C") };
    let value = host.env_get(variable.as_bytes())?;
    let program = resolve_program_command(host, &host.initial_cwd(), &value)?.path;
    let reason = active::compiler_reuse_warning(host, &program, cxx)?;
    let selected = program.display();
    let message = match reason {
        ReuseWarningReason::ExcludedLauncher => format!(
            "{variable} selects {selected}. This selection uses an excluded compiler launcher, so {language} compiler probes will bypass the probe cache."
        ),
        ReuseWarningReason::WrapperChainTooDeep => format!(
            "{variable} selects {selected}. The wrapper chain exceeds the supported depth, so {language} compiler probes will bypass the probe cache."
        ),
        ReuseWarningReason::WrapperTooManyArgLists => format!(
            "{variable} selects {selected}. The wrapper has too many possible argument lists, so {language} compiler probes will bypass the probe cache."
        ),
        ReuseWarningReason::UnverifiableWrapperScript => format!(
            "{variable} selects {selected}. rcmake cannot verify this wrapper script, so {language} compiler probes will bypass the probe cache."
        ),
        ReuseWarningReason::UnreadableWrapper => format!(
            "{variable} selects {selected}. rcmake cannot read the compiler or wrapper, so {language} compiler probes will bypass the probe cache."
        ),
        ReuseWarningReason::UnverifiedAppleDriver => format!(
            "{variable} selects {selected}. rcmake must verify this driver with the configured toolchain before confirming probe cache reuse."
        ),
    };
    Some(message)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use rcmake_host::{HostError, HostResult, PureHost};
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn doctor_uses_wrapper_admission_without_running_a_compiler() -> HostResult<()> {
        let host = PureHost::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&calls);
        host.set_command_handler(move |_, _, _| {
            count.fetch_add(1, Ordering::Relaxed);
            Err(HostError::unsupported("Unexpected process request."))
        })?;
        let mut system = host.system();
        system.sysname = "Darwin".into();
        host.set_system(system)?;
        for (file, bytes) in [
            ("/usr/bin/cc", b"native".as_slice()),
            ("/tool/bin/sccache", b"native"),
            ("/tool/bin/ccache", b"native"),
            ("/tool/bin/other", b"native"),
            ("/tool/bin/forward", b"#!/bin/sh\nexec /usr/bin/cc \"$@\"\n"),
            (
                "/tool/bin/with spaces",
                b"#!/bin/sh\nx=$(pwd)\nexec /usr/bin/cc \"$@\"\n",
            ),
            ("/bin/sh", b"native"),
        ] {
            host.seed_file(Path::new(file), bytes)?;
            host.set_permissions(Path::new(file), 0o755)?;
        }
        host.env_set(b"PATH", Some(b"/tool/bin:/usr/bin"));
        host.env_set(b"CC", Some(b"\"/tool/bin/with spaces\" -DVALUE=1"));
        host.env_set(b"CXX", Some(b"sccache clang++"));
        assert!(compiler_reuse_warning(&host, false).is_some_and(|warning| {
            warning.contains("CC selects /tool/bin/with spaces")
                && warning.contains("cannot verify this wrapper script")
        }));
        assert!(
            compiler_reuse_warning(&host, true)
                .is_some_and(|warning| warning.contains("CXX selects /tool/bin/sccache")
                    && warning.contains("C++ compiler probes"))
        );
        for selection in [b"cc".as_slice(), b"forward"] {
            host.env_set(b"CC", Some(selection));
            assert!(compiler_reuse_warning(&host, false).is_none());
        }
        host.env_set(b"CC", Some(b"ccache cc"));
        assert!(
            compiler_reuse_warning(&host, false)
                .is_some_and(|warning| warning.contains("excluded compiler launcher"))
        );
        host.env_set(b"CC", Some(b"other"));
        assert!(compiler_reuse_warning(&host, false).is_some_and(|warning| {
            warning.contains("configured toolchain") && !warning.contains("bypass")
        }));
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod path_tests {
    use super::*;
    use crate::sysctl_path_warning;
    use rcmake_host::{HostResult, PureHost};
    use std::path::Path;

    #[test]
    fn macos_sysctl_warning_checks_path_in_search_order() -> HostResult<()> {
        let host = PureHost::default();
        let mut system = host.system();
        system.sysname = "Darwin".into();
        host.set_system(system.clone())?;
        for program in ["/usr/sbin/sysctl", "/shadow/sysctl"] {
            let program = Path::new(program);
            host.seed_file(program, b"")?;
            host.set_permissions(program, 0o755)?;
        }
        host.env_set(b"PATH", Some(b"/usr/bin:/bin"));
        assert!(
            sysctl_path_warning(&host)
                .is_some_and(|warning| warning.contains("does not include /usr/sbin"))
        );
        host.env_set(b"PATH", Some(b"/usr/bin:/usr/sbin"));
        assert_eq!(sysctl_path_warning(&host), None);
        host.env_set(b"PATH", Some(b"/shadow:/usr/sbin"));
        assert!(
            sysctl_path_warning(&host)
                .is_some_and(|warning| warning.contains("selects /shadow/sysctl before"))
        );
        host.env_set(b"PATH", Some(b"relative:/usr/sbin"));
        assert!(
            sysctl_path_warning(&host)
                .is_some_and(|warning| warning.contains("Use absolute directories"))
        );
        system.sysname = "Linux".into();
        host.set_system(system)?;
        assert_eq!(sysctl_path_warning(&host), None);
        Ok(())
    }
}
