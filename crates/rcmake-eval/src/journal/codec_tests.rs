use std::error::Error;
use std::path::PathBuf;

use rcmake_diag::{Backtrace, Frame, FrameKind};
use rcmake_host::PureHost;
use rcmake_json::{FromJson, ToJson, Value};
use rcmake_model::{CacheEntry, CacheType, PropertyMap};

use super::{bytes, from_bytes, from_path, path, round_trip};
use crate::journal::region::RegionPayload;
use crate::journal::{
    CacheValue, ConfigureSnapshot, EntryContext, JournalDigest, JournalEffect, JournalFileStat,
    JournalInput, JournalStamp, OpaqueOperation, OpaqueReason, PropertyEdit, PropertyKey,
    PropertyOrigin, PropertyOwner, RegionKey, RegionKind, RegionRecord, RegionState, StateKey,
    TargetKey, VariableScope,
};

type TestResult = Result<(), Box<dyn Error>>;

fn root() -> PathBuf {
    PathBuf::from(if cfg!(windows) { "C:/work" } else { "/work" })
}

fn digest(seed: &[u8]) -> JournalDigest {
    JournalDigest::hash(seed)
}

fn origin() -> PropertyOrigin {
    let backtrace = Backtrace::default()
        .push(Frame {
            file: b"\xff/CMakeLists.txt".as_slice().into(),
            line: 3,
            name: Some(b"set_property".as_slice().into()),
            kind: FrameKind::Command,
        })
        .push(Frame {
            file: b"/work/deferred.cmake".as_slice().into(),
            line: 0,
            name: None,
            kind: FrameKind::Deferred,
        })
        .push(Frame {
            file: "/work/é.cmake".as_bytes().into(),
            line: u32::MAX,
            name: Some(b"\xfe".as_slice().into()),
            kind: FrameKind::ModuleInclude,
        });
    PropertyOrigin::new(root().join("build"), &backtrace)
}

fn stat(host: &PureHost) -> Result<JournalFileStat, Box<dyn Error>> {
    let file = root().join("stat.txt");
    host.seed_file(&file, b"content")?;
    Ok(JournalFileStat::read(host, &file)?)
}

fn cache_value() -> CacheValue {
    let mut props = PropertyMap::default();
    props.set(b"STRINGS".as_slice().into(), b"\xff;two".as_slice().into());
    props.set(b"ADVANCED".as_slice().into(), b"1".as_slice().into());
    CacheValue::from(&CacheEntry {
        value: b"one\0\xff".as_slice().into(),
        ty: CacheType::Filepath,
        help: "Help \"text\"\n".into(),
        advanced: true,
        props,
    })
}

fn target_key() -> TargetKey {
    TargetKey {
        binary_dir: root().join("build/sub"),
        name: b"lib\xff".to_vec(),
    }
}

fn property_key(owner: PropertyOwner) -> PropertyKey {
    PropertyKey {
        owner,
        name: b"COMPILE_OPTIONS".to_vec(),
    }
}

fn region_key(kind: RegionKind) -> RegionKey {
    RegionKey {
        binary_dir: root().join("build"),
        source_dir: root().join("src"),
        kind,
    }
}

fn context() -> EntryContext {
    EntryContext {
        rcmake: digest(b"rcmake"),
        policies: digest(b"policies"),
        diag: digest(b"diag"),
        log_level: u8::MAX,
        flags: digest(b"flags"),
    }
}

fn same<T: ToJson + FromJson + PartialEq + std::fmt::Debug>(value: T) -> TestResult {
    assert_eq!(round_trip(&value)?, value);
    Ok(())
}

#[test]
fn bytes_keep_every_byte() -> TestResult {
    let samples: [&[u8]; 6] = [
        b"",
        b"plain",
        "é and \u{1f600}".as_bytes(),
        b"\0\x1f\"\\\n",
        b"\xff\xfe-raw",
        b"ok\xc3",
    ];
    for sample in samples {
        let encoded = bytes(sample);
        assert_eq!(
            matches!(encoded, Value::String(_)),
            core::str::from_utf8(sample).is_ok()
        );
        let text = rcmake_json::write_compact(&encoded);
        let options = rcmake_json::ReadOptions::default();
        assert_eq!(from_bytes(&rcmake_json::parse(&text, &options)?)?, sample);
    }
    assert!(from_bytes(&Value::Array(vec![Value::UInt(256)])).is_err());
    assert!(from_bytes(&Value::Bool(true)).is_err());
    Ok(())
}

#[test]
fn paths_keep_native_bytes() -> TestResult {
    for sample in [root().join("a b/é.cmake"), PathBuf::new()] {
        assert_eq!(from_path(&path(&sample))?, sample);
    }
    #[cfg(unix)]
    {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let raw = root().join(OsStr::from_bytes(b"raw-\xff-dir"));
        assert!(matches!(path(&raw), Value::Array(_)));
        assert_eq!(from_path(&path(&raw))?, raw);
    }
    Ok(())
}

#[test]
fn digest_uses_lowercase_hex() -> TestResult {
    let value = digest(b"abc");
    let encoded = value.to_json();
    assert_eq!(
        encoded.as_str(),
        Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
    );
    same(value)?;
    for bad in [
        "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD",
        "ba7816bf",
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015a",
        "zz7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
    ] {
        assert!(JournalDigest::from_json(&Value::string(bad)).is_err());
    }
    Ok(())
}

#[test]
fn trace_records_round_trip() -> TestResult {
    let host = PureHost::new(root(), root().join("rcmake"));
    let stat = stat(&host)?;
    same(stat)?;
    let stamps = [
        JournalStamp::FileStat {
            path: root().join("a.txt"),
            stat,
            digest: Some(digest(b"a")),
        },
        JournalStamp::FileStat {
            path: root().join("a.txt"),
            stat,
            digest: None,
        },
        JournalStamp::FileHash {
            path: root().join("b.txt"),
            digest: digest(b"b"),
        },
        JournalStamp::DirEntries {
            path: root().join("dir"),
            digest: digest(b"dir"),
        },
        JournalStamp::Missing {
            path: root().join("missing"),
        },
        JournalStamp::Link {
            path: root().join("link"),
            target: PathBuf::from("../target"),
        },
        JournalStamp::Canonical {
            path: root().join("x/../y"),
            target: root().join("y"),
        },
        JournalStamp::Metadata {
            path: root().join("meta"),
            follow: false,
            stat,
        },
        JournalStamp::EnvVar {
            name: b"PATH\xff".to_vec(),
            digest: Some(digest(b"value")),
        },
        JournalStamp::EnvVar {
            name: b"UNSET".to_vec(),
            digest: None,
        },
        JournalStamp::NegativeLookup {
            root: root().join("include"),
            relative: PathBuf::from("missing/header.h"),
            deepest: root().join("include"),
            listing: digest(b"listing"),
        },
    ];
    for stamp in stamps {
        same(stamp.clone())?;
        same(JournalInput::Host(stamp))?;
    }
    same(VariableScope::Current)?;
    same(VariableScope::Parent)?;
    same(target_key())?;
    let owners = [
        PropertyOwner::Global,
        PropertyOwner::Directory(root().join("build")),
        PropertyOwner::Target(target_key()),
    ];
    for owner in owners {
        same(owner.clone())?;
        same(property_key(owner))?;
    }
    let states = [
        StateKey::Variable {
            scope: VariableScope::Parent,
            name: b"X".to_vec(),
        },
        StateKey::EffectiveVariable(b"Y\xff".to_vec()),
        StateKey::Cache(b"CACHE".to_vec()),
        StateKey::Property(property_key(PropertyOwner::Global)),
        StateKey::Policy(u16::MAX),
        StateKey::Target(target_key()),
        StateKey::Command(b"my_command".to_vec()),
        StateKey::Probe(digest(b"probe")),
    ];
    for key in states {
        same(key.clone())?;
        same(JournalInput::State {
            key: key.clone(),
            value: Some(digest(b"value")),
        })?;
        same(JournalInput::State { key, value: None })?;
    }
    Ok(())
}

#[test]
fn region_records_round_trip() -> TestResult {
    let kinds = [
        RegionKind::Configure,
        RegionKind::Subdirectory,
        RegionKind::Include {
            file: root().join("src/include.cmake"),
        },
        RegionKind::Bootstrap {
            language: b"CXX".to_vec(),
        },
    ];
    for kind in kinds {
        same(kind.clone())?;
        same(region_key(kind))?;
    }
    same(context())?;
    let reasons = [
        OpaqueReason::UnclassifiedCommand,
        OpaqueReason::ExecuteProcess,
        OpaqueReason::RandomOrTimestamp,
        OpaqueReason::ExternalMutation,
        OpaqueReason::LimitExceeded,
    ];
    for reason in reasons {
        same(reason)?;
        same(OpaqueOperation {
            reason,
            command: Some(b"execute_process".to_vec()),
            file: root().join("src/CMakeLists.txt"),
            line: 7,
        })?;
        same(OpaqueOperation {
            reason,
            command: None,
            file: PathBuf::new(),
            line: 0,
        })?;
    }
    let cache = cache_value();
    same(cache.clone())?;
    let origin = origin();
    same(origin.clone())?;
    let edits = [
        PropertyEdit::Set(Some(b"\xffvalue".to_vec())),
        PropertyEdit::Set(None),
        PropertyEdit::AppendList(b"item".to_vec()),
        PropertyEdit::AppendString(b"".to_vec()),
    ];
    let mut effects = vec![
        JournalEffect::SetVar {
            scope: VariableScope::Current,
            name: b"A".to_vec(),
            value: Some(b"1".to_vec()),
        },
        JournalEffect::SetVar {
            scope: VariableScope::Parent,
            name: b"B".to_vec(),
            value: None,
        },
        JournalEffect::SetCache {
            name: b"C".to_vec(),
            entry: Some(cache),
        },
        JournalEffect::SetCache {
            name: b"D".to_vec(),
            entry: None,
        },
    ];
    for edit in edits {
        same(edit.clone())?;
        effects.push(JournalEffect::Property {
            key: property_key(PropertyOwner::Directory(root().join("build"))),
            edit,
            origin: origin.clone(),
        });
    }
    for effect in &effects {
        same(effect.clone())?;
    }
    let inputs = vec![
        JournalInput::Host(JournalStamp::Missing {
            path: root().join("missing"),
        }),
        JournalInput::State {
            key: StateKey::Cache(b"C".to_vec()),
            value: None,
        },
    ];
    let payloads = [
        RegionPayload::Complete { inputs, effects },
        RegionPayload::Complete {
            inputs: Vec::new(),
            effects: Vec::new(),
        },
        RegionPayload::Opaque(OpaqueOperation {
            reason: OpaqueReason::ExecuteProcess,
            command: Some(b"execute_process".to_vec()),
            file: root().join("src/CMakeLists.txt"),
            line: 2,
        }),
    ];
    for payload in payloads {
        same(payload.clone())?;
        same(RegionRecord {
            key: region_key(RegionKind::Subdirectory),
            context: context(),
            payload,
        })?;
    }
    Ok(())
}

#[test]
fn snapshot_round_trips_with_unit_model() -> TestResult {
    let mut state = RegionState::default();
    state.record_input(JournalInput::Host(JournalStamp::Missing {
        path: root().join("src/input"),
    }));
    let snapshot = ConfigureSnapshot::new(
        state.finish(region_key(RegionKind::Configure), context()),
        (),
    )?;
    same(snapshot.clone())?;
    assert_eq!(
        ConfigureSnapshot::<()>::decode(&snapshot.encode()?)?,
        snapshot
    );
    Ok(())
}

#[test]
fn decoders_reject_unknown_fields_and_variants() {
    let mut key = region_key(RegionKind::Configure).to_json();
    if let Value::Object(members) = &mut key {
        members.insert("extra".into(), Value::Null);
    }
    assert!(RegionKey::from_json(&key).is_err());
    assert!(RegionKind::from_json(&Value::string("Other")).is_err());
    assert!(RegionKind::from_json(&Value::object_from([("Configure", Value::Null)])).is_err());
    assert!(
        PropertyOwner::from_json(&Value::object_from([
            ("Global", Value::Null),
            ("Target", Value::Null),
        ]))
        .is_err()
    );
    assert!(VariableScope::from_json(&Value::string("current")).is_err());
    assert!(OpaqueReason::from_json(&Value::UInt(0)).is_err());
}

#[test]
fn origin_digest_covers_every_frame_field() {
    let base = origin();
    assert_eq!(base.digest(), origin().digest());
    let frame = |line, name: Option<&[u8]>, kind| {
        let backtrace = Backtrace::default().push(Frame {
            file: b"/work/CMakeLists.txt".as_slice().into(),
            line,
            name: name.map(Into::into),
            kind,
        });
        PropertyOrigin::new(root().join("build"), &backtrace).digest()
    };
    let reference = frame(1, Some(b"set".as_slice()), FrameKind::Command);
    assert_ne!(
        reference,
        frame(2, Some(b"set".as_slice()), FrameKind::Command)
    );
    assert_ne!(reference, frame(1, None, FrameKind::Command));
    assert_ne!(
        reference,
        frame(1, Some(b"get".as_slice()), FrameKind::Command)
    );
    assert_ne!(
        reference,
        frame(1, Some(b"set".as_slice()), FrameKind::Deferred)
    );
    let other_dir = PropertyOrigin::new(root().join("other"), &Backtrace::default());
    assert_ne!(
        other_dir.digest(),
        PropertyOrigin::new(root().join("build"), &Backtrace::default()).digest()
    );
}
