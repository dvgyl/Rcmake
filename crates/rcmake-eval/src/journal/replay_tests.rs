use super::*;
use crate::journal::{
    CacheValue, JournalFileStat, JournalStamp, OpaqueReason, PropertyKey, PropertyOrigin,
    RegionState, VariableScope, directory_digest, policy_digest,
};
use rcmake_diag::{Backtrace, Frame, FrameKind, PolicyStatus};
use rcmake_host::{FileTime, HostResult, PureHost};
use rcmake_model::{CacheEntry, CacheType, PropertyMap};
use std::path::PathBuf;

fn fixture(kind: RegionKind) -> (PureHost, RegionKey, EntryContext) {
    let root = PathBuf::from(if cfg!(windows) { "C:/work" } else { "/work" });
    let host = PureHost::new(root.clone(), root.join("rcmake"));
    let key = RegionKey {
        binary_dir: root.join("build"),
        source_dir: root.join("src"),
        kind,
    };
    let context = EntryContext {
        rcmake: JournalDigest::hash(b"rcmake"),
        policies: JournalDigest::hash(b"policies"),
        diag: JournalDigest::hash(b"diagnostics"),
        log_level: 0,
        flags: JournalDigest::hash(b"flags"),
    };
    (host, key, context)
}

fn variable(scope: VariableScope, name: &[u8]) -> StateKey {
    StateKey::Variable {
        scope,
        name: name.to_vec(),
    }
}

fn read(state: &mut RegionState, key: StateKey, bytes: Option<&[u8]>) {
    state.record_input(JournalInput::State {
        key,
        value: bytes.map(JournalDigest::hash),
    });
}

fn unavailable(_: &StateKey) -> StateLookup {
    StateLookup::Unavailable
}

#[test]
fn only_first_read_before_write_constrains_replay() -> Result<(), ReplayMiss> {
    let (host, key, context) = fixture(RegionKind::Subdirectory);
    let mut state = RegionState::default();
    let parent = variable(VariableScope::Parent, b"X");
    let current = variable(VariableScope::Current, b"X");
    read(&mut state, parent.clone(), Some(b"old"));
    read(&mut state, parent.clone(), Some(b"old"));
    read(
        &mut state,
        StateKey::EffectiveVariable(b"X".to_vec()),
        Some(b"old"),
    );
    state.record_effect(JournalEffect::SetVar {
        scope: VariableScope::Parent,
        name: b"X".to_vec(),
        value: Some(b"write".to_vec()),
    });
    read(&mut state, parent.clone(), Some(b"write"));
    state.record_effect(JournalEffect::SetVar {
        scope: VariableScope::Current,
        name: b"X".to_vec(),
        value: None,
    });
    read(&mut state, current, None);
    let record = state.finish(key.clone(), context.clone());
    let verified = verify_region(&host, &record, &key, &context, |seen| {
        assert!(seen == &parent || seen == &StateKey::EffectiveVariable(b"X".to_vec()));
        StateLookup::Observed(Some(JournalDigest::hash(b"old")))
    })?;
    assert_eq!(verified.inputs().len(), 2);
    let result = verify_region(&host, &record, &key, &context, |_| {
        StateLookup::Observed(None)
    });
    assert!(matches!(
        result,
        Err(ReplayMiss {
            reason: ReplayMissReason::Input { index: 0, .. },
            ..
        })
    ));
    Ok(())
}

#[test]
fn absent_cache_input_and_empty_cache_value_are_distinct() {
    let (host, key, context) = fixture(RegionKind::Configure);
    let mut state = RegionState::default();
    read(&mut state, StateKey::Cache(b"X".to_vec()), None);
    let record = state.finish(key.clone(), context.clone());
    let empty = CacheValue::from(&CacheEntry {
        value: "".into(),
        ty: CacheType::String,
        help: "help".into(),
        advanced: false,
        props: PropertyMap::default(),
    });
    assert!(
        verify_region(&host, &record, &key, &context, |_| StateLookup::Observed(
            None
        ))
        .is_ok()
    );
    assert!(matches!(
        verify_region(&host, &record, &key, &context, unavailable),
        Err(ReplayMiss {
            reason: ReplayMissReason::Input { index: 0, .. },
            ..
        })
    ));
    assert!(matches!(
        verify_region(&host, &record, &key, &context, |_| StateLookup::Observed(
            Some(empty.digest())
        )),
        Err(ReplayMiss {
            reason: ReplayMissReason::Input { index: 0, .. },
            ..
        })
    ));
}

#[test]
fn cache_digest_and_restore_include_metadata() {
    let mut props = PropertyMap::default();
    props.set(b"STRINGS".as_slice().into(), b"\xff;two".as_slice().into());
    let mut entry = CacheEntry {
        value: "one".into(),
        ty: CacheType::String,
        help: "Help text".into(),
        advanced: true,
        props,
    };
    let saved = CacheValue::from(&entry);
    assert_eq!(saved.to_entry().as_ref(), Some(&entry));
    entry.advanced = false;
    assert_ne!(saved.digest(), CacheValue::from(&entry).digest());
    entry.advanced = true;
    entry.props.unset(b"STRINGS");
    assert_ne!(saved.digest(), CacheValue::from(&entry).digest());
}

#[test]
fn append_to_an_existing_property_requires_an_input() {
    let (host, key, context) = fixture(RegionKind::Subdirectory);
    let backtrace = Backtrace::default().push(Frame {
        file: b"\xff/CMakeLists.txt".as_slice().into(),
        line: 3,
        name: Some(b"set_property".as_slice().into()),
        kind: FrameKind::Command,
    });
    let origin = PropertyOrigin::new(key.binary_dir.clone(), &backtrace);
    assert_eq!(origin.backtrace().top(), backtrace.top());
    let property = PropertyKey {
        owner: PropertyOwner::Global,
        name: b"LIBS".to_vec(),
    };
    let append = JournalEffect::Property {
        key: property.clone(),
        edit: PropertyEdit::AppendList(b"next".to_vec()),
        origin,
    };

    let mut missing = RegionState::default();
    missing.record_effect(append.clone());
    assert!(matches!(
        missing,
        RegionState::Opaque(OpaqueOperation {
            reason: OpaqueReason::ExternalMutation,
            ..
        })
    ));

    let input = StateKey::Property(property);
    let mut tracked = RegionState::default();
    read(&mut tracked, input.clone(), Some(b"first"));
    tracked.record_effect(append);
    read(&mut tracked, input.clone(), Some(b"first;next"));
    let record = tracked.finish(key.clone(), context.clone());
    let verify = |value: &[u8]| {
        verify_region(&host, &record, &key, &context, |seen| {
            assert_eq!(seen, &input);
            StateLookup::Observed(Some(JournalDigest::hash(value)))
        })
    };
    assert!(verify(b"first").is_ok());
    assert!(matches!(
        verify(b"other"),
        Err(ReplayMiss {
            reason: ReplayMissReason::Input { index: 0, .. },
            ..
        })
    ));
}

#[test]
fn policy_input_and_region_context_are_checked() {
    let (host, key, context) = fixture(RegionKind::Subdirectory);
    let mut state = RegionState::default();
    state.record_input(JournalInput::State {
        key: StateKey::Policy(77),
        value: Some(policy_digest(PolicyStatus::New)),
    });
    let record = state.finish(key.clone(), context.clone());
    let read_new = |key: &StateKey| {
        assert_eq!(key, &StateKey::Policy(77));
        StateLookup::Observed(Some(policy_digest(PolicyStatus::New)))
    };
    assert!(verify_region(&host, &record, &key, &context, read_new).is_ok());
    assert!(matches!(
        verify_region(&host, &record, &key, &context, |_| {
            StateLookup::Observed(Some(policy_digest(PolicyStatus::Old)))
        }),
        Err(ReplayMiss {
            reason: ReplayMissReason::Input { index: 0, .. },
            ..
        })
    ));
    let mut changed = context;
    changed.log_level += 1;
    assert!(matches!(
        verify_region(&host, &record, &key, &changed, unavailable),
        Err(ReplayMiss {
            reason: ReplayMissReason::Context(_),
            ..
        })
    ));
}

#[test]
fn changed_shared_value_without_a_tracked_write_makes_region_opaque() {
    let (_, key, context) = fixture(RegionKind::Subdirectory);
    let mut state = RegionState::default();
    let cache = StateKey::Cache(b"X".to_vec());
    read(&mut state, cache.clone(), Some(b"old"));
    read(&mut state, cache, Some(b"new"));
    let record = state.finish(key, context);
    assert!(matches!(
        record.payload,
        RegionPayload::Opaque(OpaqueOperation {
            reason: OpaqueReason::ExternalMutation,
            ..
        })
    ));
}

#[test]
fn trace_rechecks_file_directory_missing_path_and_environment() -> HostResult<()> {
    let (host, key, context) = fixture(RegionKind::Configure);
    let file = key.source_dir.join("CMakeLists.txt");
    let missing = key.binary_dir.join("candidate");
    host.seed_file(&file, b"original")?;
    host.env_set(b"OPTION", Some(b"initial"));
    let mut state = RegionState::default();
    for stamp in [
        JournalStamp::FileHash {
            path: file.clone(),
            digest: JournalDigest::hash(b"original"),
        },
        JournalStamp::DirEntries {
            path: key.source_dir.clone(),
            digest: directory_digest(&host, &key.source_dir)?,
        },
        JournalStamp::Missing {
            path: missing.clone(),
        },
        JournalStamp::EnvVar {
            name: b"OPTION".to_vec(),
            digest: Some(JournalDigest::hash(b"initial")),
        },
    ] {
        state.record_input(JournalInput::Host(stamp));
    }
    let record = state.finish(key.clone(), context.clone());
    let verify = || verify_region(&host, &record, &key, &context, unavailable);
    assert!(verify().is_ok());
    host.env_set(b"OPTION", None);
    assert!(matches!(
        verify(),
        Err(ReplayMiss {
            reason: ReplayMissReason::Input { index: 3, .. },
            ..
        })
    ));
    host.seed_file(&missing, b"new")?;
    assert!(matches!(
        verify(),
        Err(ReplayMiss {
            reason: ReplayMissReason::Input { index: 2, .. },
            ..
        })
    ));
    host.seed_file(&key.source_dir.join("sibling.cmake"), b"new")?;
    assert!(matches!(
        verify(),
        Err(ReplayMiss {
            reason: ReplayMissReason::Input { index: 1, .. },
            ..
        })
    ));
    host.seed_file(&file, b"changed!")?;
    assert!(matches!(
        verify(),
        Err(ReplayMiss {
            reason: ReplayMissReason::Input { index: 0, .. },
            ..
        })
    ));
    Ok(())
}

#[test]
fn file_stat_trace_accepts_a_touch_and_detects_changed_contents() -> HostResult<()> {
    let (host, key, context) = fixture(RegionKind::Configure);
    let file = key.source_dir.join("input.cmake");
    host.seed_file(&file, b"initial")?;
    let mut state = RegionState::default();
    state.record_input(JournalInput::Host(JournalStamp::FileStat {
        path: file.clone(),
        stat: JournalFileStat::read(&host, &file)?,
        digest: Some(JournalDigest::hash(b"initial")),
    }));
    let record = state.finish(key.clone(), context.clone());
    let verify = || verify_region(&host, &record, &key, &context, unavailable);
    assert!(verify().is_ok());
    let timestamp = FileTime {
        secs: 123,
        nanos: 0,
    };
    host.set_times(&file, None, Some(timestamp))?;
    assert!(verify().is_ok());
    host.seed_file(&file, b"new content")?;
    assert!(matches!(
        verify(),
        Err(ReplayMiss {
            reason: ReplayMissReason::Input { index: 0, .. },
            ..
        })
    ));
    Ok(())
}

#[test]
fn exact_link_canonical_and_timestamp_inputs_detect_changes() -> HostResult<()> {
    let (host, key, context) = fixture(RegionKind::Configure);
    let first = key.source_dir.join("first");
    let second = key.source_dir.join("second");
    let alias = key.source_dir.join("link");
    host.seed_file(&first, b"identical contents")?;
    host.seed_file(&second, b"identical contents")?;
    host.symlink(&first, &alias)?;
    let stamps = [
        JournalStamp::Link {
            path: alias.clone(),
            target: first.clone(),
        },
        JournalStamp::Canonical {
            path: alias.clone(),
            target: first.clone(),
        },
        JournalStamp::Metadata {
            path: first.clone(),
            follow: true,
            stat: JournalFileStat::read(&host, &first)?,
        },
    ];
    let records: Vec<_> = stamps
        .into_iter()
        .map(|stamp| {
            let mut state = RegionState::default();
            state.record_input(JournalInput::Host(stamp));
            state.finish(key.clone(), context.clone())
        })
        .collect();
    assert!(
        records
            .iter()
            .all(|record| verify_region(&host, record, &key, &context, unavailable).is_ok())
    );
    host.remove_file(&alias)?;
    host.symlink(&second, &alias)?;
    host.set_times(
        &first,
        None,
        Some(FileTime {
            secs: 321,
            nanos: 0,
        }),
    )?;
    assert!(
        records
            .iter()
            .all(|record| verify_region(&host, record, &key, &context, unavailable).is_err())
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn frame_round_trips_native_paths_and_rejects_wrong_lengths()
-> Result<(), Box<dyn std::error::Error>> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let (_, mut key, context) = fixture(RegionKind::Configure);
    key.source_dir.push(OsStr::from_bytes(b"some-\xff-path"));
    let mut state = RegionState::default();
    state.record_input(JournalInput::Host(JournalStamp::Missing {
        path: key.source_dir.join("input"),
    }));
    let snapshot = ConfigureSnapshot::new(state.finish(key, context), vec![255_u8])?;
    let mut framed = snapshot.encode()?;
    assert_eq!(ConfigureSnapshot::<Vec<u8>>::decode(&framed)?, snapshot);
    framed.push(0);
    assert!(ConfigureSnapshot::<Vec<u8>>::decode(&framed).is_err());
    Ok(())
}

#[test]
fn snapshot_codec_rejects_truncation_and_bit_changes() -> Result<(), Box<dyn std::error::Error>> {
    let (_, key, context) = fixture(RegionKind::Configure);
    let region = RegionState::default().finish(key, context);
    let snapshot = ConfigureSnapshot::new(region, vec![0, 1, 255])?;
    let seed = snapshot.encode()?;
    assert_eq!(ConfigureSnapshot::<Vec<u8>>::decode(&seed)?, snapshot);
    for offset in (0..seed.len()).step_by(7) {
        let mut changed = seed.clone();
        if let Some(byte) = changed.get_mut(offset) {
            *byte ^= 0x40;
        }
        assert!(ConfigureSnapshot::<Vec<u8>>::decode(&changed).is_err());
        assert!(
            ConfigureSnapshot::<Vec<u8>>::decode(seed.get(..offset).unwrap_or_default()).is_err()
        );
    }
    let mut previous = snapshot;
    previous.schema = "configure.v1".into();
    assert!(ConfigureSnapshot::<Vec<u8>>::decode(&previous.encode()?).is_err());
    Ok(())
}

#[test]
fn snapshot_encode_enforces_budget_and_utf8_on_public_models()
-> Result<(), Box<dyn std::error::Error>> {
    let (_, key, context) = fixture(RegionKind::Configure);
    let region = RegionState::default().finish(key, context);

    // 8 MiB of 255_u8 requires >16 MiB as a JSON array and hits the array lower bound immediately.
    let oversized_8m = ConfigureSnapshot::new(region.clone(), vec![255_u8; 8 * 1024 * 1024])?;
    assert_eq!(oversized_8m.encode(), Err(ReplayCodecError::Payload));

    // 5 MiB of 255_u8 passes the 2*len+1 array lower bound (~10 MiB) and stops while streaming
    // into the 16 MiB budget (~20 MiB encoded) without allocating a JSON Value tree.
    let oversized_5m = ConfigureSnapshot::new(region.clone(), vec![255_u8; 5 * 1024 * 1024])?;
    assert_eq!(oversized_5m.encode(), Err(ReplayCodecError::Payload));

    // Invalid UTF-8 strings and object keys inside Value models still fail with Payload.
    let bad_value_str =
        ConfigureSnapshot::new(region.clone(), rcmake_json::Value::string(&b"bad-\xff"[..]))?;
    assert_eq!(bad_value_str.encode(), Err(ReplayCodecError::Payload));

    let bad_value_key = ConfigureSnapshot::new(
        region.clone(),
        rcmake_json::Value::object_from([(&b"bad-\xff"[..], rcmake_json::Value::Null)]),
    )?;
    assert_eq!(bad_value_key.encode(), Err(ReplayCodecError::Payload));

    let valid_value = ConfigureSnapshot::new(
        region,
        rcmake_json::Value::object_from([("ok", rcmake_json::Value::string("valid-é-😀"))]),
    )?;
    let framed = valid_value.encode()?;
    assert_eq!(
        ConfigureSnapshot::<rcmake_json::Value>::decode(&framed)?,
        valid_value
    );
    Ok(())
}
