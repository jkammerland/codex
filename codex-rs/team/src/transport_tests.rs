use std::io::Write;
use std::process::Command;

use anyhow::Result;
use pretty_assertions::assert_eq;

#[test]
fn ssh_forwarding_survives_alias_that_clears_forwardings() -> Result<()> {
    let mut config = tempfile::NamedTempFile::new()?;
    writeln!(
        config,
        "Host codex-team-config-test\n  HostName 127.0.0.1\n  CanonicalizeHostname no\n  ClearAllForwardings yes"
    )?;
    config.flush()?;

    // External SSH reads a completed fixture while its temporary path stays owned.
    let config = config.into_temp_path();

    // Establish that this isolated alias really removes an explicit forward.
    let cleared = Command::new("ssh")
        .args(["-G", "-F"])
        .arg(config.as_os_str())
        .args([
            "-N",
            "-L",
            "127.0.0.1:56123:127.0.0.1:4501",
            "--",
            "codex-team-config-test",
        ])
        .output()?;
    assert!(cleared.status.success());
    let cleared = String::from_utf8(cleared.stdout)?;
    assert!(
        cleared
            .lines()
            .any(|line| line == "clearallforwardings yes")
    );
    assert!(
        !cleared
            .lines()
            .any(|line| line.starts_with("localforward "))
    );

    // OpenSSH evaluates the production command without connecting or reading user config.
    let forwarding = super::ssh_forward_command(
        "codex-team-config-test",
        /*local_port*/ 56123,
        /*remote_port*/ 4501,
    );
    let forwarding = forwarding.as_std();
    let effective = Command::new(forwarding.get_program())
        .args(["-G", "-F"])
        .arg(config.as_os_str())
        .args(forwarding.get_args())
        .output()?;
    assert!(effective.status.success());
    let effective = String::from_utf8(effective.stdout)?;
    let forwarding_settings = effective
        .lines()
        .filter(|line| {
            line.starts_with("clearallforwardings ") || line.starts_with("localforward ")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        forwarding_settings,
        vec![
            "clearallforwardings no",
            "localforward [127.0.0.1]:56123 [127.0.0.1]:4501",
        ]
    );
    Ok(())
}
