//! Background discovery must not bypass the foreground's demand-driven history window.

use super::*;
use crate::app_server_session::ThreadParamsMode;
use codex_app_server_protocol::JSONRPCMessage;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn background_history_stays_recent_across_foreground_switch_and_expands_on_request()
-> Result<()> {
    let (mut app, mut events, _) = crate::app::tests::make_test_app_with_channels().await;
    let mut foreground = AppServerSession::discovery_placeholder();
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let machine = key("history-host");
    let history: Vec<_> = (0..24)
        .map(|index| {
            let mut thread = overview_thread(
                ThreadId::from_u128(200 + index),
                /*parent_thread_id*/ None,
                &format!("Saved task {index:02}"),
                ThreadStatus::NotLoaded,
            );
            thread.updated_at = 4_000_000_000 - index as i64;
            thread.recency_at = Some(thread.updated_at);
            thread
        })
        .collect();
    let mut loaded = overview_thread(
        ThreadId::from_u128(300),
        /*parent_thread_id*/ None,
        "Loaded teammate",
        ThreadStatus::Idle,
    );
    loaded.preview.clear();
    let pinned = history[23].clone();
    let initial: HashSet<_> = history[..10]
        .iter()
        .chain([&pinned, &loaded])
        .map(|thread| ThreadId::from_string(&thread.id).unwrap())
        .collect();
    let expanded: HashSet<_> = history[..20]
        .iter()
        .chain([&pinned, &loaded])
        .map(|thread| ThreadId::from_string(&thread.id).unwrap())
        .collect();

    // Honor requested page sizes so the old eager loop exposes all 24 saved rows.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = crate::resolve_remote_addr(&format!("ws://{}", listener.local_addr()?))?;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = tokio_tungstenite::accept_async(stream).await?;
        while let Some(Ok(Message::Text(text))) = socket.next().await {
            let JSONRPCMessage::Request(request) = serde_json::from_str(&text)? else {
                continue;
            };
            let params = request.params.unwrap_or_default();
            let result = match request.method.as_str() {
                "initialize" => json!({"userAgent": "history-discovery-test/1.0"}),
                "thread/list" => {
                    let interactive = params["sourceKinds"] == json!([])
                        || params["sourceKinds"]
                            .as_array()
                            .is_some_and(|kinds| kinds.contains(&json!("cli")));
                    if params["sectionId"] == codex_state::PINNED_THREAD_SECTION_ID {
                        json!({"data": if interactive { vec![pinned.clone()] } else { vec![] }, "nextCursor": null})
                    } else {
                        let start = params["cursor"].as_str().unwrap_or("0").parse::<usize>()?;
                        let limit = params["limit"].as_u64().unwrap_or(100) as usize;
                        let end = (start + limit).min(history.len());
                        json!({
                            "data": if interactive { history[start..end].to_vec() } else { vec![] },
                            "nextCursor": (interactive && end < history.len()).then(|| end.to_string()),
                        })
                    }
                }
                "thread/loaded/list" => json!({"data": [loaded.id], "nextCursor": null}),
                "thread/read" => {
                    let params: codex_app_server_protocol::ThreadReadParams =
                        serde_json::from_value(params)?;
                    assert!(!params.include_turns);
                    let id = params.thread_id.as_str();
                    let thread = if id == loaded.id {
                        &loaded
                    } else {
                        history.iter().find(|thread| thread.id == id).unwrap()
                    };
                    json!({"thread": thread})
                }
                "thread/turns/list" => json!({"data": [], "nextCursor": null}),
                method => panic!("history discovery must not hydrate or mutate sessions: {method}"),
            };
            socket
                .send(Message::Text(
                    json!({"id": request.id, "result": result})
                        .to_string()
                        .into(),
                ))
                .await?;
        }
        Ok::<_, color_eyre::Report>(())
    });
    let session = AppServerSession::new(
        crate::backend_id::BackendId::new(),
        crate::connect_remote_app_server(endpoint).await?,
        ThreadParamsMode::Remote,
    );
    let mut parked = host("History host / main");
    parked.session = Some(session);
    app.agents_hosts.started = true;
    app.agents_hosts.manifest = Some(PathBuf::from("manifest.toml"));
    app.agents_hosts.hosts.insert(machine.clone(), parked);
    app.refresh_other_agents_hosts(&foreground);
    while let Some(completed) = app.agents_hosts.tasks.join_next().await {
        completed?;
    }
    let mut listed = false;
    let mut pins = false;
    while let Ok(event) = events.try_recv() {
        if let AppEvent::AgentsHostUpdate(update) = event {
            listed |= matches!(update.as_ref(), HostUpdate::Listed { .. });
            pins |= matches!(update.as_ref(), HostUpdate::PinsListed { .. });
            app.handle_agents_host_update(&mut tui, &mut foreground, *update)
                .await?;
        }
    }
    assert_eq!((listed, pins), (true, true));
    assert_eq!(
        app.agents_hosts.hosts[&machine]
            .state
            .agents_overview
            .threads
            .keys()
            .copied()
            .collect::<HashSet<_>>(),
        initial,
    );
    let view = app.agents_overview_view(Vec::new(), /*selected_thread_id*/ None);
    insta::assert_snapshot!(
        "background_recent_history",
        render_with_height(&view, /*height*/ 52)
    );

    // Switching the same host into the foreground must retain that recent window.
    let parked = app.agents_hosts.hosts.get_mut(&machine).unwrap();
    foreground = parked.session.take().unwrap();
    let mut owner = std::mem::take(&mut parked.state);
    owner.swap(&mut app);
    app.agents_hosts.foreground = machine;
    app.open_agents_overview(&foreground);
    loop {
        if let AppEvent::AgentsOverviewThreadsLoaded { request_id, result } =
            events.recv().await.expect("foreground discovery")
        {
            app.apply_agents_overview_thread_refresh(&foreground, request_id, result);
            break;
        }
    }
    assert_eq!(
        app.agents_overview
            .threads
            .keys()
            .copied()
            .collect::<HashSet<_>>(),
        initial,
    );
    assert!(app.agents_overview.discovery.has_more());

    // Older saved tasks remain available through the existing Show more action.
    app.show_more_agents_overview(&foreground);
    loop {
        if let AppEvent::AgentsOverviewThreadsLoaded { request_id, result } =
            events.recv().await.expect("requested history page")
        {
            app.apply_agents_overview_thread_refresh(&foreground, request_id, result);
            break;
        }
    }
    assert_eq!(
        app.agents_overview
            .threads
            .keys()
            .copied()
            .collect::<HashSet<_>>(),
        expanded,
    );
    foreground.shutdown().await?;
    server.await??;
    Ok(())
}
