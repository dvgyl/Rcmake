#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub(super) use unix::run_pipeline;
#[cfg(windows)]
pub(super) use windows::run_pipeline;
