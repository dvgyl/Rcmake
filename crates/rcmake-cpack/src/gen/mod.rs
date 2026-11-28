//! Generator registry and the External, Linux, macOS, Windows, and IFW generators.

mod appimage;
pub mod bundle;
mod deb;
pub mod dmg;
pub(crate) mod external;
mod ifw;
pub(crate) mod installer;
mod linux;
mod nsis;
mod nuget;
mod productbuild;
mod rpm;
mod wix;

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rcmake_base::BString;
use rcmake_host::{
    CommandSpec, CopyOptions, FileKind, Follow, Host, Input, Output, ProcessResult, RunRequest,
};

use self::installer::HostPlatform;
use crate::archive_gen::{ArchiveGenerator, ArchiveKind};
use crate::config::absolute;
use crate::stage::clear_output_file;
use crate::{CpackError, PackageContext, PackageGenerator, error};

/// The generator named `name` (a `CPACK_GENERATOR` entry), or `None` when Rcmake does
/// not have it on the current target platform.
#[must_use]
pub fn create(name: &[u8]) -> Option<Box<dyn PackageGenerator>> {
    let platform = if cfg!(target_os = "macos") {
        HostPlatform::MacOs
    } else if cfg!(windows) {
        HostPlatform::Windows
    } else if cfg!(target_os = "linux") {
        HostPlatform::Linux
    } else {
        HostPlatform::OtherUnix
    };
    create_for_platform(name, platform, false)
}

/// Resolve a generator for the runtime host reported by [`Host::system`].
///
/// Platform gating:
/// - `NSIS`, `NSIS64`, `IFW`, `External`, and all archive generators are available on all platforms.
/// - `productbuild` is available on macOS (`Darwin`).
/// - `WIX` is available on [`HostPlatform::Windows`], which also covers Cygwin, MSYS, and MinGW
///   system names ([`installer::classify_sysname`]). Upstream CMake enables `WIX` on Windows,
///   and on Cygwin only for builds linked with `LibUUID`.
/// - `PureOS` (the default `PureHost` identity in existing unit tests) falls back to the
///   compile-time platform for platform-gated installers while keeping existing `DragNDrop`,
///   `Bundle`, `DEB`, and `RPM` tests working.
pub(crate) fn create_for_host(name: &[u8], host: &dyn Host) -> Option<Box<dyn PackageGenerator>> {
    let sysname = host.system().sysname;
    if sysname == b"PureOS" {
        return create(name);
    }
    create_for_platform(name, installer::classify_sysname(&sysname), false)
}

/// Construct any implemented generator regardless of runtime host platform (used by
/// cross-platform portable test suites to verify every backend across macOS/Linux/Windows).
#[cfg(test)]
pub(crate) fn create_unrestricted(
    name: &[u8],
    _host: &dyn Host,
) -> Option<Box<dyn PackageGenerator>> {
    create_for_platform(name, HostPlatform::OtherUnix, true)
}

fn create_for_platform(
    name: &[u8],
    platform: HostPlatform,
    unrestricted: bool,
) -> Option<Box<dyn PackageGenerator>> {
    if let Some(kind) = ArchiveKind::ALL
        .into_iter()
        .find(|kind| kind.name().as_bytes() == name)
    {
        return Some(Box::new(ArchiveGenerator { kind }));
    }
    match name {
        b"DEB" => Some(Box::new(deb::Deb)),
        b"RPM" => Some(Box::new(rpm::Rpm)),
        b"AppImage" if unrestricted || platform == HostPlatform::Linux => {
            Some(Box::new(appimage::AppImage))
        }
        b"NuGet" => Some(Box::new(nuget::NuGet)),
        b"External" => Some(Box::new(external::ExternalGenerator)),
        b"DragNDrop" => Some(Box::new(dmg::DragNDropGenerator)),
        b"Bundle" => Some(Box::new(bundle::BundleGenerator)),
        b"NSIS" => Some(Box::new(nsis::Nsis::bits32())),
        b"NSIS64" => Some(Box::new(nsis::Nsis::bits64())),
        b"IFW" => Some(Box::new(ifw::Ifw)),
        b"productbuild" if unrestricted || platform == HostPlatform::MacOs => {
            Some(Box::new(productbuild::ProductBuild))
        }
        b"WIX" if unrestricted || platform == HostPlatform::Windows => Some(Box::new(wix::Wix)),
        _ => None,
    }
}

/// The generators that CMake 4.5.1 registers on every platform, with their
/// `cpack --help` descriptions (`cmCPackGeneratorFactory`).
const COMMON_GENERATORS: &[(&str, &str)] = &[
    ("7Z", "7-Zip file format with default compression algorithm"),
    ("7Z_STORE", "7-Zip file format without compression"),
    ("7Z_DEFLATE", "7-Zip file format with Deflate compression"),
    ("7Z_BZ2", "7-Zip file format with BZip2 compression"),
    ("7Z_LZMA", "7-Zip file format with LZMA compression"),
    ("7Z_LZMA2", "7-Zip file format with LZMA2 compression"),
    ("7Z_ZSTD", "7-Zip file format with Zstandard compression"),
    ("7Z_PPMD", "7-Zip file format with PPMd compression"),
    ("TBZ2", "Tar BZip2 compression"),
    ("TGZ", "Tar GZip compression"),
    ("TXZ", "Tar XZ compression"),
    ("TZ", "Tar Compress compression"),
    ("TZST", "Tar Zstandard compression"),
    ("TAR", "Tar no compression"),
    ("ZIP", "ZIP file format with default compression algorithm"),
    ("ZIP_STORE", "ZIP file format without compression"),
    ("ZIP_DEFLATE", "ZIP file format with Deflate compression"),
    ("ZIP_BZ2", "ZIP file format with BZip2 compression"),
    ("ZIP_LZMA", "ZIP file format with LZMA compression"),
    ("ZIP_LZMA2", "ZIP file format with LZMA2 compression"),
    ("ZIP_ZSTD", "ZIP file format with Zstandard compression"),
    ("STGZ", "Self extracting Tar GZip compression"),
    ("INNOSETUP", "Inno Setup packages"),
    ("NSIS", "Null Soft Installer"),
    ("NSIS64", "Null Soft Installer (64-bit)"),
    ("IFW", "Qt Installer Framework"),
    ("NuGet", "NuGet packages"),
    ("APK", "Android APK packages"),
    ("External", "CPack External packages"),
];

/// The generators that CMake 4.5.1 registers only on this platform. On macOS,
/// CMake adds `DEB` and `RPM` only when it finds `dpkg` or `rpmbuild`, so the
/// list leaves them out.
const PLATFORM_GENERATORS: &[(&str, &str)] = if cfg!(target_os = "macos") {
    &[
        ("DragNDrop", "Mac OSX Drag And Drop"),
        ("Bundle", "Mac OSX bundle"),
        ("productbuild", "Mac OSX pkg"),
    ]
} else if cfg!(windows) {
    &[
        ("WIX", "MSI file format via WiX tools"),
        ("DEB", "Debian packages"),
    ]
} else if cfg!(target_os = "linux") {
    &[
        ("DEB", "Debian packages"),
        ("RPM", "RPM packages"),
        ("AppImage", "AppImage packages"),
    ]
} else {
    &[("DEB", "Debian packages"), ("RPM", "RPM packages")]
};

/// The `Generators` section that CPack prints after an unknown generator name.
///
/// The section lists the generators of CMake 4.5.1 on this platform, as the
/// CMake error does, in byte order. The layout is the `cmDocumentationFormatter`
/// layout: the name in a 29-column field after two spaces, then `= ` and the
/// description, wrapped at 77 columns with a 33-column indent.
#[must_use]
pub fn documentation() -> String {
    const NAME_WIDTH: usize = 29;
    const INDENT: usize = NAME_WIDTH + 4;
    const TEXT_WIDTH: usize = 77;
    let mut generators: Vec<_> = COMMON_GENERATORS
        .iter()
        .chain(PLATFORM_GENERATORS)
        .collect();
    generators.sort_unstable_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    let mut text = String::from("Generators\n");
    for (name, description) in generators {
        text.push_str(&format!("  {name:<width$}= ", width = NAME_WIDTH));
        let mut width = INDENT;
        for (index, word) in description.split(' ').enumerate() {
            if index > 0 && width + 1 + word.len() > TEXT_WIDTH {
                text.push('\n');
                text.push_str(&" ".repeat(INDENT));
                width = INDENT;
            } else if index > 0 {
                text.push(' ');
                width += 1;
            }
            text.push_str(word);
            width += word.len();
        }
        text.push('\n');
    }
    text
}

fn option<'a>(cx: &PackageContext<'a>, name: &[u8]) -> Option<&'a [u8]> {
    cx.vars
        .get(name)
        .map(|value| value.as_slice())
        .filter(|value| !value.is_empty())
}

fn copy_asset(host: &dyn Host, source: &[u8], dest: &Path) -> Result<(), CpackError> {
    let source = absolute(&host.initial_cwd(), source);
    if host.metadata(&source, Follow::Yes)?.kind != FileKind::File {
        return Err(error(format!(
            "Expected a regular file at {}",
            source.display()
        )));
    }
    if source != dest {
        clear_output_file(host, dest)?;
        host.copy_file(
            &source,
            dest,
            CopyOptions {
                preserve_permissions: true,
                preserve_mtime: true,
            },
        )?;
    }
    Ok(())
}

fn run_tool(
    host: &dyn Host,
    cwd: &Path,
    program: &[u8],
    args: Vec<BString>,
) -> Result<(), CpackError> {
    let output = host.run(
        &RunRequest {
            pipeline: vec![CommandSpec {
                program: program.into(),
                args,
            }],
            cwd: cwd.to_owned(),
            env: host.env_all(),
            stdin: Input::Null,
            stdout: Output::Capture,
            stderr: Output::SharedWithStdout,
            timeout: None,
            tee: false,
            default_error_mode: false,
        },
        &mut |_, _| {},
    )?;
    match output.results.as_slice() {
        [ProcessResult::Exited(0)] => Ok(()),
        [ProcessResult::SpawnFailed(err)] => Err(error(format!(
            "Cannot start {}: {err}",
            BString::from(program)
        ))),
        results => Err(error(format!(
            "{} failed ({results:?})\n{}",
            BString::from(program),
            BString::from(output.stdout)
        ))),
    }
}

struct ScratchDir<'a> {
    host: &'a dyn Host,
    path: PathBuf,
    cleanup: bool,
}

impl<'a> ScratchDir<'a> {
    fn new(host: &'a dyn Host, root: &Path) -> Result<Self, CpackError> {
        Self::for_generator(host, root, "dmg")
    }

    fn for_generator(host: &'a dyn Host, root: &Path, generator: &str) -> Result<Self, CpackError> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        for _ in 0..100 {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let path = root.join(format!(".rcmake-{generator}-{}-{id}", host.system().pid));
            match host.create_private_dir(&path) {
                Ok(()) => {
                    return Ok(Self {
                        host,
                        path,
                        cleanup: true,
                    });
                }
                Err(err) if err.kind == ErrorKind::AlreadyExists => {}
                Err(err) => return Err(err.into()),
            }
        }
        Err(error("Cannot create a temporary staging directory"))
    }
}

impl Drop for ScratchDir<'_> {
    fn drop(&mut self) {
        if self.cleanup {
            let _ = self.host.remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use rcmake_base::hash::HashAlgo;
    use rcmake_host::path::{from_native, to_native};
    use rcmake_host::{CommandSpec, Host, ProcessResult, ReadRange, RunOutput, WriteMode};

    #[test]
    fn documentation_lists_the_cmake_generators_in_cmake_layout() {
        let text = super::documentation();
        for kind in crate::archive_gen::ArchiveKind::ALL {
            assert!(
                text.contains(&format!("\n  {:<29}= ", kind.name())),
                "{}",
                kind.name()
            );
        }
        assert!(text.starts_with(
            "Generators\n  7Z                           = 7-Zip file format with default compression\n                                 algorithm\n  7Z_BZ2                       = 7-Zip file format with BZip2 compression\n"
        ));
    }

    /// The full stderr of a bad `-G` on macOS, as CMake 4.5.1 writes it.
    #[cfg(target_os = "macos")]
    #[test]
    fn unknown_generator_error_matches_the_macos_cmake_list() {
        let host = package_host();
        host.seed_file(
            Path::new("/work/CPackConfig.cmake"),
            b"set(CPACK_PACKAGE_NAME x)\nset(CPACK_PACKAGE_VERSION 1.0)\n",
        )
        .unwrap();
        let argv = ["cpack", "-G", "BOGUS_GEN"].map(rcmake_base::BString::from);
        assert_eq!(crate::main(&argv, host.clone()), 1);
        let expected = "CPack Error: Could not create CPack generator: BOGUS_GEN

Generators
  7Z                           = 7-Zip file format with default compression
                                 algorithm
  7Z_BZ2                       = 7-Zip file format with BZip2 compression
  7Z_DEFLATE                   = 7-Zip file format with Deflate compression
  7Z_LZMA                      = 7-Zip file format with LZMA compression
  7Z_LZMA2                     = 7-Zip file format with LZMA2 compression
  7Z_PPMD                      = 7-Zip file format with PPMd compression
  7Z_STORE                     = 7-Zip file format without compression
  7Z_ZSTD                      = 7-Zip file format with Zstandard compression
  APK                          = Android APK packages
  Bundle                       = Mac OSX bundle
  DragNDrop                    = Mac OSX Drag And Drop
  External                     = CPack External packages
  IFW                          = Qt Installer Framework
  INNOSETUP                    = Inno Setup packages
  NSIS                         = Null Soft Installer
  NSIS64                       = Null Soft Installer (64-bit)
  NuGet                        = NuGet packages
  STGZ                         = Self extracting Tar GZip compression
  TAR                          = Tar no compression
  TBZ2                         = Tar BZip2 compression
  TGZ                          = Tar GZip compression
  TXZ                          = Tar XZ compression
  TZ                           = Tar Compress compression
  TZST                         = Tar Zstandard compression
  ZIP                          = ZIP file format with default compression
                                 algorithm
  ZIP_BZ2                      = ZIP file format with BZip2 compression
  ZIP_DEFLATE                  = ZIP file format with Deflate compression
  ZIP_LZMA                     = ZIP file format with LZMA compression
  ZIP_LZMA2                    = ZIP file format with LZMA2 compression
  ZIP_STORE                    = ZIP file format without compression
  ZIP_ZSTD                     = ZIP file format with Zstandard compression
  productbuild                 = Mac OSX pkg

";
        assert_eq!(
            String::from_utf8_lossy(&host.stream_bytes(rcmake_host::Stream::Stderr)),
            expected
        );
    }

    pub(super) fn package_host() -> Arc<rcmake_host::PureHost> {
        let exe = if cfg!(windows) {
            "C:/bin/cpack.exe"
        } else {
            "/bin/cpack"
        };
        Arc::new(rcmake_host::PureHost::new("/work".into(), exe.into()))
    }

    pub(crate) fn invoke(host: Arc<dyn Host>, generator: &str, options: &str) -> i32 {
        let root = host.initial_cwd();
        let source = root.join("payload");
        host.create_dir_all(&source).unwrap();
        host.write(&source.join("readme.txt"), b"payload", WriteMode::Truncate)
            .unwrap();
        let config = root.join("CPackConfig.cmake");
        let source = rcmake_host::path::from_native_cmake(&source);
        host.write(
            &config,
            format!(
                r#"set(CPACK_GENERATOR {generator})
set(CPACK_PACKAGE_NAME sample)
set(CPACK_PACKAGE_VERSION 1)
set(CPACK_PACKAGE_FILE_NAME "Sample Package")
set(CPACK_INSTALLED_DIRECTORIES [=[{source};/]=])
set(CPACK_PACKAGE_CHECKSUM "MD5;SHA256")
{options}
"#,
            )
            .as_bytes(),
            WriteMode::Truncate,
        )
        .unwrap();

        crate::main(
            &["cpack".into(), "--config".into(), from_native(&config)],
            host,
        )
    }

    pub(crate) fn read(host: &dyn Host, path: &Path) -> Vec<u8> {
        host.read(
            path,
            ReadRange::All {
                limit: 16 * 1024 * 1024,
            },
        )
        .unwrap()
    }

    pub(super) fn check_checksums(host: &dyn Host) {
        let root = host.initial_cwd();
        let bytes = read(host, &root.join("Sample Package.dmg"));
        for (extension, algorithm) in [("md5", HashAlgo::Md5), ("sha256", HashAlgo::Sha256)] {
            let expected = format!("{}  Sample Package.dmg\n", algorithm.hex_digest(&bytes));
            assert_eq!(
                read(host, &root.join(format!("Sample Package.dmg.{extension}"))),
                expected.as_bytes()
            );
        }
    }

    pub(super) fn fake_image(host: &dyn Host, command: &CommandSpec) {
        let output = to_native(command.args.last().unwrap());
        host.write(&output, b"disk image", WriteMode::CreateNew)
            .unwrap();
    }

    pub(super) fn success() -> RunOutput {
        RunOutput {
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        }
    }
}
