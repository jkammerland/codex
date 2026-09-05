use std::io::Cursor;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use codex_protocol::models::ExecutedToolCall;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ImageDetail;
use codex_protocol::models::ResponseItem;
use image::DynamicImage;
use image::ImageFormat;
use pretty_assertions::assert_eq;

use crate::context_manager::item_token_upper_bound;
use crate::session::tests::make_session_and_context;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;

use super::CodeModeOutputKind;
use super::CodeModeResultEnvelope;
use super::ScriptStatus;
use super::StatusRequirement;
use super::bound_prepared_code_mode_output;
use super::code_mode_result_token_count;
use super::finalize_code_mode_result as finalize_code_mode_result_with_envelope;
use super::live_exec_sessions_footer;
use super::live_exec_sessions_metadata;
use super::script_status;
use super::truncate_code_mode_result;

const TEST_TURN_ID: &str = "test-turn";

fn terminal_status() -> ScriptStatus {
    ScriptStatus {
        text: "Script completed".to_string(),
        requirement: StatusRequirement::BestEffort,
    }
}

fn finalize_code_mode_result(
    content_items: Vec<FunctionCallOutputContentItem>,
    max_output_tokens: Option<usize>,
    status: &ScriptStatus,
    wall_time: Duration,
    session_ids: &[i32],
) -> Vec<FunctionCallOutputContentItem> {
    finalize_code_mode_result_with_envelope(
        content_items,
        max_output_tokens,
        status,
        wall_time,
        session_ids,
        &CodeModeResultEnvelope::new("test-call", TEST_TURN_ID, CodeModeOutputKind::Execute, true),
    )
}

fn final_response_item(
    content: Vec<FunctionCallOutputContentItem>,
    call_id: &str,
    custom_tool: bool,
) -> ResponseItem {
    let payload = if custom_tool {
        ToolPayload::Custom {
            input: String::new(),
        }
    } else {
        ToolPayload::Function {
            arguments: String::new(),
        }
    };
    FunctionToolOutput::from_content(content, Some(true))
        .to_response_item(call_id, &payload)
        .into()
}

fn png_data_url(width: u32, height: u32) -> String {
    let mut png = Cursor::new(Vec::new());
    DynamicImage::new_luma8(width, height)
        .write_to(&mut png, ImageFormat::Png)
        .expect("encode PNG image");
    format!(
        "data:image/png;base64,{}",
        BASE64_STANDARD.encode(png.into_inner())
    )
}

#[test]
fn truncated_text_output_starts_with_warning_and_preserves_head_and_tail() {
    let items = vec![
        FunctionCallOutputContentItem::InputText {
            text: "abcdefgh".to_string(),
        },
        FunctionCallOutputContentItem::InputText {
            text: "ijklmnop".to_string(),
        },
    ];

    assert_eq!(
        truncate_code_mode_result(items, Some(2)),
        vec![FunctionCallOutputContentItem::InputText {
            text: concat!(
                "Warning: truncated output (original token count: 5)\n",
                "Total output lines: 2\n\n",
                "abcd…3 tokens truncated…mnop"
            )
            .to_string(),
        }]
    );
}

#[test]
fn over_budget_audio_output_is_omitted() {
    let items = vec![FunctionCallOutputContentItem::InputAudio {
        audio_url: format!("data:audio/wav;base64,{}", "A".repeat(100)),
    }];

    assert_eq!(
        truncate_code_mode_result(items, Some(5)),
        vec![FunctionCallOutputContentItem::InputText {
            text: "[omitted 1 audio items ...]".to_string(),
        }]
    );
}

#[test]
fn final_output_default_cap_preserves_the_maximum_live_session_envelope() {
    let session_ids = (0..crate::unified_exec::MAX_UNIFIED_EXEC_PROCESSES)
        .map(|index| i32::try_from(index).expect("process cap should fit in i32"))
        .collect::<Vec<_>>();
    let metadata = live_exec_sessions_metadata(&session_ids)
        .expect("maximum live-session set should produce metadata");

    for max_output_tokens in [
        None,
        Some(crate::unified_exec::DEFAULT_MAX_OUTPUT_TOKENS * 2),
    ] {
        let output = finalize_code_mode_result(
            vec![FunctionCallOutputContentItem::InputText {
                text: "body".repeat(crate::unified_exec::DEFAULT_MAX_OUTPUT_TOKENS),
            }],
            max_output_tokens,
            &terminal_status(),
            Duration::ZERO,
            &session_ids,
        );
        assert!(
            code_mode_result_token_count(output.iter())
                <= crate::unified_exec::DEFAULT_MAX_OUTPUT_TOKENS
        );
        assert_eq!(output.last(), Some(&live_exec_sessions_footer(&metadata)));
    }
}

#[test]
fn final_output_keeps_only_bounded_recovery_metadata_over_a_tiny_requested_cap() {
    let session_ids = (0..crate::unified_exec::MAX_UNIFIED_EXEC_PROCESSES)
        .map(|index| i32::try_from(index).expect("process cap should fit in i32"))
        .collect::<Vec<_>>();
    let metadata = live_exec_sessions_metadata(&session_ids)
        .expect("maximum live-session set should produce metadata");

    assert_eq!(
        finalize_code_mode_result(
            vec![FunctionCallOutputContentItem::InputText {
                text: "body should be omitted".to_string(),
            }],
            Some(1),
            &terminal_status(),
            Duration::ZERO,
            &session_ids,
        ),
        vec![FunctionCallOutputContentItem::InputText { text: metadata }]
    );
}

#[test]
fn terminal_output_without_recovery_metadata_honors_tiny_requested_budgets() {
    for max_output_tokens in [0, 1] {
        let output = finalize_code_mode_result(
            Vec::new(),
            Some(max_output_tokens),
            &terminal_status(),
            Duration::ZERO,
            &[],
        );
        assert!(
            code_mode_result_token_count(output.iter()) <= max_output_tokens,
            "terminal output exceeded its {max_output_tokens}-token budget: {output:?}"
        );
    }
}

#[test]
fn yielded_output_preserves_normal_and_maximum_recovery_handles_at_tiny_budgets() {
    let maximum = format!(
        "g{}:{}",
        u64::MAX,
        "x".repeat(codex_code_mode::grpc::MAX_IDENTIFIER_BYTES)
    );
    for value in ["recoverable-cell".to_string(), maximum] {
        let cell_id = codex_code_mode::CellId::try_new(value)
            .expect("normal and maximum-size cell IDs should be admitted");
        let response = codex_code_mode::RuntimeResponse::Yielded {
            cell_id: cell_id.clone(),
            content_items: Vec::new(),
            reason: codex_code_mode::YieldReason::Requested,
            code_mode_host_duration: None,
        };
        let status = script_status(&response).expect("bounded cell ID should produce status");

        for max_output_tokens in [0, 1] {
            let output = finalize_code_mode_result(
                Vec::new(),
                Some(max_output_tokens),
                &status,
                Duration::ZERO,
                &[],
            );
            assert_eq!(
                output,
                vec![FunctionCallOutputContentItem::InputText {
                    text: format!("Script running with cell ID {cell_id}"),
                }]
            );
            assert!(
                code_mode_result_token_count(output.iter())
                    <= crate::unified_exec::DEFAULT_MAX_OUTPUT_TOKENS
            );
        }
    }
}

#[test]
fn yielded_status_rejects_an_oversized_utf8_recovery_handle_at_tiny_budgets() {
    let oversized = "🦀".repeat(/*n*/ 2_501);
    let response = codex_code_mode::RuntimeResponse::Yielded {
        cell_id: codex_code_mode::CellId::new(oversized.clone()),
        content_items: Vec::new(),
        reason: codex_code_mode::YieldReason::Requested,
        code_mode_host_duration: None,
    };
    let expected_error = format!(
        "code-mode host returned invalid cell ID: cell ID is {} bytes; maximum is {} bytes",
        oversized.len(),
        codex_code_mode::MAX_CELL_ID_BYTES
    );

    for max_output_tokens in [0, 1] {
        let result = script_status(&response).map(|status| {
            finalize_code_mode_result(
                Vec::new(),
                Some(max_output_tokens),
                &status,
                Duration::ZERO,
                &[],
            )
        });
        assert_eq!(result, Err(expected_error.clone()));
    }
    assert!(!expected_error.contains(&oversized));
}

#[test]
fn final_output_keeps_useful_images_but_omits_images_that_exceed_the_hard_cap() {
    let image_url = png_data_url(/*width*/ 64, /*height*/ 64);
    let image = || FunctionCallOutputContentItem::InputImage {
        image_url: image_url.clone(),
        detail: Some(ImageDetail::High),
    };
    let input_image_count = 8;
    let output = finalize_code_mode_result(
        (0..input_image_count).map(|_| image()).collect(),
        None,
        &terminal_status(),
        Duration::ZERO,
        &[],
    );
    let retained_image_count = output
        .iter()
        .filter(|item| matches!(item, FunctionCallOutputContentItem::InputImage { .. }))
        .count();

    assert!(
        retained_image_count > 0,
        "a normal image should remain useful under the default budget"
    );
    assert!(
        retained_image_count < input_image_count,
        "the hard context cap must bound the number of retained images"
    );
    assert!(
        code_mode_result_token_count(output.iter())
            <= crate::unified_exec::DEFAULT_MAX_OUTPUT_TOKENS,
        "image-aware result accounting must enforce the repository context ceiling"
    );
}

#[test]
fn final_output_omits_a_huge_opaque_image_url_by_actual_serialized_size() {
    // Quotes make the serialized value much larger than the raw URL, so this
    // assertion cannot pass with an accountant based only on `image_url.len()`.
    let opaque_url = format!("opaque:{}", "\"".repeat(/*n*/ 30_000));
    let output = finalize_code_mode_result(
        vec![FunctionCallOutputContentItem::InputImage {
            image_url: opaque_url,
            detail: Some(ImageDetail::High),
        }],
        None,
        &terminal_status(),
        Duration::ZERO,
        &[],
    );

    assert!(
        output
            .iter()
            .all(|item| !matches!(item, FunctionCallOutputContentItem::InputImage { .. })),
        "opaque image URLs survive image preparation and must be omitted atomically when oversized"
    );
    let serialized = serde_json::to_vec(&output).expect("serialize complete code-mode result");
    assert!(
        serialized.len()
            <= codex_utils_output_truncation::approx_bytes_for_tokens(
                crate::unified_exec::DEFAULT_MAX_OUTPUT_TOKENS,
            ),
        "complete serialized result exceeded the repository context ceiling"
    );
}

#[test]
fn final_output_omits_a_huge_invalid_base64_image_by_actual_serialized_size() {
    let invalid_image_url = format!("data:image/png;base64,{}", "A".repeat(/*n*/ 60_000));
    let output = finalize_code_mode_result(
        vec![FunctionCallOutputContentItem::InputImage {
            image_url: invalid_image_url,
            detail: Some(ImageDetail::High),
        }],
        None,
        &terminal_status(),
        Duration::ZERO,
        &[],
    );

    assert!(
        output
            .iter()
            .all(|item| !matches!(item, FunctionCallOutputContentItem::InputImage { .. })),
        "invalid image data must not receive multimodal token accounting"
    );
    let serialized = serde_json::to_vec(&output).expect("serialize complete code-mode result");
    assert!(
        serialized.len()
            <= codex_utils_output_truncation::approx_bytes_for_tokens(
                crate::unified_exec::DEFAULT_MAX_OUTPUT_TOKENS,
            ),
        "complete serialized result exceeded the repository context ceiling"
    );
}

#[test]
fn original_detail_image_at_the_hard_cap_is_omitted_after_reserving_the_envelope() {
    let image = FunctionCallOutputContentItem::InputImage {
        image_url: png_data_url(/*width*/ 3_200, /*height*/ 3_200),
        detail: Some(ImageDetail::Original),
    };
    assert!(
        code_mode_result_token_count(std::iter::once(&image))
            > crate::unified_exec::DEFAULT_MAX_OUTPUT_TOKENS,
        "canonical image cost plus serialized item framing should exceed the hard cap"
    );

    let output =
        finalize_code_mode_result(vec![image], None, &terminal_status(), Duration::ZERO, &[]);
    assert!(
        output
            .iter()
            .all(|item| !matches!(item, FunctionCallOutputContentItem::InputImage { .. })),
        "the result envelope must be reserved before an atomic original-detail image"
    );
    assert!(
        code_mode_result_token_count(output.iter())
            <= crate::unified_exec::DEFAULT_MAX_OUTPUT_TOKENS
    );
}

#[test]
fn complete_mixed_output_includes_item_framing_at_tiny_and_global_budgets() {
    let cell_id = codex_code_mode::CellId::new("recoverable-cell".to_string());
    let status = script_status(&codex_code_mode::RuntimeResponse::Yielded {
        cell_id,
        content_items: Vec::new(),
        reason: codex_code_mode::YieldReason::Requested,
        code_mode_host_duration: None,
    })
    .expect("bounded cell ID should produce status");
    let session_ids = (0..crate::unified_exec::MAX_UNIFIED_EXEC_PROCESSES)
        .map(|index| i32::try_from(index).expect("process cap should fit in i32"))
        .collect::<Vec<_>>();
    let serialized_byte_cap = codex_utils_output_truncation::approx_bytes_for_tokens(
        crate::unified_exec::DEFAULT_MAX_OUTPUT_TOKENS,
    );

    for max_output_tokens in [0, 1, crate::unified_exec::DEFAULT_MAX_OUTPUT_TOKENS] {
        let mut content_items = Vec::new();
        for _ in 0..2_000 {
            content_items.push(FunctionCallOutputContentItem::InputText {
                text: String::new(),
            });
            content_items.push(FunctionCallOutputContentItem::InputText {
                text: "x".to_string(),
            });
        }
        for _ in 0..100 {
            content_items.push(FunctionCallOutputContentItem::InputAudio {
                audio_url: "data:audio/wav;base64,UklGRg==".to_string(),
            });
        }
        content_items.push(FunctionCallOutputContentItem::InputImage {
            image_url: png_data_url(/*width*/ 64, /*height*/ 64),
            detail: Some(ImageDetail::High),
        });

        let output = finalize_code_mode_result(
            content_items,
            Some(max_output_tokens),
            &status,
            Duration::ZERO,
            &session_ids,
        );
        let serialized = serde_json::to_vec(&output).expect("serialize complete mixed output");
        assert!(
            serialized.len() <= serialized_byte_cap,
            "serialized output used {} bytes at requested budget {max_output_tokens}",
            serialized.len()
        );
        assert!(
            code_mode_result_token_count(output.iter())
                <= crate::unified_exec::DEFAULT_MAX_OUTPUT_TOKENS,
            "model-visible output exceeded the global cap at requested budget {max_output_tokens}"
        );
        assert!(
            output.iter().any(|item| matches!(
                item,
                FunctionCallOutputContentItem::InputText { text }
                    if text.contains("Script running with cell ID recoverable-cell")
            )),
            "mandatory cell recovery status was lost at requested budget {max_output_tokens}"
        );
        assert!(
            output.iter().any(|item| matches!(
                item,
                FunctionCallOutputContentItem::InputText { text }
                    if text.contains("live_sessions")
            )),
            "mandatory nested-session recovery metadata was lost at requested budget {max_output_tokens}"
        );
    }
}

#[test]
fn final_response_item_including_long_call_id_stays_below_the_global_cap() {
    let status = script_status(&codex_code_mode::RuntimeResponse::Yielded {
        cell_id: codex_code_mode::CellId::new("recoverable-cell".to_string()),
        content_items: Vec::new(),
        reason: codex_code_mode::YieldReason::Requested,
        code_mode_host_duration: None,
    })
    .expect("bounded cell ID should produce status");
    let session_ids = (0..crate::unified_exec::MAX_UNIFIED_EXEC_PROCESSES)
        .map(|index| i32::try_from(index).expect("process cap should fit in i32"))
        .collect::<Vec<_>>();
    let call_id = "c".repeat(8 * 1024);

    for custom_tool in [false, true] {
        for max_output_tokens in [0, 1, crate::unified_exec::DEFAULT_MAX_OUTPUT_TOKENS] {
            let output_kind = if custom_tool {
                CodeModeOutputKind::Execute
            } else {
                CodeModeOutputKind::Wait
            };
            let output = finalize_code_mode_result_with_envelope(
                vec![FunctionCallOutputContentItem::InputText {
                    text: "body".repeat(40_000),
                }],
                Some(max_output_tokens),
                &status,
                Duration::ZERO,
                &session_ids,
                &CodeModeResultEnvelope::new(&call_id, TEST_TURN_ID, output_kind, true),
            );
            let item = final_response_item(output, &call_id, custom_tool);
            let token_bound = item_token_upper_bound(&item);
            assert!(
                token_bound
                    <= i64::try_from(crate::unified_exec::DEFAULT_MAX_OUTPUT_TOKENS)
                        .expect("default cap should fit in i64"),
                "complete response item had a {token_bound}-token upper bound at requested budget \
                 {max_output_tokens}: {item:?}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn fully_prepared_history_items_stay_at_or_below_the_global_cap() {
    let (session, turn) = make_session_and_context().await;
    let call_id = "c".repeat(/*n*/ 256);
    let max_item_tokens = i64::try_from(crate::unified_exec::DEFAULT_MAX_OUTPUT_TOKENS)
        .expect("default cap should fit in i64");

    for output_kind in [CodeModeOutputKind::Execute, CodeModeOutputKind::Wait] {
        for include_image in [false, true] {
            for include_recovery_metadata in [false, true] {
                let mut content = vec![FunctionCallOutputContentItem::InputText {
                    text: "!🦀?,".repeat(40_000),
                }];
                if include_image {
                    content.push(FunctionCallOutputContentItem::InputImage {
                        image_url: png_data_url(/*width*/ 64, /*height*/ 64),
                        detail: Some(ImageDetail::High),
                    });
                }
                let output = finalize_code_mode_result_with_envelope(
                    content,
                    Some(crate::unified_exec::DEFAULT_MAX_OUTPUT_TOKENS),
                    &terminal_status(),
                    Duration::ZERO,
                    &[7],
                    &CodeModeResultEnvelope::new(&call_id, turn.sub_id.as_str(), output_kind, true),
                );
                let custom_tool = matches!(output_kind, CodeModeOutputKind::Execute);
                let item = final_response_item(output, &call_id, custom_tool);
                let (prepared, _) = session
                    .prepare_conversation_items_for_history(&turn, std::slice::from_ref(&item));
                let [prepared] = prepared.as_ref() else {
                    panic!("history preparation changed the code-mode output item count");
                };
                let mut prepared = prepared.clone();
                let expected_metadata = if include_recovery_metadata {
                    prepared.set_tool_call_cell_id("recoverable-cell");
                    prepared.append_executed_tool_calls(vec![ExecutedToolCall::new(
                        "nested_tool".to_string(),
                        serde_json::json!({ "payload": "x".repeat(/*n*/ 256) }),
                    )]);
                    prepared.mark_tool_calls_complete();
                    codex_protocol::models::bound_executed_tool_calls_for_prompt(
                        std::slice::from_mut(&mut prepared),
                    );
                    assert!(
                        item_token_upper_bound(&prepared) > max_item_tokens,
                        "late recovery metadata should exercise the prepared-item cap"
                    );
                    let metadata = prepared
                        .executed_tool_call_metadata()
                        .expect("recovery metadata should be attached")
                        .clone();
                    assert!(bound_prepared_code_mode_output(
                        &mut prepared,
                        crate::unified_exec::DEFAULT_MAX_OUTPUT_TOKENS,
                    ));
                    Some(metadata)
                } else {
                    None
                };
                let token_bound = item_token_upper_bound(&prepared);

                assert!(
                    token_bound <= max_item_tokens,
                    "fully prepared item had a {token_bound}-token upper bound: {prepared:?}"
                );
                assert!(
                    token_bound > max_item_tokens - 128,
                    "boundary case left an unexpectedly large unused budget: {token_bound}"
                );
                assert!(prepared.id().is_some());
                assert_eq!(prepared.turn_id(), Some(turn.sub_id.as_str()));
                let metadata = prepared
                    .executed_tool_call_metadata()
                    .expect("history preparation should stamp output metadata");
                if let Some(expected_metadata) = expected_metadata {
                    assert_eq!(metadata, &expected_metadata);
                } else {
                    assert_eq!(metadata.cell_id, None);
                    assert_eq!(metadata.executed_tool_calls, None);
                    assert_eq!(metadata.tool_calls_complete, None);
                }
                let content_items = match &prepared {
                    ResponseItem::CustomToolCallOutput { output, .. }
                    | ResponseItem::FunctionCallOutput { output, .. } => output
                        .content_items()
                        .expect("status, body, and recovery footer should remain structured"),
                    _ => panic!("prepared item should remain a code-mode output"),
                };
                assert!(matches!(
                    content_items.first(),
                    Some(FunctionCallOutputContentItem::InputText { text })
                        if text.starts_with("Script completed")
                ));
                assert!(matches!(
                    content_items.last(),
                    Some(FunctionCallOutputContentItem::InputText { text })
                        if text.contains("live_sessions")
                ));
                if include_image {
                    assert!(
                        content_items.iter().any(|item| matches!(
                            item,
                            FunctionCallOutputContentItem::InputImage { .. }
                        )),
                        "a useful image was lost even though visible text could be reclaimed"
                    );
                }
            }
        }
    }
}
