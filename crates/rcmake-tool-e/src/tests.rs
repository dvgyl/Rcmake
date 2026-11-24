use std::path::{Path, PathBuf};
use std::time::Duration;

use rcmake_host::{
    FileKind, FileTime, Follow, HostResult, Input, Output, ProcessResult, PureHost, RunOutput,
};

use super::*;

struct Fixture {
    host: PureHost,
    cwd: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let cwd = to_native(if cfg!(windows) { b"C:/work" } else { b"/work" });
        let host = PureHost::new(cwd.clone(), cwd.join("cmake"));
        Self { host, cwd }
    }

    fn run(&self, args: &[&str]) -> i32 {
        let args: Vec<_> = args.iter().map(|arg| BString::from(*arg)).collect();
        run(
            &args,
            &ToolCtx {
                cwd: &self.cwd,
                host: &self.host,
                exe_display: b"test-cmake",
                capabilities: &|| Value::Null,
            },
        )
    }

    fn seed(&self, path: &str, bytes: &[u8]) -> HostResult<()> {
        self.host.seed_file(&self.cwd.join(path), bytes)
    }

    fn read(&self, path: &str) -> HostResult<Vec<u8>> {
        self.host.read(&self.cwd.join(path), read_all())
    }

    fn out(&self) -> BString {
        self.host.stream_bytes(Stream::Stdout)
    }

    fn err(&self) -> BString {
        self.host.stream_bytes(Stream::Stderr)
    }
}

#[test]
fn status_echo_and_capabilities() {
    let f = Fixture::new();
    assert_eq!(f.run(&["true", "ignored"]), 0);
    assert_eq!(f.run(&["false"]), 1);
    assert_eq!(f.run(&["echo", "hello world", "", "-n"]), 0);
    assert_eq!(f.run(&["echo_append", "before", "after"]), 0);
    assert_eq!(f.run(&["echo"]), 0);
    assert_eq!(f.run(&["capabilities"]), 0);
    assert_eq!(f.run(&["capabilities", "extra"]), 1);
    assert_eq!(f.out(), b"hello world  -n\nbefore after\nnull\n");
    assert_eq!(
        f.err(),
        b"-E capabilities accepts no additional arguments\n"
    );
}

#[test]
fn usage_and_unknown_commands() {
    let f = Fixture::new();
    assert_eq!(f.run(&["copp"]), 1);
    assert!(
        f.err()
            .starts_with(b"CMake Error: Unknown command \"copp\". Did you mean \"copy\"?\n")
    );
    assert!(
        f.err()
            .windows(b"Usage: test-cmake -E <command> [arguments...]\nAvailable commands:\n".len())
            .any(|line| line
                == b"Usage: test-cmake -E <command> [arguments...]\nAvailable commands:\n")
    );
    let f = Fixture::new();
    assert_eq!(f.run(&[]), 1);
    assert!(f.err().starts_with(b"CMake Error: cmake version 4.5.1\n"));
    let f = Fixture::new();
    assert_eq!(f.run(&["touch_creat"]), 1);
    assert!(f.err().starts_with(
        b"CMake Error: Unknown command \"touch_creat\". Did you mean \"touch_nocreate\"?\n"
    ));
    let f = Fixture::new();
    assert_eq!(f.run(&["TRUE"]), 1);
    assert!(
        f.err()
            .starts_with(b"CMake Error: Unknown command \"TRUE\".\n")
    );
}

#[test]
fn cat_copies_binary_data_and_continues_after_failures() -> HostResult<()> {
    let f = Fixture::new();
    f.seed("src", &vec![0xff; 70_000])?;
    f.seed("--flag", b"\x00\r\n")?;
    f.host.seed_stdin(b"stdin")?;
    assert_eq!(
        f.run(&["cat", "missing", "src", "-x", "--", "-", "--flag", "-"]),
        1
    );
    assert_eq!(
        f.out(),
        [vec![0xff; 70_000], b"stdin\x00\r\n".to_vec()].concat()
    );
    assert_eq!(f.err(), b"CMake Error: missing: no such file or directory (ignoring)\nCMake Error: -x: option not handled\n");
    Ok(())
}

#[test]
fn make_directory_and_touch() -> HostResult<()> {
    let f = Fixture::new();
    assert_eq!(f.run(&["make_directory", "one/sub", "two"]), 0);
    f.seed("one/keep", b"keep")?;
    assert_eq!(f.run(&["make_directory", "one/keep", "later"]), 1);
    assert_eq!(
        f.err(),
        b"Error creating directory \"one/keep\": File exists\n"
    );
    assert_eq!(
        f.host.metadata(&f.cwd.join("later"), Follow::No)?.kind,
        FileKind::Dir
    );
    assert_eq!(f.run(&["touch", "one/sub/new"]), 0);
    let file = f.cwd.join("one/keep");
    let old = f.host.metadata(&file, Follow::Yes)?.mtime;
    f.host.sleep(Duration::from_secs(1));
    assert_eq!(f.run(&["touch_nocreate", "one/keep", "absent/new"]), 0);
    assert!(f.host.metadata(&file, Follow::Yes)?.mtime > old);
    assert_eq!(f.read("one/keep")?, b"keep");
    assert_eq!(f.read("one/sub/new")?, b"");
    assert!(f.read("absent/new").is_err());
    Ok(())
}

#[test]
fn copy_files_preserves_permissions_and_checks_contents_and_time() -> HostResult<()> {
    let f = Fixture::new();
    f.seed("a", b"first")?;
    f.seed("b", b"second")?;
    f.host.set_permissions(&f.cwd.join("a"), 0o755)?;
    assert_eq!(f.run(&["make_directory", "target"]), 0);
    assert_eq!(f.run(&["copy", "a", "-t", "target", "b"]), 0);
    assert_eq!(f.read("target/a")?, b"first");
    assert_eq!(f.read("target/b")?, b"second");
    let destination = f.cwd.join("target/a");
    let copied = f.host.metadata(&destination, Follow::Yes)?;
    assert_eq!(copied.mode & 0o777, 0o755);
    assert_eq!(f.run(&["copy_if_different", "a", "target"]), 0);
    assert_eq!(
        f.host.metadata(&destination, Follow::Yes)?.mtime,
        copied.mtime
    );
    f.seed("a", b"third")?;
    assert_eq!(f.run(&["copy_if_newer", "a", "target"]), 0);
    assert_eq!(f.read("target/a")?, b"third");
    f.seed("target/a", b"keep")?;
    assert_eq!(f.run(&["copy_if_newer", "a", "target"]), 0);
    assert_eq!(f.read("target/a")?, b"keep");
    assert_eq!(f.run(&["copy", "a", "b", "missing"]), 1);
    assert_eq!(
        f.err(),
        b"Error: Target (for copy command) \"missing\" is not a directory.\n"
    );
    Ok(())
}

#[test]
fn copy_replaces_links_and_leaves_alias_contents_intact() -> HostResult<()> {
    let f = Fixture::new();
    f.seed("source", b"new")?;
    f.seed("target", b"old")?;
    assert_eq!(f.run(&["create_hardlink", "target", "hard"]), 0);
    assert_eq!(f.run(&["create_symlink", "target", "symbolic"]), 0);
    assert_eq!(f.run(&["copy", "target", "symbolic"]), 0);
    assert_eq!(
        f.host.metadata(&f.cwd.join("symbolic"), Follow::No)?.kind,
        FileKind::Symlink
    );
    assert_eq!(f.run(&["copy", "source", "symbolic"]), 0);
    assert_eq!(
        f.host.metadata(&f.cwd.join("symbolic"), Follow::No)?.kind,
        FileKind::File
    );
    assert_eq!(f.read("symbolic")?, b"new");
    assert_eq!(f.read("target")?, b"old");

    assert_eq!(f.run(&["copy", "source", "target"]), 0);
    assert_eq!(f.read("target")?, b"new");
    assert_eq!(f.read("hard")?, b"old");
    assert_eq!(f.run(&["copy", "missing", "target"]), 1);
    assert!(f.read("target").is_err());
    assert_eq!(f.read("hard")?, b"old");
    Ok(())
}

#[test]
fn copy_directories_merges_contents_and_follows_symlinks() -> HostResult<()> {
    let f = Fixture::new();
    f.seed("one/sub/a", b"a")?;
    f.seed("two/b", b"b")?;
    assert_eq!(f.run(&["create_symlink", "sub/a", "one/sym"]), 0);
    assert_eq!(f.run(&["copy_directory", "-t", "dest", "one", "two"]), 0);
    assert_eq!(f.read("dest/sub/a")?, b"a");
    assert_eq!(f.read("dest/b")?, b"b");
    assert_eq!(f.read("dest/sym")?, b"a");
    assert_eq!(
        f.host.metadata(&f.cwd.join("dest/sym"), Follow::No)?.kind,
        FileKind::File
    );

    let target = f.cwd.join("dest/sub/a");
    let mtime = f.host.metadata(&target, Follow::Yes)?.mtime;
    assert_eq!(f.run(&["copy_directory_if_different", "one", "dest"]), 0);
    assert_eq!(f.host.metadata(&target, Follow::Yes)?.mtime, mtime);
    f.seed("one/sub/a", b"new")?;
    assert_eq!(f.run(&["copy_directory_if_newer", "one", "dest"]), 0);
    assert_eq!(f.read("dest/sub/a")?, b"new");
    assert_eq!(f.run(&["copy_directory", "one", "one/sub"]), 1);
    assert!(
        f.err()
            .starts_with(b"Error copying directory from \"one\" to \"one/sub\": ")
    );
    Ok(())
}

#[test]
fn remove_rename_and_links() -> HostResult<()> {
    let f = Fixture::new();
    f.seed("dir/input", b"x")?;
    f.seed("dir/removable", b"")?;
    assert_eq!(f.run(&["rename", "dir/input", "dir/source"]), 0);
    assert!(f.read("dir/input").is_err());
    assert_eq!(f.run(&["create_hardlink", "dir/source", "dir/hard"]), 0);
    assert_eq!(f.run(&["create_symlink", "source", "dir/symbolic"]), 0);
    assert_eq!(
        f.host.read_link(&f.cwd.join("dir/symbolic"))?,
        Path::new("source")
    );
    assert_eq!(f.run(&["remove", "dir/source", "missing"]), 0);
    assert_eq!(f.read("dir/hard")?, b"x");
    assert!(f.read("dir/symbolic").is_err());
    assert_eq!(f.run(&["rm", "dir"]), 1);
    assert_eq!(
        f.err(),
        b"Error removing directory \"dir\" without recursive option.\n"
    );
    assert_eq!(f.run(&["rm", "-Rf", "--", "dir", "missing"]), 0);
    assert_eq!(f.run(&["remove_directory", "dir", "missing"]), 0);
    f.seed("obsolete/item", b"")?;
    assert_eq!(f.run(&["remove_directory", "obsolete"]), 0);
    assert!(f.read("obsolete/item").is_err());
    Ok(())
}

#[test]
fn removing_a_directory_symlink_with_a_trailing_slash_uses_the_target() -> HostResult<()> {
    let f = Fixture::new();
    f.seed("actual/file", b"contents")?;
    assert_eq!(f.run(&["create_symlink", "actual", "link"]), 0);
    assert_eq!(f.run(&["rm", "link/"]), 1);
    assert_eq!(f.read("actual/file")?, b"contents");
    assert_eq!(f.run(&["rm", "-r", "link/"]), 0);
    assert_eq!(f.host.read_link(&f.cwd.join("link"))?, Path::new("actual"));
    assert!(f.read("actual/file").is_err());
    assert_eq!(f.run(&["rm", "-r", "link/"]), 1);
    assert!(
        f.err()
            .ends_with(b"CMake Error: File to remove does not exist and force is not set: link/\n")
    );
    Ok(())
}

#[test]
fn file_operation_errors_match_the_oracle() -> HostResult<()> {
    let f = Fixture::new();
    f.seed("file", b"x")?;
    assert_eq!(f.run(&["touch", "no-parent/file", "later"]), 1);
    assert!(f.read("later").is_err());
    assert_eq!(f.run(&["copy", "no-source", "out"]), 1);
    assert_eq!(f.run(&["rename", "missing", "new"]), 1);
    assert_eq!(f.run(&["create_hardlink", "missing", "link"]), 1);
    assert_eq!(f.run(&["create_symlink", "file", "absent/link"]), 1);
    assert_eq!(f.run(&["rm", "missing", "-x", "file"]), 1);
    assert_eq!(f.read("file")?, b"x");
    assert_eq!(
        f.err(),
        concat!(
            "cmake -E touch: failed to update \"no-parent/file\".\n",
            "Error copying file \"no-source\" to \"out\": No such file or directory\n",
            "Error renaming from \"missing\" to \"new\": No such file or directory\n",
            "failed to create hard link because source path 'missing' does not exist\n",
            "CMake Error: failed to create symbolic link 'absent/link': No such file or directory\n",
            "CMake Error: File to remove does not exist and force is not set: missing\n",
            "CMake Error: Unknown -E rm argument: -x\n",
        )
    );
    Ok(())
}

#[test]
fn compare_files_handles_binary_and_line_endings() -> HostResult<()> {
    let f = Fixture::new();
    f.seed("unix", b"a\nb\x00\xff\n")?;
    f.seed("windows", b"a\r\nb\x00\xff")?;
    assert_eq!(f.run(&["compare_files", "unix", "unix"]), 0);
    assert_eq!(f.run(&["compare_files", "unix", "windows"]), 1);
    assert_eq!(
        f.run(&["compare_files", "--ignore-eol", "unix", "windows"]),
        0
    );
    assert_eq!(f.run(&["compare_files", "missing", "missing"]), 1);
    assert_eq!(f.run(&["compare_files", "-unknown", "unix", "windows"]), 2);
    Ok(())
}

#[test]
fn all_checksum_algorithms_emit_two_spaces_and_count_failures() -> HostResult<()> {
    for (command, digest) in [
        ("md5sum", "900150983cd24fb0d6963f7d28e17f72"),
        ("sha1sum", "a9993e364706816aba3e25717850c26c9cd0d89d"),
        (
            "sha224sum",
            "23097d223405d8228642a477bda255b32aadbce4bda0b3f7e36c9da7",
        ),
        (
            "sha256sum",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        ),
        (
            "sha384sum",
            "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7",
        ),
        (
            "sha512sum",
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
        ),
    ] {
        let f = Fixture::new();
        f.seed("name with spaces", b"abc")?;
        f.host.create_dir_all(&f.cwd.join("dir"))?;
        assert_eq!(f.run(&[command, "missing", "name with spaces", "dir"]), 2);
        assert_eq!(f.out(), format!("{digest}  name with spaces\n"));
        assert_eq!(
            f.err(),
            b"missing: No such file or directory\nError: dir is a directory\n"
        );

        let f = Fixture::new();
        f.host.seed_stdin(b"abc")?;
        assert_eq!(f.run(&[command, "-"]), 0);
        assert_eq!(f.out(), format!("{digest}  -\n"));
        assert!(f.err().is_empty());
    }
    Ok(())
}

#[test]
fn checksum_requires_an_operand_and_reads_stdin_once() -> HostResult<()> {
    let f = Fixture::new();
    f.host.seed_stdin(b"abc")?;
    f.seed("-", b"")?;
    assert_eq!(f.run(&["sha256sum"]), 1);
    assert!(f.out().is_empty());
    assert_eq!(
        f.err(),
        concat!(
            "CMake Error: cmake version 4.5.1\n",
            "Usage: test-cmake -E <command> [arguments...]\nAvailable commands:\n",
            include_str!("usage.txt"),
            "\n",
        )
    );
    assert_eq!(f.run(&["sha256sum", "-", "./-", "-"]), 1);
    assert_eq!(
        f.out(),
        concat!(
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  -\n",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  ./-\n",
        )
    );
    assert!(f.err().ends_with(b"\n\n-: No such file or directory\n"));

    let f = Fixture::new();
    assert_eq!(f.run(&["sha256sum", "-"]), 0);
    assert_eq!(
        f.out(),
        b"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  -\n"
    );
    assert!(f.err().is_empty());
    Ok(())
}

#[test]
fn checksums_cover_full_and_partial_chunks() -> HostResult<()> {
    let exact = vec![b'a'; 1024 * 1024];
    let mut partial = exact.clone();
    partial.extend_from_slice(b"tail");
    for (command, algo) in [("md5sum", HashAlgo::Md5), ("sha256sum", HashAlgo::Sha256)] {
        let f = Fixture::new();
        f.seed("exact", &exact)?;
        f.seed("partial", &partial)?;
        assert_eq!(f.run(&[command, "exact", "partial"]), 0);
        assert_eq!(
            f.out(),
            format!(
                "{}  exact\n{}  partial\n",
                algo.hex_digest(&exact),
                algo.hex_digest(&partial)
            )
        );
        assert!(f.err().is_empty());
    }
    Ok(())
}

#[test]
fn env_prints_sorted_values_and_modifies_a_local_snapshot() -> HostResult<()> {
    let f = Fixture::new();
    f.host.env_set(b"Z", Some(b"original"));
    f.host.env_set(b"A", Some(b"keep"));
    assert_eq!(f.run(&["environment"]), 0);
    f.host.set_command_handler(|_, request, _| {
        assert_eq!(
            request.env,
            [
                ("X".into(), "here".into()),
                ("Y".into(), "y".into()),
                ("Z".into(), "pre-original;last".into()),
            ]
        );
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })?;
    assert_eq!(
        f.run(&[
            "env",
            "--unset=A",
            "Z=changed",
            "--modify",
            "Z=reset:",
            "--modify",
            "Z=string_prepend:pre-",
            "--modify",
            "Z=cmake_list_append:last",
            "--modify",
            "Y=set:y",
            "--modify",
            "X=string_append:here",
            "check-env",
        ]),
        0
    );
    assert_eq!(f.out(), b"A=keep\nZ=original\n");
    assert_eq!(f.run(&["env", "Z=changed"]), 1);
    assert_eq!(f.err(), b"cmake -E env: no command given\n");
    assert_eq!(f.host.env_get(b"A"), Some(BString::from("keep")));
    assert_eq!(f.host.env_get(b"Z"), Some(BString::from("original")));
    assert_eq!(f.run(&["env", "--modify", "X=wrong:foo"]), 1);
    assert_eq!(f.err(), b"cmake -E env: no command given\nCMake Error: Error: Unrecognized environment manipulation argument: wrong\n\ncmake -E env: invalid parameter to --modify: X=wrong:foo\n");
    Ok(())
}

#[test]
fn env_chdir_time_and_sleep_use_host() -> HostResult<()> {
    let f = Fixture::new();
    let cwd = f.cwd.clone();
    f.host.set_command_handler(move |host, request, _| {
        let command = request
            .pipeline
            .first()
            .ok_or_else(|| rcmake_host::HostError::unsupported("missing command"))?;
        assert_eq!(request.stdin, Input::Inherit);
        assert_eq!(request.stdout, Output::Inherit);
        assert_eq!(request.stderr, Output::Inherit);
        assert_eq!(command.args, [BString::from("one arg")]);
        match command.program.as_slice() {
            b"with-env" => {
                assert_eq!(request.cwd, cwd);
                assert_eq!(request.env, [(BString::from("A"), BString::from("value"))]);
            }
            b"with-cwd" => assert_eq!(request.cwd, cwd.join("child")),
            b"timed" => host.sleep(Duration::from_millis(125)),
            _ => return Err(rcmake_host::HostError::unsupported("unknown command")),
        }
        host.write_out(Stream::Stdout, b"child\n");
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(7)],
            ..RunOutput::default()
        })
    })?;
    f.host.create_dir_all(&f.cwd.join("child"))?;
    assert_eq!(f.run(&["env", "A=value", "--", "with-env", "one arg"]), 7);
    assert_eq!(f.run(&["chdir", "child", "with-cwd", "one arg"]), 7);
    assert_eq!(f.run(&["time", "timed", "one arg"]), 7);
    assert_eq!(
        f.out(),
        b"child\nchild\nchild\nElapsed time (seconds): 0.125\n"
    );
    assert_eq!(f.run(&["chdir", "missing", "timed", "one arg"]), 1);
    assert_eq!(
        f.err(),
        b"CMake Error: Directory does not exist for chdir command: missing\n"
    );
    let start = f.host.monotonic();
    assert_eq!(f.run(&["sleep", "0.125s", ".375"]), 0);
    assert_eq!(f.host.monotonic() - start, Duration::from_millis(500));
    assert_eq!(f.run(&["sleep", "0.5", "-1"]), 1);
    assert_eq!(f.host.monotonic() - start, Duration::from_millis(500));
    Ok(())
}

#[test]
fn bin2c_formats_bytes_and_expands_templates() -> HostResult<()> {
    let f = Fixture::new();
    f.host.seed_stdin(b"A\x80\xff\x00")?;
    assert_eq!(f.run(&["bin2c", "--signed", "-"]), 0);
    assert_eq!(f.out(), b"  0x41,-0x80,-0x01, 0x00\n");
    f.seed("binary", b"\x00\xff")?;
    f.seed(
        "template",
        b"int x[] = {@arr@};\r\nsize_t n = @len@; @other@\r\n",
    )?;
    assert_eq!(
        f.run(&[
            "bin2c",
            "--decimal",
            "--trailing-comma",
            "--template-file",
            "template",
            "--template-array-placeholder",
            "arr",
            "--template-length-placeholder",
            "len",
            "--",
            "binary",
            "out/embedded.inc",
        ]),
        0
    );
    assert_eq!(
        f.read("out/embedded.inc")?,
        b"int x[] = {\n    0, 255,\n};\nsize_t n = 2; @other@\n"
    );
    f.seed("template", b"prefix @length@ @array@")?;
    assert_eq!(
        f.run(&["bin2c", "--template-file", "template", "binary"]),
        1
    );
    assert!(f.out().ends_with(b"prefix "));
    assert_eq!(f.err(), b"Cannot print length before array\n");
    Ok(())
}

#[test]
fn tar_option_and_missing_archive_errors() -> HostResult<()> {
    let f = Fixture::new();
    f.seed("options", b"\n-file\n")?;
    assert_eq!(f.run(&["tar", "tjz", "out"]), 1);
    assert_eq!(f.run(&["tar", "cf", "out", "--format=invalid"]), 1);
    assert_eq!(f.run(&["tar", "cf", "out", "--cmake-tar-threads=nan"]), 1);
    assert_eq!(f.run(&["tar", "cf", "out", "--files-from=options"]), 1);
    assert_eq!(f.run(&["tar", "tf", "missing"]), 1);
    assert_eq!(
        f.err(),
        concat!(
            "CMake Error: Can only compress a tar file one way; at most one flag of z, j, or J may be used\n",
            "CMake Error: Unknown -E tar --format= argument: invalid\n",
            "CMake Error: Invalid --cmake-tar-threads value: 'nan' - not a number\n",
            "CMake Error: -E tar --files-from='options' file invalid line:\n-file\n\n",
            "CMake Error: Problem with archive_read_open_filename(): Failed to open 'missing'\n",
            "CMake Error: Problem listing tar: missing\n",
        )
    );
    Ok(())
}

#[test]
fn tar_reports_missing_members_for_extract_and_both_list_modes() -> HostResult<()> {
    for (flags, summary) in [
        ("xf", "CMake Error: Problem extracting tar:\n  archive\n"),
        ("tf", "CMake Error: Problem listing tar: archive\n"),
        ("tvf", "CMake Error: Problem listing tar: archive\n"),
    ] {
        let f = Fixture::new();
        f.seed("member with spaces", b"content")?;
        assert_eq!(f.run(&["tar", "cf", "archive", "member with spaces"]), 0);
        assert_eq!(f.run(&["tar", flags, "archive", "missing member"]), 1);
        assert!(f.out().is_empty());
        assert_eq!(
            f.err(),
            format!("CMake Error: tar: missing member: Not found in archive\n{summary}")
        );
    }
    Ok(())
}

#[test]
fn tar_round_trip_with_the_shared_archive_backend() -> HostResult<()> {
    for [flags, format, mtime] in [
        ["cf", "--format=gnutar", "--mtime=2023-11-14 23:13:20+0100"],
        ["czf", "--format=gnutar", "--mtime=@1700000000"],
        ["cf", "--format=paxr", "--mtime=2023-11-14 22:13:20 UTC"],
        ["czf", "--format=paxr", "--mtime=@1700000000"],
        ["cf", "--format=zip", "--mtime=@1700000000"],
    ] {
        let f = Fixture::new();
        f.seed("-input", b"content")?;
        f.seed("paths", b"--add-file=-input\n")?;
        assert_eq!(
            f.run(&["tar", flags, "archive", format, mtime, "--files-from=paths"]),
            0,
            "{}",
            f.err()
        );
        assert_eq!(f.run(&["tar", "tf", "archive"]), 0);
        assert_eq!(f.out(), b"-input\n");
        assert_eq!(f.run(&["rm", "--", "-input"]), 0);
        assert_eq!(f.run(&["tar", "xvf", "archive", "--", "-input"]), 0);
        assert_eq!(f.out(), b"-input\nx -input\n");
        assert_eq!(f.read("-input")?, b"content");
        let metadata = f.host.metadata(&f.cwd.join("-input"), Follow::Yes)?;
        assert_eq!(
            metadata.mtime,
            FileTime {
                secs: 1_700_000_000,
                nanos: 0
            }
        );
    }
    Ok(())
}

#[test]
fn tar_records_and_lists_absolute_input_as_a_relative_path() -> HostResult<()> {
    let f = Fixture::new();
    let outside = f.cwd.with_file_name("outside");
    f.host.seed_file(&outside, b"content")?;
    f.host.set_permissions(&outside, 0o644)?;
    assert_eq!(
        f.run(&[
            "tar",
            "cfq",
            "archive",
            "--format=gnutar",
            "--mtime=1970-01-01UTC",
            "--cmake-tar-threads=4",
            &outside.to_string_lossy(),
        ]),
        0,
    );
    assert_eq!(f.err(), b"tar: Unknown argument: q\n");
    assert_eq!(
        f.read("archive")?.get(257..265),
        Some(b"ustar  \0".as_slice())
    );
    assert_eq!(f.run(&["tar", "tvf", "archive"]), 0);
    let day = if cfg!(windows) { "01" } else { " 1" };
    assert_eq!(
        f.out(),
        format!("-rw-r--r--  0 0      0           7 {day} Jan  1970 ../outside\n"),
    );
    assert_eq!(f.run(&["tar", "xf", "archive"]), 1);
    assert!(f.err().ends_with(
        b"CMake Error: Problem with archive_write_header(): Path contains '..'\nCMake Error: Current file:\n  ../outside\nCMake Error: Problem extracting tar:\n  archive\n",
    ));
    assert_eq!(f.host.read(&outside, read_all())?, b"content");
    Ok(())
}

#[test]
fn generator_symlinks_use_basenames_and_replace_files() -> HostResult<()> {
    let f = Fixture::new();
    f.seed("lib/libdemo.2.1", b"library")?;
    f.seed("lib/libdemo.2", b"old")?;
    f.host
        .symlink(Path::new("wrong"), &f.cwd.join("lib/libdemo"))?;
    let args = [
        "cmake_symlink_library",
        "lib/libdemo.2.1",
        "lib/libdemo.2",
        "lib/libdemo",
    ];
    assert_eq!(f.run(&args), 0);
    assert_eq!(f.run(&args), 0);
    assert_eq!(
        f.host.read_link(&f.cwd.join("lib/libdemo"))?,
        Path::new("libdemo.2")
    );
    assert_eq!(
        f.host.read_link(&f.cwd.join("lib/libdemo.2"))?,
        Path::new("libdemo.2.1")
    );
    assert_eq!(f.read("lib/libdemo")?, b"library");
    assert_eq!(
        f.run(&["cmake_symlink_executable", "lib/libdemo.2.1", "app"]),
        0
    );
    assert_eq!(
        f.host.read_link(&f.cwd.join("app"))?,
        Path::new("libdemo.2.1")
    );
    assert_eq!(
        f.run(&[
            "cmake_symlink_executable",
            "lib/libdemo.2.1",
            "lib/libdemo.2.1"
        ]),
        0
    );
    assert_eq!(f.read("lib/libdemo.2.1")?, b"library");
    assert_eq!(f.err(), b"");
    Ok(())
}

#[test]
fn library_symlink_attempts_both_links_after_first_failure() -> HostResult<()> {
    let f = Fixture::new();
    assert_eq!(
        f.run(&["cmake_symlink_library", "source", "missing/soname", "name"]),
        1
    );
    assert_eq!(f.host.read_link(&f.cwd.join("name"))?, Path::new("soname"));
    assert_eq!(
        f.err(),
        b"CMake Error: failed to create symbolic link 'missing/soname': No such file or directory\nCMake Error: cmake_symlink_library: System Error: No such file or directory\n"
    );
    Ok(())
}

#[test]
fn depfile_transform_rebases_and_escapes_inputs() -> HostResult<()> {
    let f = Fixture::new();
    f.seed("input.d", b"a.o b.o: a.c ./a.c ../b.h space\\ name.h hash\\#x.h dollar$$name.h colon\\:x.h \\\r\n other.h\r\nfoo.h:\n")?;
    for (generator, format) in [("Ninja", "gccdepfile"), ("Unix Makefiles", "makedepfile")] {
        assert_eq!(
            f.run(&[
                "cmake_transform_depfile",
                generator,
                format,
                "src",
                "src/sub",
                "build",
                "build/sub",
                "input.d",
                "output.d",
            ]),
            0
        );
        let mut expected = b"sub/a.o \\\n  sub/b.o: \\\n  b.h \\\n  sub/a.c \\\n  sub/a.c \\\n  sub/colon:x.h \\\n  sub/dollar$$name.h \\\n  sub/hash\\#x.h \\\n  sub/other.h \\\n  sub/space\\ name.h\nsub/foo.h:\n".to_vec();
        if format == "makedepfile" {
            expected.extend_from_slice(b"\n\nb.h:\n\nsub/a.c:\n\nsub/a.c:\n\nsub/colon:x.h:\n\nsub/dollar$$name.h:\n\nsub/hash\\#x.h:\n\nsub/other.h:\n\nsub/space\\ name.h:\n");
        }
        assert_eq!(f.read("output.d")?, expected);
    }
    assert_eq!(f.err(), b"");
    Ok(())
}

#[test]
fn depfile_transform_distinguishes_missing_and_invalid_input() -> HostResult<()> {
    let f = Fixture::new();
    let args = [
        "cmake_transform_depfile",
        "Ninja",
        "gccdepfile",
        "src",
        "src",
        "build",
        "build",
        "input.d",
        "output.d",
    ];
    assert_eq!(f.run(&args), 0);
    assert_eq!(f.read("output.d")?, b"");
    assert_eq!(
        f.err(),
        b"CMake Warning:\n  Expected depfile does not exist.\n\n    input.d\n\n\n"
    );
    f.seed("input.d", b"invalid dependency rule\n")?;
    assert_eq!(f.run(&args), 2);
    assert_eq!(f.read("output.d")?, b"");
    Ok(())
}

#[test]
fn echo_color_updates_make_progress_and_formats_messages() -> HostResult<()> {
    let f = Fixture::new();
    f.seed("total", b"4\n")?;
    assert_eq!(
        f.run(&["cmake_progress_start", "build/CMakeFiles", "total"]),
        0
    );
    assert_eq!(f.read("build/CMakeFiles/Progress/count.txt")?, b"4\n");
    assert_eq!(
        f.run(&["cmake_progress_report", "build/CMakeFiles", "4"]),
        0
    );
    assert!(f.read("build/CMakeFiles/Progress/4").is_err());
    assert_eq!(
        f.run(&[
            "cmake_echo_color",
            "--switch=ON",
            "--green",
            "--bold",
            "--progress-dir=build/CMakeFiles",
            "--progress-num=1,2,2",
            "--no-newline",
            "Linking ",
            "app",
            "--normal",
            "--newline",
            ".",
            "--progress-num=2,3",
            "Done",
        ]),
        0
    );
    assert_eq!(f.out(), b"[ 50%] Linking app.\n[ 75%] Done\n");
    assert_eq!(f.read("build/CMakeFiles/Progress/3")?, b"empty");
    assert_eq!(f.run(&["cmake_progress_start", "build/CMakeFiles", "2"]), 0);
    assert!(f.read("build/CMakeFiles/Progress/3").is_err());
    assert_eq!(f.read("build/CMakeFiles/Progress/count.txt")?, b"2\n");
    assert_eq!(f.run(&["cmake_progress_start", "build/CMakeFiles", "0"]), 0);
    assert!(!is_dir(
        &ToolCtx {
            host: &f.host,
            cwd: &f.cwd,
            exe_display: b"cmake",
            capabilities: &|| Value::Null,
        },
        &f.cwd.join("build/CMakeFiles/Progress")
    ));
    Ok(())
}

#[test]
fn echo_color_uses_ansi_styles_when_color_is_forced() {
    let f = Fixture::new();
    f.host.env_set(b"CLICOLOR_FORCE", Some(b"1"));
    assert_eq!(
        f.run(&[
            "cmake_echo_color",
            "plain",
            "--red",
            "one",
            "--bold",
            "two",
            "--switch=OFF",
            "three",
            "--switch=ON",
            "--normal",
            "four",
        ]),
        0
    );
    assert_eq!(
        f.out(),
        b"plain\n\x1b[31mone\x1b[0m\n\x1b[1m\x1b[31mtwo\x1b[0m\nthree\n\x1b[0mfour\x1b[0m\n"
    );
}

#[test]
fn ninja_dyndep_validates_arguments_like_the_oracle() {
    let f = Fixture::new();
    assert_eq!(f.run(&["cmake_ninja_dyndep"]), 1);
    assert_eq!(
        f.err(),
        b"CMake Error: -E cmake_ninja_dyndep requires value for --tdi=\n"
    );
    let f = Fixture::new();
    assert_eq!(f.run(&["cmake_ninja_dyndep", "--bogus"]), 1);
    assert_eq!(
        f.err(),
        b"CMake Error: -E cmake_ninja_dyndep unknown argument: --bogus\n"
    );
    let f = Fixture::new();
    assert_eq!(
        f.run(&[
            "cmake_ninja_dyndep",
            "--tdi=x.json",
            "--lang=CXX",
            "--dd=o.dd"
        ]),
        1
    );
    assert_eq!(
        f.err(),
        b"CMake Error: -E cmake_ninja_dyndep failed to parse x.jsonFile not found: x.json\n"
    );
}

#[test]
fn icstat_initializes_checks_and_runs_before_the_compiler() -> HostResult<()> {
    use std::sync::{Arc, Mutex};

    let f = Fixture::new();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&calls);
    f.host.set_command_handler(move |_, request, _| {
        let command = &request.pipeline[0];
        if let Ok(mut calls) = observed.lock() {
            calls.push(command.program.clone());
        }
        let stdout = match command.program.as_slice() {
            b"iar/ichecks" => {
                assert_eq!(command.args, ["--default=stdchecks"]);
                Vec::new()
            }
            b"icstat" => {
                assert_eq!(
                    command.args,
                    ["analyze", "--", "iar/icc", "-c", "src/main.c"]
                );
                b"Results:\n\"../../src/main.c\", line 3\n".to_vec()
            }
            b"iar/icc" => Vec::new(),
            other => panic!("Unexpected command: {}", BString::from(other)),
        };
        Ok(RunOutput {
            stdout,
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })?;
    assert_eq!(
        f.run(&[
            "__run_co_compile",
            "--icstat=icstat;analyze",
            "--source=src/main.c",
            "--",
            "iar/icc",
            "-c",
            "src/main.c",
        ]),
        0
    );
    assert_eq!(
        f.err(),
        b"C-STAT: Analyzing main.c (cached)\nResults:\n\"src/main.c\", line 3\n"
    );
    let calls = calls
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(*calls, ["iar/ichecks", "icstat", "iar/icc"]);
    Ok(())
}
