use std::process::Command;

use anyhow::Result;
use pretty_assertions::assert_eq;

#[test]
fn version_uses_runtime_identity() -> Result<()> {
    let output = Command::new(codex_utils_cargo_bin::cargo_bin("codex")?)
        .arg("--version")
        .output()?;
    let actual = (
        output.status.success(),
        String::from_utf8(output.stdout)?,
        String::from_utf8(output.stderr)?,
    );
    let expected = (
        true,
        format!("codex-cli {}\n", codex_tui::CODEX_CLI_VERSION),
        String::new(),
    );

    assert_eq!(actual, expected);
    Ok(())
}
