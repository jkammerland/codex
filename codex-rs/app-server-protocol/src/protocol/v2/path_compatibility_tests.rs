use super::*;
use crate::ServerNotification;
use codex_utils_absolute_path::test_support::PathBufExt;
use codex_utils_absolute_path::test_support::test_path_buf;
use pretty_assertions::assert_eq;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use serde_json::json;
use std::io;

fn thread_json(cwd: &str) -> Value {
    json!({
        "id": "thread-foreign", "sessionId": "session-foreign", "preview": "",
        "ephemeral": false, "modelProvider": "mock", "createdAt": 0, "updatedAt": 0,
        "status": {"type": "idle"}, "cwd": cwd, "cliVersion": "test",
        "source": "cli", "turns": []
    })
}

fn workspace_policy(roots: &[&str]) -> Value {
    json!({"type": "workspaceWrite", "writableRoots": roots,
        "networkAccess": false, "excludeTmpdirEnvVar": true, "excludeSlashTmp": true})
}

#[test]
fn thread_metadata_preserves_foreign_path_spelling() {
    for cwd in [r"C:\Pilot\work", r"\\server\share\work", "/pilot/work"] {
        let wire = thread_json(cwd);
        let read: ThreadReadResponse = serde_json::from_value(json!({"thread": wire.clone()}))
            .expect("read response keeps the app-server path");
        let listed: ThreadListResponse = serde_json::from_value(json!({"data": [wire.clone()]}))
            .expect("list response keeps the app-server path");
        assert_eq!(listed.data, vec![read.thread.clone()]);
        assert_eq!(serde_json::to_value(&read.thread).unwrap()["cwd"], cwd);

        let started: ServerNotification = serde_json::from_value(json!({
            "method": "thread/started", "params": {"thread": wire}
        }))
        .expect("foreign thread notification must remain decodable");
        assert_eq!(
            serde_json::to_value(started).unwrap()["params"]["thread"]["cwd"],
            cwd
        );
    }
}

fn assert_lifecycle_paths<T: DeserializeOwned + Serialize>(wire: &Value) {
    let response: T = serde_json::from_value(wire.clone()).expect("portable lifecycle response");
    let returned = serde_json::to_value(response).unwrap();
    assert_eq!(returned["cwd"], wire["cwd"]);
    assert_eq!(returned["thread"]["cwd"], wire["thread"]["cwd"]);
    assert_eq!(
        returned["runtimeWorkspaceRoots"],
        wire["runtimeWorkspaceRoots"]
    );
    assert_eq!(
        returned["sandbox"]["writableRoots"],
        wire["sandbox"]["writableRoots"]
    );
}

#[test]
fn lifecycle_and_settings_preserve_windows_and_unc_roots() {
    let cwd = r"C:\Pilot\work";
    let roots = [cwd, r"\\server\share\work"];
    let policy = workspace_policy(&roots);
    let wire = json!({
        "thread": thread_json(cwd), "model": "mock", "modelProvider": "mock",
        "cwd": cwd, "runtimeWorkspaceRoots": roots, "approvalPolicy": "never",
        "approvalsReviewer": "user", "sandbox": policy
    });
    assert_lifecycle_paths::<ThreadStartResponse>(&wire);
    assert_lifecycle_paths::<ThreadResumeResponse>(&wire);
    assert_lifecycle_paths::<ThreadForkResponse>(&wire);

    let settings = json!({
        "cwd": cwd, "approvalPolicy": "never", "approvalsReviewer": "user",
        "sandboxPolicy": policy, "model": "mock", "modelProvider": "mock",
        "collaborationMode": {"mode": "default", "settings": {"model": "mock"}}
    });
    let notification: ServerNotification = serde_json::from_value(json!({
        "method": "thread/settings/updated",
        "params": {"threadId": "thread-foreign", "threadSettings": settings}
    }))
    .expect("foreign settings notification must remain decodable");
    let returned = serde_json::to_value(notification).unwrap();
    assert_eq!(returned["params"]["threadSettings"]["cwd"], cwd);
    assert_eq!(
        returned["params"]["threadSettings"]["sandboxPolicy"]["writableRoots"],
        json!(roots)
    );
}

#[test]
fn native_sandbox_execution_rejects_foreign_and_relative_roots() {
    let foreign = if cfg!(windows) {
        "/pilot/work"
    } else {
        r"C:\Pilot\work"
    };
    for root in [foreign, "relative/work"] {
        let policy: SandboxPolicy = serde_json::from_value(workspace_policy(&[root]))
            .expect("the wire boundary preserves foreign path text");
        assert_eq!(
            policy
                .to_core()
                .expect_err("native execution rejects the root")
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    let native = test_path_buf("/pilot/work").abs();
    let policy: SandboxPolicy =
        serde_json::from_value(workspace_policy(&[native.as_path().to_str().unwrap()]))
            .expect("native policy JSON");
    assert_eq!(
        policy.to_core().expect("valid native execution policy"),
        codex_protocol::protocol::SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![native],
            network_access: false,
            exclude_tmpdir_env_var: true,
            exclude_slash_tmp: true
        }
    );
}
