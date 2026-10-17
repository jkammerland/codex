use super::*;
use crate::TeamAction;
use crate::TeamCommand;
use clap::Parser;
use pretty_assertions::assert_eq;

fn request(args: &[&str]) -> Result<TeamBoardParams> {
    let command = TeamCommand::try_parse_from(args)?;
    let TeamAction::Board(board) = command.command else {
        anyhow::bail!("expected board command");
    };
    board.into_request()
}

#[test]
fn post_defaults_to_general_without_inventing_mentions() -> Result<()> {
    assert_eq!(
        request(&["agents", "board", "post", "Ask @Reviewer later"])?,
        TeamBoardParams::Post {
            topic: "general".into(),
            body: "Ask @Reviewer later".into(),
            mentions: Vec::new(),
        }
    );
    Ok(())
}

#[test]
fn explicit_mentions_preserve_utf8_byte_ranges_and_identity() -> Result<()> {
    let body = "Hej 👋 @Réview helper, ask @user. @Réview helper!";
    let address = TeamAddress {
        machine_id: "mac".into(),
        server_id: "primary".into(),
        thread_id: "11111111-1111-4111-8111-111111111111".into(),
    };
    let TeamBoardParams::Post { mentions, .. } = request(&[
        "agents",
        "board",
        "post",
        body,
        "--mention",
        "Réview helper=mac/primary/11111111-1111-4111-8111-111111111111",
        "--mention",
        "user=user",
    ])?
    else {
        anyhow::bail!("expected post");
    };
    assert_eq!(
        mentions,
        vec![
            TeamBoardMention {
                identity: TeamBoardIdentity::Agent {
                    address: address.clone()
                },
                display: "@Réview helper".into(),
                start: 9,
                end: 24,
            },
            TeamBoardMention {
                identity: TeamBoardIdentity::User,
                display: "@user".into(),
                start: 30,
                end: 35,
            },
            TeamBoardMention {
                identity: TeamBoardIdentity::Agent { address },
                display: "@Réview helper".into(),
                start: 37,
                end: 52,
            },
        ]
    );
    for mention in &mentions {
        assert_eq!(
            &body[mention.start as usize..mention.end as usize],
            mention.display
        );
    }
    Ok(())
}

#[test]
fn explicit_mentions_do_not_bind_email_or_handle_prefixes() {
    for body in ["mail@user.example", "@user2", "@user-helper", "@user/child"] {
        let error = request(&["agents", "board", "post", body, "--mention", "user=user"])
            .expect_err("a partial label must not notify the user");
        assert!(error.to_string().contains("complete label"));
    }
}

#[test]
fn conflicting_bindings_are_rejected_but_duplicate_arguments_are_idempotent() -> Result<()> {
    let agent = "user=mac/primary/11111111-1111-4111-8111-111111111111";
    let error = request(&[
        "agents",
        "board",
        "post",
        "Hi @user",
        "--mention",
        "user=user",
        "--mention",
        agent,
    ])
    .expect_err("same text cannot notify two identities");
    assert!(error.to_string().contains("different identities"));

    assert_eq!(
        request(&[
            "agents",
            "board",
            "post",
            "Hi @user",
            "--mention",
            "user=user",
            "--mention",
            "user=user",
        ])?,
        TeamBoardParams::Post {
            topic: "general".into(),
            body: "Hi @user".into(),
            mentions: vec![TeamBoardMention {
                identity: TeamBoardIdentity::User,
                display: "@user".into(),
                start: 3,
                end: 8,
            }],
        }
    );
    Ok(())
}

#[test]
fn follow_and_topic_follow_distinguish_human_and_explicit_agent() -> Result<()> {
    assert_eq!(
        request(&["agents", "board", "follow", "conversation"])?,
        TeamBoardParams::Follow {
            conversation_id: "conversation".into(),
            identity: None,
            enabled: true,
        }
    );
    assert_eq!(
        request(&[
            "agents",
            "board",
            "topic-follow",
            "builds",
            "--remove",
            "--agent",
            "mac/primary/11111111-1111-4111-8111-111111111111",
        ])?,
        TeamBoardParams::TopicFollow {
            topic: "builds".into(),
            identity: Some(TeamBoardIdentity::Agent {
                address: TeamAddress {
                    machine_id: "mac".into(),
                    server_id: "primary".into(),
                    thread_id: "11111111-1111-4111-8111-111111111111".into(),
                }
            }),
            enabled: false,
        }
    );
    Ok(())
}

#[test]
fn conversation_queries_preserve_pagination_and_read_is_explicit() -> Result<()> {
    assert_eq!(
        request(&[
            "agents",
            "board",
            "list",
            "--filter",
            "mentions",
            "--topic",
            "trial",
            "--query",
            "reconnect",
            "--cursor",
            "next-page",
            "--limit",
            "7",
        ])?,
        TeamBoardParams::List {
            filter: TeamBoardFilter::Mentions,
            query: Some("reconnect".into()),
            topic: Some("trial".into()),
            cursor: Some("next-page".into()),
            limit: Some(7),
            identity: None,
        }
    );
    assert_eq!(
        request(&[
            "agents",
            "board",
            "mark-read",
            "conversation",
            "--through",
            "12"
        ])?,
        TeamBoardParams::MarkRead {
            conversation_id: "conversation".into(),
            identity: None,
            append_seq: 12
        }
    );
    Ok(())
}
