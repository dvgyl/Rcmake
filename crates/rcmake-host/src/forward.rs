/// Forward groups of [`Host`](crate::Host) methods to an adapter's base field.
///
/// Use `forward_host!(base, fs, env, process, services)` in the trait implementation.
/// Omit each group whose methods the adapter implements itself.
#[macro_export]
macro_rules! forward_host {
    ($base:ident, $($group:ident),+ $(,)?) => {
        $($crate::forward_host!(@ $base, $group);)+
    };
    (@method $base:ident, $method:ident($($arg:ident : $ty:ty),* $(,)?) $(-> $result:ty)?) => {
        fn $method(&self, $($arg: $ty),*) $(-> $result)? {
            self.$base.$method($($arg),*)
        }
    };
    (@ $base:ident, fs) => {
        $crate::forward_host!(@method $base, metadata(p: &::std::path::Path, follow: $crate::Follow) -> $crate::HostResult<$crate::Metadata>);
        $crate::forward_host!(@ $base, read);
        $crate::forward_host!(@ $base, read_with_origin);
        $crate::forward_host!(@ $base, read_chunks);
        $crate::forward_host!(@method $base, write(p: &::std::path::Path, data: &[u8], mode: $crate::WriteMode) -> $crate::HostResult<()>);
        $crate::forward_host!(@method $base, read_dir(p: &::std::path::Path) -> $crate::HostResult<Vec<$crate::DirEntry>>);
        $crate::forward_host!(@method $base, create_dir_all(p: &::std::path::Path) -> $crate::HostResult<()>);
        $crate::forward_host!(@method $base, remove_file(p: &::std::path::Path) -> $crate::HostResult<()>);
        $crate::forward_host!(@method $base, remove_dir(p: &::std::path::Path) -> $crate::HostResult<()>);
        $crate::forward_host!(@method $base, remove_dir_all(p: &::std::path::Path) -> $crate::HostResult<()>);
        $crate::forward_host!(@method $base, rename(from: &::std::path::Path, to: &::std::path::Path) -> $crate::HostResult<()>);
        $crate::forward_host!(@method $base, rename_noreplace(from: &::std::path::Path, to: &::std::path::Path) -> $crate::HostResult<()>);
        $crate::forward_host!(@method $base, lock_file(path: &::std::path::Path,
            timeout: Option<::core::time::Duration>,
        ) -> $crate::HostResult<Box<dyn $crate::FileLockGuard>>);
        $crate::forward_host!(@method $base, copy_file(from: &::std::path::Path, to: &::std::path::Path, opts: $crate::CopyOptions) -> $crate::HostResult<()>);
        $crate::forward_host!(@method $base, symlink(target: &::std::path::Path, link: &::std::path::Path) -> $crate::HostResult<()>);
        $crate::forward_host!(@method $base, hard_link(src: &::std::path::Path, dst: &::std::path::Path) -> $crate::HostResult<()>);
        $crate::forward_host!(@ $base, read_link);
        $crate::forward_host!(@method $base, canonicalize(p: &::std::path::Path) -> $crate::HostResult<::std::path::PathBuf>);
        $crate::forward_host!(@method $base, set_permissions(p: &::std::path::Path, mode: u32) -> $crate::HostResult<()>);
        $crate::forward_host!(@method $base, set_times(p: &::std::path::Path,
            atime: Option<$crate::FileTime>,
            mtime: Option<$crate::FileTime>,
        ) -> $crate::HostResult<()>);
        $crate::forward_host!(@method $base, set_symlink_times(path: &::std::path::Path,
            atime: Option<$crate::FileTime>,
            mtime: Option<$crate::FileTime>,
        ) -> $crate::HostResult<()>);
        $crate::forward_host!(@ $base, open_read);
        $crate::forward_host!(@ $base, scratch_file);
        $crate::forward_host!(@ $base, scratch_file_in);
        $crate::forward_host!(@method $base, write_from(path: &::std::path::Path,
            mode: $crate::WriteMode,
            input: &mut dyn ::std::io::Read,
            len: u64,
        ) -> $crate::HostResult<()>);
    };
    (@ $base:ident, env) => {
        $crate::forward_host!(@method $base, env_get(key: &[u8]) -> Option<$crate::BString>);
        $crate::forward_host!(@method $base, env_set(key: &[u8], value: Option<&[u8]>));
        $crate::forward_host!(@method $base, env_all() -> Vec<($crate::BString, $crate::BString)>);
    };
    (@ $base:ident, process) => {
        $crate::forward_host!(@ $base, supports_process_execution);
        $crate::forward_host!(@method $base, run(req: &$crate::RunRequest,
            on_output: &mut dyn FnMut($crate::Stream, &[u8]),
        ) -> $crate::HostResult<$crate::RunOutput>);
        $crate::forward_host!(@method $base, run_confined(req: &$crate::RunRequest,
            scope: $crate::WriteScope<'_>,
            on_output: &mut dyn FnMut($crate::Stream, &[u8]),
        ) -> $crate::HostResult<$crate::RunOutput>);
        $crate::forward_host!(@method $base, run_bounded_confined(req: &$crate::RunRequest,
            limit: usize,
            scope: $crate::WriteScope<'_>,
        ) -> $crate::HostResult<$crate::RunOutput>);
        $crate::forward_host!(@method $base, run_with_timeout_signal(req: &$crate::RunRequest,
            timeout: $crate::GracefulTimeout,
            on_output: &mut dyn FnMut($crate::Stream, &[u8]),
        ) -> $crate::HostResult<$crate::RunOutput>);
        $crate::forward_host!(@method $base, run_controlled(req: &$crate::RunRequest,
            graceful: Option<$crate::GracefulTimeout>,
            hard_limit: Option<::core::time::Duration>,
            update: &mut $crate::DeadlineUpdate<'_>,
        ) -> $crate::HostResult<$crate::ControlledOutput>);
        $crate::forward_host!(@method $base, run_bounded(req: &$crate::RunRequest, limit: usize) -> $crate::HostResult<$crate::RunOutput>);
    };
    (@ $base:ident, services) => {
        $crate::forward_host!(@method $base, registry_host_view() -> Option<$crate::RegistryView>);
        $crate::forward_host!(@method $base, registry_value(root: $crate::RegistryRoot,
            subkey: &[u8],
            name: Option<&[u8]>,
            view: $crate::RegistryView,
        ) -> $crate::HostResult<Option<$crate::BString>>);
        $crate::forward_host!(@method $base, write_out(stream: $crate::Stream, bytes: &[u8]));
        $crate::forward_host!(@method $base, read_stdin(limit: u64) -> $crate::HostResult<Vec<u8>>);
        $crate::forward_host!(@method $base, is_terminal(stream: $crate::Stream) -> bool);
        $crate::forward_host!(@method $base, terminal_width(stream: $crate::Stream) -> Option<usize>);
        $crate::forward_host!(@method $base, now() -> $crate::FileTime);
        $crate::forward_host!(@method $base, monotonic() -> ::core::time::Duration);
        $crate::forward_host!(@method $base, sleep(duration: ::core::time::Duration));
        $crate::forward_host!(@method $base, time_zone() -> $crate::TimeZone);
        $crate::forward_host!(@method $base, current_exe() -> $crate::HostResult<::std::path::PathBuf>);
        $crate::forward_host!(@method $base, initial_cwd() -> ::std::path::PathBuf);
        $crate::forward_host!(@method $base, system() -> $crate::SystemInfo);
    };
    (@ $base:ident, read) => {
        $crate::forward_host!(@method $base, read(p: &::std::path::Path, range: $crate::ReadRange) -> $crate::HostResult<Vec<u8>>);
    };
    (@ $base:ident, read_with_origin) => {
        $crate::forward_host!(@method $base, read_with_origin(p: &::std::path::Path, range: $crate::ReadRange, origin: &::std::path::Path) -> $crate::HostResult<Vec<u8>>);
    };
    (@ $base:ident, read_chunks) => {
        $crate::forward_host!(@method $base, read_chunks(p: &::std::path::Path, consume: &mut dyn FnMut(&[u8])) -> $crate::HostResult<()>);
    };
    (@ $base:ident, read_link) => {
        $crate::forward_host!(@method $base, read_link(p: &::std::path::Path) -> $crate::HostResult<::std::path::PathBuf>);
    };
    (@ $base:ident, open_read) => {
        $crate::forward_host!(@method $base, open_read(path: &::std::path::Path) -> $crate::HostResult<Box<dyn $crate::ReadSeek + '_>>);
    };
    (@ $base:ident, scratch_file) => {
        $crate::forward_host!(@method $base, scratch_file() -> $crate::HostResult<Box<dyn $crate::ScratchFile + '_>>);
    };
    (@ $base:ident, scratch_file_in) => {
        $crate::forward_host!(@method $base, scratch_file_in(dir: &::std::path::Path) -> $crate::HostResult<Box<dyn $crate::ScratchFile + '_>>);
    };
    (@ $base:ident, supports_process_execution) => {
        $crate::forward_host!(@method $base, supports_process_execution() -> bool);
    };
}
