use std::sync::Arc;

use rcmake_base::BString;
use rcmake_host::{Host, PureHost, Stream};

use crate::archive_gen::ArchiveKind;
use crate::generators;

#[test]
fn registry_knows_milestone_three_generators() {
    for kind in ArchiveKind::ALL {
        let generator = generators::create(kind.name().as_bytes());
        assert_eq!(generator.map(|g| g.extension()), Some(kind.extension()));
    }
    assert_eq!(
        generators::create(b"DragNDrop").map(|g| g.name()),
        Some("DragNDrop")
    );
    assert_eq!(
        generators::create(b"Bundle").map(|g| g.name()),
        Some("Bundle")
    );
    assert!(generators::create(b"DEB").is_none());
}

#[test]
fn main_reports_unsupported_until_implemented() {
    let host = Arc::new(PureHost::new("/work".into(), "/bin/cpack".into()));
    let shared: Arc<dyn Host> = host.clone();
    let code = crate::main(&[BString::from("cpack")], shared);
    assert_eq!(code, 1);
    let stderr = host.stream_bytes(Stream::Stderr);
    assert!(stderr.starts_with(b"CPack Error: "));
}
