use std::path::Path;

use rcmake_host::{PureHost, ReadRange};

use super::*;

#[test]
fn error_index_preserves_successful_reply() -> Result<(), Box<dyn std::error::Error>> {
    let host = PureHost::new("/src".into(), "/bin/cmake".into());
    let build = Path::new("/build");
    let api = build.join(API_DIR);
    let query = api.join("query");
    let reply = api.join("reply");
    let mut model = ProjectModel::default();
    model.set_root_dirs("/src".into(), "/build".into());
    host.create_dir_all(&query)?;
    for query_file in ["cache-v2", "configureLog-v1"] {
        host.write(&query.join(query_file), b"", WriteMode::CreateNew)?;
    }

    let result = GenerationResult::default();
    write_replies(&model, &result, &host, build)?;
    let mut saved = BTreeMap::new();
    let range = || ReadRange::All { limit: 65_536 };
    for entry in host.read_dir(&reply)? {
        let path = reply.join(to_native(&entry.name));
        saved.insert(path.clone(), host.read(&path, range())?);
    }
    assert!(saved.keys().any(|path| {
        path.file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("index-"))
    }));

    write_error_replies(&model, &host, build)?;
    for (path, data) in saved {
        assert_eq!(host.read(&path, range())?, data);
    }
    let name = host
        .read_dir(&reply)?
        .into_iter()
        .find(|entry| entry.name.starts_with(b"error-"))
        .ok_or("Missing error index.")?
        .name;
    let bytes = host.read(&reply.join(to_native(&name)), range())?;
    let Object(fields) = rcmake_json::parse(&bytes, &Default::default())? else {
        return Err("Expected an error index object.".into());
    };
    let Some(Object(replies)) = fields.get(b"reply".as_slice()) else {
        return Err("Missing query replies.".into());
    };
    assert_eq!(
        replies.get(b"cache-v2".as_slice()),
        Some(&queries::error("no buildsystem generated"))
    );
    let Some(Object(configure_log)) = replies.get(b"configureLog-v1".as_slice()) else {
        return Err("Missing configure log reply.".into());
    };
    assert!(configure_log.contains_key(b"jsonFile".as_slice()));
    assert!(
        host.metadata(
            &build.join("CMakeFiles/CMakeConfigureLog.yaml"),
            Follow::Yes
        )
        .is_ok()
    );

    write_replies(&model, &result, &host, build)?;
    assert!(
        !host
            .read_dir(&reply)?
            .iter()
            .any(|entry| entry.name.starts_with(b"error-"))
    );
    Ok(())
}
