//! Exercises daemon compatibility recovery through real TUI startup.

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
    let mut files = vec![
        (codex, "bin/codex"),
        (code_mode_host, "bin/codex-code-mode-host"),
        (rg, "codex-path/rg"),
    ];
    #[cfg(target_os = "linux")]
    {
        let bwrap = [Path::new("/usr/bin/bwrap"), Path::new("/bin/bwrap")]
            .into_iter()
            .find(|path| path.is_file())
            .context("find bwrap for complete daemon package fixture")?
            .to_path_buf();
        files.push((bwrap, "codex-resources/bwrap"));
    }
    for (source, relative) in files {
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
async fn incompatible_daemon_can_run_embedded_for_explicit_features() -> Result<()> {
    for scenario in ["explicit enable", "explicit disable"] {
        let cwd = codex_utils_cargo_bin::repo_root()?;
        let home = test_codex_home()?;
        install_complete_daemon_package(home.path())?;
        write_test_config(home.path(), &cwd)?;
        let config_path = home.path().join("config.toml");
        let config = std::fs::read_to_string(&config_path)?;
        std::fs::write(
            config_path,
            config.replace(
                "features.daemon_auto_start = false",
                "features.daemon_auto_start = true",
            ),
        )?;
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
                        "config/read" => {
                            json!({"id": request.id, "result": {"config": {}, "layers": []}})
                        }
                        "configRequirements/read" => {
                            json!({"id": request.id, "result": {"requirements": null}})
                        }
                        "experimentalFeature/list" => {
                            feature_list_seen = true;
                            json!({"id": request.id, "result": {"data": [{
                                "name": "api_key_model_discovery", "stage": "stable",
                                "displayName": null, "description": null, "announcement": null,
                                "enabled": scenario == "explicit disable", "defaultEnabled": true,
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
        let args = if scenario == "explicit disable" {
            vec!["-c", "features.api_key_model_discovery=false"]
        } else {
            vec!["-c", "features.api_key_model_discovery=true"]
        };
        let mut terminal = PtyCodex::start(&cwd, home, &args)?;
        terminal.wait_for_screen("Background server has incompatible feature settings")?;
        terminal.write_input(b"1")?;
        terminal.wait_for_startup()?;
        terminal.wait_for_screen("Background server has incompatible feature settings")?;
        let (snapshot, warning_end) = match scenario {
            "explicit enable" => (
                "daemon_feature_mismatch",
                "api_key_model_discovery to be enabled",
            ),
            "explicit disable" => (
                "daemon_override_mismatch",
                "api_key_model_discovery to be disabled",
            ),
            _ => unreachable!(),
        };
        terminal.wait_for_screen(warning_end)?;
        let screen = terminal.screen_contents();
        let warning = screen
            .lines()
            .find(|line| line.contains(warning_end))
            .context("missing warning line")?;
        insta::assert_snapshot!(snapshot, warning);
        terminal.write_input(b"1")?;
        terminal.wait_for_screen("GPT-5.6-Terra")?;
        terminal.write_input(b"/status")?;
        terminal.wait_for_screen("show current session configuration")?;
        terminal.read_output(std::time::Duration::from_millis(/*millis*/ 200))?;
        terminal.write_input(b"\r")?;
        terminal.wait_for_screen("Model:")?;
        ensure!(!terminal.screen_contains("unix://"));
        tokio::time::timeout(std::time::Duration::from_secs(/*secs*/ 5), server).await???;
    }
    Ok(())
}
