use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u32,
    pub coordinator: CoordinatorConfig,
    pub machines: Vec<Machine>,
    pub gitea: Option<GiteaConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoordinatorConfig {
    pub listen: SocketAddr,
    pub url: String,
    pub token_file: PathBuf,
    pub state_dir: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Machine {
    pub id: String,
    pub label: String,
    pub servers: Vec<Server>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    pub id: String,
    pub label: String,
    pub endpoint: Endpoint,
    #[serde(default)]
    pub cancellation_adapters: Vec<codex_app_server_protocol::TeamCancellationAdapter>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case", deny_unknown_fields)]
pub enum Endpoint {
    Local {
        socket: PathBuf,
    },
    Websocket {
        url: String,
        token_file: Option<PathBuf>,
    },
    Ssh {
        destination: String,
        remote_port: u16,
        token_file: Option<PathBuf>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GiteaConfig {
    pub url: String,
    pub token_file: PathBuf,
    pub webhook_secret_file: PathBuf,
    pub repositories: Vec<String>,
    #[serde(default)]
    pub assignees: std::collections::BTreeMap<String, String>,
}

impl Manifest {
    pub async fn load(path: &Path) -> Result<Self> {
        let path = std::path::absolute(path)?;
        let mut manifest: Self = toml::from_str(&tokio::fs::read_to_string(&path).await?)
            .context("invalid team manifest")?;
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        resolve_path(base, &mut manifest.coordinator.token_file);
        resolve_path(base, &mut manifest.coordinator.state_dir);
        for machine in &mut manifest.machines {
            for server in &mut machine.servers {
                match &mut server.endpoint {
                    Endpoint::Local { socket } => resolve_path(base, socket),
                    Endpoint::Websocket { token_file, .. } | Endpoint::Ssh { token_file, .. } => {
                        if let Some(path) = token_file {
                            resolve_path(base, path);
                        }
                    }
                }
            }
        }
        if let Some(gitea) = &mut manifest.gitea {
            resolve_path(base, &mut gitea.token_file);
            resolve_path(base, &mut gitea.webhook_secret_file);
        }
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "unsupported team manifest version");
        ensure!(
            self.coordinator.listen.ip().is_loopback(),
            "bind the coordinator to loopback; use an authenticated TLS reverse proxy for remote access"
        );
        validate_url(&self.coordinator.url, &["http", "https"])?;
        let mut machine_ids = HashSet::new();
        for machine in &self.machines {
            validate_id(&machine.id)?;
            ensure!(
                !matches!(
                    machine.id.to_ascii_lowercase().as_str(),
                    "operator" | "gitea"
                ),
                "machine id is reserved for an integration inbox"
            );
            ensure!(
                machine_ids.insert(&machine.id),
                "duplicate machine id: {}",
                machine.id
            );
            let mut server_ids = HashSet::new();
            for server in &machine.servers {
                validate_id(&server.id)?;
                ensure!(
                    server_ids.insert(&server.id),
                    "duplicate server id on machine {}",
                    machine.id
                );
                match &server.endpoint {
                    Endpoint::Local { socket } => {
                        ensure!(!socket.as_os_str().is_empty(), "socket path is required")
                    }
                    Endpoint::Websocket { url, .. } => validate_url(url, &["ws", "wss"])?,
                    Endpoint::Ssh {
                        destination,
                        remote_port,
                        ..
                    } => {
                        ensure!(
                            !destination.is_empty()
                                && !destination.starts_with('-')
                                && !destination.contains(char::is_whitespace),
                            "SSH destination must be a configured host alias or user@host"
                        );
                        ensure!(
                            *remote_port != 0,
                            "SSH remote websocket port must be nonzero"
                        );
                    }
                }
            }
        }
        if let Some(gitea) = &self.gitea {
            validate_url(&gitea.url, &["http", "https"])?;
            for repository in &gitea.repositories {
                let parts = repository.split('/').collect::<Vec<_>>();
                ensure!(
                    parts.len() == 2
                        && parts.iter().all(|part| !part.is_empty()
                            && !matches!(*part, "." | "..")
                            && part
                                .chars()
                                .all(|ch| ch.is_ascii_alphanumeric()
                                    || matches!(ch, '_' | '-' | '.'))),
                    "Gitea repositories must be owner/name"
                );
            }
        }
        Ok(())
    }
}

fn validate_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "machine and server ids must use letters, digits, '-' or '_'"
    );
    Ok(())
}

fn resolve_path(base: &Path, path: &mut PathBuf) {
    if path.is_relative() {
        *path = base.join(&*path);
    }
}

pub(crate) fn validate_url(value: &str, schemes: &[&str]) -> Result<()> {
    let url = url::Url::parse(value)?;
    ensure!(schemes.contains(&url.scheme()), "unsupported URL scheme");
    ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "URLs must not contain credentials, query parameters or fragments"
    );
    if !matches!(url.scheme(), "https" | "wss") {
        let loopback = match url.host() {
            Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            None => false,
        };
        ensure!(
            loopback,
            "unencrypted connections are only allowed on loopback"
        );
    }
    Ok(())
}
