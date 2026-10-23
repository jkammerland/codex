use super::*;

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
