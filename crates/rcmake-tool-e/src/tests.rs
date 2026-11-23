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
    }
    Ok(())
}

#[test]
fn env_prints_sorted_values_and_modifies_a_local_snapshot() {
    let f = Fixture::new();
    f.host.env_set(b"Z", Some(b"original"));
    f.host.env_set(b"A", Some(b"keep"));
    assert_eq!(f.run(&["environment"]), 0);
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
        ]),
        0
    );
    assert_eq!(
        f.out(),
        b"A=keep\nZ=original\nX=here\nY=y\nZ=pre-original;last\n"
    );
    assert_eq!(f.host.env_get(b"A"), Some(BString::from("keep")));
    assert_eq!(f.host.env_get(b"Z"), Some(BString::from("original")));
    assert_eq!(f.run(&["env", "--modify", "X=wrong:foo"]), 1);
    assert_eq!(f.err(), b"CMake Error: Error: Unrecognized environment manipulation argument: wrong\n\ncmake -E env: invalid parameter to --modify: X=wrong:foo\n");
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
    assert_eq!(f.run(&["tar", "cf", "out", "--files-from=options"]), 1);
    assert_eq!(f.run(&["tar", "tf", "missing"]), 1);
    assert_eq!(
        f.err(),
        concat!(
            "CMake Error: Can only compress a tar file one way; at most one flag of z, j, or J may be used\n",
            "CMake Error: Unknown -E tar --format= argument: invalid\n",
            "CMake Error: -E tar --files-from='options' file invalid line:\n-file\n\n",
            "CMake Error: Problem with archive_read_open_filename(): Failed to open 'missing'\n",
            "CMake Error: Problem listing tar: missing\n",
        )
    );
    Ok(())
}

#[test]
fn tar_round_trip_with_the_shared_archive_backend() -> HostResult<()> {
    for args in [
        ["cf", "--format=paxr"],
        ["czf", "--format=paxr"],
        ["cf", "--format=zip"],
    ] {
        let [flags, format] = args;
        let f = Fixture::new();
        f.seed("-input", b"content")?;
        f.seed("paths", b"--add-file=-input\n")?;
        assert_eq!(
            f.run(&[
                "tar",
                flags,
                "archive",
                format,
                "--mtime=@1700000000",
                "--files-from=paths"
            ]),
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
