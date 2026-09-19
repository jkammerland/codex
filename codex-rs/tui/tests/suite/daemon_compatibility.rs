//! Exercises daemon compatibility fallback through real TUI startup.

use super::focus_palette::PtyCodex;
use super::focus_palette::test_codex_home;
use super::focus_palette::write_test_config;
use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::JSONRPCMessage;
use futures::SinkExt;
use futures::StreamExt;
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tokio::net::UnixListener;
use tokio_tungstenite::tungstenite::Message;

fn install_complete_daemon_package(home: &Path) -> Result<()> {
    let package = home.join("packages/app-server-daemon/releases/test");
    for directory in ["bin", "codex-path", "codex-resources"] {
        std::fs::create_dir_all(package.join(directory))?;
    }
    let codex = codex_utils_cargo_bin::cargo_bin("codex")?;
    let code_mode_host = codex_utils_cargo_bin::cargo_bin("codex-code-mode-host")?;
    let rg = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join("rg"))
        .find(|path| path.is_file())
        .context("find rg for complete daemon package fixture")?;
    let bwrap = [Path::new("/usr/bin/bwrap"), Path::new("/bin/bwrap")]
        .into_iter()
        .find(|path| path.is_file())
        .context("find bwrap for complete daemon package fixture")?
        .to_path_buf();
    for (source, relative) in [
        (&codex, "bin/codex"),
        (&code_mode_host, "bin/codex-code-mode-host"),
        (&rg, "codex-path/rg"),
        (&bwrap, "codex-resources/bwrap"),
    ] {
        std::fs::copy(source, package.join(relative))?;
        std::fs::set_permissions(
            package.join(relative),
            std::fs::Permissions::from_mode(0o755),
        )?;
    }
    let target = if cfg!(target_os = "macos") {
        format!("{}-apple-darwin", std::env::consts::ARCH)
    } else if cfg!(target_env = "gnu") {
        format!("{}-unknown-linux-gnu", std::env::consts::ARCH)
    } else {
        format!("{}-unknown-linux-musl", std::env::consts::ARCH)
    };
    std::fs::write(
        package.join("codex-package.json"),
        serde_json::to_vec(&json!({
            "layoutVersion": 1,
            "version": codex_tui::CODEX_CLI_VERSION,
            "target": target,
            "variant": "codex",
            "entrypoint": "bin/codex",
            "resourcesDir": "codex-resources",
            "pathDir": "codex-path",
        }))?,
    )?;
    std::os::unix::fs::symlink(
        "releases/test",
        home.join("packages/app-server-daemon/current"),
    )?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incompatible_daemon_falls_back_for_default_and_explicit_features() -> Result<()> {
    for scenario in ["default", "explicit", "host policy"] {
        let cwd = codex_utils_cargo_bin::repo_root()?;
        let home = test_codex_home()?;
        install_complete_daemon_package(home.path())?;
        write_test_config(home.path(), &cwd)?;
        let config_path = home.path().join("config.toml");
        let config = std::fs::read_to_string(&config_path)?;
        std::fs::write(
            config_path,
            format!("features.daemon_auto_start = true\n{config}"),
        )?;
        if scenario == "host policy" {
            let path = home.path().join("config.toml");
            let contents = std::fs::read_to_string(&path)?;
            std::fs::write(
                path,
                format!(
                    "features.code_mode_host = {{enabled=false, disable_in_process_fallback=true}}\n{contents}"
                ),
            )?;
        }
        let socket = codex_app_server_client::app_server_control_socket_path(home.path())?;
        std::fs::create_dir_all(socket.parent().unwrap())?;
        let listener = UnixListener::bind(socket.as_path())?;
        let server = tokio::spawn(async move {
            let mut feature_list_seen = false;
            loop {
                let (stream, _) = listener.accept().await?;
                let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
                    continue;
                };
                while let Some(Ok(Message::Text(text))) = socket.next().await {
                    let JSONRPCMessage::Request(request) = serde_json::from_str(&text)? else {
                        continue;
                    };
                    let response = match request.method.as_str() {
                        "initialize" => json!({"id": request.id, "result": {
                            "userAgent": format!("daemon-test/{}", codex_tui::CODEX_CLI_VERSION),
                            "codexHome": "/fake/codex-home",
                            "platformFamily": std::env::consts::FAMILY,
                            "platformOs": std::env::consts::OS,
                        }}),
                        "experimentalFeature/list" => {
                            feature_list_seen = true;
                            json!({"id": request.id, "result": {"data": [{
                                "name": "api_key_model_discovery", "stage": "underDevelopment",
                                "displayName": null, "description": null, "announcement": null,
                                "enabled": scenario == "default", "defaultEnabled": false,
                            }], "nextCursor": null}})
                        }
                        method => anyhow::bail!("unexpected daemon request {method}"),
                    };
                    socket
                        .send(Message::Text(response.to_string().into()))
                        .await?;
                }
                if feature_list_seen {
                    return Ok::<_, anyhow::Error>(());
                }
            }
        });
        let args = if scenario == "explicit" {
            vec!["-c", "features.api_key_model_discovery=true"]
        } else {
            vec![]
        };
        let mut terminal = PtyCodex::start(&cwd, home, &args)?;
        terminal.wait_for_startup()?;
        terminal.wait_for_screen("warning")?;
        terminal.write_input(b"\x14")?;
        let (snapshot, warning_end) = match scenario {
            "default" => (
                "daemon_feature_mismatch",
                "api_key_model_discovery to be disabled.",
            ),
            "explicit" => (
                "daemon_override_mismatch",
                "api_key_model_discovery to be enabled.",
            ),
            "host policy" => ("daemon_host_policy_mismatch", "requires embedded mode."),
            _ => unreachable!(),
        };
        terminal.wait_for_screen(warning_end)?;
        let screen = terminal.screen_contents();
        let warning = screen
            .lines()
            .find(|line| line.contains("Running without the shared background server"))
            .context("missing warning line")?;
        insta::assert_snapshot!(snapshot, warning);
        terminal.write_input(b"\x14")?;
        terminal.write_input(b"/status")?;
        terminal.wait_for_screen("show current session configuration")?;
        terminal.read_output(std::time::Duration::from_millis(/*millis*/ 200))?;
        terminal.write_input(b"\r")?;
        terminal.wait_for_screen("Model:")?;
        ensure!(!terminal.screen_contains("unix://"));
        if scenario == "host policy" {
            server.abort();
            continue;
        }
        tokio::time::timeout(std::time::Duration::from_secs(/*secs*/ 5), server).await???;
    }
    Ok(())
}
