#[allow(dead_code)] // Legacy stream-chunk adapter retained for compatibility paths.
mod agent;
mod commands;
pub mod context;
pub mod convert;
#[allow(dead_code)] // Formatters wired incrementally.
pub mod format;
mod history;
mod input;
pub mod markdown;
mod pricing;
mod proto;
pub mod provider;
pub(crate) mod providers;
#[allow(dead_code)] // State variants/modes used as backends mature.
pub mod session;
#[allow(dead_code)] // Team event formatter wired when gRPC streaming lands.
pub mod team;
pub mod tools;

use iocraft::components::TextWrap;
use iocraft::prelude::*;
use owo_colors::OwoColorize;
use roz_agent::agent_loop::{AgentInput, AgentInputSeed, AgentLoop, AgentLoopMode};
use roz_agent::dispatch::ToolDispatcher;
use roz_agent::error::AgentError;
use roz_agent::model::types::{MessageRole, Model};
use roz_agent::safety::SafetyStack;
use roz_agent::session_runtime::{
    PreparedTurn, SessionConfig, SessionRuntime, StreamingTurnExecutor, StreamingTurnHandle, StreamingTurnResult,
    TurnExecutionFailure, TurnInput, TurnOutput,
};
use roz_agent::spatial_provider::NullWorldStateProvider;
use roz_core::session::activity::RuntimeFailureKind;
use roz_core::session::control::{CognitionMode, SessionMode};
use roz_core::session::event::SessionEvent;
use unicode_width::UnicodeWidthStr;

use commands::CommandResult;
use history::InputHistory;
use provider::{AgentEvent, Provider, ProviderConfig, classify_error_message};
use session::{Mode, Session, SessionEntry, UiState};

/// An action sent from the TUI to the provider loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserAction {
    /// A chat message to send to the agent.
    Message(String),
    /// Switch to a different model mid-session.
    SwitchModel { model_ref: String },
}

/// Channels for TUI <-> provider communication.
struct Channels {
    /// Receive agent events (text deltas, tool calls, etc.)
    event_rx: async_channel::Receiver<AgentEvent>,
    /// Send user actions to the provider.
    msg_tx: async_channel::Sender<UserAction>,
}

/// Options for session resume behavior, passed into the TUI.
#[derive(Debug, Clone, Default)]
pub struct SessionOpts {
    /// Resume the latest session.
    pub resume_latest: bool,
    /// Resume a specific session by ID.
    pub resume_id: Option<String>,
}

fn truncate_to_width_with_ellipsis(s: &str, max_width: usize) -> String {
    if s.width() <= max_width {
        return s.to_string();
    }
    if max_width <= 1 {
        return if max_width == 1 {
            "\u{2026}".to_string()
        } else {
            String::new()
        };
    }
    let target = max_width - 1;
    let mut result = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw > target {
            break;
        }
        result.push(c);
        w += cw;
    }
    result.push('\u{2026}');
    result
}

/// The main interactive REPL component.
#[component]
fn RozRepl(mut hooks: Hooks) -> impl Into<AnyElement<'static>> {
    let mut system = hooks.use_context_mut::<SystemContext>();
    let channels = hooks.use_context::<Channels>();
    let session_opts = hooks.use_context::<SessionOpts>();
    let mut text = hooks.use_state(String::new);
    let mut cursor = hooks.use_state(|| 0usize);
    let mut should_exit = hooks.use_state(|| false);
    let mut tokens = hooks.use_state(|| 0u32);
    let mut cost = hooks.use_state(|| 0.0f64);
    let mode = hooks.use_state(|| Mode::React);
    let mut model = hooks.use_state(|| "claude-sonnet-4-6".to_string());
    let mut connected = hooks.use_state(|| false);
    let mut ui_state = hooks.use_state(|| UiState::Idle);
    let robot_name = hooks.use_state(|| context::read_robot_name(std::path::Path::new(".")));
    let mut saved_input = hooks.use_state(String::new);
    let (stdout, _stderr) = hooks.use_output();
    let mut history = hooks.use_state(InputHistory::load);

    // Session state: initialized once on first render via use_state
    let session = hooks.use_state(|| {
        let opts = session_opts.clone();
        initialize_session(&opts)
    });

    // Track accumulated assistant response text for session recording
    let mut response_buf = hooks.use_state(String::new);

    // One-shot: replay resumed session entries on first render
    let mut did_replay = hooks.use_state(|| false);
    if !did_replay.get() {
        did_replay.set(true);
        if let Some(ref s) = *session.read()
            && let Ok(entries) = s.entries()
            && !entries.is_empty()
        {
            commands::print_resumed_entries(&entries, &stdout);
        }
    }

    // Receive streaming events from the provider
    hooks.use_future({
        let event_rx = channels.event_rx.clone();
        let stdout = stdout.clone();
        async move {
            let mut md = markdown::MarkdownRenderer::new();
            let mut line_buf = String::new();
            let mut in_stream = false;

            while let Ok(event) = event_rx.recv().await {
                match event {
                    AgentEvent::Connected { model: m } => {
                        model.set(m);
                        connected.set(true);
                    }
                    AgentEvent::TextDelta(chunk) => {
                        if !in_stream {
                            in_stream = true;
                            ui_state.set(UiState::Streaming);
                        }

                        // Accumulate for session recording
                        response_buf.write().push_str(&chunk);

                        // Buffer text and render complete lines through markdown
                        line_buf.push_str(&chunk);
                        while let Some(pos) = line_buf.find('\n') {
                            let line = line_buf[..pos].to_string();
                            line_buf.drain(..=pos);
                            stdout.println(md.render_line(&line));
                        }
                    }
                    AgentEvent::ThinkingDelta(chunk) => {
                        ui_state.set(UiState::Thinking);
                        stdout.print(chunk.dimmed().to_string());
                    }
                    AgentEvent::ToolRequest { name, params, .. } => {
                        flush_line_buf(&mut line_buf, &mut md, &stdout);
                        ui_state.set(UiState::ToolExec);
                        stdout.println(format::tool_call(&name, &params));
                    }
                    AgentEvent::ToolResultDisplay {
                        name,
                        content,
                        is_error,
                    } => {
                        // Try formatted display for known tool types
                        let formatted = if name == "get_robot_state" && !is_error {
                            serde_json::from_str(&content)
                                .ok()
                                .and_then(|v| format::format_robot_state(&v))
                        } else {
                            None
                        };

                        if let Some(pretty) = formatted {
                            stdout.println(format::tool_result("ok", true));
                            stdout.println(pretty);
                        } else {
                            let display = if content.len() > 200 {
                                // Truncate at a char boundary to avoid panicking on multi-byte UTF-8
                                let end = content.char_indices().nth(200).map_or(content.len(), |(i, _)| i);
                                format!("{}...", &content[..end])
                            } else {
                                content
                            };
                            stdout.println(format::tool_result(&display, !is_error));
                        }
                    }
                    AgentEvent::TurnComplete {
                        input_tokens,
                        output_tokens,
                        ..
                    } => {
                        // Flush remaining buffered text
                        flush_line_buf(&mut line_buf, &mut md, &stdout);
                        tokens.set(input_tokens + output_tokens);

                        // Calculate and accumulate cost
                        let turn_cost = pricing::calculate_cost(&model.to_string(), input_tokens, output_tokens);
                        cost.set(cost.get() + turn_cost);

                        // Record assistant response to session
                        let buf_text = response_buf.to_string();
                        if !buf_text.is_empty() {
                            if let Some(ref s) = *session.read() {
                                let entry = SessionEntry::now("assistant", &buf_text).with_usage(
                                    &model.to_string(),
                                    input_tokens,
                                    output_tokens,
                                );
                                s.append(&entry);
                            }
                            response_buf.set(String::new());
                        }

                        stdout.println(String::new());
                        ui_state.set(UiState::Idle);
                        in_stream = false;
                        md = markdown::MarkdownRenderer::new();
                    }
                    AgentEvent::ImageSnapshot { camera, caption, .. } => {
                        let label = caption.as_deref().unwrap_or("image");
                        stdout.println(format!("[Snapshot: {camera} \u{2014} {label}]"));
                    }
                    AgentEvent::Error(msg) => {
                        flush_line_buf(&mut line_buf, &mut md, &stdout);

                        // Record error to session
                        if let Some(ref s) = *session.read() {
                            s.append(&SessionEntry::now("error", &msg));
                        }
                        response_buf.set(String::new());

                        stdout.println(format::error(&msg));
                        ui_state.set(UiState::Idle);
                        in_stream = false;
                        md = markdown::MarkdownRenderer::new();
                    }
                }
            }
        }
    });

    // Keyboard input
    hooks.use_terminal_events({
        let msg_tx = channels.msg_tx.clone();
        let mut session = session;
        move |event| {
            if let TerminalEvent::Key(KeyEvent {
                code, kind, modifiers, ..
            }) = event
            {
                if kind == KeyEventKind::Release {
                    return;
                }

                if ui_state.get() == UiState::AwaitingApproval {
                    return;
                }

                if matches!(
                    ui_state.get(),
                    UiState::Streaming | UiState::Thinking | UiState::ToolExec
                ) {
                    if code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
                        stdout.println("^C".dimmed().to_string());
                    }
                    return;
                }

                handle_key(
                    code,
                    modifiers,
                    &mut text,
                    &mut cursor,
                    &mut should_exit,
                    &mut history,
                    &mut saved_input,
                    &mut ui_state,
                    &msg_tx,
                    &model,
                    &tokens,
                    &cost,
                    &connected,
                    &stdout,
                    &mut session,
                );
            }
        }
    });

    if should_exit.get() {
        system.exit();
    }

    // --- Render ---
    let input_text = text.to_string();
    let char_count = input_text.chars().count();
    let pos = cursor.get().min(char_count);

    let byte_pos = input_text.char_indices().nth(pos).map_or(input_text.len(), |(b, _)| b);
    let prompt = "> ";
    #[allow(clippy::cast_possible_truncation)]
    let cursor_col = (prompt.width() + input_text[..byte_pos].width()) as u32;

    let cursor_char = if pos < char_count {
        input_text[byte_pos..].chars().next().unwrap().to_string()
    } else {
        " ".to_string()
    };
    #[allow(clippy::cast_possible_truncation)]
    let cursor_char_width = cursor_char.width().max(1) as u32;

    let term_width = crossterm::terminal::size().map_or(80, |(w, _)| w as usize);

    let mode_label = format!("[{}]", mode.get());
    let model_name = model.to_string();
    let tok_display = format!("{} tok", tokens.get());
    let cost_display = if cost.get() > 0.0 {
        format!("${:.4}", cost.get())
    } else {
        "--".to_string()
    };

    // Session ID in status bar (first 8 chars)
    let session_label = session
        .read()
        .as_ref()
        .map(|s| format!(" [{}]", s.short_id()))
        .unwrap_or_default();

    // Robot name prefix for status bar (e.g. "[reachy-mini] ")
    let robot_label = robot_name
        .read()
        .as_ref()
        .map(|n| format!("[{n}] "))
        .unwrap_or_default();

    let connected_label = if connected.get() { "connected" } else { "" };
    let fixed_w = mode_label.width()
        + robot_label.width()
        + " \u{00b7}  \u{00b7} ".width()
        + tok_display.width()
        + cost_display.width()
        + session_label.width()
        + if connected_label.is_empty() {
            0
        } else {
            connected_label.width() + 1
        };
    let model_budget = term_width.saturating_sub(fixed_w + 1); // +1 for leading space
    let display_model = truncate_to_width_with_ellipsis(&model_name, model_budget);
    let status_left = format!(" {display_model} \u{00b7} {tok_display} \u{00b7} {cost_display}{session_label}");

    // Explicit width gives taffy a constraint so Overflow::Hidden actually clips.
    // Without it, NoWrap text expands the layout to content width, the status bar
    // wraps in narrow terminals, the component renders 3 lines instead of 2, and
    // iocraft's cursor-rewind math breaks — every keystroke appears as a new line.
    #[allow(clippy::cast_possible_truncation)]
    let term_w = term_width as u32;

    element! {
        View(flex_direction: FlexDirection::Column, width: term_w) {
            View(height: 1u32, position: Position::Relative) {
                Text(content: "> ".to_string(), color: Color::Rgb { r: 233, g: 196, b: 106 }, weight: Weight::Bold)
                Text(content: input_text.clone(), color: Color::White)
                View(
                    position: Position::Absolute,
                    left: cursor_col,
                    top: 0u32,
                    width: cursor_char_width,
                    height: 1u32,
                    background_color: Color::Grey,
                ) {
                    Text(content: cursor_char.clone(), color: Color::Black)
                }
            }
            View(height: 1u32, flex_direction: FlexDirection::Row, overflow: Overflow::Hidden, width: term_w) {
                Text(
                    content: mode_label,
                    color: Color::Rgb { r: 233, g: 196, b: 106 },
                    weight: Weight::Bold,
                    wrap: TextWrap::NoWrap,
                )
                #(if robot_label.is_empty() {
                    None
                } else {
                    Some(element! {
                        Text(content: robot_label.clone(), color: Color::Cyan, wrap: TextWrap::NoWrap)
                    })
                })
                Text(content: status_left, color: Color::DarkGrey, wrap: TextWrap::NoWrap)
                #(if connected.get() {
                    Some(element! {
                        View(flex_grow: 1.0)
                    })
                } else {
                    None
                })
                #(if connected.get() {
                    Some(element! {
                        Text(content: "connected".to_string(), color: Color::Green, wrap: TextWrap::NoWrap)
                    })
                } else {
                    None
                })
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_key(
    code: KeyCode,
    modifiers: KeyModifiers,
    text: &mut State<String>,
    cursor: &mut State<usize>,
    should_exit: &mut State<bool>,
    history: &mut State<InputHistory>,
    saved_input: &mut State<String>,
    ui_state: &mut State<UiState>,
    msg_tx: &async_channel::Sender<UserAction>,
    model: &State<String>,
    tokens: &State<u32>,
    cost: &State<f64>,
    connected: &State<bool>,
    stdout: &StdoutHandle,
    session: &mut State<Option<Session>>,
) {
    let is_ctrl = modifiers.contains(KeyModifiers::CONTROL);

    match code {
        KeyCode::Enter => {
            let input_text = text.to_string();
            if input_text.is_empty() {
                return;
            }
            history.write().push(&input_text);
            dispatch(
                &input_text,
                should_exit,
                ui_state,
                msg_tx,
                model,
                tokens,
                cost,
                connected,
                stdout,
                session,
            );
            text.set(String::new());
            cursor.set(0);
        }

        KeyCode::Char('d') if is_ctrl => should_exit.set(true),
        KeyCode::Char('c') if is_ctrl => {}

        KeyCode::Char('u') if is_ctrl => {
            text.set(String::new());
            cursor.set(0);
            history.write().reset();
        }
        KeyCode::Char('w') if is_ctrl => {
            let (new_text, new_pos) = input::delete_word_back(&text.to_string(), cursor.get());
            text.set(new_text);
            cursor.set(new_pos);
            history.write().reset();
        }
        KeyCode::Char('a') if is_ctrl => cursor.set(0),
        KeyCode::Char('e') if is_ctrl => cursor.set(text.to_string().chars().count()),

        KeyCode::Char(c) => {
            let (new_text, new_pos) = input::insert_char(&text.to_string(), cursor.get(), c);
            text.set(new_text);
            cursor.set(new_pos);
            history.write().reset();
        }

        KeyCode::Backspace => {
            let (new_text, new_pos) = input::delete_before(&text.to_string(), cursor.get());
            text.set(new_text);
            cursor.set(new_pos);
            history.write().reset();
        }
        KeyCode::Delete => {
            let new_text = input::delete_at(&text.to_string(), cursor.get());
            text.set(new_text);
            history.write().reset();
        }

        KeyCode::Left => {
            if cursor.get() > 0 {
                cursor.set(cursor.get() - 1);
            }
        }
        KeyCode::Right => {
            let max = text.to_string().chars().count();
            if cursor.get() < max {
                cursor.set(cursor.get() + 1);
            }
        }
        KeyCode::Home => cursor.set(0),
        KeyCode::End => cursor.set(text.to_string().chars().count()),

        KeyCode::Up => {
            if !history.read().is_browsing() {
                saved_input.set(text.to_string());
            }
            let entry = history.write().up().map(String::from);
            if let Some(e) = entry {
                cursor.set(e.chars().count());
                text.set(e);
            }
        }
        KeyCode::Down => {
            let entry = history.write().down().map(String::from);
            if let Some(e) = entry {
                cursor.set(e.chars().count());
                text.set(e);
            } else {
                let saved = saved_input.to_string();
                cursor.set(saved.chars().count());
                text.set(saved);
            }
        }

        _ => {}
    }
}

/// Flush any remaining partial line from the buffer through the markdown renderer.
fn flush_line_buf(buf: &mut String, md: &mut markdown::MarkdownRenderer, stdout: &StdoutHandle) {
    if !buf.is_empty() {
        let line = std::mem::take(buf);
        stdout.println(md.render_line(&line));
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn dispatch(
    text: &str,
    should_exit: &mut State<bool>,
    ui_state: &mut State<UiState>,
    msg_tx: &async_channel::Sender<UserAction>,
    model: &State<String>,
    tokens: &State<u32>,
    cost: &State<f64>,
    connected: &State<bool>,
    stdout: &StdoutHandle,
    session: &mut State<Option<Session>>,
) {
    if text.starts_with('/') {
        // Handle /model inline — it needs access to channel + model state.
        if text == "/model" {
            stdout.println(format!("Current model: {}", model.to_string().bold()));
            return;
        } else if let Some(arg) = text.strip_prefix("/model ") {
            let model_ref = arg.trim();
            if msg_tx
                .try_send(UserAction::SwitchModel {
                    model_ref: model_ref.to_string(),
                })
                .is_ok()
            {
                stdout.println(format!("Switching to {}...", model_ref.bold()));
            } else {
                stdout.println(format::not_connected());
            }
            return;
        }

        // Handle /session inline — show current session info.
        if text.trim() == "/session" {
            if let Some(ref s) = *session.read() {
                let count = s.entries().map(|e| e.len()).unwrap_or(0);
                let entries_label = if count == 1 { "entry" } else { "entries" };
                stdout.println(format!("Session: {} ({count} {entries_label})", s.short_id()));
            } else {
                stdout.println("No active session.".dimmed().to_string());
            }
            return;
        }

        // Handle /compact inline — needs access to session state.
        if text == "/compact" || text.starts_with("/compact ") {
            let focus = text.strip_prefix("/compact").unwrap_or("").trim();
            let result = session
                .read()
                .as_ref()
                .map(|s| session::compact_entries(s, if focus.is_empty() { None } else { Some(focus) }));
            match result {
                Some(Ok(compacted_count)) => {
                    stdout.println(format!("Compacted {compacted_count} entries into summary."));
                    if !focus.is_empty() {
                        stdout.println(format!("Preserved focus: {focus}"));
                    }
                    stdout.println(
                        "Note: compaction applies to saved session history (for /resume). In-memory context is unchanged."
                            .to_string(),
                    );
                }
                Some(Err(e)) => {
                    let msg = e.to_string();
                    if msg.contains("Not enough") {
                        stdout.println("Not enough history to compact (need 4+ entries).".to_string());
                    } else {
                        stdout.println(format!("Error compacting session: {msg}"));
                    }
                }
                None => {
                    stdout.println("No active session.".to_string());
                }
            }
            return;
        }

        // Handle /usage inline — needs access to tokens, cost, and model state.
        if text.trim() == "/usage" {
            let total = tokens.get();
            let cost_val = cost.get();
            let cost_str = if cost_val > 0.0 {
                format!("${cost_val:.4}")
            } else {
                "--".to_string()
            };
            stdout.println("Session usage:".bold().to_string());
            stdout.println(format!("  Tokens: {total}"));
            stdout.println(format!("  Cost:   {cost_str}"));
            stdout.println(format!("  Model:  {model}"));
            return;
        }

        // Handle /status inline — show full session overview.
        if text.trim() == "/status" {
            let provider_name = if msg_tx.is_full() || msg_tx.is_closed() {
                "unknown"
            } else {
                "roz cli"
            };
            let model_name = model.to_string();
            let session_id = session
                .read()
                .as_ref()
                .map_or_else(|| "--".to_string(), |s| s.short_id().to_string());
            let conn_status = if connected.get() { "connected" } else { "disconnected" };
            let total = tokens.get();
            let cost_val = cost.get();
            let cost_str = if cost_val > 0.0 {
                format!("${cost_val:.4}")
            } else {
                "--".to_string()
            };

            stdout.println(format!("Provider:  {provider_name}"));
            stdout.println(format!("Model:     {model_name}"));
            stdout.println(format!("Session:   {session_id}"));
            stdout.println(format!("Status:    {conn_status}"));
            stdout.println(format!("Tokens:    {}", format_token_count(total)));
            stdout.println(format!("Cost:      {cost_str}"));
            return;
        }

        // Handle /team inline — placeholder for team event streaming.
        if text.trim() == "/team" {
            stdout.println("Team monitoring: not connected to a team session.".to_string());
            stdout.println(
                "Use `roz task run SPEC --host HOST --phases '...'` to create a phased team task.".to_string(),
            );
            return;
        }

        // Handle /phases inline — informational only.
        if text.trim() == "/phases" {
            stdout.println("Phase system:".bold().to_string());
            stdout.println("  Available modes: react, ooda_react".to_string());
            stdout.println(String::new());
            stdout.println("  Phases are configured via `roz task run --phases '[...]'`".to_string());
            stdout.println("  Example:".to_string());
            stdout.println(r#"    [{"mode":"react","tools":"all","trigger":"immediate"},"#.to_string());
            stdout.println(
                r#"     {"mode":"ooda_react","tools":{"named":["goto","arm"]},"trigger":{"after_cycles":5}}]"#
                    .to_string(),
            );
            return;
        }

        // Handle /context inline — show context window breakdown.
        if text.trim() == "/context" {
            print_context_breakdown(session, &model.to_string(), stdout);
            return;
        }

        match commands::dispatch(text, stdout) {
            CommandResult::Exit => {
                should_exit.set(true);
            }
            CommandResult::NewSession => {
                handle_new_session(session, stdout);
            }
            CommandResult::ResumeLatest => {
                handle_resume_latest(session, stdout);
            }
            CommandResult::ResumeById(id) => {
                handle_resume_by_id(&id, session, stdout);
            }
            CommandResult::None => {}
        }
        return;
    }

    // Record user message to session
    if let Some(ref s) = *session.read() {
        s.append(&SessionEntry::now("user", text));
    }

    // Echo user message
    stdout.println(format::user_echo(text));

    // Send to provider (non-blocking — unbounded channel)
    if msg_tx.try_send(UserAction::Message(text.to_string())).is_ok() {
        ui_state.set(UiState::Thinking);
    } else {
        stdout.println(format::not_connected());
    }
}

fn handle_new_session(session: &mut State<Option<Session>>, stdout: &StdoutHandle) {
    match Session::new() {
        Ok(s) => {
            stdout.println(format!("New session: {}", s.short_id().bold()));
            session.set(Some(s));
        }
        Err(e) => {
            stdout.println(format!("  {} {e}", "error:".red()));
        }
    }
}

fn handle_resume_latest(session: &mut State<Option<Session>>, stdout: &StdoutHandle) {
    match Session::load_latest() {
        Ok(s) => {
            match s.entries() {
                Ok(entries) => {
                    stdout.println(format!("Resumed session: {}", s.short_id().bold()));
                    commands::print_resumed_entries(&entries, stdout);
                }
                Err(e) => {
                    stdout.println(format!("  {} {e}", "error reading session:".red()));
                }
            }
            session.set(Some(s));
        }
        Err(e) => {
            stdout.println(format!("  {} {e}", "error:".red()));
        }
    }
}

fn handle_resume_by_id(id: &str, session: &mut State<Option<Session>>, stdout: &StdoutHandle) {
    // Try exact match first, then prefix match
    match Session::load(id) {
        Ok(s) => {
            match s.entries() {
                Ok(entries) => {
                    stdout.println(format!("Resumed session: {}", s.short_id().bold()));
                    commands::print_resumed_entries(&entries, stdout);
                }
                Err(e) => {
                    stdout.println(format!("  {} {e}", "error reading session:".red()));
                }
            }
            session.set(Some(s));
        }
        Err(_) => {
            // Try prefix match from recent sessions
            match Session::list_recent(100) {
                Ok(sessions) => {
                    let matches: Vec<_> = sessions.iter().filter(|(sid, _, _)| sid.starts_with(id)).collect();
                    match matches.len() {
                        0 => {
                            stdout.println(format!("  {} no session matching '{id}'", "error:".red()));
                        }
                        1 => {
                            let full_id = &matches[0].0;
                            handle_resume_by_id(full_id, session, stdout);
                        }
                        _ => {
                            stdout.println(format!(
                                "  {} multiple sessions match '{id}', be more specific:",
                                "error:".red()
                            ));
                            for (sid, _, count) in matches {
                                let short = &sid[..8.min(sid.len())];
                                stdout.println(format!("    {short}  ({count} entries)"));
                            }
                        }
                    }
                }
                Err(e) => {
                    stdout.println(format!("  {} {e}", "error:".red()));
                }
            }
        }
    }
}

/// Initialize a session based on CLI flags.
fn initialize_session(opts: &SessionOpts) -> Option<Session> {
    if let Some(ref id) = opts.resume_id {
        match Session::load(id) {
            Ok(s) => return Some(s),
            Err(e) => {
                eprintln!("warning: could not resume session {id}: {e}");
            }
        }
    } else if opts.resume_latest {
        match Session::load_latest() {
            Ok(s) => return Some(s),
            Err(e) => {
                eprintln!("warning: could not resume latest session: {e}");
            }
        }
    }

    // Create a new session (best-effort)
    match Session::new() {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("warning: could not create session: {e}");
            None
        }
    }
}

/// Format a token count with comma separators.
fn format_token_count(n: u32) -> String {
    if n == 0 {
        return "0".to_string();
    }
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

/// Rough token estimate from character count (~4 chars per token).
const fn estimate_tokens(chars: usize) -> usize {
    chars / 4
}

/// Print context window breakdown.
fn print_context_breakdown(session: &State<Option<Session>>, model: &str, stdout: &StdoutHandle) {
    // Determine max context from model name.
    // Anthropic claude models (opus/sonnet/haiku) all support 200k.
    let max_context: usize =
        if model.contains("claude") || model.contains("opus") || model.contains("sonnet") || model.contains("haiku") {
            200_000
        } else {
            // Conservative default for unknown/non-Anthropic models.
            128_000
        };

    // Constitution size estimate.
    let constitution = roz_agent::constitution::build_constitution(roz_agent::agent_loop::AgentLoopMode::React, &[]);
    let constitution_tokens = estimate_tokens(constitution.len());

    // Project context size estimate.
    let project_ctx = context::load_project_context();
    let project_tokens = project_ctx.as_ref().map_or(0, |c| estimate_tokens(c.len()));
    let project_source = if project_ctx.is_some() { "AGENTS.md" } else { "none" };

    // History size estimate from session entries.
    let history_tokens = session
        .read()
        .as_ref()
        .and_then(|s| s.entries().ok())
        .map_or(0, |entries| {
            let total_chars: usize = entries.iter().map(|e| e.content.len() + e.role.len()).sum();
            estimate_tokens(total_chars)
        });

    let used = constitution_tokens + project_tokens + history_tokens;
    let available = max_context.saturating_sub(used);

    stdout.println(format!(
        "Context window: {}",
        format_token_count(u32::try_from(max_context).unwrap_or(u32::MAX))
    ));
    stdout.println(format!(
        "  Constitution:    ~{} tokens",
        format_token_count(u32::try_from(constitution_tokens).unwrap_or(u32::MAX))
    ));
    stdout.println(format!(
        "  Project context: ~{} tokens ({})",
        format_token_count(u32::try_from(project_tokens).unwrap_or(u32::MAX)),
        project_source
    ));
    stdout.println(format!(
        "  History:         ~{} tokens",
        format_token_count(u32::try_from(history_tokens).unwrap_or(u32::MAX))
    ));
    stdout.println(format!(
        "  Available:       ~{} tokens",
        format_token_count(u32::try_from(available).unwrap_or(u32::MAX))
    ));
}

/// Run the interactive TUI render loop with a provider backend.
///
/// `tokio_handle` is used to spawn the provider's async streaming task.
/// The iocraft render loop runs on smol; the provider runs on tokio.
pub fn run(
    config: ProviderConfig,
    tokio_handle: &tokio::runtime::Handle,
    session_opts: SessionOpts,
) -> anyhow::Result<()> {
    let (event_tx, event_rx) = async_channel::unbounded();
    let (msg_tx, msg_rx) = async_channel::unbounded::<UserAction>();

    // Spawn the provider loop on tokio
    tokio_handle.spawn(provider_loop(config, msg_rx, event_tx));

    let channels = Channels { event_rx, msg_tx };

    smol::block_on(
        element! {
            ContextProvider(value: Context::owned(channels)) {
                ContextProvider(value: Context::owned(session_opts)) {
                    RozRepl
                }
            }
        }
        .render_loop()
        .ignore_ctrl_c(),
    )?;
    Ok(())
}

fn prompt_tool_schemas(dispatcher: &ToolDispatcher) -> Vec<roz_agent::prompt_assembler::ToolSchema> {
    dispatcher
        .schemas()
        .into_iter()
        .map(|schema| roz_agent::prompt_assembler::ToolSchema {
            name: schema.name,
            description: schema.description,
            parameters_json: serde_json::to_string(&schema.parameters).unwrap_or_else(|_| "{}".to_string()),
        })
        .collect()
}

fn agent_error_to_turn_execution_failure(error: AgentError) -> TurnExecutionFailure {
    match error {
        AgentError::Safety(message) => TurnExecutionFailure::new(RuntimeFailureKind::SafetyBlocked, message),
        AgentError::ToolDispatch { message, .. } => TurnExecutionFailure::new(RuntimeFailureKind::ToolError, message),
        AgentError::CircuitBreakerTripped {
            consecutive_error_turns,
        } => TurnExecutionFailure::new(
            RuntimeFailureKind::CircuitBreakerTripped,
            format!("circuit breaker tripped after {consecutive_error_turns} consecutive all-error turns"),
        ),
        AgentError::Cancelled { .. } => TurnExecutionFailure::new(RuntimeFailureKind::OperatorAbort, "turn cancelled"),
        other => TurnExecutionFailure::new(RuntimeFailureKind::ModelError, other.to_string()),
    }
}

fn build_local_model(config: &ProviderConfig) -> Result<Box<dyn Model>, String> {
    let Some(api_key) = config.api_key.as_deref() else {
        return Err("No API key configured".to_string());
    };
    let proxy_provider = if config.provider == Provider::Openai {
        "openai"
    } else {
        "anthropic"
    };

    roz_agent::model::create_model(&config.model, "", "", 120, proxy_provider, Some(api_key))
        .map_err(|error| format!("Failed to create model: {error}"))
}

struct TuiStreamingTurnExecutor<'a> {
    agent_loop: &'a mut AgentLoop,
}

impl StreamingTurnExecutor for TuiStreamingTurnExecutor<'_> {
    fn execute_turn_streaming(&mut self, prepared: PreparedTurn) -> StreamingTurnHandle<'_> {
        let prepared_agent_mode: AgentLoopMode = prepared.cognition_mode();
        debug_assert!(
            !prepared.system_blocks.is_empty(),
            "SessionRuntime should always provide system blocks"
        );
        let system_prompt: Vec<String> = prepared.system_blocks.into_iter().map(|block| block.content).collect();
        let seed = AgentInputSeed::new(system_prompt, prepared.history, prepared.user_message);
        let (chunk_tx, chunk_rx) = tokio::sync::mpsc::channel(256);
        let (presence_tx, presence_rx) = tokio::sync::mpsc::channel(64);
        let agent_loop = &mut *self.agent_loop;
        let input = AgentInput::runtime_shell(
            uuid::Uuid::new_v4().to_string(),
            "cli",
            "",
            prepared_agent_mode,
            20,
            8192,
            200_000,
            true,
            None,
            roz_core::safety::ControlMode::default(),
        );

        StreamingTurnHandle {
            completion: Box::pin(async move {
                let output = agent_loop
                    .run_streaming_seeded(input, seed, chunk_tx, presence_tx)
                    .await
                    .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> {
                        Box::new(agent_error_to_turn_execution_failure(error))
                    })?;

                let assistant_message: String = output
                    .messages
                    .iter()
                    .filter(|message| message.role == MessageRole::Assistant)
                    .filter_map(roz_agent::model::types::Message::text)
                    .collect();

                Ok(TurnOutput {
                    assistant_message,
                    tool_calls_made: output.cycles,
                    input_tokens: u64::from(output.total_usage.input_tokens),
                    output_tokens: u64::from(output.total_usage.output_tokens),
                    cache_read_tokens: u64::from(output.total_usage.cache_read_tokens),
                    cache_creation_tokens: u64::from(output.total_usage.cache_creation_tokens),
                    messages: output.messages,
                })
            }),
            chunk_rx,
            presence_rx,
            tool_call_rx: None,
        }
    }
}

struct LocalByokRuntimeSession {
    runtime: SessionRuntime,
    agent_loop: AgentLoop,
    _copper_handle: Option<roz_copper::handle::CopperHandle>,
}

impl LocalByokRuntimeSession {
    fn new(config: &ProviderConfig) -> Result<Self, String> {
        let model = build_local_model(config)?;
        Ok(Self::build_with_model(std::path::Path::new("."), &config.model, model))
    }

    fn build_with_model(project_dir: &std::path::Path, model_name: &str, model: Box<dyn Model>) -> Self {
        let all_tools = tools::build_all_tools_with_copper(project_dir);
        let tool_names = all_tools.dispatcher.tool_names();
        let tool_name_refs: Vec<&str> = tool_names.iter().map(String::as_str).collect();
        let system_prompt = tools::build_system_prompt(project_dir, &tool_name_refs);
        let constitution = system_prompt.first().cloned().unwrap_or_default();
        let project_context = system_prompt.get(1..).unwrap_or_default().to_vec();
        let tool_schemas = prompt_tool_schemas(&all_tools.dispatcher);

        let session_config = SessionConfig {
            session_id: uuid::Uuid::new_v4().to_string(),
            tenant_id: "cli".to_string(),
            mode: SessionMode::Local,
            cognition_mode: CognitionMode::React,
            constitution_text: constitution,
            blueprint_toml: String::new(),
            model_name: Some(model_name.to_string()),
            permissions: Vec::new(),
            tool_schemas,
            project_context,
            initial_history: Vec::new(),
        };

        let runtime = SessionRuntime::new(&session_config);
        let approval_handle = runtime.approval_handle();
        let safety = SafetyStack::new(vec![]);
        let spatial = Box::new(NullWorldStateProvider);
        let agent_loop = AgentLoop::new(model, all_tools.dispatcher, safety, spatial)
            .with_extensions(all_tools.extensions)
            .with_approval_runtime(approval_handle);

        Self {
            runtime,
            agent_loop,
            _copper_handle: all_tools.copper_handle,
        }
    }

    async fn run_message(
        &mut self,
        user_text: String,
        event_tx: &async_channel::Sender<AgentEvent>,
    ) -> Result<(), String> {
        let message_id = uuid::Uuid::new_v4().to_string();
        let mut runtime_events = self.runtime.subscribe_events();
        let event_tx_forward = event_tx.clone();
        let turn_message_id = message_id.clone();
        let forwarder = tokio::spawn(async move {
            while let Ok(envelope) = runtime_events.recv().await {
                match envelope.event {
                    SessionEvent::TextDelta { message_id, content } if message_id == turn_message_id => {
                        let _ = event_tx_forward.send(AgentEvent::TextDelta(content)).await;
                    }
                    SessionEvent::ThinkingDelta { message_id, content } if message_id == turn_message_id => {
                        let _ = event_tx_forward.send(AgentEvent::ThinkingDelta(content)).await;
                    }
                    SessionEvent::ToolCallRequested {
                        call_id,
                        tool_name,
                        parameters,
                        ..
                    } => {
                        let params =
                            serde_json::to_string_pretty(&parameters).unwrap_or_else(|_| parameters.to_string());
                        let _ = event_tx_forward
                            .send(AgentEvent::ToolRequest {
                                id: call_id,
                                name: tool_name,
                                params,
                            })
                            .await;
                    }
                    SessionEvent::TurnFinished {
                        message_id,
                        input_tokens,
                        output_tokens,
                        stop_reason,
                        ..
                    } if message_id == turn_message_id => {
                        let _ = event_tx_forward
                            .send(AgentEvent::TurnComplete {
                                input_tokens,
                                output_tokens,
                                stop_reason,
                            })
                            .await;
                        break;
                    }
                    _ => {}
                }
            }
        });

        let runtime = &mut self.runtime;
        let agent_loop = &mut self.agent_loop;
        let mut executor = TuiStreamingTurnExecutor { agent_loop };
        let result = runtime
            .run_turn_streaming(
                TurnInput {
                    user_message: user_text,
                    cognition_mode: CognitionMode::React,
                    custom_context: Vec::new(),
                    volatile_blocks: Vec::new(),
                },
                Some(message_id),
                &mut executor,
            )
            .await;

        match result {
            Ok(StreamingTurnResult::Completed(_) | StreamingTurnResult::Cancelled) => {
                let _ = forwarder.await;
                Ok(())
            }
            Err(error) => {
                forwarder.abort();
                Err(error.to_string())
            }
        }
    }
}

/// Provider loop: receives user actions and streams responses.
///
/// For Cloud: delegates to a persistent gRPC session.
/// For local BYOK providers: delegates turns through `SessionRuntime`.
#[allow(clippy::too_many_lines)]
async fn provider_loop(
    config: ProviderConfig,
    msg_rx: async_channel::Receiver<UserAction>,
    event_tx: async_channel::Sender<AgentEvent>,
) {
    // Spawn background token refresh for OpenAI OAuth sessions.
    // OpenAI access tokens expire after ~1 hour; refreshing every 8 minutes
    // keeps the session alive without user intervention.
    if config.provider == Provider::Openai {
        tokio::spawn(async {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(8 * 60)).await;
                if let Err(e) = crate::commands::auth::refresh_openai_token().await {
                    eprintln!("warning: OpenAI token refresh failed: {e}");
                }
            }
        });
    }

    if config.provider == Provider::Cloud {
        // Cloud path: forward only Message actions, handle SwitchModel locally.
        let (text_tx, text_rx) = async_channel::unbounded::<String>();
        let event_tx_cloud = event_tx.clone();

        // Build unified tool set (CLI built-ins + daemon tools from embodiment.toml,
        // with legacy robot.toml accepted as fallback).
        let local_tool_opts = providers::cloud::build_local_tool_opts(std::path::Path::new("."));

        // Bridge: filter UserAction into plain text for the gRPC stream.
        let bridge = tokio::spawn({
            let msg_rx = msg_rx.clone();
            let event_tx = event_tx.clone();
            async move {
                while let Ok(action) = msg_rx.recv().await {
                    match action {
                        UserAction::Message(text) => {
                            let _ = text_tx.send(text).await;
                        }
                        UserAction::SwitchModel { model_ref } => {
                            let _ = event_tx.send(AgentEvent::Connected { model: model_ref }).await;
                        }
                    }
                }
            }
        });

        if let Err(e) = providers::cloud::stream_session(&config, text_rx, event_tx_cloud, local_tool_opts).await {
            let display = classify_error_message(&e.to_string(), &config);
            let _ = event_tx.send(AgentEvent::Error(display)).await;
        }
        bridge.abort();
        return;
    }

    let mut local_session = match LocalByokRuntimeSession::new(&config) {
        Ok(session) => session,
        Err(error) => {
            let _ = event_tx.send(AgentEvent::Error(error)).await;
            return;
        }
    };

    let _ = event_tx
        .send(AgentEvent::Connected {
            model: config.model.clone(),
        })
        .await;

    // Process user actions
    while let Ok(action) = msg_rx.recv().await {
        match action {
            UserAction::SwitchModel { model_ref } => {
                // Notify the TUI of the model change. Full session tear-down is future work.
                let _ = event_tx.send(AgentEvent::Connected { model: model_ref }).await;
            }
            UserAction::Message(user_text) => {
                if let Err(error) = local_session.run_message(user_text, &event_tx).await {
                    let display = classify_error_message(&error, &config);
                    let _ = event_tx.send(AgentEvent::Error(display)).await;
                    let _ = event_tx
                        .send(AgentEvent::TurnComplete {
                            input_tokens: 0,
                            output_tokens: 0,
                            stop_reason: "error".to_string(),
                        })
                        .await;
                }
            }
        }
    }
}

#[cfg(test)]
mod truncate_tests {
    use super::*;

    #[test]
    fn no_truncation_needed() {
        assert_eq!(truncate_to_width_with_ellipsis("hello", 10), "hello");
    }

    #[test]
    fn truncate_with_ellipsis() {
        assert_eq!(truncate_to_width_with_ellipsis("hello world", 8), "hello w\u{2026}");
    }

    #[test]
    fn truncate_to_one() {
        assert_eq!(truncate_to_width_with_ellipsis("hello", 1), "\u{2026}");
    }

    #[test]
    fn truncate_to_zero() {
        assert_eq!(truncate_to_width_with_ellipsis("hello", 0), "");
    }
}

#[cfg(test)]
mod user_action_tests {
    use super::*;

    #[test]
    fn user_action_message() {
        let action = UserAction::Message("hello".to_string());
        assert!(matches!(action, UserAction::Message(s) if s == "hello"));
    }

    #[test]
    fn user_action_switch_model() {
        let action = UserAction::SwitchModel {
            model_ref: "anthropic/claude-opus-4-6".to_string(),
        };
        assert!(matches!(action, UserAction::SwitchModel { model_ref } if model_ref == "anthropic/claude-opus-4-6"));
    }

    #[test]
    fn user_action_equality() {
        let a = UserAction::Message("hello".to_string());
        let b = UserAction::Message("hello".to_string());
        assert_eq!(a, b);

        let c = UserAction::SwitchModel {
            model_ref: "ollama/llama3".to_string(),
        };
        let d = UserAction::SwitchModel {
            model_ref: "ollama/llama3".to_string(),
        };
        assert_eq!(c, d);
        assert_ne!(a, c);
    }
}

#[cfg(test)]
mod local_byok_runtime_tests {
    use super::*;
    use roz_agent::model::types::{
        CompletionResponse, ContentPart, MockModel, ModelCapability, StopReason, TokenUsage,
    };

    fn text_mock(responses: Vec<&str>) -> MockModel {
        MockModel::new(
            vec![ModelCapability::TextReasoning],
            responses
                .into_iter()
                .map(|text| CompletionResponse {
                    parts: vec![ContentPart::Text { text: text.to_string() }],
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage {
                        input_tokens: 10,
                        output_tokens: 5,
                        ..Default::default()
                    },
                })
                .collect(),
        )
    }

    #[tokio::test]
    async fn local_byok_runtime_session_retains_history_across_turns() {
        let dir = tempfile::tempdir().expect("create temp project dir");
        let model: Box<dyn Model> = Box::new(text_mock(vec!["First response", "Second response"]));
        let mut session = LocalByokRuntimeSession::build_with_model(dir.path(), "anthropic/claude-sonnet-4-6", model);
        let (event_tx, _event_rx) = async_channel::unbounded();

        session
            .run_message("first turn".to_string(), &event_tx)
            .await
            .expect("first turn should succeed");
        let history_after_first = session.runtime.export_bootstrap().history.len();
        assert!(
            history_after_first >= 2,
            "first turn should persist at least the user + assistant messages"
        );

        session
            .run_message("second turn".to_string(), &event_tx)
            .await
            .expect("second turn should succeed");
        let bootstrap = session.runtime.export_bootstrap();
        assert!(
            bootstrap.history.len() > history_after_first,
            "second turn should append to runtime-owned history"
        );
        assert_eq!(bootstrap.model_name.as_deref(), Some("anthropic/claude-sonnet-4-6"));
    }
}
