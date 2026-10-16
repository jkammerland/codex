use anyhow::Context;
use anyhow::Result;
use codex_app_server_protocol::TeamAddress;
use futures::StreamExt;
use tokio::net::TcpStream;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::Manifest;
use crate::TeamRequest;
use crate::TeamResponse;
use crate::TeamSnapshot;

#[derive(Clone)]
pub struct TeamClient {
    url: url::Url,
    token: String,
    http: reqwest::Client,
}

pub struct TeamEvents {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl TeamClient {
    pub async fn from_manifest(path: &std::path::Path) -> Result<Self> {
        let manifest = Manifest::load(path).await?;
        let token = crate::transport::token(Some(&manifest.coordinator.token_file))
            .await?
            .context("operator credential is missing")?;
        Self::new(&manifest.coordinator.url, token)
    }

    pub fn new(url: &str, token: String) -> Result<Self> {
        crate::config::validate_url(url, &["http", "https"])?;
        Ok(Self {
            url: url::Url::parse(&format!("{}/", url.trim_end_matches('/')))?,
            token,
            http: codex_http_client::build_reqwest_client_with_custom_ca(
                reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()),
            )?,
        })
    }

    pub async fn request(&self, request: TeamRequest) -> Result<TeamResponse> {
        let response = self
            .http
            .post(self.url.join("rpc")?)
            .bearer_auth(&self.token)
            .json(&request)
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status();
            anyhow::bail!("coordinator returned {status}: {}", response.text().await?);
        }
        Ok(response.json().await?)
    }

    pub async fn snapshot(&self) -> Result<TeamSnapshot> {
        match self.request(TeamRequest::Snapshot).await? {
            TeamResponse::Snapshot { snapshot } => Ok(snapshot),
            _ => anyhow::bail!("unexpected coordinator response"),
        }
    }

    pub async fn events(&self) -> Result<TeamEvents> {
        codex_utils_rustls_provider::ensure_rustls_crypto_provider();
        let mut request = self
            .websocket_url("events")?
            .as_str()
            .into_client_request()?;
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {}", self.token).parse()?);
        let connector = codex_http_client::maybe_build_rustls_client_config_with_custom_ca()?
            .map(tokio_tungstenite::Connector::Rustls);
        let (socket, _) =
            tokio_tungstenite::connect_async_tls_with_config(request, None, false, connector)
                .await?;
        Ok(TeamEvents { socket })
    }

    fn websocket_url(&self, path: &str) -> Result<url::Url> {
        let mut url = self.url.join(path)?;
        url.set_scheme(if self.url.scheme() == "https" {
            "wss"
        } else {
            "ws"
        })
        .map_err(|()| anyhow::anyhow!("invalid coordinator URL"))?;
        Ok(url)
    }

    /// Credentials belong only to this child process. Its independent relay
    /// connection cannot collide with coordinator request IDs or approvals.
    pub fn attach_command(&self, address: &TeamAddress) -> Result<tokio::process::Command> {
        let relay = self.websocket_url(&format!(
            "relay/{}/{}",
            address.machine_id, address.server_id
        ))?;
        // The interactive CLI requires an explicit port, including the default
        // port that Url otherwise omits when serializing an HTTPS endpoint.
        let remote = format!(
            "{}:{}{}",
            &relay[..url::Position::AfterHost],
            relay
                .port_or_known_default()
                .context("relay port is missing")?,
            &relay[url::Position::BeforePath..],
        );
        let mut command = tokio::process::Command::new(std::env::current_exe()?);
        command
            .arg("--remote")
            .arg(remote)
            .args([
                "--remote-auth-token-env",
                "CODEX_TEAM_RELAY_TOKEN",
                "resume",
            ])
            .arg(&address.thread_id)
            .env("CODEX_TEAM_RELAY_TOKEN", &self.token);
        Ok(command)
    }
}

impl TeamEvents {
    pub async fn changed(&mut self) -> Result<()> {
        loop {
            match self.socket.next().await {
                Some(Ok(tokio_tungstenite::tungstenite::Message::Text(_))) => return Ok(()),
                Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) | None => {
                    anyhow::bail!("coordinator event stream closed")
                }
                Some(Err(error)) => return Err(error.into()),
                _ => {}
            }
        }
    }
}
