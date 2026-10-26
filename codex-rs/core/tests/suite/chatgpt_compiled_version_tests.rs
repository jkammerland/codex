use anyhow::Result;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_models_manager::ModelsManagerConfig;
use codex_models_manager::model_info::model_info_from_slug;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::openai_models::ToolMode;
use codex_protocol::protocol::MultiAgentVersion;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use wiremock::MockServer;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chatgpt_discovery_sends_compiled_version_and_uses_remote_sol_for_team_turn() -> Result<()>
{
    skip_if_no_network!(Ok(()));

    // This test owns /models; the generic server also mounts an empty catalog response.
    let server = MockServer::start().await;
    let model_slug = "gpt-6.1-sol";
    let instructions = "Remote GPT-6.1 Sol catalog instructions.";
    let mut remote_model = model_info_from_slug(model_slug);
    remote_model.visibility = ModelVisibility::List;
    remote_model.supported_in_api = false;
    remote_model.used_fallback_model_metadata = false;
    remote_model.tool_mode = Some(ToolMode::Direct);
    remote_model.multi_agent_version = Some(MultiAgentVersion::V2);
    remote_model
        .model_messages
        .as_mut()
        .expect("model messages")
        .instructions_template = Some(instructions.to_string());
    let models_mock = responses::mount_models_once(
        &server,
        ModelsResponse {
            models: vec![remote_model.clone()],
        },
    )
    .await;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_assistant_message("msg-1", "done"),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;

    // Let the discovered model, not a feature override, choose the team backend.
    let mut builder = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_model(model_slug)
        .with_config(|config| {
            config.agents_enabled = true;
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("disable multi-agent override");
            config.model_provider.request_max_retries = Some(0);
        });
    let test = builder.build_with_auto_env(&server).await?;
    let thread_originator = test.codex.config_snapshot().await.originator;

    // Inspect the real HTTP request: a zeroed fork version changes backend discovery.
    let requests = models_mock.requests();
    let [request] = requests.as_slice() else {
        panic!("expected one /models request, got {}", requests.len());
    };
    let expected_version = format!(
        "{}.{}.{}",
        env!("CARGO_PKG_VERSION_MAJOR"),
        env!("CARGO_PKG_VERSION_MINOR"),
        env!("CARGO_PKG_VERSION_PATCH")
    );
    let client_versions = request
        .url
        .query_pairs()
        .filter(|(name, _)| name == "client_version")
        .map(|(_, version)| version.into_owned())
        .collect::<Vec<_>>();
    assert_eq!(client_versions, vec![expected_version]);
    assert_ne!(client_versions[0], "0.0.0");
    assert_eq!(request.url.path(), "/v1/models");
    assert_eq!(
        request
            .headers
            .get("originator")
            .and_then(|value| value.to_str().ok()),
        Some(thread_originator.as_str())
    );

    // Full metadata equality rejects fallback even when its slug matches the selection.
    let models_manager = test.thread_manager.get_models_manager();
    let models_manager = models_manager
        .for_originator(&thread_originator)
        .unwrap_or(models_manager);
    assert_eq!(test.session_configured.model, model_slug);
    assert_eq!(
        models_manager
            .get_model_info(model_slug, &ModelsManagerConfig::default())
            .await,
        remote_model
    );

    test.submit_text_turn("Use the selected remote model.")
        .await?;
    let response = response_mock.single_request();
    assert_eq!(response.body_json()["model"], model_slug);
    assert_eq!(response.instructions_text(), instructions);
    assert_eq!(
        test.codex.multi_agent_version(),
        Some(MultiAgentVersion::V2)
    );
    assert_eq!(models_mock.requests().len(), 1);

    Ok(())
}
