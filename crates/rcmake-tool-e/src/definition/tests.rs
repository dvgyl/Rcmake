use rcmake_host::{Host, PureHost};

use super::*;

fn object() -> Vec<u8> {
    let sections = 20;
    let symbol_start = sections + 80;
    let mut data = vec![0; symbol_start];
    data[..2].copy_from_slice(&0x8664_u16.to_le_bytes());
    data[2..4].copy_from_slice(&2_u16.to_le_bytes());
    data[8..12].copy_from_slice(&(symbol_start as u32).to_le_bytes());
    data[12..16].copy_from_slice(&4_u32.to_le_bytes());
    data[sections + 36..sections + 40].copy_from_slice(&0x6000_0020_u32.to_le_bytes());
    data[sections + 76..sections + 80].copy_from_slice(&0xc000_0040_u32.to_le_bytes());
    for (name, section, kind) in [
        (b"function".as_slice(), 1_u16, 0x20_u16),
        (b"variable", 2, 0),
        (b"undef", 0, 0),
        (b"\0\0\0\0\x04\0\0\0", 1, 0x20),
    ] {
        let mut symbol = [0_u8; 18];
        symbol[..name.len()].copy_from_slice(name);
        symbol[12..14].copy_from_slice(&section.to_le_bytes());
        symbol[14..16].copy_from_slice(&kind.to_le_bytes());
        symbol[16] = 2;
        data.extend_from_slice(&symbol);
    }
    let long = b"more_than_eight\0";
    data.extend_from_slice(&(4 + long.len() as u32).to_le_bytes());
    data.extend_from_slice(long);
    data
}

#[test]
fn merges_coff_and_explicit_exports_without_nm() {
    let host = PureHost::default();
    let cwd = Path::new("/work");
    let ctx = ToolCtx {
        host: &host,
        cwd,
        exe_display: b"cmake",
        capabilities: &|| rcmake_json::Value::Null,
    };
    host.seed_file(&cwd.join("objects.rsp"), b"my object.obj\nexplicit.DEF\n")
        .unwrap();
    host.seed_file(&cwd.join("my object.obj"), &object())
        .unwrap();
    host.seed_file(
        &cwd.join("explicit.DEF"),
        b"LIBRARY demo\nEXPORTS\nfunction @5\nexplicit=other DATA\n; skip this comment\n",
    )
    .unwrap();
    let args = ["export.def".into(), "objects.rsp".into()];
    assert_eq!(run(&args, &ctx), 0);
    assert_eq!(
        host.read(&cwd.join("export.def"), read_all()).unwrap(),
        b"EXPORTS \n\tvariable \t DATA\n\texplicit=other DATA\n\tfunction @5\n\tmore_than_eight\n",
    );
    let mut corrupt = object();
    corrupt.truncate(105);
    host.seed_file(&cwd.join("my object.obj"), &corrupt)
        .unwrap();
    assert_ne!(run(&args, &ctx), 0);
    assert!(host.stream_bytes(Stream::Stderr).contains_str("COFF"));
}

#[test]
fn sends_only_non_coff_objects_to_nm() {
    let host = PureHost::default();
    let cwd = Path::new("/work");
    let ctx = ToolCtx {
        host: &host,
        cwd,
        exe_display: b"cmake",
        capabilities: &|| rcmake_json::Value::Null,
    };
    host.seed_file(&cwd.join("objects.rsp"), b"native.obj\nbitcode.obj\n")
        .unwrap();
    host.seed_file(&cwd.join("native.obj"), &object()).unwrap();
    host.seed_file(&cwd.join("bitcode.obj"), b"BC\xc0\xde")
        .unwrap();
    host.set_command_handler(|host, request, _| {
        let tool = &request.pipeline[0];
        let response = tool.args.last().unwrap().strip_prefix(b"@").unwrap();
        assert_eq!(
            host.read(&native::to_native(response), read_all())?,
            b"\"bitcode.obj\"\n"
        );
        Ok(rcmake_host::RunOutput {
            results: vec![ProcessResult::Exited(0)],
            stdout: b"bitcode.obj: _external@8 T 0 1\nbitcode.obj: ??_Gexcluded T 0 1\n".to_vec(),
            ..Default::default()
        })
    })
    .unwrap();
    assert_eq!(
        run(
            &["export.def".into(), "objects.rsp".into(), "--nm=nm".into()],
            &ctx
        ),
        0
    );
    assert_eq!(
        host.read(&cwd.join("export.def"), read_all()).unwrap(),
        b"EXPORTS \n\tvariable \t DATA\n\t_external\n\tfunction\n\tmore_than_eight\n"
    );
}
