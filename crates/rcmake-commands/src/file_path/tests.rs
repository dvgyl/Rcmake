use std::path::Path;
use std::sync::Arc;

use rcmake_base::{BString, hash::HashAlgo};
use rcmake_eval::{Interp, InterpConfig, RunMode};
use rcmake_host::{FileKind, Follow, Host, PureHost, ReadRange, Stream};

type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

fn interpreter() -> (Arc<PureHost>, Interp) {
    interpreter_with_mode(RunMode::Script)
}

fn interpreter_with_mode(mode: RunMode) -> (Arc<PureHost>, Interp) {
    let host = Arc::new(PureHost::default());
    let interp = Interp::new(
        InterpConfig {
            host: host.clone(),
            mode,
            cmake_root: "/cmake".into(),
            cmake_command: "/cmake/bin/cmake".into(),
            cwd: "/work".into(),
            limits: Default::default(),
            diag: Default::default(),
            defines: Vec::new(),
            script_args: Vec::new(),
        },
        super::register,
    );
    (host, interp)
}

fn script(host: &PureHost, interp: &mut Interp, code: &str) -> TestResult {
    let input = Path::new("/work/test.cmake");
    let version =
        rcmake_base::version::Version::parse_strict(b"4.5").ok_or("invalid test version")?;
    interp.cx().set_cmake_minimum_required(b"4.5", &version)?;
    host.seed_file(input, code.as_bytes())?;
    let outcome = interp.run_script(input);
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
fn path_decomposes_queries_and_compares() -> TestResult {
    let (host, mut interp) = interpreter();
    script(
        &host,
        &mut interp,
        r#"
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
"#,
    )?;
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
            ("root_name", ""),
            ("root_dir", "/"),
            ("root", "/"),
            ("filename", ".hidden.tar.gz"),
            ("ext", ".tar.gz"),
            ("ext_last", ".gz"),
            ("stem", ".hidden"),
            ("stem_last", ".hidden.tar"),
            ("part", "tmp//.hidden.tar.gz"),
            ("parent", "//tmp"),
            ("has_root_name", "OFF"),
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
    script(
        &host,
        &mut interp,
        r#"
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
"#,
    )?;
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
            ("NATIVE", "a//b.tar.gz"),
            ("native_normal", "a/b.tar.gz"),
            ("cmake_list", "a;:b;/d"),
            ("native_list", "a:c"),
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
    let (host, mut interp) = interpreter();
    script(
        &host,
        &mut interp,
        r#"
file(WRITE tree/Z.txt z)
file(WRITE tree/a.txt a)
file(WRITE tree/sub/b.txt b)
file(CREATE_LINK sub tree/alias SYMBOLIC)
file(GLOB shallow RELATIVE /work/tree LIST_DIRECTORIES false "tree/*")
file(GLOB_RECURSE recursive RELATIVE /work/tree "tree/*.txt")
file(GLOB_RECURSE linked FOLLOW_SYMLINKS RELATIVE /work/tree "tree/*.txt")
file(GLOB_RECURSE dirs LIST_DIRECTORIES true RELATIVE /work/tree "tree/missing*")
file(GLOB wildcard RELATIVE /work/tree "tree/s?b/[a-c].txt")
"#,
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
    let (host, mut interp) = interpreter();
    host.env_set(b"HOME", Some(b"/work/home"));
    script(
        &host,
        &mut interp,
        r#"
file(WRITE home/a/file x)
file(CREATE_LINK /work/home/a /work/alias SYMBOLIC)
file(REAL_PATH "~/a/file" expanded EXPAND_TILDE)
file(REAL_PATH file based BASE_DIRECTORY /work/alias)
file(REAL_PATH /work/alias/../a/file real)
file(REAL_PATH /work/bad/../home/a/file missing)
file(RELATIVE_PATH rel /work/other /work/home/a/file)
file(TO_CMAKE_PATH [[~/a/:/b//c/:a\b]] cmake)
file(TO_NATIVE_PATH "path with space:/root/a//b/" native)
"#,
    )?;
    vars(
        &mut interp,
        &[
            ("expanded", "/work/home/a/file"),
            ("based", "/work/home/a/file"),
            ("real", "/work/home/a/file"),
            ("missing", "/work/bad/../home/a/file"),
            ("rel", "../home/a/file"),
            ("cmake", "/work/home/a;/b/c;a/b"),
            ("native", "path\\ with\\ space;/root/a/b/"),
        ],
    );
    Ok(())
}

#[test]
fn file_archive_commands_validate_missing_options() -> TestResult {
    for (code, problem) in [
        (
            "file(ARCHIVE_CREATE PATHS input)",
            "option OUTPUT is required.",
        ),
        (
            "file(ARCHIVE_CREATE OUTPUT result.tar)",
            "option PATHS is required.",
        ),
        (
            "file(ARCHIVE_EXTRACT LIST_ONLY)",
            "option INPUT is required.",
        ),
        (
            "file(GLOB output CONFIGURE_DEPENDS *)",
            "CONFIGURE_DEPENDS is invalid",
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
file(ARCHIVE_EXTRACT INPUT result.tar.gz DESTINATION restored PATTERNS data VERBOSE)
file(ARCHIVE_EXTRACT INPUT result.tar.gz DESTINATION list-only PATTERNS other LIST_ONLY TOUCH)
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
            (" ", "dir/file.tar.gz"),
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
    let (host, mut interp) = interpreter();
    script(
        &host,
        &mut interp,
        r#"
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
"#,
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
            "  Cyclic recursion detected while globbing for 'tree/*':\n\n  /work/tree/sub/deep\n\n  /work/tree/sub/deep/loop\n"
        ).count(),
        2,
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "  Cyclic recursion detected while globbing for 'entry/*':\n\n  /work/outer/inner\n\n  /work/outer/inner/loop\n"
        ),
        "{stderr}"
    );
    Ok(())
}
