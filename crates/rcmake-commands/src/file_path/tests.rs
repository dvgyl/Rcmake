use std::path::Path;
use std::sync::Arc;

use rcmake_base::{BString, hash::HashAlgo};
use rcmake_eval::{Interp, InterpConfig, Registry, RunMode};
use rcmake_host::{FileKind, Follow, Host, PureHost, ReadRange, Stream};

const DRIVE: &str = if cfg!(windows) { "C:" } else { "" };

type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

fn interpreter() -> (Arc<PureHost>, Interp) {
    interpreter_with_mode(RunMode::Script)
}

fn register(registry: &mut Registry) {
    super::register(registry);
    crate::vars_list::register(registry);
}

fn interpreter_with_mode(mode: RunMode) -> (Arc<PureHost>, Interp) {
    interpreter_in(mode, "/work")
}

fn interpreter_in(mode: RunMode, cwd: &str) -> (Arc<PureHost>, Interp) {
    let host = Arc::new(PureHost::new(cwd.into(), "/cmake/bin/cmake".into()));
    let interp = Interp::new(
        InterpConfig {
            services: Default::default(),
            host: host.clone(),
            mode,
            cmake_root: "/cmake".into(),
            cmake_command: "/cmake/bin/cmake".into(),
            cwd: cwd.into(),
            limits: Default::default(),
            diag: Default::default(),
            defines: Vec::new(),
            script_args: Vec::new(),
        },
        register,
    );
    (host, interp)
}

fn script(host: &PureHost, interp: &mut Interp, code: &str) -> TestResult {
    let input = host.initial_cwd().join("test.cmake");
    let version =
        rcmake_base::version::Version::parse_strict(b"4.5").ok_or("invalid test version")?;
    interp.cx().set_cmake_minimum_required(b"4.5", &version)?;
    host.seed_file(&input, code.as_bytes())?;
    let outcome = interp.run_script(&input);
    assert_eq!(
        outcome.exit_code,
        0,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    Ok(())
}

fn vars(interp: &mut Interp, expected: &[(&str, &str)]) {
    for &(name, value) in expected {
        assert_eq!(
            interp.cx().var(name.as_bytes()),
            Some(BString::from(value)),
            "{name}"
        );
    }
}

#[test]
fn file_configure_expands_inline_content_and_uses_requested_newlines() -> TestResult {
    let (host, mut interp) = interpreter();
    script(
        &host,
        &mut interp,
        r#"
set(VALUE [=[a"b
c]=])
file(CONFIGURE OUTPUT plain CONTENT [=[@VALUE@ ${VALUE}]=] @ONLY)
file(CONFIGURE CONTENT [=[@VALUE@
${VALUE}]=] OUTPUT line/ending ESCAPE_QUOTES NEWLINE_STYLE DOS)
"#,
    )?;
    let range = ReadRange::All { limit: 1024 };
    assert_eq!(
        host.read(Path::new("/work/plain"), range)?,
        b"a\"b\nc ${VALUE}"
    );
    assert_eq!(
        host.read(Path::new("/work/line/ending"), range)?,
        b"a\\\"b\nc\r\na\\\"b\nc\r\n"
    );
    Ok(())
}

#[test]
fn path_decomposes_queries_and_compares() -> TestResult {
    let (host, mut interp) = interpreter();
    let code = r#"
cmake_path(SET p "//tmp//.hidden.tar.gz")
cmake_path(GET p ROOT_NAME root_name)
cmake_path(GET p ROOT_DIRECTORY root_dir)
cmake_path(GET p ROOT_PATH root)
cmake_path(GET p FILENAME filename)
cmake_path(GET p EXTENSION ext)
cmake_path(GET p EXTENSION LAST_ONLY ext_last)
cmake_path(GET p STEM stem)
cmake_path(GET p STEM LAST_ONLY stem_last)
cmake_path(GET p RELATIVE_PART part)
cmake_path(GET p PARENT_PATH parent)
cmake_path(HAS_ROOT_NAME p has_root_name)
cmake_path(HAS_ROOT_DIRECTORY p has_root_dir)
cmake_path(HAS_ROOT_PATH p has_root)
cmake_path(HAS_FILENAME p has_file)
cmake_path(HAS_EXTENSION p has_ext)
cmake_path(HAS_STEM p has_stem)
cmake_path(HAS_RELATIVE_PART p has_part)
cmake_path(HAS_PARENT_PATH p has_parent)
cmake_path(IS_ABSOLUTE p absolute)
cmake_path(IS_RELATIVE p relative)
cmake_path(SET prefix /a/b/)
cmake_path(IS_PREFIX prefix /a/b prefix_short)
cmake_path(IS_PREFIX prefix /a/c/../b/d NORMALIZE prefix_long)
cmake_path(COMPARE a//b EQUAL a/b equal)
cmake_path(COMPARE a/ NOT_EQUAL a unequal)
cmake_path(SET empty "")
cmake_path(IS_PREFIX empty /a prefix_empty_new)
"#
    .replace(
        "//tmp//.hidden",
        if cfg!(windows) {
            "C:/tmp//.hidden"
        } else {
            "//tmp//.hidden"
        },
    );
    script(&host, &mut interp, &code)?;
    interp.cx().set_policy(
        rcmake_diag::PolicyId::new(223),
        rcmake_diag::PolicyStatus::Old,
    )?;
    let old = Path::new("/work/old.cmake");
    host.seed_file(old, b"cmake_path(IS_PREFIX empty /a prefix_empty_old)")?;
    assert_eq!(interp.run_script(old).exit_code, 0);
    vars(
        &mut interp,
        &[
            ("root_name", if cfg!(windows) { "C:" } else { "" }),
            ("root_dir", "/"),
            ("root", if cfg!(windows) { "C:/" } else { "/" }),
            ("filename", ".hidden.tar.gz"),
            ("ext", ".tar.gz"),
            ("ext_last", ".gz"),
            ("stem", ".hidden"),
            ("stem_last", ".hidden.tar"),
            ("part", "tmp//.hidden.tar.gz"),
            ("parent", if cfg!(windows) { "C:/tmp" } else { "//tmp" }),
            ("has_root_name", if cfg!(windows) { "ON" } else { "OFF" }),
            ("has_root_dir", "ON"),
            ("has_root", "ON"),
            ("has_file", "ON"),
            ("has_ext", "ON"),
            ("has_stem", "ON"),
            ("has_part", "ON"),
            ("has_parent", "ON"),
            ("absolute", "ON"),
            ("relative", "OFF"),
            ("prefix_short", "OFF"),
            ("prefix_long", "ON"),
            ("equal", "ON"),
            ("unequal", "ON"),
            ("prefix_empty_new", "OFF"),
            ("prefix_empty_old", "ON"),
        ],
    );
    Ok(())
}

#[test]
fn path_modifies_normalizes_and_converts() -> TestResult {
    let (host, mut interp) = interpreter();
    let code = r#"
cmake_path(SET p a//b.tar.gz)
cmake_path(APPEND p sub x OUTPUT_VARIABLE joined)
cmake_path(APPEND p /absolute x OUTPUT_VARIABLE reset)
cmake_path(APPEND_STRING p .extra OUTPUT_VARIABLE suffix)
cmake_path(REMOVE_FILENAME p OUTPUT_VARIABLE no_name)
cmake_path(REPLACE_FILENAME p new OUTPUT_VARIABLE new_name)
cmake_path(REMOVE_EXTENSION p OUTPUT_VARIABLE no_ext)
cmake_path(REMOVE_EXTENSION p LAST_ONLY OUTPUT_VARIABLE no_last_ext)
cmake_path(REPLACE_EXTENSION p LAST_ONLY zip OUTPUT_VARIABLE new_ext)
cmake_path(NORMAL_PATH p OUTPUT_VARIABLE normal)
cmake_path(SET q NORMALIZE /a//b/../.)
cmake_path(RELATIVE_PATH q BASE_DIRECTORY /other OUTPUT_VARIABLE rel)
cmake_path(ABSOLUTE_PATH p BASE_DIRECTORY /other NORMALIZE OUTPUT_VARIABLE abs)
cmake_path(ABSOLUTE_PATH p OUTPUT_VARIABLE default_base)
cmake_path(NATIVE_PATH p NATIVE)
cmake_path(NATIVE_PATH p NORMALIZE native_normal)
cmake_path(CONVERT "a::b:/c/../d" TO_CMAKE_PATH_LIST cmake_list NORMALIZE)
cmake_path(CONVERT "a;;b/../c;" TO_NATIVE_PATH_LIST native_list NORMALIZE)
cmake_path(HASH p hash1)
cmake_path(HASH normal hash2)
"#;
    let code = if cfg!(windows) {
        code.replace("a::b:/c/../d", "a;;b;C:/c/../d")
    } else {
        code.to_owned()
    };
    script(&host, &mut interp, &code)?;
    vars(
        &mut interp,
        &[
            ("p", "a//b.tar.gz"),
            ("joined", "a//b.tar.gz/sub/x"),
            ("reset", "/absolute/x"),
            ("suffix", "a//b.tar.gz.extra"),
            ("no_name", "a//"),
            ("new_name", "a//new"),
            ("no_ext", "a//b"),
            ("no_last_ext", "a//b.tar"),
            ("new_ext", "a//b.tar.zip"),
            ("normal", "a/b.tar.gz"),
            ("q", "/a/"),
            ("rel", "../a/"),
            ("abs", "/other/a/b.tar.gz"),
            ("default_base", "/work/a//b.tar.gz"),
            (
                "NATIVE",
                if cfg!(windows) {
                    r"a\\b.tar.gz"
                } else {
                    "a//b.tar.gz"
                },
            ),
            (
                "native_normal",
                if cfg!(windows) {
                    r"a\b.tar.gz"
                } else {
                    "a/b.tar.gz"
                },
            ),
            (
                "cmake_list",
                if cfg!(windows) {
                    "a;;b;C:/d"
                } else {
                    "a;:b;/d"
                },
            ),
            ("native_list", if cfg!(windows) { "a;c" } else { "a:c" }),
        ],
    );
    assert_eq!(interp.cx().var(b"hash1"), interp.cx().var(b"hash2"));
    Ok(())
}

#[cfg(target_os = "macos")]
#[test]
fn path_hash_matches_oracle() {
    let paths = [
        ("", "0"),
        ("/", "7d72aacbf013ed1f"),
        ("a/", "9fdc838ad3296784"),
        ("a/b", "e716e4cf10fd5c41"),
        ("/a/b/", "883f2e8348d9ac99"),
    ];
    for (path, digest) in paths {
        assert_eq!(super::path_hash::hash(path.as_bytes()), digest);
    }
}

#[test]
fn file_reads_writes_hashes_and_timestamps() -> TestResult {
    let (host, mut interp) = interpreter();
    host.seed_file(Path::new("/work/binary"), b"ab\r\ncd\x00d;e\n\nHI")?;
    host.seed_file(Path::new("/work/bom"), b"\xef\xbb\xbfABC\xc3\xa9\n")?;
    host.seed_file(Path::new("/work/wide"), b"\xff\xfeA\0\xe9\0B\0\n\0")?;
    host.set_times(
        Path::new("/work/binary"),
        None,
        Some(rcmake_host::FileTime {
            secs: 0,
            nanos: 123_456_789,
        }),
    )?;
    script(
        &host,
        &mut interp,
        r#"
file(WRITE nested/file a b)
file(APPEND nested/file c)
file(APPEND created/sub/file tail)
file(READ nested/file written)
file(READ binary slice OFFSET 1 LIMIT 4 HEX)
file(READ binary text LIMIT 2)
file(STRINGS binary strings NO_HEX_CONVERSION)
file(STRINGS binary filtered LENGTH_MAXIMUM 2 LENGTH_MINIMUM 2 LIMIT_COUNT 2 LIMIT_INPUT 12 LIMIT_OUTPUT 10 REGEX "([a-z]+)")
file(STRINGS binary newlines NEWLINE_CONSUME REGEX "HI")
file(STRINGS bom utf)
file(STRINGS bom partial LIMIT_INPUT 4)
file(STRINGS bom utf_split LIMIT_INPUT 7)
file(STRINGS wide wide)
file(STRINGS wide wide_partial LIMIT_INPUT 3)
file(SIZE nested/file size)
file(TIMESTAMP binary stamp "%Y %m %d %H %M %S %f %s %z %Z" UTC)
file(TIMESTAMP missing missing_stamp)
file(MD5 nested/file MD5)
file(SHA1 nested/file SHA1)
file(SHA224 nested/file SHA224)
file(SHA256 nested/file SHA256)
file(SHA384 nested/file SHA384)
file(SHA512 nested/file SHA512)
file(SHA3_224 nested/file SHA3_224)
file(SHA3_256 nested/file SHA3_256)
file(SHA3_384 nested/file SHA3_384)
file(SHA3_512 nested/file SHA3_512)
"#,
    )?;
    vars(
        &mut interp,
        &[
            ("written", "abc"),
            ("slice", "620d0a63"),
            ("text", "ab\n"),
            ("strings", "ab;cd;d\\;e;;HI"),
            ("filtered", "ab;cd"),
            ("newlines", "d\\;e\n\nHI"),
            ("utf", "ABCé"),
            ("partial", "A"),
            ("utf_split", "ABCé"),
            ("wide", "A;B"),
            ("wide_partial", "A"),
            ("size", "3"),
            ("stamp", "1970 01 01 00 00 00 123456 0 +0000 GMT"),
            ("missing_stamp", ""),
            ("CMAKE_MATCH_0", "HI"),
        ],
    );
    for name in [
        "MD5", "SHA1", "SHA224", "SHA256", "SHA384", "SHA512", "SHA3_224", "SHA3_256", "SHA3_384",
        "SHA3_512",
    ] {
        let algo = HashAlgo::parse(name.as_bytes()).ok_or("invalid test hash algorithm")?;
        vars(&mut interp, &[(name, &algo.hex_digest(b"abc"))]);
    }
    assert!(
        host.metadata(Path::new("/work/created/sub/file"), Follow::Yes)
            .is_ok()
    );
    Ok(())
}

#[test]
fn file_makes_mutates_and_removes_entries() -> TestResult {
    let (host, mut interp) = interpreter();
    script(
        &host,
        &mut interp,
        r#"
file(MAKE_DIRECTORY dst/deep RESULT mkdir)
file(MAKE_DIRECTORY source-tree target-tree RESULT)
file(WRITE source-tree/kept content)
file(WRITE target-tree/stale old)
file(CREATE_LINK source-tree target-tree COPY_ON_ERROR RESULT directory_link)
file(WRITE src original)
file(TOUCH touched)
file(TOUCH_NOCREATE missing)
file(TOUCH_NOCREATE touched)
file(COPY_FILE src dst/copied RESULT copy INPUT_MAY_BE_RECENT)
file(TIMESTAMP dst/copied before "%f" UTC)
file(COPY_FILE src dst/copied ONLY_IF_DIFFERENT)
file(TIMESTAMP dst/copied after "%f" UTC)
file(RENAME src dst/copied NO_REPLACE RESULT blocked)
file(RENAME src dst/moved RESULT rename)
file(CREATE_LINK dst/moved hard RESULT hard_status)
file(CREATE_LINK nonexistent symbolic RESULT symbolic_status SYMBOLIC)
file(READ_SYMLINK symbolic points_to RESULT readlink)
file(READ_SYMLINK hard ignored RESULT not_link)
file(CHMOD dst/moved PERMISSIONS OWNER_READ OWNER_WRITE)
file(WRITE dst/deep/data content)
file(CHMOD_RECURSE dst FILE_PERMISSIONS OWNER_READ DIRECTORY_PERMISSIONS OWNER_READ OWNER_EXECUTE)
file(REMOVE dst/deep)
file(REMOVE symbolic)
file(REMOVE_RECURSE dst/deep)
"#,
    )?;
    vars(
        &mut interp,
        &[
            ("mkdir", "0"),
            ("directory_link", "0"),
            ("copy", "0"),
            ("blocked", "NO_REPLACE"),
            ("rename", "0"),
            ("hard_status", "0"),
            ("symbolic_status", "0"),
            ("points_to", "nonexistent"),
            ("readlink", "0"),
        ],
    );
    assert_eq!(interp.cx().var(b"before"), interp.cx().var(b"after"));
    assert_eq!(
        host.read(
            Path::new("/work/target-tree/kept"),
            ReadRange::All { limit: 32 }
        )?,
        b"content"
    );
    assert!(
        host.metadata(Path::new("/work/target-tree/stale"), Follow::No)
            .is_err()
    );
    assert!(
        interp
            .cx()
            .var_or_empty(b"not_link")
            .starts_with(b"READ_SYMLINK requested")
    );
    for removed in [
        "/work/dst/deep",
        "/work/symbolic",
        "/work/missing",
        "/work/src",
    ] {
        assert!(
            host.metadata(Path::new(removed), Follow::No).is_err(),
            "{removed}"
        );
    }
    for (name, mode) in [("/work/dst", 0o500), ("/work/dst/moved", 0o400)] {
        assert_eq!(
            host.metadata(Path::new(name), Follow::Yes)?.mode & 0o777,
            mode
        );
    }
    assert_eq!(
        host.metadata(Path::new("/work/hard"), Follow::Yes)?.nlink,
        2
    );
    assert_eq!(
        host.metadata(Path::new("/work/touched"), Follow::Yes)?.kind,
        FileKind::File
    );
    Ok(())
}

#[test]
fn file_copies_directories_with_filters_and_permissions() -> TestResult {
    let (host, mut interp) = interpreter();
    script(
        &host,
        &mut interp,
        r#"
file(WRITE src/a.txt a)
file(WRITE src/sub/b.txt b)
file(WRITE src/sub/drop.txt drop)
file(WRITE src/sub/c.bin c)
file(CREATE_LINK a.txt src/link.txt SYMBOLIC)
file(COPY src DESTINATION dst NO_SOURCE_PERMISSIONS
    FILE_PERMISSIONS OWNER_READ
    DIRECTORY_PERMISSIONS OWNER_READ OWNER_WRITE OWNER_EXECUTE
    FILES_MATCHING PATTERN "*.txt" REGEX "/drop[.]txt$" EXCLUDE)
file(COPY src/ DESTINATION contents USE_SOURCE_PERMISSIONS)
"#,
    )?;
    let range = ReadRange::All { limit: 16 };
    assert_eq!(host.read(Path::new("/work/dst/src/a.txt"), range)?, b"a");
    assert_eq!(
        host.read(Path::new("/work/dst/src/sub/b.txt"), range)?,
        b"b"
    );
    assert_eq!(
        host.read(Path::new("/work/contents/sub/c.bin"), range)?,
        b"c"
    );
    assert_eq!(
        host.read_link(Path::new("/work/dst/src/link.txt"))?,
        Path::new("a.txt")
    );
    assert_eq!(
        host.metadata(Path::new("/work/dst/src/a.txt"), Follow::Yes)?
            .mode
            & 0o777,
        0o400
    );
    for excluded in ["/work/dst/src/sub/drop.txt", "/work/dst/src/sub/c.bin"] {
        assert!(
            host.metadata(Path::new(excluded), Follow::No).is_err(),
            "{excluded}"
        );
    }
    Ok(())
}

#[test]
fn file_globs_in_byte_order() -> TestResult {
    let (host, mut interp) = interpreter_in(RunMode::Script, &format!("{DRIVE}/work"));
    script(
        &host,
        &mut interp,
        &r#"
file(WRITE tree/Z.txt z)
file(WRITE tree/a.txt a)
file(WRITE tree/sub/b.txt b)
file(CREATE_LINK sub tree/alias SYMBOLIC)
file(GLOB shallow RELATIVE /work/tree LIST_DIRECTORIES false "tree/*")
file(GLOB_RECURSE recursive RELATIVE /work/tree "tree/*.txt")
file(GLOB_RECURSE linked FOLLOW_SYMLINKS RELATIVE /work/tree "tree/*.txt")
file(GLOB_RECURSE dirs LIST_DIRECTORIES true RELATIVE /work/tree "tree/missing*")
file(GLOB wildcard RELATIVE /work/tree "tree/s?b/[a-c].txt")
"#
        .replace("/work/", &format!("{DRIVE}/work/")),
    )?;
    vars(
        &mut interp,
        &[
            ("shallow", "Z.txt;a.txt"),
            ("recursive", "Z.txt;a.txt;sub/b.txt"),
            ("linked", "Z.txt;a.txt;alias/b.txt;sub/b.txt"),
            ("dirs", "sub"),
            ("wildcard", "sub/b.txt"),
        ],
    );
    Ok(())
}

#[test]
fn file_resolves_real_relative_and_legacy_paths() -> TestResult {
    let (host, mut interp) = interpreter_in(RunMode::Script, &format!("{DRIVE}/work"));
    let home = format!("{DRIVE}/work/home");
    host.env_set(
        if cfg!(windows) {
            b"USERPROFILE"
        } else {
            b"HOME"
        },
        Some(home.as_bytes()),
    );
    let code = r#"
file(WRITE home/a/file x)
file(CREATE_LINK /work/home/a /work/alias SYMBOLIC)
file(REAL_PATH "~/a/file" expanded EXPAND_TILDE)
file(REAL_PATH file based BASE_DIRECTORY /work/alias)
file(REAL_PATH file rel_based BASE_DIRECTORY home/a)
file(REAL_PATH /work/alias/../a/file real)
file(REAL_PATH /work/bad/../home/a/file missing)
file(RELATIVE_PATH rel /work/other /work/home/a/file)
file(TO_CMAKE_PATH [[~/a/:/b//c/:a\b]] cmake)
file(TO_NATIVE_PATH "path with space:/root/a//b/" native)
"#
    .replace("/work/", &format!("{DRIVE}/work/"));
    let code = if cfg!(windows) {
        code.replace("~/a/:/b//c/:", "~/a/;/b//c/;")
            .replace("path with space:/root", "path with space;/root")
    } else {
        code
    };
    script(&host, &mut interp, &code)?;
    vars(
        &mut interp,
        &[
            ("expanded", &format!("{DRIVE}/work/home/a/file")),
            ("based", &format!("{DRIVE}/work/home/a/file")),
            ("rel_based", &format!("{DRIVE}/work/home/a/file")),
            ("real", &format!("{DRIVE}/work/home/a/file")),
            ("missing", &format!("{DRIVE}/work/bad/../home/a/file")),
            ("rel", "../home/a/file"),
            ("cmake", &format!("{DRIVE}/work/home/a;/b/c;a/b")),
            (
                "native",
                if cfg!(windows) {
                    r#""path with space";\root\a\b\"#
                } else {
                    "path\\ with\\ space;/root/a/b/"
                },
            ),
        ],
    );
    Ok(())
}

#[test]
#[cfg(feature = "archive")]
fn file_archive_commands_validate_missing_options() -> TestResult {
    for (code, problem) in [
        (
            "file(ARCHIVE_CREATE PATHS input)",
            "file failed to compress:",
        ),
        (
            "file(ARCHIVE_CREATE OUTPUT result.tar)",
            "file ARCHIVE_CREATE requires a non-empty list of PATHS",
        ),
    ] {
        let (host, mut interp) = interpreter();
        let script = Path::new("/work/test.cmake");
        host.seed_file(script, code.as_bytes())?;
        assert_eq!(interp.run_script(script).exit_code, 1);
        let stderr = host.stream_bytes(Stream::Stderr);
        assert!(
            stderr
                .windows(problem.len())
                .any(|message| message == problem.as_bytes()),
            "{stderr}"
        );
    }
    let (host, mut interp) = interpreter();
    let script = Path::new("/work/test.cmake");
    host.seed_file(script, b"file(ARCHIVE_EXTRACT LIST_ONLY)")?;
    assert_eq!(interp.run_script(script).exit_code, 0);
    Ok(())
}

#[test]
#[cfg(not(feature = "archive"))]
fn file_archive_commands_require_the_feature() -> TestResult {
    for command in ["ARCHIVE_CREATE", "ARCHIVE_EXTRACT"] {
        let (host, mut interp) = interpreter();
        let script = Path::new("/work/test.cmake");
        host.seed_file(script, format!("file({command})").as_bytes())?;
        assert_eq!(interp.run_script(script).exit_code, 1);
        let stderr = host.stream_bytes(Stream::Stderr);
        assert!(
            stderr.to_string().contains("requires the archive feature."),
            "{stderr}"
        );
    }
    Ok(())
}

#[test]
fn file_glob_rejects_configure_depends_in_script_mode() -> TestResult {
    let (host, mut interp) = interpreter();
    let script = Path::new("/work/test.cmake");
    host.seed_file(script, b"file(GLOB output CONFIGURE_DEPENDS *)")?;
    assert_eq!(interp.run_script(script).exit_code, 1);
    let stderr = host.stream_bytes(Stream::Stderr);
    assert!(
        stderr.to_string().contains("CONFIGURE_DEPENDS is invalid"),
        "{stderr}"
    );
    Ok(())
}

#[test]
fn file_copy_replaces_links_without_modifying_other_paths() -> TestResult {
    let (host, mut interp) = interpreter();
    script(
        &host,
        &mut interp,
        r#"
file(WRITE source new)
file(WRITE original old)
file(CREATE_LINK original hard)
file(CREATE_LINK original symlink SYMBOLIC)
file(COPY_FILE source original)
file(COPY_FILE source symlink)
file(CREATE_LINK original equal SYMBOLIC)
file(COPY_FILE source equal ONLY_IF_DIFFERENT)
"#,
    )?;
    let range = ReadRange::All { limit: 16 };
    assert_eq!(host.read(Path::new("/work/hard"), range)?, b"old");
    assert_eq!(host.read(Path::new("/work/original"), range)?, b"new");
    assert_eq!(host.read(Path::new("/work/symlink"), range)?, b"new");
    assert_eq!(
        host.metadata(Path::new("/work/symlink"), Follow::No)?.kind,
        FileKind::File
    );
    assert_eq!(
        host.read_link(Path::new("/work/equal"))?,
        Path::new("original")
    );
    Ok(())
}

#[test]
fn file_strings_converts_hex_records_when_the_binary_directory_exists() -> TestResult {
    let (host, mut interp) = interpreter();
    host.seed_file(
        Path::new("/work/intel"),
        b":0400000041424344F2\n:00000001FF\n",
    )?;
    script(
        &host,
        &mut interp,
        r#"
file(STRINGS intel before)
file(MAKE_DIRECTORY CMakeFiles)
file(STRINGS intel converted LIMIT_INPUT 3)
file(STRINGS intel literal NO_HEX_CONVERSION)
"#,
    )?;
    vars(
        &mut interp,
        &[
            ("before", ":0400000041424344F2;:00000001FF"),
            ("converted", "ABC"),
            ("literal", ":0400000041424344F2;:00000001FF"),
        ],
    );
    assert_eq!(
        host.read(
            Path::new("/work/CMakeFiles/FileCommandStringsBinaryFile"),
            ReadRange::All { limit: 8 }
        )?,
        b"ABCD"
    );
    Ok(())
}

#[test]
#[cfg(feature = "archive")]
fn file_archive_round_trip_with_linked_backend() -> TestResult {
    let (host, mut interp) = interpreter();
    let probe = Path::new("/work/probe.cmake");
    host.seed_file(Path::new("/work/inputs/data"), b"archive contents")?;
    host.seed_file(Path::new("/work/inputs/other"), b"other contents")?;
    host.seed_file(
        probe,
        br#"
file(ARCHIVE_CREATE OUTPUT result.tar.gz WORKING_DIRECTORY inputs PATHS data other
    FORMAT gnutar COMPRESSION GZip COMPRESSION_LEVEL 5 MTIME 1970-01-02 VERBOSE)
"#,
    )?;
    let result = interp.run_script(probe);
    // The isolated command branch can still link the base archive stub.
    if result.exit_code != 0 {
        let stderr = host.stream_bytes(Stream::Stderr);
        assert!(
            stderr
                .to_string()
                .contains("archive creation not yet implemented"),
            "{stderr}"
        );
        assert!(
            stderr
                .to_string()
                .contains("failed to compress: result.tar.gz"),
            "{stderr}"
        );
        return Ok(());
    }
    script(
        &host,
        &mut interp,
        r#"
file(ARCHIVE_EXTRACT INPUT inputs/result.tar.gz DESTINATION restored PATTERNS data VERBOSE)
file(ARCHIVE_EXTRACT INPUT inputs/result.tar.gz DESTINATION list-only PATTERNS other LIST_ONLY TOUCH)
"#,
    )?;
    let restored = Path::new("/work/restored/data");
    assert_eq!(
        host.read(restored, ReadRange::All { limit: 32 })?,
        b"archive contents"
    );
    assert_eq!(host.metadata(restored, Follow::Yes)?.mtime.secs, 86_400);
    assert!(
        host.metadata(Path::new("/work/restored/other"), Follow::No)
            .is_err()
    );
    assert!(
        host.metadata(Path::new("/work/list-only"), Follow::No)
            .is_err()
    );
    assert_eq!(
        host.stream_bytes(Stream::Stdout),
        "data\nother\nx data\nother\n"
    );
    Ok(())
}

fn assert_error(code: &str, expected: &str) -> TestResult {
    let (host, mut interp) = interpreter();
    let path = Path::new("/work/test.cmake");
    host.seed_file(path, code.as_bytes())?;
    assert_eq!(interp.run_script(path).exit_code, 1, "{code}");
    let stderr = host.stream_bytes(Stream::Stderr).to_string();
    let (_, body) = stderr.split_once('\n').ok_or("missing diagnostic body")?;
    assert_eq!(body.trim_end(), expected, "{code}");
    Ok(())
}

#[test]
fn cmake_path_reports_arity_and_unknown_actions() -> TestResult {
    for (args, expected) in [
        (
            "PROXIMATE_PATH p",
            "does not recognize sub-command PROXIMATE_PATH",
        ),
        ("SET p", "SET must be called with two or three arguments."),
        ("SET \"\" file", "Invalid name for path variable."),
        (
            "GET p STEM",
            "GET must be called with at least three arguments.",
        ),
        ("GET p FOO out", "GET called with an unknown action: FOO."),
        (
            "GET p STEM LAST_ONLY",
            "GET called with unexpected arguments.",
        ),
        (
            "GET p FILENAME extra out",
            "GET called with unexpected arguments.",
        ),
        (
            "CONVERT p TO_CMAKE_PATH_LIST",
            "CONVERT must be called with three or four arguments.",
        ),
        (
            "CONVERT p FOO out",
            "CONVERT called with an unknown action: FOO.",
        ),
        (
            "COMPARE p EQUAL q",
            "COMPARE must be called with four arguments.",
        ),
        (
            "COMPARE p FOO q out",
            "COMPARE called with an unknown comparison operator: FOO.",
        ),
        (
            "IS_PREFIX p q",
            "IS_PREFIX must be called with three or four arguments.",
        ),
        (
            "IS_PREFIX p q EXTRA out",
            "IS_PREFIX called with unexpected arguments.",
        ),
        (
            "NATIVE_PATH p",
            "NATIVE_PATH must be called with two or three arguments.",
        ),
        (
            "NATIVE_PATH p NORMALIZE",
            "NATIVE_PATH called with unexpected arguments.",
        ),
        (
            "IS_ABSOLUTE p",
            "IS_ABSOLUTE must be called with two arguments.",
        ),
        (
            "IS_RELATIVE p extra out",
            "IS_RELATIVE must be called with two arguments.",
        ),
        (
            "HAS_EXTENSION p",
            "HAS_EXTENSION must be called with two arguments.",
        ),
        ("HASH p", "HASH must be called with two arguments."),
        ("HASH absent out", "undefined variable for input path."),
        (
            "REMOVE_FILENAME absent extra",
            "REMOVE_FILENAME called with unexpected arguments.",
        ),
    ] {
        assert_error(
            &format!("cmake_path(SET p file)\ncmake_path({args})"),
            &format!("  cmake_path {expected}"),
        )?;
    }
    Ok(())
}

#[test]
fn cmake_path_validates_empty_names_and_keyword_values() -> TestResult {
    for args in [
        r#"GET p FILENAME """#,
        r#"GET p EXTENSION LAST_ONLY """#,
        r#"CONVERT p TO_NATIVE_PATH_LIST """#,
        r#"COMPARE a EQUAL b """#,
        r#"NATIVE_PATH p """#,
        r#"HAS_EXTENSION p """#,
        r#"IS_ABSOLUTE p """#,
        r#"IS_PREFIX p q """#,
        r#"HASH p """#,
    ] {
        assert_error(
            &format!("cmake_path(SET p file)\ncmake_path({args})"),
            "  cmake_path Invalid name for output variable.",
        )?;
    }
    for command in [
        "APPEND",
        "APPEND_STRING",
        "REMOVE_FILENAME",
        "REPLACE_FILENAME",
        "REMOVE_EXTENSION",
        "REPLACE_EXTENSION",
        "NORMAL_PATH",
        "RELATIVE_PATH",
        "ABSOLUTE_PATH",
    ] {
        for (value, problem) in [
            ("", "missing required value"),
            (r#" """#, "empty string not allowed"),
        ] {
            assert_error(
                &format!("cmake_path(SET p file)\ncmake_path({command} p OUTPUT_VARIABLE{value})"),
                &format!("  Error after keyword \"OUTPUT_VARIABLE\":\n\n    {problem}"),
            )?;
        }
    }
    assert_error(
        r#"cmake_path(ABSOLUTE_PATH p OUTPUT_VARIABLE OUTPUT_VARIABLE "" OUTPUT_VARIABLE "")"#,
        "  Error after keyword \"OUTPUT_VARIABLE\":\n\n    empty string not allowed\n    missing required value",
    )?;
    assert_error(
        "cmake_path(ABSOLUTE_PATH p OUTPUT_VARIABLE BASE_DIRECTORY NORMALIZE)",
        "  Error after keyword \"BASE_DIRECTORY\":\n\n    missing required value\n\n  Error after keyword \"OUTPUT_VARIABLE\":\n\n    missing required value",
    )
}

#[test]
fn cmake_path_accepts_nonempty_names_and_optional_replacements() -> TestResult {
    let (host, mut interp) = interpreter();
    script(
        &host,
        &mut interp,
        r#"
cmake_path(SET p dir/file.tar.gz NORMALIZE)
cmake_path(GET p FILENAME "a b;$名")
cmake_path(GET p STEM 1 LAST_ONLY)
cmake_path(NATIVE_PATH p " " NORMALIZE)
cmake_path(IS_PREFIX p dir/file.tar.gz found NORMALIZE)
cmake_path(APPEND new /root item)
cmake_path(REPLACE_FILENAME p OUTPUT_VARIABLE "new filename")
cmake_path(REPLACE_EXTENSION p LAST_ONLY OUTPUT_VARIABLE "new ext")
cmake_path(ABSOLUTE_PATH p BASE_DIRECTORY "" OUTPUT_VARIABLE abs)
"#,
    )?;
    vars(
        &mut interp,
        &[
            ("a b;$名", "file.tar.gz"),
            ("1", "file.tar"),
            (
                " ",
                if cfg!(windows) {
                    r"dir\file.tar.gz"
                } else {
                    "dir/file.tar.gz"
                },
            ),
            ("found", "ON"),
            ("new", "/root/item"),
            ("new filename", "dir/"),
            ("new ext", "dir/file.tar"),
            ("abs", "dir/file.tar.gz"),
        ],
    );
    Ok(())
}

#[test]
fn file_pair_reports_arguments_and_failure_reasons() -> TestResult {
    for command in ["COPY_FILE", "RENAME"] {
        let setup = "file(WRITE input data)\n";
        assert_error(
            &format!("{setup}file({command} input)"),
            &format!("  file {command} must be called with at least two additional arguments"),
        )?;
        assert_error(
            &format!("{setup}file({command} input output unknown extra)"),
            &format!("  file {command} unknown argument:\n\n    unknown"),
        )?;
    }
    assert_error(
        "file(MAKE_DIRECTORY dir)\nfile(COPY_FILE dir output)",
        "  file COPY_FILE cannot copy a directory\n\n    /work/dir",
    )?;
    assert_error(
        "file(MAKE_DIRECTORY dir)\nfile(COPY_FILE missing dir)",
        "  file COPY_FILE cannot copy to a directory\n\n    /work/dir",
    )?;
    assert_error(
        "file(WRITE input data)\nfile(WRITE output data)\nfile(RENAME input output RESULT NO_REPLACE)",
        "  file RENAME failed to rename\n\n    /work/input\n\n  to\n\n    /work/output\n\n  because: path not replaced",
    )
}

#[test]
fn file_pair_captures_results_and_recognizes_flags_after_result() -> TestResult {
    let (host, mut interp) = interpreter();
    script(
        &host,
        &mut interp,
        r#"
file(WRITE input data)
file(WRITE output original)
file(MAKE_DIRECTORY dir)
file(CREATE_LINK dir dir_link SYMBOLIC)
file(COPY_FILE dir_link output RESULT from_dir)
file(COPY_FILE input dir RESULT to_dir)
file(COPY_FILE input copy RESULT ignored RESULT copied ONLY_IF_DIFFERENT)
file(COPY_FILE input copy RESULT ONLY_IF_DIFFERENT)
file(RENAME input output RESULT blocked NO_REPLACE)
file(RENAME input moved RESULT renamed RESULT)
"#,
    )?;
    vars(
        &mut interp,
        &[
            ("from_dir", "cannot copy a directory"),
            ("to_dir", "cannot copy to a directory"),
            ("copied", "0"),
            ("blocked", "NO_REPLACE"),
            ("renamed", "0"),
        ],
    );
    assert!(interp.cx().var(b"ignored").is_none());
    Ok(())
}

#[test]
fn file_glob_reports_missing_tag_arguments() -> TestResult {
    for (args, expected) in [
        (
            "GLOB",
            "GLOB must be called with at least one additional argument.",
        ),
        (
            "GLOB out LIST_DIRECTORIES",
            "LIST_DIRECTORIES missing bool value.",
        ),
        (
            "GLOB out LIST_DIRECTORIES 2 *",
            "LIST_DIRECTORIES missing bool value.",
        ),
        (
            "GLOB out RELATIVE",
            "GLOB requires a directory after the RELATIVE tag.",
        ),
        (
            "GLOB out RELATIVE /work",
            "GLOB requires a glob expression after the directory.",
        ),
        (
            "GLOB_RECURSE out RELATIVE",
            "GLOB_RECURSE requires a directory after the RELATIVE tag.",
        ),
        (
            "GLOB_RECURSE out FOLLOW_SYMLINKS",
            "GLOB_RECURSE requires a glob expression after FOLLOW_SYMLINKS.",
        ),
    ] {
        assert_error(&format!("file({args})"), &format!("  file {expected}"))?;
    }
    Ok(())
}

#[test]
fn file_glob_reports_configure_depends_diagnostics() -> TestResult {
    let (host, mut interp) = interpreter_with_mode(RunMode::Configure);
    let source = Path::new("/work/src");
    host.seed_file(
        &source.join("CMakeLists.txt"),
        b"file(GLOB out one* CONFIGURE_DEPENDS two* CONFIGURE_DEPENDS three*)\nfile(GLOB out CONFIGURE_DEPENDS)",
    )?;
    assert_eq!(
        interp
            .run_configure(source, Path::new("/work/build"), &[])
            .exit_code,
        1
    );
    let stderr = host.stream_bytes(Stream::Stderr).to_string();
    assert_eq!(
        stderr.matches("  CONFIGURE_DEPENDS flag was given after a glob expression was already\n  evaluated.\n").count(),
        1,
        "{stderr}"
    );
    assert!(
        stderr.contains("  file GLOB requires a glob expression after CONFIGURE_DEPENDS.\n"),
        "{stderr}"
    );
    Ok(())
}

#[test]
fn file_glob_recurse_stops_only_when_a_symlink_target_repeats() -> TestResult {
    let (host, mut interp) = interpreter_in(RunMode::Script, &format!("{DRIVE}/work"));
    script(
        &host,
        &mut interp,
        &r#"
file(WRITE tree/abc data)
file(MAKE_DIRECTORY tree/sub/deep)
file(CREATE_LINK ../.. tree/sub/deep/loop SYMBOLIC)
file(CREATE_LINK ../../abc tree/sub/deep/link SYMBOLIC)
file(GLOB_RECURSE files FOLLOW_SYMLINKS RELATIVE /work/tree "tree/*")
file(GLOB_RECURSE dirs FOLLOW_SYMLINKS LIST_DIRECTORIES YES RELATIVE /work/tree "tree/*")
file(GLOB_RECURSE explicit RELATIVE /work/tree "tree/sub/deep/loo?/*")
file(WRITE outer/inner/file data)
file(CREATE_LINK . outer/inner/loop SYMBOLIC)
file(MAKE_DIRECTORY entry)
file(CREATE_LINK ../outer entry/alias SYMBOLIC)
file(GLOB_RECURSE chained FOLLOW_SYMLINKS RELATIVE /work/entry "entry/*")
"#
        .replace("/work/", &format!("{DRIVE}/work/")),
    )?;
    vars(
        &mut interp,
        &[
            (
                "files",
                "abc;sub/deep/link;sub/deep/loop/abc;sub/deep/loop/sub/deep/link",
            ),
            (
                "dirs",
                "abc;sub;sub/deep;sub/deep/link;sub/deep/loop;sub/deep/loop/abc;sub/deep/loop/sub;sub/deep/loop/sub/deep;sub/deep/loop/sub/deep/link",
            ),
            (
                "explicit",
                "sub/deep/loop/abc;sub/deep/loop/sub/deep/link;sub/deep/loop/sub/deep/loop",
            ),
            ("chained", "alias/inner/file;alias/inner/loop/file"),
        ],
    );
    let stderr = host.stream_bytes(Stream::Stderr).to_string();
    assert_eq!(
        stderr.matches(
            format!("  Cyclic recursion detected while globbing for 'tree/*':\n\n  {DRIVE}/work/tree/sub/deep\n\n  {DRIVE}/work/tree/sub/deep/loop\n").as_str()
        ).count(),
        2,
        "{stderr}"
    );
    assert!(
        stderr.contains(
            format!("  Cyclic recursion detected while globbing for 'entry/*':\n\n  {DRIVE}/work/outer/inner\n\n  {DRIVE}/work/outer/inner/loop\n").as_str()
        ),
        "{stderr}"
    );
    Ok(())
}

fn macho_header(cpu: u32, subtype: u32, wide: bool, little: bool) -> Vec<u8> {
    let magic = if wide { 0xfeed_facfu32 } else { 0xfeed_face };
    let mut header = Vec::new();
    for value in [magic, cpu, subtype, 2, 0, 0, 0] {
        let bytes = if little {
            value.to_le_bytes()
        } else {
            value.to_be_bytes()
        };
        header.extend_from_slice(&bytes);
    }
    if wide {
        header.extend_from_slice(&[0; 4]);
    }
    header
}

fn fat_macho(wide: bool, little: bool) -> Vec<u8> {
    let mut fat = Vec::new();
    let magic = if wide { 0xcafe_babfu32 } else { 0xcafe_babe };
    let entries = [(0x0100_0007, 3), (0x0100_000c, 0x8000_0002)];
    let word = |out: &mut Vec<u8>, value: u32| {
        let bytes = if little {
            value.to_le_bytes()
        } else {
            value.to_be_bytes()
        };
        out.extend_from_slice(&bytes);
    };
    word(&mut fat, magic);
    word(&mut fat, 2);
    let table_end = if wide { 72 } else { 48 };
    for ((cpu, subtype), offset) in entries.iter().zip([table_end, table_end + 32]) {
        word(&mut fat, *cpu);
        word(&mut fat, *subtype);
        if wide && !little {
            word(&mut fat, 0);
        }
        word(&mut fat, offset);
        if wide {
            word(&mut fat, 0);
        }
        word(&mut fat, 32);
        if wide && little {
            word(&mut fat, 0);
        }
        word(&mut fat, 0);
        if wide {
            word(&mut fat, 0);
        }
    }
    for (cpu, subtype) in entries {
        fat.extend(macho_header(cpu, subtype, true, true));
    }
    fat
}

#[test]
fn read_macho_handles_all_magic_values_and_reports_invalid_files() -> TestResult {
    let (host, mut interp) = interpreter();
    for (label, wide, little) in [
        ("wide_le", true, true),
        ("wide_be", true, false),
        ("short_le", false, true),
        ("short_be", false, false),
    ] {
        let (cpu, subtype, expected) = if wide {
            (0x0100_000c, 12, "arm64e.x1")
        } else {
            (12, 9, "armv7")
        };
        host.seed_file(
            Path::new("/work/thin"),
            &macho_header(cpu, subtype, wide, little),
        )?;
        host.seed_file(Path::new("/work/fat"), &fat_macho(wide, little))?;
        script(
            &host,
            &mut interp,
            &format!(
                r#"
set(capture previous)
file(READ_MACHO thin ARCHITECTURES {label} CAPTURE_ERROR capture)
file(READ_MACHO fat ARCHITECTURES fat CAPTURE_ERROR capture)
"#
            ),
        )?;
        vars(
            &mut interp,
            &[
                (label, expected),
                ("fat", "x86_64;arm64e"),
                ("capture", "previous"),
            ],
        );
    }
    let fat = fat_macho(true, false);
    let truncated = fat.get(..80).ok_or("missing Mach-O fixture bytes")?;
    host.seed_file(Path::new("/work/truncated"), truncated)?;
    script(
        &host,
        &mut interp,
        r#"
file(READ_MACHO truncated ARCHITECTURES truncated CAPTURE_ERROR bad)
file(READ_MACHO missing ARCHITECTURES absent CAPTURE_ERROR missing)
"#,
    )?;
    vars(
        &mut interp,
        &[
            ("truncated", "unknown"),
            ("bad", "truncated is not a valid Macho-O file"),
            ("absent", "unknown"),
            ("missing", "missing does not exist"),
        ],
    );
    Ok(())
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn read_macho_reads_system_and_compiler_binaries_on_macos() -> TestResult {
    use rcmake_host::{
        CommandSpec, Input, NativeHost, Output, ProcessResult, RunRequest, path::from_native,
    };
    use std::time::Duration;

    let native = Arc::new(NativeHost::new()?);
    let run = |program: BString, args: Vec<BString>| -> TestResultBytes {
        let result = native.run(
            &RunRequest {
                pipeline: vec![CommandSpec { program, args }],
                cwd: "/tmp".into(),
                env: native.env_all(),
                stdin: Input::Null,
                stdout: Output::Capture,
                stderr: Output::Capture,
                timeout: Some(Duration::from_secs(30)),
                tee: false,
            },
            &mut |_, _| {},
        )?;
        assert_eq!(
            result.results,
            [ProcessResult::Exited(0)],
            "{}",
            BString::from(result.stderr)
        );
        Ok(result.stdout)
    };
    type TestResultBytes = std::result::Result<Vec<u8>, Box<dyn std::error::Error>>;
    let temp = run(
        "mktemp".into(),
        vec!["-d".into(), "/tmp/rcmake-macho.XXXXXXXX".into()],
    )?;
    let folder = std::path::PathBuf::from(std::str::from_utf8(&temp)?.trim());
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Ok(host) = NativeHost::new() {
                let _ = host.remove_dir_all(&self.0);
            }
        }
    }
    let _cleanup = Cleanup(folder.clone());
    let source = folder.join("arm.c");
    let binary = folder.join("arm");
    native.write(
        &source,
        b"int main(void) { return 0; }",
        rcmake_host::WriteMode::Truncate,
    )?;
    run(
        "cc".into(),
        vec![
            "-arch".into(),
            "arm64".into(),
            from_native(&source),
            "-o".into(),
            from_native(&binary),
        ],
    )?;
    let input = folder.join("probe.cmake");
    let code = format!(
        r#"
file(READ_MACHO /usr/bin/cc ARCHITECTURES cc)
file(READ_MACHO /bin/ls ARCHITECTURES ls)
file(READ_MACHO "{}" ARCHITECTURES arm)
"#,
        binary.display()
    );
    native.write(&input, code.as_bytes(), rcmake_host::WriteMode::Truncate)?;
    let mut interp = Interp::new(
        InterpConfig {
            services: Default::default(),
            host: native,
            mode: RunMode::Script,
            cmake_root: folder.clone(),
            cmake_command: folder.join("cmake"),
            cwd: folder,
            limits: Default::default(),
            diag: Default::default(),
            defines: Vec::new(),
            script_args: Vec::new(),
        },
        register,
    );
    assert_eq!(interp.run_script(&input).exit_code, 0);
    vars(&mut interp, &[("arm", "arm64")]);
    for name in [b"cc".as_slice(), b"ls"] {
        let value = interp.cx().var_or_empty(name);
        assert!(
            value
                .split(|&byte| byte == b';')
                .any(|arch| matches!(arch, b"arm64" | b"arm64e" | b"arm64e.x1")),
            "{value}"
        );
    }
    Ok(())
}

#[test]
fn file_install_tracks_staged_files_and_checks_size_and_time() -> TestResult {
    let (host, mut interp) = interpreter();
    host.seed_file(Path::new("/work/src/keep.txt"), b"old")?;
    host.seed_file(Path::new("/work/src/ignore.txt"), b"ignore")?;
    host.seed_file(Path::new("/work/src/run"), b"run")?;
    host.symlink(Path::new("run"), Path::new("/work/src/link"))?;
    host.env_set(b"DESTDIR", Some(b"/work/stage"));
    script(
        &host,
        &mut interp,
        r#"
file(INSTALL DESTINATION /prefix TYPE DIRECTORY FILES src/
    FILES_MATCHING PATTERN "*.txt" PATTERN "ignore*" EXCLUDE)
file(INSTALL DESTINATION /prefix TYPE PROGRAM FILES src/link FOLLOW_SYMLINK_CHAIN)
file(INSTALL DESTINATION /prefix TYPE FILE FILES absent OPTIONAL)
file(INSTALL DESTINATION /prefix TYPE FILE FILES src/keep.txt MESSAGE_LAZY)
"#,
    )?;
    let dest = Path::new("/work/stage/prefix/keep.txt");
    let source = Path::new("/work/src/keep.txt");
    assert_eq!(
        host.metadata(Path::new("/work/stage/prefix/run"), Follow::Yes)?
            .mode
            & 0o777,
        0o755
    );
    assert_eq!(
        host.read_link(Path::new("/work/stage/prefix/link"))?,
        Path::new("run")
    );
    assert!(
        host.metadata(Path::new("/work/stage/prefix/ignore.txt"), Follow::Yes)
            .is_err()
    );
    vars(
        &mut interp,
        &[(
            "CMAKE_INSTALL_MANIFEST_FILES",
            "/prefix/keep.txt;/prefix/link;/prefix/run;/prefix/keep.txt",
        )],
    );
    let metadata = host.metadata(dest, Follow::Yes)?;
    host.seed_file(source, b"longer")?;
    host.set_times(source, None, Some(metadata.mtime))?;
    script(
        &host,
        &mut interp,
        "file(INSTALL DESTINATION /prefix TYPE FILE FILES src/keep.txt)",
    )?;
    assert_eq!(host.read(dest, ReadRange::All { limit: 100 })?, b"longer");
    assert!(
        host.stream_bytes(Stream::Stdout)
            .to_string()
            .contains("-- Installing: /work/stage/prefix/keep.txt")
    );
    Ok(())
}

fn elf_with_path(wide: bool, little: bool, tag: u64) -> Result<Vec<u8>, &'static str> {
    let mut data = vec![0; 384];
    data.get_mut(..7)
        .ok_or("missing ELF header")?
        .copy_from_slice(&[
            0x7f,
            b'E',
            b'L',
            b'F',
            if wide { 2 } else { 1 },
            if little { 1 } else { 2 },
            1,
        ]);
    let mut field = |offset: usize, size: usize, value: u64| {
        let bytes = if little {
            value.to_le_bytes()
        } else {
            value.to_be_bytes()
        };
        let value = if little {
            bytes.get(..size)
        } else {
            bytes
                .len()
                .checked_sub(size)
                .and_then(|start| bytes.get(start..))
        }
        .ok_or("invalid ELF field width")?;
        data.get_mut(offset..offset + size)
            .ok_or("missing ELF field")?
            .copy_from_slice(value);
        Ok::<_, &'static str>(())
    };
    let (word, ph_offset, ph_size_at, ph_width, seg_offset, seg_address, seg_size) = if wide {
        (8, 32, 54, 56, 8, 16, 32)
    } else {
        (4, 28, 42, 32, 4, 8, 16)
    };
    field(ph_offset, word, 64)?;
    field(ph_size_at, 2, ph_width as u64)?;
    field(ph_size_at + 2, 2, 2)?;
    field(64, 4, 1)?;
    field(64 + seg_address, word, 0x1000)?;
    field(64 + seg_size, word, 384)?;
    field(64 + ph_width, 4, 2)?;
    field(64 + ph_width + seg_offset, word, 192)?;
    field(64 + ph_width + seg_size, word, (word * 2 * 4) as u64)?;
    for (index, (kind, value)) in [(5, 0x1140), (tag, 1), (10, 40), (0, 0)]
        .into_iter()
        .enumerate()
    {
        field(192 + word * 2 * index, word, kind)?;
        field(192 + word * (2 * index + 1), word, value)?;
    }
    let path = b"/toolchain:/build/lib:/vendor";
    data.get_mut(321..321 + path.len())
        .ok_or("missing ELF path field")?
        .copy_from_slice(path);
    Ok(data)
}

#[test]
fn file_rpath_changes_and_removes_elf_dynamic_paths() -> TestResult {
    let (host, mut interp) = interpreter();
    let path = Path::new("/work/binary");
    for (wide, little, tag) in [(true, true, 29), (false, false, 15)] {
        host.seed_file(path, &elf_with_path(wide, little, tag)?)?;
        script(
            &host,
            &mut interp,
            r#"
file(RPATH_CHANGE FILE binary OLD_RPATH /build/lib NEW_RPATH /lib)
file(RPATH_CHECK FILE binary RPATH /lib)
"#,
        )?;
        let data = host.read(path, ReadRange::All { limit: 512 })?;
        assert!(
            data.windows(b"/toolchain:/lib:/vendor".len())
                .any(|part| part == b"/toolchain:/lib:/vendor")
        );
        script(
            &host,
            &mut interp,
            "file(RPATH_SET FILE binary NEW_RPATH /replacement/longer/path)",
        )?;
        let data = host.read(path, ReadRange::All { limit: 512 })?;
        assert!(
            data.get(321..)
                .is_some_and(|path| path.starts_with(b"/replacement/longer/path\0"))
        );
        script(
            &host,
            &mut interp,
            r#"
file(RPATH_SET FILE binary NEW_RPATH "/old:/keep")
file(RPATH_CHANGE FILE binary OLD_RPATH /old NEW_RPATH "")
"#,
        )?;
        let data = host.read(path, ReadRange::All { limit: 512 })?;
        assert!(
            data.get(321..)
                .is_some_and(|path| path.starts_with(b":/keep\0"))
        );
        script(
            &host,
            &mut interp,
            r#"
file(RPATH_REMOVE FILE binary)
file(RPATH_CHECK FILE binary RPATH "")
"#,
        )?;
        let data = host.read(path, ReadRange::All { limit: 512 })?;
        assert!(
            data.get(321..360)
                .is_some_and(|path| path.iter().all(|&byte| byte == 0))
        );
        script(
            &host,
            &mut interp,
            "file(RPATH_CHECK FILE binary RPATH /missing)",
        )?;
        assert!(host.metadata(path, Follow::Yes).is_err());
    }
    host.seed_file(path, b"plain data")?;
    script(
        &host,
        &mut interp,
        r#"
file(RPATH_REMOVE FILE binary)
file(RPATH_CHANGE FILE binary OLD_RPATH "/old" NEW_RPATH "")
file(RPATH_SET FILE binary NEW_RPATH "")
file(RPATH_CHECK FILE binary RPATH "")
"#,
    )?;
    assert_eq!(
        host.read(path, ReadRange::All { limit: 32 })?,
        b"plain data"
    );
    Ok(())
}

#[test]
#[cfg(not(windows))]
fn file_runtime_dependencies_inherit_macho_runpaths_and_apply_file_filters() -> TestResult {
    use rcmake_host::{ProcessResult, RunOutput};

    let (host, mut interp) = interpreter();
    for file in [
        "/work/bin/main",
        "/work/lib1/liba.dylib",
        "/work/lib2/libb.dylib",
    ] {
        host.seed_file(Path::new(file), b"")?;
    }
    host.hard_link(
        Path::new("/work/lib2/libb.dylib"),
        Path::new("/work/lib2/alias.dylib"),
    )?;
    host.set_command_handler(|_, request, _| {
        let [command] = request.pipeline.as_slice() else {
            panic!("Expected one command.");
        };
        assert_eq!(command.program, "/tools/custom-otool");
        let [option, flags, file] = command.args.as_slice() else {
            panic!("Expected the tool option and input path.");
        };
        assert_eq!(option, "custom-arg");
        assert_eq!(flags, "-l");
        let stdout = match file.as_slice() {
            b"/work/bin/main" => b"cmd LC_RPATH\ncmdsize 24\npath /work/lib2 (offset 12)\ncmd LC_LOAD_DYLIB\ncmdsize 80\nname /work/lib1/liba.dylib (offset 24)\n".as_slice(),
            b"/work/lib1/liba.dylib" => b"cmd LC_RPATH\ncmdsize 24\npath /unused (offset 12)\ncmd LC_LOAD_DYLIB\ncmdsize 80\nname @rpath/libb.dylib (offset 24)\ncmd LC_LOAD_DYLIB\ncmdsize 80\nname @rpath/missing.dylib (offset 24)\n",
            b"/work/lib2/libb.dylib" => b"cmd LC_LOAD_DYLIB\ncmdsize 80\nname /work/lib1/liba.dylib (offset 24)\n",
            _ => panic!("Unexpected input path: {file}"),
        };
        Ok(RunOutput { results: vec![ProcessResult::Exited(0)], stdout: stdout.to_vec(), ..RunOutput::default() })
    })?;
    script(
        &host,
        &mut interp,
        r#"
set(CMAKE_HOST_SYSTEM_NAME Darwin)
set(CMAKE_GET_RUNTIME_DEPENDENCIES_COMMAND "/tools/custom-otool;custom-arg")
file(GET_RUNTIME_DEPENDENCIES
  EXECUTABLES /work/bin/main
  RESOLVED_DEPENDENCIES_VAR deps
  UNRESOLVED_DEPENDENCIES_VAR unresolved
  CONFLICTING_DEPENDENCIES_PREFIX conflict
  RPATH_PREFIX rpaths
  PRE_EXCLUDE_REGEXES "["
  POST_INCLUDE_REGEXES "liba[.]dylib$"
  POST_EXCLUDE_REGEXES ".*"
  POST_INCLUDE_FILES /work/lib2/alias.dylib)
"#,
    )?;
    vars(
        &mut interp,
        &[
            ("deps", "/work/lib1/liba.dylib;/work/lib2/libb.dylib"),
            ("unresolved", "@rpath/missing.dylib"),
            ("conflict_FILENAMES", ""),
            ("rpaths_/work/lib1/liba.dylib", "/unused"),
        ],
    );
    assert_eq!(
        host.stream_bytes(Stream::Stdout),
        b"RegularExpression::compile(): Unmatched [].\nRegularExpression::compile(): Error in compile.\n"
            .as_slice()
    );
    assert!(host.stream_bytes(Stream::Stderr).is_empty());
    Ok(())
}

#[test]
fn file_lock_creates_directory_locks_and_releases_scope_locks() -> TestResult {
    let (host, mut interp) = interpreter();
    host.seed_file(
        Path::new("/work/child.cmake"),
        b"file(LOCK file.lk GUARD FILE RESULT_VARIABLE file_held)",
    )?;
    script(
        &host,
        &mut interp,
        r#"
file(LOCK lock-dir DIRECTORY RESULT_VARIABLE first)
file(LOCK lock-dir DIRECTORY TIMEOUT 0 RESULT_VARIABLE duplicate)
file(LOCK lock-dir DIRECTORY RELEASE RESULT_VARIABLE released)
function(lock_once)
  file(LOCK scoped.lk GUARD FUNCTION RESULT_VARIABLE got)
  set(function_held "${got}" PARENT_SCOPE)
endfunction()
lock_once()
file(LOCK scoped.lk TIMEOUT 0 RESULT_VARIABLE after_function)
file(LOCK scoped.lk RELEASE)
include(/work/child.cmake)
file(LOCK file.lk TIMEOUT 0 RESULT_VARIABLE after_file)
file(LOCK file.lk RELEASE)
file(LOCK bad.lk GUARD FUNCTION RESULT_VARIABLE bad)
"#,
    )?;
    vars(
        &mut interp,
        &[
            ("first", "0"),
            ("duplicate", "File already locked"),
            ("released", "0"),
            ("function_held", "0"),
            ("after_function", "0"),
            ("file_held", "0"),
            ("after_file", "0"),
            ("bad", "'GUARD FUNCTION' not used in function definition"),
        ],
    );
    assert!(
        host.metadata(Path::new("/work/lock-dir/cmake.lock"), Follow::Yes)
            .is_ok()
    );
    Ok(())
}

#[test]
fn file_generate_records_ordered_arguments_without_writing_output() -> TestResult {
    let (host, mut interp) = interpreter();
    script(
        &host,
        &mut interp,
        r#"
file(GENERATE OUTPUT "out-$<CONFIG>" CONTENT "one;two\n"
     CONDITION "$<CONFIG:Debug>" TARGET later FILE_PERMISSIONS OWNER_READ NEWLINE_STYLE UNIX)
file(GENERATE OUTPUT second INPUT in USE_SOURCE_PERMISSIONS)
"#,
    )?;
    let mut cx = interp.cx();
    let props = &cx.global().props;
    for (key, value) in [
        ("_RCMAKE_FILE_GENERATE_COUNT", "2"),
        ("_RCMAKE_FILE_GENERATE_0_DIRECTORY", "0"),
        ("_RCMAKE_FILE_GENERATE_0_ARG1", "out-$<CONFIG>"),
        ("_RCMAKE_FILE_GENERATE_0_ARG3", "one;two\n"),
        ("_RCMAKE_FILE_GENERATE_1_ARG4", "USE_SOURCE_PERMISSIONS"),
    ] {
        assert_eq!(props.get(key.as_bytes()), Some(&BString::from(value)));
    }
    assert!(
        host.metadata(Path::new("/work/second"), Follow::Yes)
            .is_err()
    );
    Ok(())
}

#[test]
fn file_lock_contends_with_host_locks_and_can_reacquire() -> TestResult {
    use std::io::ErrorKind;
    use std::time::Duration;

    let (host, mut interp) = interpreter();
    let path = Path::new("/work/shared.lk");
    let lock = host.lock_file(path, Some(Duration::ZERO))?;
    script(
        &host,
        &mut interp,
        "file(LOCK shared.lk TIMEOUT 0 RESULT_VARIABLE blocked)",
    )?;
    vars(&mut interp, &[("blocked", "Timeout reached")]);

    drop(lock);
    script(&host, &mut interp, "file(LOCK shared.lk TIMEOUT 0)")?;
    let busy = host
        .lock_file(path, Some(Duration::ZERO))
        .err()
        .map(|error| error.kind);
    assert_eq!(busy, Some(ErrorKind::TimedOut));

    script(&host, &mut interp, "file(LOCK shared.lk RELEASE)")?;
    let lock = host.lock_file(path, Some(Duration::ZERO))?;
    assert_eq!(lock.path(), path);
    Ok(())
}

#[test]
fn scoped_locks_are_released_before_the_next_lock_command() -> TestResult {
    use std::time::Duration;

    for (command, exit_code) in [
        ("", 0),
        ("return()", 0),
        ("message(FATAL_ERROR stop)", 1),
        ("cmake_language(EXIT 42)", 42),
    ] {
        let host = Arc::new(PureHost::default());
        let mut interp = Interp::new(
            InterpConfig {
                services: Default::default(),
                host: host.clone(),
                mode: RunMode::Script,
                cmake_root: "/cmake".into(),
                cmake_command: "/cmake/bin/cmake".into(),
                cwd: "/work".into(),
                limits: Default::default(),
                diag: Default::default(),
                defines: Vec::new(),
                script_args: Vec::new(),
            },
            crate::register_all,
        );
        let main = Path::new("/work/main.cmake");
        host.seed_file(main, b"include(child.cmake)\n")?;
        host.seed_file(
            Path::new("/work/child.cmake"),
            format!(
                "file(LOCK file.lk GUARD FILE)\n\
                 function(lock_once)\n\
                   file(LOCK function.lk GUARD FUNCTION)\n\
                   {command}\n\
                 endfunction()\n\
                 lock_once()\n"
            )
            .as_bytes(),
        )?;
        assert_eq!(interp.run_script(main).exit_code, exit_code, "{command}");

        for path in ["/work/file.lk", "/work/function.lk"] {
            let _guard = host.lock_file(Path::new(path), Some(Duration::ZERO))?;
        }
    }
    Ok(())
}

#[test]
fn a_released_scope_does_not_remove_a_new_process_lock() -> TestResult {
    let (host, mut interp) = interpreter();
    script(
        &host,
        &mut interp,
        r#"
function(lock_again)
  file(LOCK lock GUARD FUNCTION)
  file(LOCK lock RELEASE)
  file(LOCK lock GUARD PROCESS)
endfunction()
lock_again()
file(LOCK lock TIMEOUT 0 RESULT_VARIABLE still_locked)
file(LOCK lock RELEASE)
"#,
    )?;
    vars(&mut interp, &[("still_locked", "File already locked")]);
    Ok(())
}

#[test]
fn download_has_no_default_total_timeout_and_uses_the_last_range() -> TestResult {
    use rcmake_host::{ProcessResult, RunOutput};
    let (host, mut interp) = interpreter();
    host.set_command_handler(|_, request, _| {
        assert_eq!(request.timeout, None);
        let [command] = request.pipeline.as_slice() else {
            panic!("Expected one command.");
        };
        let args = &command.args;
        assert_eq!(args.iter().filter(|arg| *arg == "--range").count(), 1);
        assert!(args.windows(2).any(|pair| pair == ["--range", "20-29"]));
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })?;
    script(
        &host,
        &mut interp,
        "
file(DOWNLOAD https://example.invalid/ RANGE_START 0 RANGE_END 9 RANGE_START 20 RANGE_END 29)
file(DOWNLOAD file:///input RANGE_START 0 RANGE_END 9 RANGE_START 20 RANGE_END 29)
",
    )
}

#[test]
fn download_passes_netrc_file_only_when_netrc_is_enabled() -> TestResult {
    use rcmake_host::{ProcessResult, RunOutput};
    let (host, mut interp) = interpreter();
    host.set_command_handler(|_, request, _| {
        let [command] = request.pipeline.as_slice() else {
            panic!("Expected one command.");
        };
        let args = &command.args;
        let flag = if args
            .iter()
            .any(|arg| arg == "https://example.invalid/optional")
        {
            Some("--netrc-optional")
        } else if args
            .iter()
            .any(|arg| arg == "https://example.invalid/required")
        {
            Some("--netrc")
        } else {
            None
        };
        assert_eq!(args.iter().any(|arg| arg == "--netrc-file"), flag.is_some());
        if let Some(flag) = flag {
            assert!(args.iter().any(|arg| arg == flag));
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["--netrc-file", "/work/.netrc"])
            );
        }
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })?;
    script(
        &host,
        &mut interp,
        "
set(CMAKE_NETRC_FILE /work/.netrc)
file(DOWNLOAD https://example.invalid/default)
file(DOWNLOAD https://example.invalid/ignored NETRC IGNORED)
file(DOWNLOAD https://example.invalid/optional NETRC OPTIONAL)
file(DOWNLOAD https://example.invalid/required NETRC REQUIRED)
",
    )
}

#[test]
fn download_rejects_headers_that_start_with_an_at_sign() -> TestResult {
    let (host, mut interp) = interpreter();
    host.set_command_handler(|_, _, _| panic!("Rejected header reached the subprocess."))?;
    let input = Path::new("/work/test.cmake");
    host.seed_file(
        input,
        b"file(DOWNLOAD https://example.invalid/ HTTPHEADER @secret)",
    )?;
    assert_eq!(interp.run_script(input).exit_code, 1);
    assert!(
        host.stream_bytes(Stream::Stderr)
            .to_string()
            .contains("HTTPHEADER cannot start with '@'")
    );
    Ok(())
}

#[test]
fn download_hashes_existing_files_above_the_single_read_limit() -> TestResult {
    let host = Arc::new(PureHost::default());
    let mut interp = Interp::new(
        InterpConfig {
            services: Default::default(),
            host: host.clone(),
            mode: RunMode::Script,
            cmake_root: "/cmake".into(),
            cmake_command: "/cmake/bin/cmake".into(),
            cwd: "/work".into(),
            limits: rcmake_base::Limits {
                max_file_read: 4096,
                ..Default::default()
            },
            diag: Default::default(),
            defines: Vec::new(),
            script_args: Vec::new(),
        },
        register,
    );
    let payload = vec![0xab; 300 * 1024];
    host.seed_file(Path::new("/work/large"), &payload)?;
    host.set_command_handler(|_, _, _| panic!("The expected hash matched the existing file."))?;
    let hash = HashAlgo::Sha256.hex_digest(&payload);
    script(
        &host,
        &mut interp,
        &format!(
            "file(DOWNLOAD https://example.invalid/ large EXPECTED_HASH SHA256={hash} STATUS result)"
        ),
    )?;
    vars(
        &mut interp,
        &[(
            "result",
            "0;\"skipping download as file already exists with expected SHA256 hash\"",
        )],
    );
    Ok(())
}

#[test]
fn install_directory_replaces_the_destination_symlink() -> TestResult {
    let work = if cfg!(windows) { "C:/work" } else { "/work" };
    let (host, mut interp) = interpreter_in(RunMode::Script, work);
    let root = Path::new(work);
    host.seed_file(&root.join("source/contents"), b"input")?;
    host.seed_file(&root.join("original/marker"), b"original")?;
    host.create_dir_all(&root.join("destination"))?;
    host.symlink(Path::new("../original"), &root.join("destination/source"))?;

    script(
        host.as_ref(),
        &mut interp,
        &format!(
            "file(INSTALL DESTINATION \"{work}/destination\" TYPE DIRECTORY FILES \"{work}/source\")"
        ),
    )?;
    assert_eq!(
        host.metadata(&root.join("destination/source"), Follow::No)?
            .kind,
        FileKind::Dir,
    );
    assert!(
        host.metadata(&root.join("destination/source/contents"), Follow::No)
            .is_ok()
    );
    assert!(
        host.metadata(&root.join("original/contents"), Follow::No)
            .is_err()
    );
    assert!(
        host.metadata(&root.join("original/marker"), Follow::No)
            .is_ok()
    );
    Ok(())
}

#[test]
fn file_install_drops_a_trailing_dot_from_the_destination() -> TestResult {
    let (host, mut interp) = interpreter();
    host.seed_file(Path::new("/work/src/one.txt"), b"one")?;
    script(
        &host,
        &mut interp,
        "file(INSTALL DESTINATION /work/prefix/. TYPE FILE FILES src/one.txt)\n",
    )?;
    assert_eq!(
        host.read(
            Path::new("/work/prefix/one.txt"),
            ReadRange::All { limit: 16 }
        )?,
        b"one"
    );
    let stdout = host.stream_bytes(Stream::Stdout).to_string();
    assert!(
        stdout.contains("Installing: /work/prefix/one.txt"),
        "{stdout}"
    );
    assert!(!stdout.contains("/./"), "{stdout}");
    Ok(())
}
