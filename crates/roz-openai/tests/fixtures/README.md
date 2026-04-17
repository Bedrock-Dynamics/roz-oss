# Fixtures — Phase 19 integration-test matrix

Each `.sse` file is raw-bytes SSE framing (`event:`/`data:` lines terminated by `\n\n`) served by
wiremock to drive the integration suite. Shapes match what OpenAI, vLLM, Ollama, and the ChatGPT
backend actually emit on the wire. Re-capture on backend regressions; fixtures are committed
verbatim so rebases against upstream can diff against known-good frames.

## Chat Completions wire (API-key path)

- `chat_simple_hello.sse` — Plan 19-07 baseline: two content deltas + stop + usage on final chunk.
- `chat_single_tool_call.sse` — single `move_arm` tool call assembled across 3 chunks; finishes on
  `finish_reason=tool_calls`.
- `chat_multi_tool_call.sse` — two tool calls (`move_arm` + `grip`) at distinct indexes 0/1.
- `chat_reasoning_stream.sse` — `<think>…</think>` reasoning then visible answer. Exercises the
  ChatChunkNormalizer ThinkTags branch.
- `chat_reasoning_field.sse` — legacy `delta.reasoning_content` field (vLLM ≤ 0.8). Exercises
  the OpenaiReasoningContent branch.
- `chat_malformed_json_structured_output.sse` — single content delta carrying `{"ok":true,}`
  (trailing comma) + stop. Drives the `json_repair` path in Plan 19-10's adapter.
- `ollama_single_tool_call.sse` — OWM-08 regression: Ollama sometimes omits `tool_calls` on the
  final chunk. Tool call is still assembled from earlier chunks.

## Responses wire

- `responses_hello.sse` — Plan 19-07 baseline: created → 2 text deltas → completed + usage.
- `responses_reasoning_included_header.sse` — minimal body used with `X-Reasoning-Included: true`.
- `responses_api_key_turn.sse` — full happy path: created → output_item.added (message) →
  output_text.delta → completed with `resp_1` + usage.
- `responses_oauth_chatgpt_turn.sse` — ChatGPT-backend shape: adds `response.reasoning.delta`
  between items + reports `reasoning_output_tokens` usage.
- `responses_reasoning_encrypted.sse` — `output_item.done` with `reasoning.encrypted_content`
  populated. Verifies the item is delivered intact through the event stream.

## JWT fixture

- `jwt_chatgpt_account_id.jwt` — unsigned, hand-crafted. Decodes to
  `{"https://api.openai.com/auth": {"chatgpt_account_id": "acct-test-123"}, "exp": 9999999999}`.
  Used by `oauth_flow::jwt_fixture_extracts_account_id` to confirm
  `parse_chatgpt_jwt_claims` extracts the expected account id.
