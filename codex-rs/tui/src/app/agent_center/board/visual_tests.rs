use super::*;
use pretty_assertions::assert_eq;

#[test]
fn reply_context_distinguishes_nested_targets_from_ordinary_conversation_replies() {
    let mut frames = Vec::new();
    for root_present in [true, false] {
        for width in [45, 120] {
            let (mut view, _) = fixture();
            let mut entries = vec![
                entry("root", "root", 1, "Release discussion", true),
                entry("human", "root", 2, "Ordinary human reply", true),
                entry("agent", "root", 3, "Ordinary agent reply", false),
            ];
            for (id, seq, body, target) in [
                ("nested-human", 4, "Answer to human reply", "human"),
                ("nested-agent", 5, "Answer to agent reply", "agent"),
                ("missing", 6, "Answer to unavailable reply", "removed"),
            ] {
                let mut nested = entry(id, "root", seq, body, false);
                nested.message.reply_to = Some(target.into());
                entries.push(nested);
            }
            if !root_present {
                entries.remove(0);
            }
            view.state().snapshot.entries = entries;
            key(&mut view, KeyCode::Enter);

            let output = render(&view, width, 36);
            // Inspect the reader pane independently of the wide feed's repeated root title.
            let reader = output
                .lines()
                .map(|line| line.rsplit('│').next().unwrap().trim())
                .collect::<Vec<_>>();
            let context = reader
                .iter()
                .copied()
                .filter(|line| line.starts_with("↳ replying to "))
                .collect::<Vec<_>>();
            assert_eq!(
                context,
                [
                    "↳ replying to You",
                    "↳ replying to Reviewer · MacBook",
                    "↳ replying to earlier entry",
                ],
                "{output}"
            );

            // Ordinary replies go directly from author to body, even if the root is unavailable.
            for (body, author) in [
                ("Ordinary human reply", "You · Human"),
                ("Ordinary agent reply", "Reviewer · MacBook"),
            ] {
                let row = reader.iter().position(|line| *line == body).unwrap();
                assert!(reader[row - 1].starts_with(author), "{output}");
            }
            for (body, target) in [
                ("Answer to human reply", "You"),
                ("Answer to agent reply", "Reviewer · MacBook"),
                ("Answer to unavailable reply", "earlier entry"),
            ] {
                let row = reader.iter().position(|line| *line == body).unwrap();
                assert_eq!(reader[row - 1], format!("↳ replying to {target}"));
            }
            frames.push(format!("{width}×36 root present: {root_present}\n{output}"));
        }
    }
    insta::assert_snapshot!(frames.join("\n\n"));
}

#[test]
fn browsing_footer_keeps_back_and_quit_visible_at_compact_and_wide_widths() {
    let (view, _) = fixture();
    for width in [45, 80, 120, 160] {
        let output = render(&view, width, 18);
        let footer = output.lines().last().unwrap();
        assert!(footer.contains("Esc Tasks"), "{output}");
        assert!(footer.contains("Ctrl+D quit"), "{output}");
    }
}

#[test]
fn shared_board_keeps_cross_host_posts_visible_at_all_sizes() {
    let (view, _) = fixture();
    let mut other = entry("other-host", "other-host", 3, "Linux build is ready", false);
    let TeamBoardIdentity::Agent { address } = &mut other.author.identity else {
        unreachable!();
    };
    address.machine_id = "builder".into();
    other.author.machine_name = Some("Builder".into());
    view.state().snapshot.entries.push(other);
    view.state().hosts = [
        ("laptop", "MacBook / Team worker", true),
        ("builder", "Builder / Team worker", false),
    ]
    .map(|(machine, label, connected)| {
        crate::app::agents_overview_view::command_center::hosts::HostSummary {
            key: crate::app::agents_hosts::HostKey {
                machine_id: machine.into(),
                server_id: "primary".into(),
            },
            label: label.into(),
            connected,
        }
    })
    .to_vec();
    let frames = [(160, 24), (100, 18), (45, 16), (140, 7)].map(|(width, height)| {
        let output = render(&view, width, height);
        assert!(output.contains("Shared Board"), "{output}");
        assert!(output.contains("all hosts"), "{output}");
        if height >= 16 {
            assert!(output.contains("Linux build is ready"), "{output}");
            assert!(output.contains("Ready for the release review?"), "{output}");
        }
        format!("{width}×{height}\n{output}")
    });
    insta::assert_snapshot!(frames.join("\n\n"));
}
