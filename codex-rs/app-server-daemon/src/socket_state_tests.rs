use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::is_confirmed_absent;

#[tokio::test]
async fn missing_and_stale_sockets_are_confirmed_absent() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let missing = temp_dir.path().join("missing.sock");
    let stale = temp_dir.path().join("stale.sock");
    let listener = tokio::net::UnixListener::bind(&stale)?;
    drop(listener);

    assert_eq!(
        (
            is_confirmed_absent(&missing).await?,
            is_confirmed_absent(&stale).await?,
        ),
        (true, true)
    );
    Ok(())
}

#[tokio::test]
async fn live_socket_is_not_absent() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let socket_path = temp_dir.path().join("live.sock");
    let _listener = tokio::net::UnixListener::bind(&socket_path)?;

    assert!(!is_confirmed_absent(&socket_path).await?);
    Ok(())
}

#[tokio::test]
async fn regular_file_is_not_accepted_as_stale_socket() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let socket_path = temp_dir.path().join("regular-file");
    std::fs::write(&socket_path, "not a socket")?;

    assert!(!is_confirmed_absent(&socket_path).await?);
    Ok(())
}
