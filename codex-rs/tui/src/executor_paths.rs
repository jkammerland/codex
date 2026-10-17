//! Executor path interpretation at the boundary between server state and local UI config.

use crate::app_server_session::ThreadParamsMode;
use crate::legacy_core::config::Config;
use codex_app_server_protocol::NetworkAccess;
use codex_app_server_protocol::SandboxPolicy;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::LegacyAppPathString;
use codex_utils_path_uri::PathUri;

/// Builds display permissions without interpreting another executor's roots as local paths.
pub(crate) fn display_permission_profile_from_app_server(
    sandbox: &SandboxPolicy,
    cwd: &LegacyAppPathString,
    mode: ThreadParamsMode,
) -> Result<PermissionProfile, String> {
    // Local sessions retain the legacy symbolic-root and native temporary-directory projection.
    // A remote POSIX path must still use the URI branch because its filesystem is foreign.
    if mode == ThreadParamsMode::Embedded {
        let cwd = AbsolutePathBuf::try_from(cwd.clone()).map_err(|error| error.to_string())?;
        let sandbox = sandbox.to_core().map_err(|error| error.to_string())?;
        return Ok(PermissionProfile::from_legacy_sandbox_policy_for_cwd(
            &sandbox,
            cwd.as_path(),
        ));
    }
    let network = |enabled| {
        if enabled {
            NetworkSandboxPolicy::Enabled
        } else {
            NetworkSandboxPolicy::Restricted
        }
    };
    Ok(match sandbox {
        SandboxPolicy::DangerFullAccess => PermissionProfile::Disabled,
        SandboxPolicy::ReadOnly { network_access } => PermissionProfile::from_runtime_permissions(
            &FileSystemSandboxPolicy::read_only(),
            network(*network_access),
        ),
        SandboxPolicy::ExternalSandbox { network_access } => PermissionProfile::External {
            network: network(*network_access == NetworkAccess::Enabled),
        },
        SandboxPolicy::WorkspaceWrite {
            writable_roots,
            network_access,
            exclude_tmpdir_env_var,
            exclude_slash_tmp,
        } => {
            let cwd = PathUri::try_from(cwd.clone()).map_err(|err| err.to_string())?;
            let roots = writable_roots
                .iter()
                .cloned()
                .map(PathUri::try_from)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|err| err.to_string())?;
            PermissionProfile::workspace_write_with_path_uris(
                &roots,
                network(*network_access),
                *exclude_tmpdir_env_var,
                *exclude_slash_tmp,
            )
            .materialize_project_roots_with_path_uris(&[cwd])
        }
    })
}

/// Copies paths only after the caller establishes that the session uses the local filesystem.
/// Conversion is atomic so a malformed root cannot leave the native config partly updated.
pub(crate) fn sync_local_config_paths(
    config: &mut Config,
    cwd: &LegacyAppPathString,
    workspace_roots: &[LegacyAppPathString],
) -> Result<(), String> {
    let cwd = AbsolutePathBuf::try_from(cwd.clone()).map_err(|err| err.to_string())?;
    let roots = workspace_roots
        .iter()
        .cloned()
        .map(AbsolutePathBuf::try_from)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    config.cwd = cwd;
    config.workspace_roots = roots.clone();
    config.permissions.set_workspace_roots(roots);
    Ok(())
}

#[cfg(test)]
#[path = "executor_paths_tests.rs"]
mod tests;
