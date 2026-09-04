use std::path::Path;

use pretty_assertions::assert_eq;

use super::DaemonOwnership;
use super::validate_managed_version;
use super::validate_probe;
use crate::client::ProbeInfo;

#[test]
fn matching_runtime_identity_is_accepted() {
    let info = ProbeInfo {
        app_server_version: codex_build_info::CODEX_CLI_VERSION.to_string(),
    };

    assert_eq!(
        [
            DaemonOwnership::ManagedExisting,
            DaemonOwnership::Unmanaged,
            DaemonOwnership::StartedByCurrentInvocation,
        ]
        .map(|ownership| validate_probe(&info, ownership).is_ok()),
        [true, true, true]
    );
    assert!(validate_managed_version(Path::new("codex"), &info.app_server_version).is_ok());
}

#[test]
fn incompatible_runtime_reports_owner_specific_recovery() {
    let info = ProbeInfo {
        app_server_version: "0.151.0".to_string(),
    };

    let errors = [
        DaemonOwnership::ManagedExisting,
        DaemonOwnership::Unmanaged,
        DaemonOwnership::StartedByCurrentInvocation,
    ]
    .map(|ownership| {
        validate_probe(&info, ownership)
            .expect_err("incompatible runtime should be rejected")
            .to_string()
    });

    assert!(errors[0].contains("codex app-server daemon restart"));
    assert!(errors[1].contains("stop the process that owns"));
    assert!(errors[2].contains("process started by this command was stopped"));
}

#[test]
fn incompatible_managed_binary_is_rejected_before_restart() {
    let error = validate_managed_version(Path::new("managed/codex"), "0.151.0")
        .expect_err("upstream managed binary should be rejected");

    assert!(
        error
            .to_string()
            .contains("managed/codex reports version 0.151.0")
    );
    assert!(
        error
            .to_string()
            .contains("running daemon was left untouched")
    );
}
