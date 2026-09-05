use std::time::Duration;

use codex_code_mode::RuntimeResponse;
use codex_protocol::ResponseItemId;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ImageDetail;
use codex_protocol::models::ResponseItem;
use codex_utils_audio::estimate_audio_token_count;
use codex_utils_image::PromptImageMode;
use codex_utils_image::load_data_url_for_prompt;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::approx_bytes_for_tokens;
use codex_utils_output_truncation::approx_tokens_from_byte_count_i64;
use codex_utils_output_truncation::formatted_truncate_text_content_items_with_policy;
use codex_utils_output_truncation::truncate_function_output_items_with_policy;

use crate::context_manager::estimate_image_bytes;
use crate::unified_exec::DEFAULT_MAX_OUTPUT_TOKENS;
use crate::unified_exec::resolve_max_tokens;

pub(super) struct ScriptStatus {
    text: String,
    requirement: StatusRequirement,
}

#[derive(Clone, Copy)]
pub(super) enum CodeModeOutputKind {
    Execute,
    Wait,
}

pub(super) struct CodeModeResultEnvelope<'a> {
    call_id: &'a str,
    turn_id: &'a str,
    kind: CodeModeOutputKind,
    success: bool,
}

impl<'a> CodeModeResultEnvelope<'a> {
    pub(super) fn new(
        call_id: &'a str,
        turn_id: &'a str,
        kind: CodeModeOutputKind,
        success: bool,
    ) -> Self {
        Self {
            call_id,
            turn_id,
            kind,
            success,
        }
    }

    fn response_item(&self, content_items: &[FunctionCallOutputContentItem]) -> ResponseItem {
        let body = match content_items {
            [FunctionCallOutputContentItem::InputText { text }] => {
                FunctionCallOutputBody::Text(text.clone())
            }
            _ => FunctionCallOutputBody::ContentItems(content_items.to_vec()),
        };
        let output = FunctionCallOutputPayload {
            body,
            success: Some(self.success),
        };
        match self.kind {
            CodeModeOutputKind::Execute => ResponseItem::CustomToolCallOutput {
                id: None,
                call_id: self.call_id.to_string(),
                name: None,
                output,
                internal_chat_message_metadata_passthrough: None,
            },
            CodeModeOutputKind::Wait => ResponseItem::FunctionCallOutput {
                id: None,
                call_id: Some(self.call_id.to_string()),
                name: None,
                namespace: None,
                output,
                internal_chat_message_metadata_passthrough: None,
            },
        }
    }

    fn prepared_response_item(
        &self,
        content_items: &[FunctionCallOutputContentItem],
    ) -> ResponseItem {
        let mut item = self.response_item(content_items);
        let prefix = item
            .id_prefix()
            .expect("code-mode output items always support response IDs");
        item.set_id(Some(ResponseItemId::with_suffix(
            prefix,
            "00000000-0000-0000-0000-000000000000",
        )));
        item.set_turn_id_if_missing(self.turn_id);
        // History adds a fractional Unix timestamp. f64::MAX has a longer JSON
        // representation than any real Unix timestamp and therefore makes this
        // estimate conservative across the later history-preparation boundary.
        item.set_create_time_if_missing(
            serde_json::Number::from_f64(f64::MAX)
                .unwrap_or_else(|| serde_json::Number::from(u64::MAX)),
        );
        item
    }

    fn token_count(&self, content_items: &[FunctionCallOutputContentItem]) -> usize {
        usize::try_from(crate::context_manager::estimate_item_token_count(
            &self.response_item(content_items),
        ))
        .unwrap_or(usize::MAX)
    }

    fn prepared_token_upper_bound(&self, content_items: &[FunctionCallOutputContentItem]) -> usize {
        usize::try_from(crate::context_manager::item_token_upper_bound(
            &self.prepared_response_item(content_items),
        ))
        .unwrap_or(usize::MAX)
    }
}

#[derive(Clone, Copy)]
enum StatusRequirement {
    RecoveryHandle,
    BestEffort,
}

pub(super) fn script_status(response: &RuntimeResponse) -> Result<ScriptStatus, String> {
    super::cell_validation::validate_host_runtime_response(response)?;
    Ok(match response {
        RuntimeResponse::Yielded { cell_id, .. } => ScriptStatus {
            text: format!("Script running with cell ID {cell_id}"),
            requirement: StatusRequirement::RecoveryHandle,
        },
        RuntimeResponse::Terminated { .. } => ScriptStatus {
            text: "Script terminated".to_string(),
            requirement: StatusRequirement::BestEffort,
        },
        RuntimeResponse::Result { error_text, .. } => ScriptStatus {
            text: if error_text.is_none() {
                "Script completed".to_string()
            } else {
                "Script failed".to_string()
            },
            requirement: StatusRequirement::BestEffort,
        },
    })
}

pub(super) fn host_failure_status() -> ScriptStatus {
    ScriptStatus {
        text: "Script failed".to_string(),
        requirement: StatusRequirement::BestEffort,
    }
}

pub(super) fn finalize_code_mode_result(
    content_items: Vec<FunctionCallOutputContentItem>,
    max_output_tokens: Option<usize>,
    status: &ScriptStatus,
    wall_time: Duration,
    session_ids: &[i32],
    response_envelope: &CodeModeResultEnvelope<'_>,
) -> Vec<FunctionCallOutputContentItem> {
    let max_output_tokens = resolve_max_tokens(max_output_tokens).min(DEFAULT_MAX_OUTPUT_TOKENS);
    let metadata = live_exec_sessions_metadata(session_ids);
    let full_header = script_status_header(&status.text, wall_time);
    let full_footer = metadata.as_deref().map(live_exec_sessions_footer);
    let full_envelope = assemble_result(Some(full_header.clone()), Vec::new(), full_footer.clone());
    let full_envelope_tokens = response_envelope.token_count(&full_envelope);

    if full_envelope_tokens <= max_output_tokens {
        return assemble_result_to_budget(
            Some(full_header),
            content_items,
            full_footer,
            max_output_tokens,
            response_envelope,
        );
    }

    let compact_status = FunctionCallOutputContentItem::InputText {
        text: status.text.clone(),
    };
    let compact_footer = metadata.map(|text| FunctionCallOutputContentItem::InputText { text });
    let mandatory_header = matches!(status.requirement, StatusRequirement::RecoveryHandle)
        .then_some(compact_status.clone());
    let mandatory_envelope =
        assemble_result(mandatory_header.clone(), Vec::new(), compact_footer.clone());
    let mandatory_tokens = response_envelope.token_count(&mandatory_envelope);
    let optional_header = matches!(status.requirement, StatusRequirement::BestEffort)
        .then(|| {
            let candidate = assemble_result(
                Some(compact_status.clone()),
                Vec::new(),
                compact_footer.clone(),
            );
            (response_envelope.token_count(&candidate) <= max_output_tokens)
                .then_some(compact_status)
        })
        .flatten();
    let header = mandatory_header.or(optional_header);
    let envelope = assemble_result(header.clone(), Vec::new(), compact_footer.clone());
    let envelope_tokens = response_envelope.token_count(&envelope);

    // Cell IDs and nested process IDs are recovery handles. Their registries
    // bound this compact envelope, so it remains under the repository-wide
    // ceiling even when a caller requests an impossibly small output budget.
    // Terminal status text is best-effort and never causes such an overage.
    let output_budget = max_output_tokens.max(mandatory_tokens);
    if envelope_tokens > output_budget {
        return envelope;
    }
    assemble_result_to_budget(
        header,
        content_items,
        compact_footer,
        output_budget,
        response_envelope,
    )
}

fn assemble_result_to_budget(
    header: Option<FunctionCallOutputContentItem>,
    body: Vec<FunctionCallOutputContentItem>,
    footer: Option<FunctionCallOutputContentItem>,
    max_output_tokens: usize,
    response_envelope: &CodeModeResultEnvelope<'_>,
) -> Vec<FunctionCallOutputContentItem> {
    let envelope = assemble_result(header.clone(), Vec::new(), footer.clone());
    let envelope_tokens = response_envelope.token_count(&envelope);
    let prepared_envelope_upper_bound = response_envelope.prepared_token_upper_bound(&envelope);
    if envelope_tokens >= max_output_tokens
        || prepared_envelope_upper_bound >= DEFAULT_MAX_OUTPUT_TOKENS
    {
        return envelope;
    }

    let mut lower_body_budget = 0_usize;
    let mut upper_body_budget = max_output_tokens.saturating_sub(envelope_tokens);
    let mut best = envelope;
    while lower_body_budget <= upper_body_budget {
        let body_budget = lower_body_budget + (upper_body_budget - lower_body_budget) / 2;
        let candidate = assemble_result(
            header.clone(),
            truncate_code_mode_result_to_budget(body.clone(), body_budget),
            footer.clone(),
        );
        let candidate_tokens = response_envelope.token_count(&candidate);
        let prepared_candidate_upper_bound =
            response_envelope.prepared_token_upper_bound(&candidate);
        if candidate_tokens <= max_output_tokens
            && prepared_candidate_upper_bound <= DEFAULT_MAX_OUTPUT_TOKENS
        {
            best = candidate;
            lower_body_budget = body_budget.saturating_add(1);
        } else if body_budget == 0 {
            break;
        } else {
            upper_body_budget = body_budget - 1;
        }
    }
    best
}

pub(crate) fn bound_prepared_code_mode_output(
    item: &mut ResponseItem,
    max_output_tokens: usize,
) -> bool {
    if !item
        .executed_tool_call_metadata()
        .and_then(|metadata| metadata.cell_id.as_deref())
        .is_some_and(|cell_id| !cell_id.is_empty())
    {
        return false;
    }
    let Some(output) = code_mode_output(item) else {
        return false;
    };
    let content_items = match &output.body {
        FunctionCallOutputBody::Text(text) => {
            vec![FunctionCallOutputContentItem::InputText { text: text.clone() }]
        }
        FunctionCallOutputBody::ContentItems(content_items) => content_items.clone(),
    };
    let (header, body, footer) = split_finalized_result(content_items);
    let envelope = assemble_result(header.clone(), Vec::new(), footer.clone());
    let envelope_item = with_code_mode_output(item, envelope);
    let max_output_token_bound = i64::try_from(max_output_tokens).unwrap_or(i64::MAX);
    if crate::context_manager::item_token_upper_bound(&envelope_item) > max_output_token_bound {
        return false;
    }

    let mut lower_body_budget = 0_usize;
    let mut upper_body_budget = max_output_tokens;
    let mut best = envelope_item;
    while lower_body_budget <= upper_body_budget {
        let body_budget = lower_body_budget + (upper_body_budget - lower_body_budget) / 2;
        let candidate_content = assemble_result(
            header.clone(),
            truncate_code_mode_result_to_budget(body.clone(), body_budget),
            footer.clone(),
        );
        let candidate = with_code_mode_output(item, candidate_content);
        if crate::context_manager::item_token_upper_bound(&candidate) <= max_output_token_bound {
            best = candidate;
            lower_body_budget = body_budget.saturating_add(1);
        } else if body_budget == 0 {
            break;
        } else {
            upper_body_budget = body_budget - 1;
        }
    }
    *item = best;
    true
}

fn code_mode_output(item: &ResponseItem) -> Option<&FunctionCallOutputPayload> {
    match item {
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => Some(output),
        _ => None,
    }
}

fn with_code_mode_output(
    item: &ResponseItem,
    content_items: Vec<FunctionCallOutputContentItem>,
) -> ResponseItem {
    let mut candidate = item.clone();
    let output = match &mut candidate {
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => output,
        _ => unreachable!("code-mode output was checked before constructing a candidate"),
    };
    output.body = match content_items.as_slice() {
        [FunctionCallOutputContentItem::InputText { text }] => {
            FunctionCallOutputBody::Text(text.clone())
        }
        _ => FunctionCallOutputBody::ContentItems(content_items),
    };
    candidate
}

fn split_finalized_result(
    mut content_items: Vec<FunctionCallOutputContentItem>,
) -> (
    Option<FunctionCallOutputContentItem>,
    Vec<FunctionCallOutputContentItem>,
    Option<FunctionCallOutputContentItem>,
) {
    let header = content_items
        .first()
        .is_some_and(is_script_status_item)
        .then(|| content_items.remove(0));
    let footer = content_items
        .last()
        .is_some_and(is_live_exec_sessions_item)
        .then(|| content_items.pop())
        .flatten();
    (header, content_items, footer)
}

fn is_script_status_item(item: &FunctionCallOutputContentItem) -> bool {
    let FunctionCallOutputContentItem::InputText { text } = item else {
        return false;
    };
    text.starts_with("Script running with cell ID ")
        || text.starts_with("Script completed")
        || text.starts_with("Script failed")
        || text.starts_with("Script terminated")
}

fn is_live_exec_sessions_item(item: &FunctionCallOutputContentItem) -> bool {
    let FunctionCallOutputContentItem::InputText { text } = item else {
        return false;
    };
    text.starts_with("\nLive nested exec sessions remain recoverable:\n")
        || text.starts_with("{\"live_sessions\":")
}

fn assemble_result(
    header: Option<FunctionCallOutputContentItem>,
    body: Vec<FunctionCallOutputContentItem>,
    footer: Option<FunctionCallOutputContentItem>,
) -> Vec<FunctionCallOutputContentItem> {
    let mut finalized = Vec::with_capacity(
        body.len()
            .saturating_add(usize::from(header.is_some()))
            .saturating_add(usize::from(footer.is_some())),
    );
    finalized.extend(header);
    finalized.extend(body);
    finalized.extend(footer);
    finalized
}

fn script_status_header(status: &str, wall_time: Duration) -> FunctionCallOutputContentItem {
    let wall_time_seconds = ((wall_time.as_secs_f32()) * 10.0).round() / 10.0;
    FunctionCallOutputContentItem::InputText {
        text: format!("{status}\nWall time {wall_time_seconds:.1} seconds\nOutput:\n"),
    }
}

fn live_exec_sessions_metadata(session_ids: &[i32]) -> Option<String> {
    if session_ids.is_empty() {
        return None;
    }

    let sessions = session_ids
        .iter()
        .map(|session_id| serde_json::json!({ "session_id": session_id }))
        .collect::<Vec<_>>();
    Some(serde_json::json!({ "live_sessions": sessions }).to_string())
}

fn live_exec_sessions_footer(metadata: &str) -> FunctionCallOutputContentItem {
    FunctionCallOutputContentItem::InputText {
        text: format!(
            "\nLive nested exec sessions remain recoverable:\n{metadata}\n\
             Continue them with wait_process or write_stdin using the original session_id."
        ),
    }
}

fn truncate_code_mode_result_to_budget(
    items: Vec<FunctionCallOutputContentItem>,
    max_output_tokens: usize,
) -> Vec<FunctionCallOutputContentItem> {
    if code_mode_result_token_count(items.iter()) <= max_output_tokens {
        return items;
    }

    let mut truncation_budget = max_output_tokens;
    loop {
        let truncated = truncate_code_mode_result(items.clone(), Some(truncation_budget));
        let truncated_tokens = code_mode_result_token_count(truncated.iter());
        if truncated_tokens <= max_output_tokens {
            return truncated;
        }
        if truncation_budget == 0 {
            return items
                .into_iter()
                .filter(|item| code_mode_result_token_count(std::iter::once(item)) == 0)
                .collect();
        }
        truncation_budget = truncation_budget
            .saturating_sub(truncated_tokens.saturating_sub(max_output_tokens))
            .min(truncation_budget.saturating_sub(1));
    }
}

fn code_mode_result_token_count<'a>(
    items: impl IntoIterator<Item = &'a FunctionCallOutputContentItem>,
) -> usize {
    let model_visible_bytes = items.into_iter().fold(0_usize, |total, item| {
        // Two bytes per item cover the enclosing array's comma/bracket framing
        // for every non-empty result while preserving a zero cost for no items.
        total
            .saturating_add(content_item_model_visible_bytes(item))
            .saturating_add(2)
    });
    usize::try_from(approx_tokens_from_byte_count_i64(
        i64::try_from(model_visible_bytes).unwrap_or(i64::MAX),
    ))
    .unwrap_or(usize::MAX)
}

fn content_item_model_visible_bytes(item: &FunctionCallOutputContentItem) -> usize {
    let serialized_bytes = serde_json::to_vec(item).map_or(usize::MAX, |item| item.len());
    match item {
        FunctionCallOutputContentItem::InputImage { image_url, detail } => {
            validated_base64_image_payload(image_url, *detail).map_or(serialized_bytes, |payload| {
                let image_bytes =
                    usize::try_from(estimate_image_bytes(image_url, *detail)).unwrap_or(usize::MAX);
                serialized_bytes
                    .saturating_sub(payload.len())
                    .saturating_add(image_bytes)
            })
        }
        FunctionCallOutputContentItem::InputAudio { audio_url } => {
            parse_base64_audio_data_url(audio_url).map_or(serialized_bytes, |payload| {
                serialized_bytes
                    .saturating_sub(payload.len())
                    .saturating_add(approx_bytes_for_tokens(estimate_audio_token_count(
                        audio_url,
                    )))
            })
        }
        FunctionCallOutputContentItem::InputText { .. }
        | FunctionCallOutputContentItem::EncryptedContent { .. } => serialized_bytes,
    }
}

fn parse_base64_image_data_url(url: &str) -> Option<&str> {
    parse_base64_data_url(url, "image/")
}

fn validated_base64_image_payload(image_url: &str, detail: Option<ImageDetail>) -> Option<&str> {
    let payload = parse_base64_image_data_url(image_url)?;
    let mode = match detail {
        Some(ImageDetail::Original) => PromptImageMode::ORIGINAL_DETAIL,
        Some(ImageDetail::Auto | ImageDetail::Low | ImageDetail::High) | None => {
            PromptImageMode::HIGH_DETAIL
        }
    };
    load_data_url_for_prompt(image_url, mode).ok()?;
    Some(payload)
}

fn parse_base64_audio_data_url(url: &str) -> Option<&str> {
    parse_base64_data_url(url, "audio/")
}

fn parse_base64_data_url<'a>(url: &'a str, media_type_prefix: &str) -> Option<&'a str> {
    if !url
        .get(.."data:".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
    {
        return None;
    }
    let (metadata, payload) = url.split_once(',')?;
    let mut metadata_parts = metadata["data:".len()..].split(';');
    let mime_type = metadata_parts.next().unwrap_or_default();
    let has_base64_marker = metadata_parts.any(|part| part.eq_ignore_ascii_case("base64"));
    if !mime_type
        .get(..media_type_prefix.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(media_type_prefix))
        || !has_base64_marker
    {
        return None;
    }
    Some(payload)
}

fn truncate_code_mode_result(
    items: Vec<FunctionCallOutputContentItem>,
    max_output_tokens: Option<usize>,
) -> Vec<FunctionCallOutputContentItem> {
    let max_output_tokens = resolve_max_tokens(max_output_tokens);
    let policy = TruncationPolicy::Tokens(max_output_tokens);
    if items
        .iter()
        .all(|item| matches!(item, FunctionCallOutputContentItem::InputText { .. }))
    {
        let (truncated_items, _) =
            formatted_truncate_text_content_items_with_policy(&items, policy);
        return truncated_items;
    }

    let mut remaining_tokens = max_output_tokens;
    let mut bounded_items = Vec::with_capacity(items.len());
    let mut omitted_images = 0_usize;
    for item in items {
        if let FunctionCallOutputContentItem::InputImage { .. } = &item {
            let image_tokens = code_mode_result_token_count(std::iter::once(&item));
            if image_tokens > remaining_tokens {
                omitted_images = omitted_images.saturating_add(1);
                continue;
            }
            remaining_tokens -= image_tokens;
        }
        bounded_items.push(item);
    }
    if omitted_images > 0 {
        bounded_items.push(FunctionCallOutputContentItem::InputText {
            text: format!("[omitted {omitted_images} image items ...]"),
        });
    }

    truncate_function_output_items_with_policy(
        &bounded_items,
        TruncationPolicy::Tokens(remaining_tokens),
        estimate_audio_token_count,
    )
}

#[cfg(test)]
#[path = "result_tests.rs"]
mod tests;
