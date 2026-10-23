use super::*;
use pretty_assertions::assert_eq;

#[test]
fn mention_bindings_use_utf8_offsets_and_complete_labels() -> Result<()> {
    let body =
        "🧪 @Ana, review this. @Ana-long is another person; mail@Ana is not a mention. @Ana!";
    let mentions = bind_mentions(body, vec![(TeamBoardIdentity::User, "@Ana".into())])?;
    assert_eq!(mentions.len(), 2);
    assert_eq!(
        mentions[0],
        TeamBoardMention {
            identity: TeamBoardIdentity::User,
            display: "@Ana".into(),
            start: 5,
            end: 9
        }
    );
    assert_eq!(
        body.get(mentions[1].start as usize..mentions[1].end as usize),
        Some("@Ana")
    );
    assert!(bind_mentions("@Ana-long", vec![(TeamBoardIdentity::User, "@Ana".into())]).is_err());
    Ok(())
}

#[test]
fn duplicate_mentions_deduplicate_and_conflicting_targets_fail() -> Result<()> {
    let user = (TeamBoardIdentity::User, "@Ana".into());
    assert_eq!(
        bind_mentions("@Ana", vec![user.clone(), user.clone()])?.len(),
        1
    );
    let agent = (
        TeamBoardIdentity::Agent {
            address: TeamAddress {
                machine_id: "m".into(),
                server_id: "s".into(),
                thread_id: "t".into(),
            },
        },
        "@Ana".into(),
    );
    assert!(bind_mentions("@Ana", vec![user, agent]).is_err());
    Ok(())
}
