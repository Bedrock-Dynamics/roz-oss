//! ChatGPT-backend transforms (numman-ali `lib/request/*`, MIT, SHA bec2ad69).
//!
//! These transforms apply ONLY on the Responses+OAuth cell. API-key path uses
//! the Responses wire directly without these modifications.
//!
//! Each function is cited to `request-transformer.ts` / `fetch-helpers.ts` /
//! `constants.ts` / `response-handler.ts` line ranges.
//!
//! # Transform Coverage (per RESEARCH.md §Transform Rules T1-T20)
//!
//! Body transforms (applied via [`apply_chatgpt_backend_transforms`]):
//!
//! - T1  `store = false` — `request-transformer.ts:450`
//! - T2  `stream = true` — `request-transformer.ts:452`
//! - T3  inject `instructions` — `request-transformer.ts:453`
//! - T4  strip IDs from input items — `request-transformer.ts:323-330`
//! - T5  remove `item_reference` items — `request-transformer.ts:318-322`
//! - T6  fixup orphaned `function_call_output` → Message — `helpers/input-utils.ts:175-210`
//! - T7  ensure `reasoning.encrypted_content` in `include` — `request-transformer.ts:166-178`
//! - T8  `text.verbosity = medium` default — `request-transformer.ts:516-519`
//! - T9  `reasoning` per model family — `request-transformer.ts:192-286`
//! - T10 unset `max_output_tokens` — `request-transformer.ts:527` (field absent in Roz wire type;
//!   documented no-op here, nothing to unset)
//! - T11 unset `max_completion_tokens` — `request-transformer.ts:528` (field absent in Roz wire
//!   type; documented no-op)
//! - T12 normalize model name — `request-transformer.ts:32-113`
//!
//! URL + header transforms (applied at request build time by the client layer):
//!
//! - T13 URL rewrite `/responses` → `/codex/responses` — [`rewrite_url_for_chatgpt`]
//! - T14-T19 headers — [`chatgpt_backend_headers`] returning [`ChatGptBackendHeaders`]
//!
//! Non-streaming synthesis:
//!
//! - T20 SSE → single JSON body — [`collect_stream_to_json`]

use crate::client::ResponseEventStream;
use crate::error::OpenAiError;
use crate::wire::events::ResponseEvent;
use crate::wire::responses::{
    OpenAiVerbosity, Reasoning, ReasoningEffort, ReasoningSummaryLevel, ResponseItem, ResponsesApiRequest, TextControls,
};
use futures::StreamExt;
use secrecy::{ExposeSecret, SecretString};

/// Key under `body.include` requesting the ChatGPT backend to emit encrypted reasoning content.
const INCLUDE_REASONING_ENCRYPTED_CONTENT: &str = "reasoning.encrypted_content";

// ============================================================================
// T1-T12: Body transforms
// ============================================================================

/// Apply every body-level ChatGPT-backend transform (T1-T12) to `req` in place.
///
/// The URL + header transforms (T13-T19) and non-streaming synthesis (T20) are separate helpers
/// below — the client layer applies the URL/header helpers at request build time, and the
/// adapter layer (Plan 19-10) applies `collect_stream_to_json` when the caller asked for a
/// non-streaming response.
///
/// `codex_instructions_text` is the model-family prompt string resolved by the caller — Plan
/// 19-09 owns the real snapshot; this function simply injects whatever string the caller
/// provides.
pub fn apply_chatgpt_backend_transforms(req: &mut ResponsesApiRequest, codex_instructions_text: &str) {
    // T1: store = false (`request-transformer.ts:450`)
    req.store = false;
    // T2: stream = true (`request-transformer.ts:452`)
    req.stream = true;
    // T3: inject codex instructions (`request-transformer.ts:453`).
    req.instructions = codex_instructions_text.to_string();
    // T4 + T5: strip IDs from input[] items; remove item_reference type
    // (`request-transformer.ts:318-322, 323-330`).
    filter_input_items(&mut req.input);
    // T6: fixup orphaned function_call_output (`helpers/input-utils.ts:175-210`).
    fixup_orphaned_tool_outputs(&mut req.input);
    // T7: ensure reasoning.encrypted_content is in include[] (`request-transformer.ts:166-178`).
    resolve_include(&mut req.include);
    // T8: text.verbosity = medium default (`request-transformer.ts:516-519`).
    set_text_verbosity_default(&mut req.text);
    // T9: body.reasoning per model family (`request-transformer.ts:192-286`).
    set_reasoning_by_model_family(&mut req.reasoning, &req.model);
    // T10 + T11: unset max_output_tokens / max_completion_tokens (`request-transformer.ts:527-528`).
    // Both fields are absent from the Roz wire type — documented no-op. Nothing to unset.
    // T12: normalize model name defensively (`request-transformer.ts:32-113`).
    req.model = normalize_model_name(&req.model).to_string();
}

/// T4 + T5: strip `id` fields from every `input[]` item and remove `item_reference` items.
///
/// Cite `request-transformer.ts:318-322` (remove item_reference) and `:323-330` (filterInput).
/// ChatGPT backend rejects request-level IDs when `store: false` because it cannot re-hydrate
/// them; `item_reference` is an AI SDK construct not present in the spec.
fn filter_input_items(input: &mut Vec<ResponseItem>) {
    input.retain(|item| !matches!(item, ResponseItem::ItemReference { .. }));
    for item in input.iter_mut() {
        match item {
            ResponseItem::Message { id, .. }
            | ResponseItem::FunctionCall { id, .. }
            | ResponseItem::Reasoning { id, .. } => {
                *id = None;
            }
            // FunctionCallOutput has no id field on our wire type; ItemReference is already
            // filtered above by retain. Nothing to strip for either.
            ResponseItem::FunctionCallOutput { .. } | ResponseItem::ItemReference { .. } => {}
        }
    }
}

/// T6: convert orphaned `function_call_output` items to plain assistant messages (never drop).
///
/// Cite `helpers/input-utils.ts:175-210`. Two-pass:
/// 1. Collect every `FunctionCall::call_id` present in the input.
/// 2. Replace each `FunctionCallOutput` whose `call_id` is NOT in the set with a `Message` whose
///    content is a single `output_text` block stringifying the original output.
///
/// Preserves context without tripping schema validation on the backend.
fn fixup_orphaned_tool_outputs(input: &mut [ResponseItem]) {
    // Pass 1: gather live call_ids from FunctionCall items.
    let live_call_ids: std::collections::HashSet<String> = input
        .iter()
        .filter_map(|item| {
            if let ResponseItem::FunctionCall { call_id, .. } = item {
                Some(call_id.clone())
            } else {
                None
            }
        })
        .collect();

    // Pass 2: replace orphans in place.
    for item in input.iter_mut() {
        if let ResponseItem::FunctionCallOutput { call_id, output } = item
            && !live_call_ids.contains(call_id)
        {
            let replacement = ResponseItem::Message {
                id: None,
                role: "assistant".to_string(),
                content: vec![serde_json::json!({
                    "type": "output_text",
                    "text": output.clone(),
                })],
            };
            *item = replacement;
        }
    }
}

/// T7: ensure `reasoning.encrypted_content` is in the `include[]` vec (dedupe).
///
/// Cite `request-transformer.ts:166-178`.
fn resolve_include(include: &mut Vec<String>) {
    if !include.iter().any(|s| s == INCLUDE_REASONING_ENCRYPTED_CONTENT) {
        include.push(INCLUDE_REASONING_ENCRYPTED_CONTENT.to_string());
    }
}

/// T8: default `text.verbosity` to `Medium` when absent.
///
/// Cite `request-transformer.ts:516-519`. Codex CLI default is `medium`, NOT opencode's `low`.
fn set_text_verbosity_default(text: &mut Option<TextControls>) {
    match text {
        None => {
            *text = Some(TextControls {
                verbosity: Some(OpenAiVerbosity::Medium),
                format: None,
            });
        }
        Some(ctrl) => {
            if ctrl.verbosity.is_none() {
                ctrl.verbosity = Some(OpenAiVerbosity::Medium);
            }
        }
    }
}

/// T9: set `reasoning` per model family — effort + summary.
///
/// Cite `request-transformer.ts:192-286` (`getReasoningConfig`). gpt-5.2/gpt-5.1 families (both
/// `-codex` and non-`-codex`) get `effort=medium, summary=auto`. Other models keep whatever the
/// caller already set.
fn set_reasoning_by_model_family(reasoning: &mut Option<Reasoning>, model: &str) {
    let is_codex_family = matches!(
        model,
        "gpt-5.2-codex" | "gpt-5.1-codex-max" | "gpt-5.1-codex-mini" | "gpt-5.1-codex"
    );
    let is_gpt5_family = matches!(model, "gpt-5.2" | "gpt-5.1") || is_codex_family;

    if is_gpt5_family {
        *reasoning = Some(Reasoning {
            effort: Some(ReasoningEffort::Medium),
            summary: Some(ReasoningSummaryLevel::Auto),
        });
    }
    // Otherwise leave as-is.
}

/// T12: normalize model name via an explicit-map-first then pattern-ladder fallback.
///
/// Cite `request-transformer.ts:32-113`. Returns a `&str` borrowed from a `'static` set of
/// known canonical names — no allocation.
#[must_use]
#[allow(
    clippy::match_same_arms,
    reason = "explicit map-first-then-pattern-fallback ladder matches upstream request-transformer.ts:32-113; merging gpt-5.1-codex-mini arms loses the fidelity note"
)]
pub fn normalize_model_name(model: &str) -> &str {
    match model {
        "gpt-5.2-codex" => "gpt-5.2-codex",
        "gpt-5.1-codex-max" => "gpt-5.1-codex-max",
        "gpt-5.1-codex-mini" => "gpt-5.1-codex-mini",
        "codex-mini-latest" => "gpt-5.1-codex-mini",
        x if x.starts_with("gpt-5.1-codex") => "gpt-5.1-codex",
        x if x.starts_with("gpt-5.1") => "gpt-5.1",
        x if x.contains("codex") => "gpt-5.1-codex",
        x if x.starts_with("gpt-5") => "gpt-5.1",
        _ => "gpt-5.1",
    }
}

// ============================================================================
// T13-T19: URL + header transforms
// ============================================================================

/// T13: rewrite `/responses` → `/codex/responses` when ChatGPT-backend.
///
/// Cite `fetch-helpers.ts:87-89` + `constants.ts:42-45`. If `base_url` already ends with
/// `/responses` it is rewritten in place; otherwise `/codex/responses` is appended.
#[must_use]
pub fn rewrite_url_for_chatgpt(base_url: &str) -> String {
    base_url.strip_suffix("/responses").map_or_else(
        || format!("{}/codex/responses", base_url.trim_end_matches('/')),
        |prefix| format!("{prefix}/codex/responses"),
    )
}

/// T14-T19: headers attached to ChatGPT-backend outbound requests.
///
/// Construct via [`chatgpt_backend_headers`]. The caller attaches these to the outbound
/// `reqwest::RequestBuilder` (see Plan 19-10 adapter).
#[derive(Debug, Clone)]
pub struct ChatGptBackendHeaders {
    /// T14 — `"Bearer {access_token}"`. `x-api-key` must NOT be sent alongside.
    pub authorization: String,
    /// T15 — `chatgpt-account-id: <jwt.chatgpt_account_id>`. Omit header when None.
    pub chatgpt_account_id: Option<String>,
    /// T16 — `OpenAI-Beta: responses=experimental`.
    pub openai_beta: &'static str,
    /// T17 — `originator: <value>`. Default `"roz"` per RESEARCH.md A9; if the ChatGPT backend
    /// rejects (observed 400/403 mentioning `originator`), fall back to `"codex_cli_rs"` — a
    /// caller-side concern, not this function's.
    pub originator: &'static str,
    /// T18 — `accept: text/event-stream`.
    pub accept: &'static str,
    /// T19 — `conversation_id: <...>` derived from `prompt_cache_key`. None when absent.
    pub conversation_id: Option<String>,
    /// T19 — `session_id: <...>` derived from `prompt_cache_key`. None when absent.
    pub session_id: Option<String>,
}

/// Constant value for T16 (`OpenAI-Beta` header).
pub const OPENAI_BETA_RESPONSES_EXPERIMENTAL: &str = "responses=experimental";

/// Constant value for T17 (`originator` header, default per RESEARCH.md A9).
///
/// If the ChatGPT backend rejects with a 400/403 mentioning `originator`, fall back to
/// `"codex_cli_rs"` at the call site (codex-rs ships that string).
pub const ORIGINATOR_ROZ: &str = "roz";

/// Constant value for T18 (`accept` header).
pub const ACCEPT_EVENT_STREAM: &str = "text/event-stream";

/// Construct the ChatGPT-backend header bundle (T14-T19).
///
/// Originator default = `"roz"` per RESEARCH.md A9. `prompt_cache_key` conventions (T19):
/// - `Some("<conversation>:<session>")` → conversation_id, session_id split on the first `:`.
/// - `Some("<conversation>")` (no delimiter) → conversation_id only, session_id = None.
/// - `None` → neither header emitted (DO NOT set empty strings).
#[must_use]
pub fn chatgpt_backend_headers(
    access_token: &SecretString,
    account_id: Option<&str>,
    prompt_cache_key: Option<&str>,
) -> ChatGptBackendHeaders {
    let authorization = format!("Bearer {}", access_token.expose_secret());

    let (conversation_id, session_id) = prompt_cache_key.map_or((None, None), |raw| {
        raw.split_once(':').map_or_else(
            || (Some(raw.to_string()), None),
            |(conv, sess)| (Some(conv.to_string()), Some(sess.to_string())),
        )
    });

    ChatGptBackendHeaders {
        authorization,
        chatgpt_account_id: account_id.map(str::to_owned),
        openai_beta: OPENAI_BETA_RESPONSES_EXPERIMENTAL,
        originator: ORIGINATOR_ROZ,
        accept: ACCEPT_EVENT_STREAM,
        conversation_id,
        session_id,
    }
}

// ============================================================================
// T20: Non-streaming synthesis
// ============================================================================

/// T20: consume a full [`ResponseEventStream`] and synthesize a single JSON response.
///
/// Cite `response-handler.ts:9-86` (`parseSseStream` + `convertSseToJson`). Matches the shape of
/// a non-streaming Responses API body so callers that expected a single JSON back from a
/// non-streaming request can keep working after the mandatory streaming upgrade (T2).
///
/// Output shape (per `response-handler.ts:49-86`):
///
/// ```json
/// {
///   "id": "<response_id | null>",
///   "object": "response",
///   "output": [
///     { "type": "message", "role": "assistant",
///       "content": [{ "type": "output_text", "text": "<accumulated>" }] },
///     { "type": "reasoning", "summary": [{ "type": "summary_text", "text": "<accumulated>" }] },
///     { "type": "function_call", "call_id": "...", "name": "...", "arguments": "..." }
///   ],
///   "usage": { "input_tokens": N, "output_tokens": N, ... } | null
/// }
/// ```
///
/// `output[]` only includes sections the stream actually produced — e.g. if no reasoning
/// delta arrived, no `reasoning` entry is emitted.
pub async fn collect_stream_to_json(mut stream: ResponseEventStream) -> Result<serde_json::Value, OpenAiError> {
    let mut output_text = String::new();
    let mut reasoning_summary = String::new();
    let mut tool_calls: Vec<ToolCallAccumulator> = Vec::new();
    let mut active_tool_index: Option<usize> = None;
    let mut response_id: Option<String> = None;
    let mut usage_value: Option<serde_json::Value> = None;
    let mut finished_items: Vec<ResponseItem> = Vec::new();

    while let Some(event) = stream.next().await {
        match event? {
            // Created, ServerReasoningIncluded, and ReasoningContentDelta are all intentionally
            // ignored in the non-streaming synthesis path: the former two carry no payload
            // relevant to the synthesized body, and raw reasoning content stays invisible to
            // the caller per codex conventions (summary block is the public surface).
            ResponseEvent::Created
            | ResponseEvent::ServerReasoningIncluded(_)
            | ResponseEvent::ReasoningContentDelta { .. } => {}
            ResponseEvent::OutputTextDelta(delta) => output_text.push_str(&delta),
            ResponseEvent::ReasoningSummaryDelta { delta, .. } => reasoning_summary.push_str(&delta),
            ResponseEvent::ToolCallStart { id, name } => {
                tool_calls.push(ToolCallAccumulator {
                    call_id: id,
                    name,
                    arguments: String::new(),
                });
                active_tool_index = Some(tool_calls.len() - 1);
            }
            ResponseEvent::ToolCallArgsDelta(delta) => {
                if let Some(idx) = active_tool_index
                    && let Some(acc) = tool_calls.get_mut(idx)
                {
                    acc.arguments.push_str(&delta);
                }
            }
            ResponseEvent::OutputItemDone(item) => {
                // Retain fully-formed items (chiefly FunctionCall with final arguments).
                finished_items.push(item);
            }
            ResponseEvent::Completed {
                response_id: rid,
                token_usage,
            } => {
                response_id = rid;
                usage_value = token_usage.map(|u| {
                    let mut obj = serde_json::json!({
                        "input_tokens": u.input_tokens,
                        "output_tokens": u.output_tokens,
                    });
                    if let Some(v) = u.cached_input_tokens {
                        obj["cached_input_tokens"] = serde_json::Value::from(v);
                    }
                    if let Some(v) = u.reasoning_output_tokens {
                        obj["reasoning_output_tokens"] = serde_json::Value::from(v);
                    }
                    obj
                });
            }
        }
    }

    // Assemble output[]. Prefer finished_items (server-authoritative) for FunctionCall; fall
    // back to the streaming accumulators when a final item never arrived.
    let mut output: Vec<serde_json::Value> = Vec::new();

    if !output_text.is_empty() {
        output.push(serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": output_text }],
        }));
    }

    if !reasoning_summary.is_empty() {
        output.push(serde_json::json!({
            "type": "reasoning",
            "summary": [{ "type": "summary_text", "text": reasoning_summary }],
        }));
    }

    // Collect finalized FunctionCall items; track their call_ids so we can skip the matching
    // streaming accumulators.
    let mut finalized_call_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for item in &finished_items {
        if let ResponseItem::FunctionCall {
            call_id,
            name,
            arguments,
            ..
        } = item
        {
            finalized_call_ids.insert(call_id.clone());
            output.push(serde_json::json!({
                "type": "function_call",
                "call_id": call_id,
                "name": name,
                "arguments": arguments,
            }));
        }
    }
    for acc in tool_calls {
        if finalized_call_ids.contains(&acc.call_id) {
            continue;
        }
        output.push(serde_json::json!({
            "type": "function_call",
            "call_id": acc.call_id,
            "name": acc.name,
            "arguments": acc.arguments,
        }));
    }

    Ok(serde_json::json!({
        "id": response_id,
        "object": "response",
        "output": output,
        "usage": usage_value,
    }))
}

struct ToolCallAccumulator {
    call_id: String,
    name: String,
    arguments: String,
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::events::TokenUsage;
    use crate::wire::responses::{TextFormat, TextFormatType};
    use futures::stream;
    use serde_json::json;

    fn base_request(model: &str) -> ResponsesApiRequest {
        ResponsesApiRequest {
            model: model.to_string(),
            instructions: String::new(),
            input: Vec::new(),
            tools: Vec::new(),
            tool_choice: "auto".into(),
            parallel_tool_calls: false,
            reasoning: None,
            store: true,   // default so T1 has something to flip
            stream: false, // default so T2 has something to flip
            include: Vec::new(),
            service_tier: None,
            prompt_cache_key: None,
            text: None,
            client_metadata: None,
        }
    }

    // ----- T1 -----

    #[test]
    fn t1_store_set_to_false() {
        let mut req = base_request("gpt-5.1");
        req.store = true;
        apply_chatgpt_backend_transforms(&mut req, "instr");
        assert!(!req.store);
    }

    // ----- T2 -----

    #[test]
    fn t2_stream_set_to_true() {
        let mut req = base_request("gpt-5.1");
        req.stream = false;
        apply_chatgpt_backend_transforms(&mut req, "instr");
        assert!(req.stream);
    }

    // ----- T3 -----

    #[test]
    fn t3_instructions_injected() {
        let mut req = base_request("gpt-5.1");
        apply_chatgpt_backend_transforms(&mut req, "CODEX_PROMPT_V1");
        assert_eq!(req.instructions, "CODEX_PROMPT_V1");
    }

    // ----- T4 -----

    #[test]
    fn t4_input_items_have_ids_stripped() {
        let mut req = base_request("gpt-5.1");
        req.input = vec![
            ResponseItem::Message {
                id: Some("msg_1".into()),
                role: "user".into(),
                content: vec![json!({"type":"input_text","text":"hi"})],
            },
            ResponseItem::FunctionCall {
                id: Some("fc_1".into()),
                call_id: "call_a".into(),
                name: "get_x".into(),
                arguments: "{}".into(),
            },
            ResponseItem::Reasoning {
                id: Some("r_1".into()),
                summary: vec![],
                content: None,
                encrypted_content: None,
            },
        ];
        apply_chatgpt_backend_transforms(&mut req, "instr");
        for item in &req.input {
            match item {
                ResponseItem::Message { id, .. }
                | ResponseItem::FunctionCall { id, .. }
                | ResponseItem::Reasoning { id, .. } => assert!(id.is_none(), "id must be stripped: {item:?}"),
                _ => {}
            }
        }
    }

    // ----- T5 -----

    #[test]
    fn t5_item_reference_items_removed() {
        let mut req = base_request("gpt-5.1");
        req.input = vec![
            ResponseItem::Message {
                id: None,
                role: "user".into(),
                content: vec![json!({"type":"input_text","text":"hi"})],
            },
            ResponseItem::ItemReference { id: "rs_123".into() },
            ResponseItem::ItemReference { id: "rs_456".into() },
        ];
        apply_chatgpt_backend_transforms(&mut req, "instr");
        assert_eq!(req.input.len(), 1);
        assert!(
            !req.input
                .iter()
                .any(|i| matches!(i, ResponseItem::ItemReference { .. }))
        );
    }

    // ----- T6 -----

    #[test]
    fn t6_orphaned_function_call_output_converted_to_message() {
        let mut req = base_request("gpt-5.1");
        req.input = vec![
            // Parented output — should stay as FunctionCallOutput.
            ResponseItem::FunctionCall {
                id: None,
                call_id: "call_parented".into(),
                name: "get_x".into(),
                arguments: "{}".into(),
            },
            ResponseItem::FunctionCallOutput {
                call_id: "call_parented".into(),
                output: "{\"ok\":true}".into(),
            },
            // Orphan — parent call is absent; should be converted to a Message.
            ResponseItem::FunctionCallOutput {
                call_id: "call_orphan".into(),
                output: "orphan result".into(),
            },
        ];
        apply_chatgpt_backend_transforms(&mut req, "instr");

        assert_eq!(req.input.len(), 3, "no item dropped: {:?}", req.input);

        // Parented output still present.
        assert!(matches!(
            &req.input[1],
            ResponseItem::FunctionCallOutput { call_id, .. } if call_id == "call_parented"
        ));

        // Orphan converted to a Message with an output_text block whose text matches the
        // original output.
        match &req.input[2] {
            ResponseItem::Message { role, content, .. } => {
                assert_eq!(role, "assistant");
                assert_eq!(content.len(), 1);
                assert_eq!(content[0]["type"], "output_text");
                assert_eq!(content[0]["text"], "orphan result");
            }
            other => panic!("expected orphan to become Message, got {other:?}"),
        }
    }

    // ----- T7 -----

    #[test]
    fn t7_include_contains_encrypted_content() {
        let mut req = base_request("gpt-5.1");
        apply_chatgpt_backend_transforms(&mut req, "instr");
        assert!(req.include.iter().any(|s| s == INCLUDE_REASONING_ENCRYPTED_CONTENT));
    }

    #[test]
    fn t7_include_dedupes_when_already_present() {
        let mut req = base_request("gpt-5.1");
        req.include = vec!["reasoning.encrypted_content".into()];
        apply_chatgpt_backend_transforms(&mut req, "instr");
        // Exactly one occurrence after the transform.
        let count = req
            .include
            .iter()
            .filter(|s| s.as_str() == INCLUDE_REASONING_ENCRYPTED_CONTENT)
            .count();
        assert_eq!(count, 1);
    }

    // ----- T8 -----

    #[test]
    fn t8_text_verbosity_defaults_to_medium() {
        // Case a: text None → Medium.
        let mut req = base_request("gpt-5.1");
        apply_chatgpt_backend_transforms(&mut req, "instr");
        let tc = req.text.expect("text populated");
        assert_eq!(tc.verbosity, Some(OpenAiVerbosity::Medium));

        // Case b: text Some with verbosity None → Medium (and existing format preserved).
        let mut req = base_request("gpt-5.1");
        req.text = Some(TextControls {
            verbosity: None,
            format: Some(TextFormat {
                kind: TextFormatType::JsonSchema,
                strict: true,
                schema: json!({"type":"object"}),
                name: "roz_output_schema".into(),
            }),
        });
        apply_chatgpt_backend_transforms(&mut req, "instr");
        let tc = req.text.expect("text populated");
        assert_eq!(tc.verbosity, Some(OpenAiVerbosity::Medium));
        assert!(tc.format.is_some(), "existing format must be preserved");

        // Case c: caller set verbosity High → keep High (do not override).
        let mut req = base_request("gpt-5.1");
        req.text = Some(TextControls {
            verbosity: Some(OpenAiVerbosity::High),
            format: None,
        });
        apply_chatgpt_backend_transforms(&mut req, "instr");
        let tc = req.text.expect("text populated");
        assert_eq!(tc.verbosity, Some(OpenAiVerbosity::High));
    }

    // ----- T9 -----

    #[test]
    fn t9_reasoning_effort_medium_for_codex_family() {
        for m in [
            "gpt-5.2-codex",
            "gpt-5.1-codex-max",
            "gpt-5.1-codex-mini",
            "gpt-5.1-codex",
            "gpt-5.2",
            "gpt-5.1",
        ] {
            let mut req = base_request(m);
            apply_chatgpt_backend_transforms(&mut req, "instr");
            let r = req
                .reasoning
                .as_ref()
                .unwrap_or_else(|| panic!("reasoning unset for {m}"));
            assert_eq!(r.effort, Some(ReasoningEffort::Medium), "model {m}");
            assert_eq!(r.summary, Some(ReasoningSummaryLevel::Auto), "model {m}");
        }
    }

    #[test]
    fn t9_reasoning_untouched_for_unrelated_model_family() {
        let mut req = base_request("claude-3.7-sonnet");
        req.reasoning = Some(Reasoning {
            effort: Some(ReasoningEffort::Low),
            summary: None,
        });
        apply_chatgpt_backend_transforms(&mut req, "instr");
        let r = req.reasoning.expect("reasoning retained");
        assert_eq!(r.effort, Some(ReasoningEffort::Low));
        assert_eq!(r.summary, None);
    }

    // ----- T10 -----

    #[test]
    fn t10_max_output_tokens_unset() {
        // Field is absent from the Roz wire type → the transform is a documented no-op. This
        // test pins the absence so that if someone later adds the field they are forced to
        // update apply_chatgpt_backend_transforms to set it to None.
        let mut req = base_request("gpt-5.1");
        apply_chatgpt_backend_transforms(&mut req, "instr");
        // If max_output_tokens ever exists on ResponsesApiRequest, add an assertion that it is
        // None here. For now, compile-time absence is the guarantee.
        let v = serde_json::to_value(&req).expect("serialize");
        assert!(
            v.get("max_output_tokens").is_none(),
            "max_output_tokens must not serialize on ChatGPT-backend path"
        );
        assert!(
            v.get("max_completion_tokens").is_none(),
            "max_completion_tokens must not serialize on ChatGPT-backend path"
        );
    }

    // ----- T12 -----

    #[test]
    fn t12_normalize_ladder_matches_expected_table() {
        let cases = [
            ("gpt-5.2-codex", "gpt-5.2-codex"),
            ("gpt-5.1-codex-max", "gpt-5.1-codex-max"),
            ("gpt-5.1-codex-mini", "gpt-5.1-codex-mini"),
            ("codex-mini-latest", "gpt-5.1-codex-mini"),
            ("gpt-5.1-codex-something-else", "gpt-5.1-codex"),
            ("gpt-5.1-future", "gpt-5.1"),
            ("some-codex-variant", "gpt-5.1-codex"),
            ("gpt-5-turbo-preview", "gpt-5.1"),
            ("gpt-4-foo", "gpt-5.1"),
        ];
        for (input, expected) in cases {
            assert_eq!(normalize_model_name(input), expected, "normalize({input})");
        }
    }

    #[test]
    fn t12_applied_defensively_to_req_model() {
        let mut req = base_request("gpt-4-foo");
        apply_chatgpt_backend_transforms(&mut req, "instr");
        assert_eq!(req.model, "gpt-5.1");
    }

    // ============================================================================
    // T13-T19
    // ============================================================================

    // ----- T13 -----

    #[test]
    fn t13_rewrite_url_appends_codex_path() {
        // Case a: base_url already ends with /responses → rewrite in place.
        assert_eq!(
            rewrite_url_for_chatgpt("https://api.openai.com/v1/responses"),
            "https://api.openai.com/v1/codex/responses"
        );
        // Case b: base_url is the root → append /codex/responses.
        assert_eq!(
            rewrite_url_for_chatgpt("https://chatgpt.com/backend-api"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        // Case c: trailing slash on root is trimmed before append.
        assert_eq!(
            rewrite_url_for_chatgpt("https://chatgpt.com/backend-api/"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
    }

    // ----- T14 -----

    #[test]
    fn t14_authorization_header_format() {
        let tok = SecretString::from("oauth_access_abc".to_string());
        let h = chatgpt_backend_headers(&tok, None, None);
        assert_eq!(h.authorization, "Bearer oauth_access_abc");
    }

    // ----- T15 -----

    #[test]
    fn t15_chatgpt_account_id_header_omitted_when_none() {
        let tok = SecretString::from("t".to_string());
        let h_none = chatgpt_backend_headers(&tok, None, None);
        assert!(h_none.chatgpt_account_id.is_none());

        let h_some = chatgpt_backend_headers(&tok, Some("acct_123"), None);
        assert_eq!(h_some.chatgpt_account_id.as_deref(), Some("acct_123"));
    }

    // ----- T16 -----

    #[test]
    fn t16_openai_beta_header_value() {
        let tok = SecretString::from("t".to_string());
        let h = chatgpt_backend_headers(&tok, None, None);
        assert_eq!(h.openai_beta, "responses=experimental");
    }

    // ----- T17 -----

    #[test]
    fn t17_originator_defaults_to_roz() {
        let tok = SecretString::from("t".to_string());
        let h = chatgpt_backend_headers(&tok, None, None);
        // Per RESEARCH.md A9. Fallback to "codex_cli_rs" is a caller-side concern, not this
        // function's. Documented in ChatGptBackendHeaders::originator.
        assert_eq!(h.originator, "roz");
    }

    // ----- T18 -----

    #[test]
    fn t18_accept_header_is_text_event_stream() {
        let tok = SecretString::from("t".to_string());
        let h = chatgpt_backend_headers(&tok, None, None);
        assert_eq!(h.accept, "text/event-stream");
    }

    // ----- T19 -----

    #[test]
    fn t19_conversation_and_session_split_on_colon() {
        let tok = SecretString::from("t".to_string());
        // Colon split.
        let h = chatgpt_backend_headers(&tok, None, Some("conv_abc:sess_xyz"));
        assert_eq!(h.conversation_id.as_deref(), Some("conv_abc"));
        assert_eq!(h.session_id.as_deref(), Some("sess_xyz"));

        // No delimiter → whole string is conversation_id, session_id None.
        let h = chatgpt_backend_headers(&tok, None, Some("conv_only"));
        assert_eq!(h.conversation_id.as_deref(), Some("conv_only"));
        assert!(h.session_id.is_none());
    }

    #[test]
    fn t19_prompt_cache_key_none_means_no_headers() {
        let tok = SecretString::from("t".to_string());
        let h = chatgpt_backend_headers(&tok, None, None);
        assert!(h.conversation_id.is_none());
        assert!(h.session_id.is_none());
    }

    // ============================================================================
    // T20
    // ============================================================================

    #[tokio::test]
    async fn t20_collect_stream_synthesizes_text_plus_tool_calls() {
        let events: Vec<Result<ResponseEvent, OpenAiError>> = vec![
            Ok(ResponseEvent::Created),
            Ok(ResponseEvent::OutputTextDelta("Hello, ".into())),
            Ok(ResponseEvent::OutputTextDelta("world!".into())),
            Ok(ResponseEvent::ReasoningSummaryDelta {
                delta: "thought briefly".into(),
                summary_index: 0,
            }),
            Ok(ResponseEvent::ToolCallStart {
                id: "call_abc".into(),
                name: "get_weather".into(),
            }),
            Ok(ResponseEvent::ToolCallArgsDelta("{\"city\":\"SFO\"}".into())),
            Ok(ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                id: None,
                call_id: "call_abc".into(),
                name: "get_weather".into(),
                arguments: "{\"city\":\"SFO\"}".into(),
            })),
            Ok(ResponseEvent::Completed {
                response_id: Some("resp_1".into()),
                token_usage: Some(TokenUsage {
                    input_tokens: 10,
                    output_tokens: 20,
                    cached_input_tokens: Some(3),
                    reasoning_output_tokens: Some(5),
                }),
            }),
        ];
        let s = stream::iter(events).boxed();

        let json = collect_stream_to_json(s).await.expect("collect ok");

        assert_eq!(json["id"], "resp_1");
        assert_eq!(json["object"], "response");
        let output = json["output"].as_array().expect("output is array");
        // Expect: one message, one reasoning, one function_call (deduped — OutputItemDone wins
        // over the streaming accumulator).
        assert_eq!(output.len(), 3, "one of each section: got {output:?}");

        // message
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[0]["role"], "assistant");
        assert_eq!(output[0]["content"][0]["type"], "output_text");
        assert_eq!(output[0]["content"][0]["text"], "Hello, world!");

        // reasoning
        assert_eq!(output[1]["type"], "reasoning");
        assert_eq!(output[1]["summary"][0]["type"], "summary_text");
        assert_eq!(output[1]["summary"][0]["text"], "thought briefly");

        // function_call (from OutputItemDone — deduped against streaming accumulator)
        assert_eq!(output[2]["type"], "function_call");
        assert_eq!(output[2]["call_id"], "call_abc");
        assert_eq!(output[2]["name"], "get_weather");
        assert_eq!(output[2]["arguments"], "{\"city\":\"SFO\"}");

        // usage
        assert_eq!(json["usage"]["input_tokens"], 10);
        assert_eq!(json["usage"]["output_tokens"], 20);
        assert_eq!(json["usage"]["cached_input_tokens"], 3);
        assert_eq!(json["usage"]["reasoning_output_tokens"], 5);
    }

    #[tokio::test]
    async fn t20_collect_stream_omits_empty_sections() {
        // Only text delta + completed — no reasoning, no tool calls.
        let events: Vec<Result<ResponseEvent, OpenAiError>> = vec![
            Ok(ResponseEvent::OutputTextDelta("ok".into())),
            Ok(ResponseEvent::Completed {
                response_id: None,
                token_usage: None,
            }),
        ];
        let s = stream::iter(events).boxed();
        let json = collect_stream_to_json(s).await.expect("collect ok");
        let output = json["output"].as_array().expect("output is array");
        assert_eq!(output.len(), 1, "only message section should appear");
        assert_eq!(output[0]["type"], "message");
        assert!(json["id"].is_null());
        assert!(json["usage"].is_null());
    }
}
