use super::*;
use codex_app_server_protocol::SkillsListParams;
use codex_app_server_protocol::SkillsListResponse;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn remote_skills_list_preserves_foreign_icon_paths() {
    let websocket_url = start_test_remote_server(|mut websocket| async move {
        expect_remote_initialize(&mut websocket).await;
        let JSONRPCMessage::Request(request) = read_websocket_message(&mut websocket).await else {
            panic!("expected skills/list request");
        };
        assert_eq!(request.method, "skills/list");

        // Raw wire data must not pass through native path constructors on the test host.
        let result = serde_json::from_str(
            r#"{
                "data": [{
                    "cwd": "/remote/work",
                    "skills": [
                        {
                            "name": "windows-drive",
                            "description": "Cross-host skill icons",
                            "path": "C:\\Skills % #\\SKILL.md",
                            "scope": "user",
                            "enabled": true,
                            "pluginId": null,
                            "interface": {
                                "iconSmall": "C:\\Skills % #\\small icon.svg",
                                "iconLarge": "D:\\Skills % #\\large icon.png"
                            }
                        },
                        {
                            "name": "windows-unc",
                            "description": "Cross-host skill icons",
                            "path": "\\\\server\\shared skills % #\\SKILL.md",
                            "scope": "user",
                            "enabled": true,
                            "pluginId": null,
                            "interface": {
                                "iconSmall": "\\\\server\\shared skills % #\\small icon.svg",
                                "iconLarge": "\\\\server\\shared skills % #\\large icon.png"
                            }
                        },
                        {
                            "name": "posix",
                            "description": "Cross-host skill icons",
                            "path": "/skills % #/SKILL.md",
                            "scope": "user",
                            "enabled": true,
                            "pluginId": null,
                            "interface": {
                                "iconSmall": "/skills % #/small icon.svg",
                                "iconLarge": "/skills % #/large icon.png"
                            }
                        },
                        {
                            "name": "missing-icons",
                            "description": "Cross-host skill icons",
                            "path": "/missing/SKILL.md",
                            "scope": "user",
                            "enabled": true,
                            "pluginId": null,
                            "interface": {}
                        },
                        {
                            "name": "null-icons",
                            "description": "Cross-host skill icons",
                            "path": "/null/SKILL.md",
                            "scope": "user",
                            "enabled": true,
                            "pluginId": null,
                            "interface": {
                                "iconSmall": null,
                                "iconLarge": null
                            }
                        }
                    ],
                    "errors": []
                }]
            }"#,
        )
        .expect("skills response should be valid JSON");
        write_websocket_message(
            &mut websocket,
            JSONRPCMessage::Response(JSONRPCResponse {
                id: request.id,
                result,
            }),
        )
        .await;
        websocket.close(None).await.expect("close should succeed");
    })
    .await;

    let client = AppServerClient::Remote(
        RemoteAppServerClient::connect(test_remote_connect_args(websocket_url))
            .await
            .expect("remote client should connect"),
    );
    let response = client
        .request_handle()
        .request_typed::<SkillsListResponse>(ClientRequest::SkillsList {
            request_id: RequestId::Integer(1),
            params: SkillsListParams::default(),
        })
        .await
        .expect("typed skills request should preserve paths from every server platform");

    let skills: Vec<_> = [
        (
            "windows-drive",
            r"C:\Skills % #\SKILL.md",
            Some(r"C:\Skills % #\small icon.svg"),
            Some(r"D:\Skills % #\large icon.png"),
        ),
        (
            "windows-unc",
            r"\\server\shared skills % #\SKILL.md",
            Some(r"\\server\shared skills % #\small icon.svg"),
            Some(r"\\server\shared skills % #\large icon.png"),
        ),
        (
            "posix",
            "/skills % #/SKILL.md",
            Some("/skills % #/small icon.svg"),
            Some("/skills % #/large icon.png"),
        ),
        ("missing-icons", "/missing/SKILL.md", None, None),
        ("null-icons", "/null/SKILL.md", None, None),
    ]
    .into_iter()
    .map(|(name, path, icon_small, icon_large)| {
        serde_json::json!({
            "name": name,
            "description": "Cross-host skill icons",
            "interface": {
                "displayName": null,
                "shortDescription": null,
                "iconSmall": icon_small,
                "iconLarge": icon_large,
                "iconSmallUrl": null,
                "iconLargeUrl": null,
                "brandColor": null,
                "defaultPrompt": null
            },
            "path": path,
            "scope": "user",
            "enabled": true,
            "pluginId": null
        })
    })
    .collect();
    assert_eq!(
        serde_json::to_value(response).expect("typed skills response should serialize"),
        serde_json::json!({
            "data": [{
                "cwd": "/remote/work",
                "skills": skills,
                "errors": []
            }]
        })
    );

    client.shutdown().await.expect("shutdown should complete");
}
