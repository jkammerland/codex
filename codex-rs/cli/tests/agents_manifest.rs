//! Exercise configured-machine startup and shutdown through the real terminal loop.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::ensure;
use codex_team::Endpoint;
use codex_team::Machine;
use codex_team::Manifest;
use codex_team::Server;
use codex_utils_pty::ProcessHandle;
use futures::StreamExt as _;
use tokio::io::AsyncBufReadExt as _;
use tokio::io::BufReader;
use tokio::net::TcpListener;
use tokio::process::Child;
use tokio::sync::Mutex;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;
use wiremock::MockServer;

const FAILURE_GUARD: Duration = Duration::from_secs(/*secs*/ 45);

// Match terminal queries incrementally because PTY reads may split any escape.
#[derive(Default)]
struct TerminalQueries(Vec<u8>);

impl TerminalQueries {
    fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        let queries: &[(&[u8], &[u8])] = &[
            (b"\x1b[6n", b"\x1b[1;1R"),
            (b"\x1b[c", b"\x1b[?1;2c"),
            (b"\x1b[?u", b"\x1b[?0u\x1b[?1;2c"),
            (b"\x1b]10;?\x1b\\", b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\"),
            (b"\x1b]11;?\x1b\\", b"\x1b]11;rgb:0000/0000/0000\x1b\\"),
            (b"\x1b]10;?\x07", b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\"),
            (b"\x1b]11;?\x07", b"\x1b]11;rgb:0000/0000/0000\x1b\\"),
        ];
        let mut replies = Vec::new();
        for &byte in bytes {
            self.0.push(byte);
            if let Some((_, reply)) = queries.iter().find(|(query, _)| *query == self.0) {
                replies.extend_from_slice(reply);
                self.0.clear();
            } else {
                while !self.0.is_empty()
                    && !queries.iter().any(|(query, _)| query.starts_with(&self.0))
                {
                    self.0.remove(0);
                }
            }
        }
        replies
    }
}

struct Terminal {
    session: ProcessHandle,
    output: tokio::sync::mpsc::Receiver<Vec<u8>>,
    exit: tokio::sync::oneshot::Receiver<i32>,
    parser: vt100::Parser,
    queries: TerminalQueries,
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.session.terminate();
    }
}

impl Terminal {
    async fn start(program: &Path, cwd: &Path, env: &HashMap<String, String>) -> Result<Self> {
        let process = codex_utils_pty::spawn_pty_process(
            &program.to_string_lossy(),
            &["agents".into()],
            cwd,
            env,
            /*arg0*/ &None,
            codex_utils_pty::TerminalSize {
                rows: 40,
                cols: 120,
            },
            codex_utils_pty::ChildFds::Inherited(&[]),
        )
        .await?;
        Ok(Self {
            session: process.session,
            output: process.stdout_rx,
            exit: process.exit_rx,
            parser: vt100::Parser::new(
                /*rows*/ 40, /*cols*/ 120, /*scrollback_len*/ 0,
            ),
            queries: TerminalQueries::default(),
        })
    }

    async fn wait_for(&mut self, expected: &[&str]) -> Result<()> {
        self.wait_for_screen(expected, &[]).await
    }

    async fn wait_for_screen(&mut self, expected: &[&str], absent: &[&str]) -> Result<()> {
        let result = tokio::time::timeout(FAILURE_GUARD, async {
            loop {
                let screen = self.parser.screen().contents();
                if expected.iter().all(|text| screen.contains(text))
                    && absent.iter().all(|text| !screen.contains(text))
                {
                    return Ok(());
                }
                let bytes = self
                    .output
                    .recv()
                    .await
                    .context("terminal closed before the expected screen")?;
                self.parser.process(&bytes);
                let replies = self.queries.feed(&bytes);
                if !replies.is_empty() {
                    self.session.writer_sender().send(replies).await?;
                }
            }
        })
        .await;
        result.with_context(|| {
            format!(
                "expected {expected:?}; final screen:\n{}",
                self.parser.screen().contents()
            )
        })?
    }

    async fn exit(&mut self) -> Result<()> {
        self.session.writer_sender().send(b"\x03".to_vec()).await?;
        // Detect a blocked shutdown without imposing a deadline on product connections.
        let code = tokio::time::timeout(Duration::from_secs(/*secs*/ 10), async {
            let mut output_open = true;
            loop {
                tokio::select! {
                    biased;
                    status = &mut self.exit => return Ok::<_, anyhow::Error>(status?),
                    bytes = self.output.recv(), if output_open => {
                        let Some(bytes) = bytes else { output_open = false; continue; };
                        self.parser.process(&bytes);
                        let replies = self.queries.feed(&bytes);
                        if !replies.is_empty() { self.session.writer_sender().send(replies).await?; }
                    }
                }
            }
        }).await
            .with_context(|| format!("Ctrl+C did not exit; final screen:\n{}", self.parser.screen().contents()))??;
        ensure!(
            code == 0,
            "Ctrl+C exited with status {code}; final screen:\n{}",
            self.parser.screen().contents()
        );
        Ok(())
    }
}

struct HeldWorker {
    url: String,
    initialized: watch::Receiver<Vec<String>>,
    methods: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for HeldWorker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl HeldWorker {
    async fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("ws://{}", listener.local_addr()?);
        let (initialized, notifications) = watch::channel(Vec::new());
        let methods = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&methods);
        let task = tokio::spawn(async move {
            let mut clients = tokio::task::JoinSet::new();
            while let Ok((stream, _)) = listener.accept().await {
                let initialized = initialized.clone();
                let methods = Arc::clone(&recorded);
                clients.spawn(async move {
                    let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
                        return;
                    };
                    while let Some(Ok(Message::Text(text))) = socket.next().await {
                        let Ok(request) = serde_json::from_str::<serde_json::Value>(&text) else {
                            continue;
                        };
                        let Some(method) = request["method"].as_str() else {
                            continue;
                        };
                        methods.lock().await.push(method.to_owned());
                        if method == "initialize" {
                            // Keep the actual socket and its original initialize
                            // alive until test cleanup; never release the request.
                            let client = request["params"]["clientInfo"]["name"]
                                .as_str()
                                .unwrap_or("unknown")
                                .to_owned();
                            initialized.send_modify(|clients| clients.push(client));
                        }
                    }
                });
            }
        });
        Ok(Self {
            url,
            initialized: notifications,
            methods,
            task,
        })
    }

    async fn wait_for_initialization(&mut self, client: &str) -> Result<()> {
        tokio::time::timeout(FAILURE_GUARD, async {
            loop {
                let initialized = self
                    .initialized
                    .borrow_and_update()
                    .iter()
                    .any(|name| name == client);
                if initialized {
                    break;
                }
                self.initialized.changed().await?;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .with_context(|| format!("worker did not receive {client} initialize"))?
    }
}

async fn start_coordinator(
    program: &Path,
    manifest: &Path,
    home: &Path,
    cwd: &Path,
) -> Result<Child> {
    let mut child = tokio::process::Command::new(program)
        .args(["agents", "serve", "--manifest"])
        .arg(manifest)
        .current_dir(cwd)
        .envs(isolated_env(home))
        .env("CODEX_AGENTS_READY", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut readiness = BufReader::new(child.stdout.take().context("coordinator stdout")?).lines();
    tokio::time::timeout(FAILURE_GUARD, async {
        while let Some(line) = readiness.next_line().await? {
            if line == "CODEX_AGENTS_READY" {
                return Ok(());
            }
        }
        anyhow::bail!("owned foreground coordinator exited before readiness")
    })
    .await
    .context("coordinator readiness timed out")??;
    Ok(child)
}

fn isolated_env(home: &Path) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = std::env::vars().collect();
    for key in [
        "CODEX_EXEC_SERVER_URL",
        "CODEX_ACCESS_TOKEN",
        "OPENAI_API_KEY",
        "CODEX_API_KEY",
        "TMUX",
        "TMUX_PANE",
        "ZELLIJ",
        "ZELLIJ_SESSION_NAME",
        "ZELLIJ_VERSION",
    ] {
        env.remove(key);
    }
    env.insert("CODEX_HOME".into(), home.display().to_string());
    env.insert("CODEX_SQLITE_HOME".into(), home.display().to_string());
    env.insert("NO_PROXY".into(), "127.0.0.1,localhost".into());
    env.insert("no_proxy".into(), "127.0.0.1,localhost".into());
    env.insert("TERM".into(), "xterm-256color".into());
    env.insert("TERM_PROGRAM".into(), "unrecognized-terminal".into());
    env
}

async fn stop_unexpected_daemon(program: &Path, home: &Path) -> Result<()> {
    if home.join("app-server-daemon/daemon.pid").exists()
        || home.join("app-server-daemon/app-server.pid").exists()
    {
        // A failing regression must not leave an unintended daemon behind.
        tokio::time::timeout(
            FAILURE_GUARD,
            tokio::process::Command::new(program)
                .args(["app-server", "stop"])
                .envs(isolated_env(home))
                .kill_on_drop(true)
                .output(),
        )
        .await??;
    }
    Ok(())
}

fn write_config(
    home: &Path,
    workspace: &Path,
    model: &MockServer,
    manifest: Option<&Path>,
) -> Result<()> {
    let manifest = manifest
        .map(|path| serde_json::to_string(path).map(|path| format!("agents_manifest = {path}\n")))
        .transpose()?
        .unwrap_or_default();
    std::fs::write(
        home.join("config.toml"),
        format!(
            r#"{manifest}
model = "test-model"
model_provider = "local"
analytics.enabled = false
check_for_update_on_startup = false
cli_auth_credentials_store = "file"
features.daemon_auto_start = true
features.auth_elicitation = false
features.api_key_model_discovery = false
tui.disable_paste_burst = true
[otel]
metrics_exporter = "none"
[model_providers.local]
name = "local test"
base_url = "{}/v1"
wire_api = "responses"
[projects.{}]
trust_level = "trusted"
"#,
            model.uri(),
            serde_json::to_string(workspace)?
        ),
    )?;
    Ok(())
}

async fn unavailable_manifest(held: bool) -> Result<()> {
    let root = tempfile::tempdir()?;
    let home = root.path().join("home");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&home)?;
    std::fs::create_dir(&workspace)?;
    let home = home.canonicalize()?;
    let workspace = workspace.canonicalize()?;
    let model = MockServer::start().await;
    let mut worker = if held {
        Some(HeldWorker::start().await?)
    } else {
        None
    };
    // A bound, non-listening socket reserves the offline port without ever
    // accepting a connection or racing another test's ephemeral listener.
    let offline = if held {
        None
    } else {
        let socket = tokio::net::TcpSocket::new_v4()?;
        socket.bind("127.0.0.1:0".parse()?)?;
        Some(socket)
    };
    let endpoint = match &worker {
        Some(worker) => worker.url.clone(),
        None => format!(
            "ws://{}",
            offline
                .as_ref()
                .context("reserved offline endpoint")?
                .local_addr()?
        ),
    };
    let reserved = std::net::TcpListener::bind("127.0.0.1:0")?;
    let coordinator_address = reserved.local_addr()?;
    drop(reserved);
    let manifest_path = root.path().join("agents.toml");
    let label = if held {
        "Initializing Bench"
    } else {
        "Offline Bench"
    };
    let manifest = Manifest {
        version: 1,
        coordinator: serde_json::from_value(
            serde_json::json!({ "listen": coordinator_address, "url": format!("http://{coordinator_address}"), "token_file": root.path().join("operator.token"), "state_dir": root.path().join("coordinator") }),
        )?,
        machines: vec![Machine {
            id: "bench".into(),
            label: label.into(),
            servers: vec![Server {
                environment_capabilities: Default::default(),
                id: "worker".into(),
                label: "Saved worker".into(),
                endpoint: Endpoint::Websocket {
                    url: endpoint,
                    token_file: None,
                },
                cancellation_adapters: Vec::new(),
            }],
        }],
        gitea: None,
    };
    std::fs::write(&manifest_path, toml::to_string(&manifest)?)?;
    write_config(&home, &workspace, &model, Some(&manifest_path))?;
    let program = codex_utils_cargo_bin::cargo_bin("codex")?;
    let mut coordinator = start_coordinator(&program, &manifest_path, &home, &workspace).await?;
    let result = async {
        if let Some(worker) = &mut worker {
            worker.wait_for_initialization("codex-team").await?;
        }
        let mut terminal = Terminal::start(&program, &workspace, &isolated_env(&home)).await?;
        terminal
            .wait_for(&["Agent command center", label, "Saved worker"])
            .await?;
        if let Some(worker) = &mut worker {
            // Observe the UI's own client identity, so a coordinator reconnect
            // cannot stand in for the held interactive discovery request.
            worker.wait_for_initialization("codex-tui").await?;
        }
        terminal.exit().await?;
        ensure!(
            !home.join("app-server-daemon/daemon.pid").exists()
                && !home.join("app-server-daemon/app-server.pid").exists(),
            "manifest browsing started an extra daemon"
        );
        ensure!(
            !home.join("packages").exists(),
            "manifest browsing installed an extra daemon"
        );
        ensure!(
            !home.join("sessions").exists()
                || std::fs::read_dir(home.join("sessions"))?.next().is_none(),
            "browsing created an ordinary local session"
        );
        ensure!(
            model
                .received_requests()
                .await
                .context("model requests")?
                .is_empty(),
            "browsing sent a model request"
        );
        if let Some(worker) = &worker {
            ensure!(
                worker
                    .methods
                    .lock()
                    .await
                    .iter()
                    .all(|method| method == "initialize"),
                "uninitialized worker received a model or thread mutation"
            );
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    // The fixture owns the foreground coordinator, including all of its workers.
    // This runs on assertion failure as well as success; no autostart child remains.
    coordinator.kill().await?;
    coordinator.wait().await?;
    stop_unexpected_daemon(&program, &home).await?;
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_offline_workers_open_agents_and_exit_without_a_daemon_or_model_request()
-> Result<()> {
    unavailable_manifest(/*held*/ false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_initialize_held_worker_does_not_block_agents_or_exit() -> Result<()> {
    unavailable_manifest(/*held*/ true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attach_ordinary_manifest_task_return_to_agents_and_exit_preserves_conversation()
-> Result<()> {
    let root = tempfile::tempdir()?;
    let home = root.path().join("client");
    let worker_home = root.path().join("worker");
    let workspace = root.path().join("workspace");
    for path in [&home, &worker_home, &workspace] {
        std::fs::create_dir(path)?;
    }
    let home = home.canonicalize()?;
    let worker_home = worker_home.canonicalize()?;
    let workspace = workspace.canonicalize()?;
    let model = MockServer::start().await;
    write_config(&worker_home, &workspace, &model, /*manifest*/ None)?;
    let thread_id = app_test_support::create_fake_rollout(
        &worker_home,
        "2026-10-17T12-00-00",
        "2026-10-17T12:00:00Z",
        "Existing ordinary conversation",
        Some("local"),
        /*git_info*/ None,
    )?;
    let rollout = app_test_support::rollout_path(&worker_home, "2026-10-17T12-00-00", &thread_id);
    let mut original = std::fs::read_to_string(&rollout)?
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    original[0]["payload"]["cwd"] = serde_json::json!(workspace);
    std::fs::write(
        &rollout,
        original
            .iter()
            .map(serde_json::to_string)
            .collect::<std::result::Result<Vec<_>, _>>()?
            .join("\n")
            + "\n",
    )?;
    let conversation = |lines: &[serde_json::Value]| {
        lines
            .iter()
            .filter(|line| {
                line["type"] == "response_item"
                    || (line["type"] == "event_msg"
                        && matches!(
                            line["payload"]["type"].as_str(),
                            Some(
                                "user_message" | "agent_message" | "turn_started" | "task_started"
                            )
                        ))
            })
            .cloned()
            .collect::<Vec<_>>()
    };
    let expected_conversation = conversation(&original);
    ensure!(
        !expected_conversation.is_empty(),
        "fixture must contain conversation history"
    );
    let program = codex_utils_cargo_bin::cargo_bin("codex")?;
    let mut worker = tokio::process::Command::new(&program)
        .args(["app-server", "--listen", "ws://127.0.0.1:0"])
        .current_dir(&workspace)
        .envs(isolated_env(&worker_home))
        .env("RUST_LOG", "warn")
        // This fixture parses the worker's startup URL, independently of the
        // parent runner's requested terminal color depth.
        .env_remove("FORCE_COLOR")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut worker_logs =
        BufReader::new(worker.stderr.take().context("worker startup output")?).lines();
    let endpoint = tokio::time::timeout(FAILURE_GUARD, async {
        while let Some(line) = worker_logs.next_line().await? {
            if let Some(address) = line
                .split_whitespace()
                .find_map(|token| token.strip_prefix("ws://"))
                .and_then(|address| address.parse::<std::net::SocketAddr>().ok())
            {
                return Ok(format!("ws://{address}"));
            }
        }
        anyhow::bail!("owned app-server exited before listening")
    })
    .await
    .context("worker startup timed out")??;
    let log_reader =
        tokio::spawn(async move { while let Ok(Some(_)) = worker_logs.next_line().await {} });
    let reserved = std::net::TcpListener::bind("127.0.0.1:0")?;
    let address = reserved.local_addr()?;
    drop(reserved);
    let manifest_path = root.path().join("agents.toml");
    let manifest: Manifest = serde_json::from_value(serde_json::json!({
        "version":1, "coordinator": {"listen":address,"url":format!("http://{address}"),"token_file":root.path().join("operator.token"),"state_dir":root.path().join("coordinator")},
        "machines":[{"id":"bench","label":"Ordinary Bench","servers":[{"id":"worker","label":"Saved worker","endpoint":{"transport":"websocket","url":endpoint}}]}]
    }))?;
    std::fs::write(&manifest_path, toml::to_string(&manifest)?)?;
    write_config(&home, &workspace, &model, Some(&manifest_path))?;
    let mut coordinator = start_coordinator(&program, &manifest_path, &home, &workspace).await?;
    let result = async {
        let mut terminal = Terminal::start(&program, &workspace, &isolated_env(&home)).await?;
        terminal
            .wait_for(&[
                "Agent command center",
                "Ordinary Bench",
                "Existing ordinary conversation",
            ])
            .await?;
        terminal
            .session
            .writer_sender()
            .send(b"\r".to_vec())
            .await?;
        terminal
            .wait_for_screen(
                &["Existing ordinary conversation"],
                &["Agent command center"],
            )
            .await?;
        terminal
            .session
            .writer_sender()
            .send(b"/agents\r".to_vec())
            .await?;
        terminal
            .wait_for(&[
                "Agent command center",
                "Ordinary Bench",
                "Existing ordinary conversation",
            ])
            .await?;
        terminal.exit().await?;
        let after = std::fs::read_to_string(&rollout)?
            .lines()
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ensure!(
            conversation(&after) == expected_conversation,
            "attach and exit changed the original conversation"
        );
        ensure!(
            model
                .received_requests()
                .await
                .context("model requests")?
                .is_empty(),
            "browse, attach, or exit sent a model request"
        );
        ensure!(
            !home.join("app-server-daemon/daemon.pid").exists()
                && !home.join("app-server-daemon/app-server.pid").exists(),
            "manifest attach started an extra local daemon"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    coordinator.kill().await?;
    coordinator.wait().await?;
    worker.kill().await?;
    worker.wait().await?;
    log_reader.abort();
    let _ = log_reader.await;
    stop_unexpected_daemon(&program, &home).await?;
    result
}
