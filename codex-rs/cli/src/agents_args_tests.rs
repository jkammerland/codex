use super::*;
use pretty_assertions::assert_eq;

#[test]
fn agents_manifest_and_compatibility_flag_select_the_same_overview() {
    for flag in ["--manifest", "--team"] {
        let cli = MultitoolCli::try_parse_from(["codex", "agents", flag, "machines.toml"])
            .expect("manifest overview arguments");
        let Some(Subcommand::Agents(options)) = cli.subcommand else {
            panic!("expected agents command");
        };
        assert_eq!(options.manifest, Some(PathBuf::from("machines.toml")));
        assert!(options.action.is_none());
    }
}

#[test]
fn agents_automation_accepts_manifest_on_either_side_of_action() {
    for args in [
        vec!["codex", "agents", "--manifest", "machines.toml", "pause"],
        vec!["codex", "agents", "pause", "--manifest", "machines.toml"],
    ] {
        let cli = MultitoolCli::try_parse_from(args).expect("agents automation arguments");
        let Some(Subcommand::Agents(options)) = cli.subcommand else {
            panic!("expected agents command");
        };
        assert_eq!(options.manifest, Some(PathBuf::from("machines.toml")));
        assert!(matches!(
            options.action,
            Some(codex_team::TeamAction::Pause)
        ));
    }
}

#[test]
fn local_overview_rejects_an_explicit_manifest() {
    let error =
        MultitoolCli::try_parse_from(["codex", "agents", "--local", "--manifest", "machines.toml"])
            .expect_err("conflicting overview targets");
    assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
}
