// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::BTreeMap;
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_commands::ConfigureSelection;
use rcmake_eval::Interp;
use rcmake_host::Stream;
use rcmake_model::{Cache, CacheType};

use super::{ConfigureSession, Exit, GENERATOR, cache, cache_setup};

pub(super) fn selection(saved: &Cache) -> Arc<ConfigureSelection> {
    let toolchain = saved
        .get(b"CMAKE_TOOLCHAIN_FILE")
        .map_or_else(BString::default, |entry| entry.value.clone());
    Arc::new(ConfigureSelection::new(toolchain))
}

pub(super) fn reset(
    session: &ConfigureSession<'_>,
    interp: &mut Interp,
    changes: BTreeMap<BString, BString>,
) -> Result<Cache, Exit> {
    let host = session.host.as_ref();
    let mut saved = Cache::default();
    let mut cx = interp.cx();
    for name in [
        GENERATOR,
        b"CMAKE_GENERATOR_INSTANCE",
        b"CMAKE_GENERATOR_PLATFORM",
        b"CMAKE_GENERATOR_TOOLSET",
        b"CMAKE_SYSTEM_ENVIRONMENT_ID",
        b"CMAKE_TOOLCHAIN_FILE",
    ] {
        if let Some(entry) = cx.cache().get(name) {
            saved.insert(name.into(), entry.clone());
        }
    }
    cache_setup::clear(host, &session.bin).map_err(Exit::message)?;
    let mut notice = String::from(
        "You have changed variables that require your cache to be deleted.\n\
         Configure will be re-run and you may have to reset some variables.\n\
         The following variables have changed:\n",
    );
    for (name, value) in changes {
        use std::fmt::Write as _;
        let _ = writeln!(&mut notice, "{name}= {value}");
        let ty = if name.ends_with(b"_ARG1") {
            CacheType::String
        } else {
            CacheType::Filepath
        };
        cache::insert(&mut saved, &name, value, Some(ty));
    }
    notice.push('\n');
    host.write_out(Stream::Stderr, notice.as_bytes());
    Ok(saved)
}
