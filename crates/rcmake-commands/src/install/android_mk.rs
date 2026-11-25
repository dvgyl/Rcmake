// SPDX-License-Identifier: MIT OR Apache-2.0

use rcmake_base::{BString, path_lex::Lexical};
use rcmake_eval::Cx;
use rcmake_model::{InstallKind, InstallOptions};

use super::{Args, Result, common, fail, keyword, value, warn_absolute_destination};

pub(super) fn parse(
    cx: &mut Cx<'_>,
    args: &mut Args<'_>,
    options: &mut InstallOptions,
) -> Result<InstallKind> {
    const SIGNATURE: &[u8] = b"EXPORT_ANDROID_MK";
    args.next_if(|token| !keyword(token));
    let mut file = b"".as_slice();
    while let Some(key) = args.next() {
        match key {
            b"FILE" => file = args.next_if(|token| !keyword(token)).unwrap_or_default(),
            b"NAMESPACE" => {
                args.next_if(|token| !keyword(token));
            }
            b"EXPORT_LINK_INTERFACE_LIBRARIES"
            | b"OPTIONAL"
            | b"NAMELINK_ONLY"
            | b"NAMELINK_SKIP" => {}
            b"RENAME" | b"TYPE" | b"NAMELINK_COMPONENT" => {
                value(args, key)?;
            }
            _ if common(cx, SIGNATURE, options, key, args)? => {}
            _ => {
                return Err(fail(
                    [b"EXPORT_ANDROID_MK given unknown argument \"", key, b"\"."].concat(),
                ));
            }
        }
    }
    let Some(destination) = options.destination.as_ref().filter(|path| !path.is_empty()) else {
        return Err(fail("EXPORT_ANDROID_MK given no DESTINATION!"));
    };
    if Lexical::new(destination).absolute() {
        warn_absolute_destination(cx, destination);
    }
    let problem = if file.iter().any(|byte| matches!(byte, b':' | b'/' | b'\\')) {
        "The FILE argument may not contain a path.  Specify the path in the DESTINATION argument."
    } else if !file.is_empty() && !file.ends_with(b".mk") {
        "The FILE argument must specify a name ending in \".mk\"."
    } else {
        return Ok(InstallKind::Code(BString::default()));
    };
    Err(fail(format!(
        "EXPORT_ANDROID_MK given invalid export file name \"{}\".  {problem}",
        BString::from(file)
    )))
}
