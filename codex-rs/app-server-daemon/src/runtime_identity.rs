use std::path::Path;

use anyhow::Result;
use anyhow::bail;

use crate::client::ProbeInfo;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DaemonOwnership {
    ManagedExisting,
    Unmanaged,
    StartedByCurrentInvocation,
}

pub(crate) fn validate_probe(info: &ProbeInfo, ownership: DaemonOwnership) -> Result<()> {
    if info.app_server_version == codex_build_info::CODEX_CLI_VERSION {
        return Ok(());
    }

    let recovery = match ownership {
        DaemonOwnership::ManagedExisting => {
            "Finish daemon-backed jobs, then run:\n  codex app-server daemon restart"
        }
        DaemonOwnership::Unmanaged => {
            "Finish daemon-backed jobs, then stop the process that owns the local app-server socket."
        }
        DaemonOwnership::StartedByCurrentInvocation => {
            "The incompatible process started by this command was stopped. Replace the managed Codex executable with this fork before retrying."
        }
    };
    bail!(
        "app-server version {} does not match this Codex runtime {}\n\n\
         The incompatible daemon was not reused. {recovery}",
        info.app_server_version,
        codex_build_info::CODEX_CLI_VERSION,
    )
}

pub(crate) fn validate_managed_version(path: &Path, managed_version: &str) -> Result<()> {
    if managed_version == codex_build_info::CODEX_CLI_VERSION {
        return Ok(());
    }
    bail!(
        "managed Codex executable {} reports version {managed_version}, but this runtime is {}\n\n\
         The running daemon was left untouched. Install the matching maintained fork before retrying.",
        path.display(),
        codex_build_info::CODEX_CLI_VERSION,
    )
}

#[cfg(test)]
#[path = "runtime_identity_tests.rs"]
mod tests;
