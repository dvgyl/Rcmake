use std::{path::PathBuf, sync::Arc};

use rcmake_host::{PureHost, Stream};

use crate::{Interp, InterpConfig, RunMode};

#[test]
fn while_checks_cmp0218_and_deprecation_uses_the_set_value() {
    for (value, warn) in [
        (None, true),
        (Some(""), true),
        (Some("NOTFOUND"), true),
        (Some("X-NOTFOUND"), true),
        (Some("x-notfound"), false),
        (Some("OFF"), false),
    ] {
        let host = Arc::new(PureHost::default());
        let mut interp = Interp::new(
            InterpConfig::new(
                host.clone(),
                RunMode::Script,
                PathBuf::new(),
                "/usr/local/bin/cmake",
                "/work",
            ),
            |_| {},
        );
        if let Some(value) = value {
            interp.define(b"CMAKE_WARN_DEPRECATED", value.into());
        }
        let script = b"while(CMAKE_WARN_DEPRECATED)\n  break()\nendwhile()\nmessage(DEPRECATION legacy-warning)\n";
        assert!(interp.eval_code(script).is_ok());
        let stderr = host.stream_bytes(Stream::Stderr);
        let stderr = String::from_utf8_lossy(&stderr);
        assert!(stderr.contains("Policy CMP0218 is not set"), "{stderr}");
        assert_eq!(stderr.contains("legacy-warning"), warn, "{value:?}");
    }
}
