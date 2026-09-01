use anyhow::Result;
use predicates::str::contains;

#[test]
fn version_identifies_maintained_fork() -> Result<()> {
    let mut cmd = assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?);

    cmd.arg("--version")
        .assert()
        .success()
        .stdout(contains("+jkammerland.mcp.17"));

    Ok(())
}
