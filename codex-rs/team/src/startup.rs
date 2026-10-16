//! Local coordinator startup uses process and file-lock events, never polling.

use std::path::Path;
use std::process::Stdio;

use anyhow::Context;
use anyhow::Result;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;

use crate::Manifest;
use crate::TeamClient;

pub(crate) const READY_MARKER: &str = "CODEX_AGENTS_READY";

pub(crate) fn report_ready(writer: &mut impl std::io::Write) -> std::io::Result<()> {
    match writeln!(writer, "{READY_MARKER}").and_then(|()| writer.flush()) {
        Ok(()) => Ok(()),
        // Closing the UI can retire its readiness reader during startup. The
        // detached coordinator still owns its daemon lifetime and state lock.
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error),
    }
}

pub(crate) async fn acquire_startup_lock(lock_path: std::path::PathBuf) -> Result<std::fs::File> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    // An indefinite OS lock wait must not keep Tokio's blocking pool alive
    // after its UI has exited. The native waiter owns the file independently.
    std::thread::Builder::new()
        .name("agents-startup-lock".into())
        .spawn(move || {
            let result = (|| -> Result<std::fs::File> {
                if let Some(parent) = lock_path.parent() {
                    let mut directory = std::fs::DirBuilder::new();
                    directory.recursive(true);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::DirBuilderExt;
                        directory.mode(0o700);
                    }
                    directory.create(parent)?;
                }
                let mut options = std::fs::OpenOptions::new();
                options.read(true).write(true).create(true).truncate(false);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let file = options.open(lock_path)?;
                file.lock()?;
                Ok(file)
            })();
            // Cancellation drops the returned file here as soon as the OS grants
            // the lock, so the abandoned startup cannot retain its ownership.
            let _ = sender.send(result);
        })?;
    receiver
        .await
        .context("coordinator startup lock worker exited")?
}

pub(crate) async fn probe_existing(
    manifest: &Manifest,
    allow_partial_credential: bool,
) -> Result<Option<TeamClient>> {
    let token = match crate::transport::token(Some(&manifest.coordinator.token_file)).await {
        Ok(Some(token)) => token,
        Ok(None) => return Ok(None),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                || allow_partial_credential =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let client = TeamClient::new(&manifest.coordinator.url, token)?;
    match client.snapshot().await {
        Ok(_) => Ok(Some(client)),
        Err(error)
            if error.chain().any(|cause| {
                cause
                    .downcast_ref::<reqwest::Error>()
                    .is_some_and(reqwest::Error::is_connect)
            }) =>
        {
            Ok(None)
        }
        Err(error) => {
            Err(error).context("existing coordinator did not accept this manifest's credential")
        }
    }
}

/// Automatic children serialize their complete bootstrap independently of the
/// requesting UI. None means a previous child already serves this credential.
pub(crate) async fn prepare_automatic_start(manifest: &Manifest) -> Result<Option<std::fs::File>> {
    let startup = acquire_startup_lock(manifest.coordinator.state_dir.join("startup.lock")).await?;
    if probe_existing(manifest, /*allow_partial_credential*/ false)
        .await?
        .is_some()
    {
        return Ok(None);
    }
    Ok(Some(startup))
}

/// Start or reuse this manifest's local coordinator without starting worker daemons.
pub async fn ensure_coordinator(manifest_path: &Path, executable: &Path) -> Result<TeamClient> {
    let manifest_path = std::path::absolute(manifest_path)?;
    let manifest = Manifest::load(&manifest_path).await?;
    let url = url::Url::parse(&manifest.coordinator.url)?;
    let loopback = match url.host() {
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    // Remote coordinators are connected explicitly; operator discovery never
    // turns manifest SSH endpoints into commands that start remote daemons.
    if !loopback {
        let client = TeamClient::from_manifest(&manifest_path).await?;
        client.snapshot().await?;
        return Ok(client);
    }
    if let Some(client) = probe_existing(&manifest, /*allow_partial_credential*/ true).await? {
        return Ok(client);
    }
    let mut directory = tokio::fs::DirBuilder::new();
    directory.recursive(true);
    #[cfg(unix)]
    directory.mode(0o700);
    directory.create(&manifest.coordinator.state_dir).await?;
    let log_path = manifest.coordinator.state_dir.join("coordinator.log");
    let mut options = std::fs::OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let log = options.open(&log_path)?;
    let mut command = tokio::process::Command::new(executable);
    command
        .args(["agents", "serve", "--manifest"])
        .arg(&manifest_path)
        .env(READY_MARKER, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(log))
        .kill_on_drop(false);
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    command.creation_flags(/*creation_flags*/ 0x0000_0008 | 0x0000_0200);
    let mut child = command
        .spawn()
        .context("could not start local agents coordinator")?;
    let mut lines = BufReader::new(
        child
            .stdout
            .take()
            .context("coordinator readiness stream is missing")?,
    )
    .lines();
    // Give the original process handle its own owner before waiting for UI
    // readiness. Dropping the startup caller must neither kill nor orphan it.
    let process = tokio::spawn(async move { child.wait().await });
    while let Some(line) = lines.next_line().await? {
        if line == READY_MARKER {
            // The child owns the lifetime coordinator lock. Its existing wait
            // task remains responsible for reaping after this UI returns.
            drop(process);
            let client = TeamClient::from_manifest(&manifest_path).await?;
            client.snapshot().await?;
            return Ok(client);
        }
    }
    let status = process.await??;
    // A separately invoked coordinator may have won its lifetime lock. The
    // single authenticated request waits on its listener's actual readiness.
    let client = TeamClient::from_manifest(&manifest_path)
        .await
        .with_context(|| {
            format!(
                "coordinator exited ({status}); inspect {}",
                log_path.display()
            )
        })?;
    client.snapshot().await.with_context(|| {
        format!(
            "coordinator exited ({status}); inspect {}",
            log_path.display()
        )
    })?;
    Ok(client)
}

#[cfg(test)]
#[path = "startup_tests.rs"]
mod tests;
