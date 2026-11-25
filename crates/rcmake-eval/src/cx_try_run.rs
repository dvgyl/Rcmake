use std::path::Path;

use rcmake_host::{Follow, WriteMode};

use crate::{CmdError, Cx};

impl Cx<'_> {
    /// Write a cross-compiled probe result block. Replace older results on the first write.
    pub fn write_try_run_result(&mut self, path: &Path, block: &[u8]) -> Result<(), CmdError> {
        let started = self.interp.try_run_results_started;
        let previous = if started {
            self.host()
                .metadata(path, Follow::Yes)
                .map_err(|error| CmdError::status(error.to_string()))?
                .len
        } else {
            0
        };
        if previous.saturating_add(block.len() as u64) > self.limits().max_file_read {
            return Err(CmdError::status(
                "TryRunResults.cmake exceeds the size limit.",
            ));
        }
        let mode = if started {
            WriteMode::Append
        } else {
            WriteMode::Truncate
        };
        self.host()
            .write(path, block, mode)
            .map_err(|error| CmdError::status(error.to_string()))?;
        self.interp.try_run_results_started = true;
        Ok(())
    }
}
