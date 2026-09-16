//! Daemon-owned ChatGPT text-chat driver (Stage 3 MVP).
//!
//! One background worker thread owns the whole conversation: each `prompt`
//! becomes a single stateless `POST /responses` (plus bounded retries), and
//! the `text/event-stream` body is parsed incrementally into the existing
//! [`DriverEvent`] vocabulary the app pump already renders. No chat UI, wire,
//! or Codex code changes — events flow through the standard daemon forwarder.
//!
//! Architecture rules:
//! - Bearer material lives only in [`FreshAuth`] (memory) and the curl `-K -`
//!   stdin config. It never touches argv, disk, logs, events, or the wire.
//!   The JSON request body (model, input text) briefly touches a `0600` temp
//!   file because stdin is already claimed by the header config; the file
//!   holds no credentials and is removed when the turn ends.
//! - All blocking work (curl spawn, stdout reads, refresh) runs on the worker
//!   thread. Render and the UI pump only ever see [`DriverEvent`]s.
//! - Continuity is client-side: the endpoint runs `store: false`, so the
//!   worker resends its input history every turn, carrying the completed
//!   `output` items (notably `reasoning.encrypted_content`) forward. No
//!   server thread id is invented, and nothing is written to `~/.codex`.
//! - Retries are bounded: at most one re-authenticated retry per 401 and one
//!   tier-stripped retry per unsupported-`fast` rejection (mirroring the
//!   upstream proxy). No other automatic retries; 429s are reported with
//!   their `Retry-After` wait, never retried.
//!
//! Mock transports back every test here; no test touches the network or real
//! credentials.

use std::io::{Read, Write};
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::Mutex;
use serde_json::{Map, Value};

use super::{DriverControl, DriverEventSender, DriverStartOptions, SessionOptions};
use crate::chatgpt_protocol::{
    ChatGptError, ChatGptErrorCode, CodexRequestHeaders, ResponsesSseParser, ResponsesStreamEvent,
    completed_output_items, filter_codex_input, is_unsupported_service_tier_error,
    normalize_responses_body, parse_retry_after_secs, validate_responses_request,
};
use crate::chatgpt_session::{
    CURL_PATH, ChatGptSessionManager, FreshAuth, default_session_manager,
};
use crate::model::CHATGPT_HISTORY_SEED_LIMIT;
use crate::model::{ChatGptHistoryRole, ChatGptHistorySeed, DriverEvent, ProviderResumeCursor};

/// Total curl wall-clock budget per turn attempt. Streaming turns run for
/// minutes; unary auth calls keep their own 20s budget elsewhere.
const STREAM_MAX_TIME_SECS: &str = "600";
/// Upper bound on a non-2xx response body kept for error classification. Raw
/// bodies are never surfaced — only matched for known shapes — so this bounds
/// memory, not disclosure.
const ERROR_BODY_CAP_BYTES: usize = 64 * 1024;
/// Client-side input history cap (items, not tokens). Oldest turns drain
/// first; indices stay consistent via [`Worker::enforce_history_cap`].
/// Single source of truth lives in the protocol crate so the restart seed
/// builder and the live worker can never drift apart.
const MAX_HISTORY_ITEMS: usize = CHATGPT_HISTORY_SEED_LIMIT;

// ---------------------------------------------------------------------------
// Streaming transport (curl with secrets on stdin, like `usage.rs`)
// ---------------------------------------------------------------------------

/// Response head of a started stream: status plus any `Retry-After` wait the
/// caller reports instead of retrying.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StreamHead {
    pub status: u16,
    pub retry_after_secs: Option<u64>,
}

/// Incremental body of a started stream. `cancel` aborts the transfer so a
/// blocked [`StreamBody::read_chunk`] fails fast; `finish` reaps the child.
pub trait StreamBody: Send {
    fn read_chunk(&mut self, buf: &mut [u8]) -> std::io::Result<usize>;
    fn cancel(&mut self);
    fn finish(&mut self);
}

/// Starts one authenticated `/responses` stream. `header_config` carries the
/// bearer on a curl `-K` stdin config (never argv); `body` is the JSON
/// request. Implementations must never log either.
pub trait ResponsesStreamTransport: Send + Sync {
    fn start_stream(
        &self,
        url: &str,
        header_config: &str,
        body: &str,
    ) -> Result<(StreamHead, Box<dyn StreamBody>), ChatGptError>;
}

/// Production transport over the system `curl`.
pub struct CurlStreamTransport;

impl ResponsesStreamTransport for CurlStreamTransport {
    fn start_stream(
        &self,
        url: &str,
        header_config: &str,
        body: &str,
    ) -> Result<(StreamHead, Box<dyn StreamBody>), ChatGptError> {
        let body_path = write_body_file(body)?;
        let spawn_error = |message: &str| {
            remove_body_file(&body_path);
            network_error(message)
        };
        let mut child = crate::command_env::plain_command(CURL_PATH)
            .args([
                "-sS",
                // Disable curl's internal buffering: deltas must reach the
                // parser (and the UI) as they arrive, not at response end.
                "-N",
                "--max-time",
                STREAM_MAX_TIME_SECS,
                // Response headers prefix stdout; the driver consumes the
                // header block first, then streams the SSE body.
                "-D",
                "-",
                "-X",
                "POST",
                "-K",
                "-",
                "--data-binary",
                &format!("@{}", body_path.display()),
                url,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| spawn_error("Could not start the ChatGPT request"))?;
        // Bearer material travels on stdin, exactly like the `usage.rs`
        // headers. Close stdin afterwards so curl proceeds.
        let stdin_ok = child
            .stdin
            .as_mut()
            .is_some_and(|stdin| stdin.write_all(header_config.as_bytes()).is_ok());
        drop(child.stdin.take());
        if !stdin_ok {
            let _ = child.kill();
            let _ = child.wait();
            return Err(spawn_error("Failed to send the ChatGPT request"));
        }
        let mut stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(spawn_error("ChatGPT request stdout is unavailable"));
            }
        };
        match read_response_head(&mut stdout) {
            Ok(head) => Ok((
                head,
                Box::new(CurlStreamBody {
                    child: Some(child),
                    stdout: Some(stdout),
                    body_path: Some(body_path),
                }) as Box<dyn StreamBody>,
            )),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                remove_body_file(&body_path);
                Err(error)
            }
        }
    }
}

/// Writes the JSON request body to an owner-only temp file. The body holds
/// the user's prompt (already stored in Waku's own session) but no
/// credentials; stdin is claimed by the `-K -` header config, so the body
/// cannot travel there too.
fn write_body_file(body: &str) -> Result<std::path::PathBuf, ChatGptError> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "waku-chatgpt-resp-{}-{}.json",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut file| file.write_all(body.as_bytes()))
            .map_err(|_| network_error("Could not stage the ChatGPT request"))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&path, body.as_bytes())
            .map_err(|_| network_error("Could not stage the ChatGPT request"))?;
    }
    Ok(path)
}

fn remove_body_file(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
}

/// Reads the `-D -` header block (through the blank separator line),
/// skipping `1xx` interim blocks. Returns status plus any `Retry-After` wait.
fn read_response_head(stdout: &mut impl Read) -> Result<StreamHead, ChatGptError> {
    let mut raw = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        match stdout.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                raw.push(byte[0]);
                if raw.len() > 32 * 1024 {
                    return Err(network_error("ChatGPT response headers are too large"));
                }
                if raw.ends_with(b"\r\n\r\n") || raw.ends_with(b"\n\n") {
                    break;
                }
            }
            Err(_) => return Err(network_error("Could not read the ChatGPT response")),
        }
    }
    let text = String::from_utf8_lossy(&raw);
    let mut lines = text.lines();
    let status_line = lines
        .next()
        .ok_or_else(|| network_error("ChatGPT response carried no status"))?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| network_error("ChatGPT response carried no status"))?;
    if (100..200).contains(&status) {
        // Interim block (e.g. `103 Early Hints`): the final block follows.
        return read_response_head(stdout);
    }
    Ok(StreamHead {
        status,
        retry_after_secs: parse_retry_after_secs(&text),
    })
}

struct CurlStreamBody {
    child: Option<Child>,
    stdout: Option<std::process::ChildStdout>,
    body_path: Option<std::path::PathBuf>,
}

impl StreamBody for CurlStreamBody {
    fn read_chunk(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.stdout
            .as_mut()
            .ok_or_else(|| std::io::Error::other("stream closed"))
            .and_then(|stdout| stdout.read(buf))
    }

    fn cancel(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            self.child = Some(child);
        }
    }

    fn finish(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.stdout.take();
        if let Some(path) = self.body_path.take() {
            remove_body_file(&path);
        }
    }
}

impl Drop for CurlStreamBody {
    fn drop(&mut self) {
        self.finish();
    }
}

fn network_error(message: &str) -> ChatGptError {
    ChatGptError::new(ChatGptErrorCode::NetworkError, message)
}

// ---------------------------------------------------------------------------
// Request construction (pure; reused Stage 1 helpers throughout)
// ---------------------------------------------------------------------------

/// Builds one `/responses` request body: selected model, full client-side
/// input history, `stream: true`, then Stage 1 normalization (stateless
/// `store: false`, reasoning defaults, encrypted-content include, id
/// stripping, token-cap rejection). Returns the model and the JSON text.
fn build_responses_body(
    model: &str,
    input: Vec<Value>,
    reasoning_effort: Option<&str>,
    service_tier: Option<&str>,
) -> Result<(String, String), ChatGptError> {
    let mut body = Map::new();
    body.insert("model".to_owned(), Value::String(model.to_owned()));
    body.insert("input".to_owned(), Value::Array(input));
    body.insert("stream".to_owned(), Value::Bool(true));
    let resolved = validate_responses_request(&mut body, model, None)?;
    let normalized = normalize_responses_body(
        body,
        &crate::chatgpt_protocol::ResponsesDefaults {
            reasoning_effort: reasoning_effort.map(str::to_owned),
            service_tier: service_tier.map(str::to_owned),
            ..Default::default()
        },
    );
    serde_json::to_string(&Value::Object(normalized))
        .map(|json| (resolved, json))
        .map_err(|_| {
            ChatGptError::new(
                ChatGptErrorCode::InvalidRequest,
                "ChatGPT request could not be encoded",
            )
        })
}

/// Renders the `-K -` stdin config: bearer headers plus the SSE accept and
/// JSON content type. The caller holds the only copy; tests assert even
/// redacted captures retain no bearer bytes.
fn render_header_config(access_token: &str, account_id: &str, originator: &str) -> String {
    let headers = CodexRequestHeaders::new(access_token, account_id, originator);
    let mut config = headers.curl_header_config();
    for line in [
        "Accept: text/event-stream",
        "Content-Type: application/json",
    ] {
        let escaped = line.replace('\\', "\\\\").replace('"', "\\\"");
        config.push_str(&format!("\nheader = \"{escaped}\""));
    }
    config
}

// ---------------------------------------------------------------------------
// Safe error messages (static text only; upstream bodies never surface)
// ---------------------------------------------------------------------------

const NO_MODEL_MESSAGE: &str = "No ChatGPT model is selected. Discover models in Settings → Providers → ChatGPT, then pick one.";
const SESSION_EXPIRED_MESSAGE: &str = "ChatGPT session expired. Sign in again.";
const NOT_CONNECTED_MESSAGE: &str =
    "ChatGPT is not connected. Connect it in Settings → Providers → ChatGPT.";
const NETWORK_MESSAGE: &str = "Could not reach the ChatGPT service.";
const MODEL_UNAVAILABLE_MESSAGE: &str = "The selected ChatGPT model is unavailable. Discover models again in Settings → Providers → ChatGPT.";
const RESPONSE_FAILED_MESSAGE: &str = "ChatGPT could not complete the response.";
const TRUNCATED_MESSAGE: &str = "The ChatGPT response ended before it completed.";
const INVALID_REQUEST_MESSAGE: &str = "ChatGPT rejected the request.";
const SERVICE_UNAVAILABLE_MESSAGE: &str = "The ChatGPT service is unavailable. Try again shortly.";

fn auth_failure_message(error: &ChatGptError) -> String {
    match error.code {
        ChatGptErrorCode::NotAuthenticated => NOT_CONNECTED_MESSAGE.to_owned(),
        ChatGptErrorCode::RefreshTokenInvalid => SESSION_EXPIRED_MESSAGE.to_owned(),
        ChatGptErrorCode::NetworkError => NETWORK_MESSAGE.to_owned(),
        _ => "Could not refresh the ChatGPT session.".to_owned(),
    }
}

fn rate_limited_message(retry_after_secs: Option<u64>) -> String {
    match retry_after_secs {
        Some(secs) => format!("ChatGPT is rate limited. Try again in {secs}s."),
        None => "ChatGPT is rate limited. Try again shortly.".to_owned(),
    }
}

/// Narrow model sniff over an error body: the account's model set (not the
/// request shape) is at fault only when the body names a model problem.
/// Anything else stays a generic rejection — bodies never reach the UI.
fn is_model_unavailable_body(body: &str) -> bool {
    let lower = body.to_lowercase();
    lower.contains("model")
        && [
            "not found",
            "does not exist",
            "unsupported",
            "unknown model",
            "invalid model",
        ]
        .iter()
        .any(|hint| lower.contains(hint))
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

enum CommandMessage {
    Prompt(String),
    GenerateTitle(String),
    Cancel,
    Rollback {
        turns: usize,
        response: Sender<Result<Option<ProviderResumeCursor>, String>>,
    },
    Shutdown,
}

#[derive(Clone, Debug, Default)]
struct LiveOptions {
    model: Option<String>,
    reasoning_effort: Option<String>,
    service_tier: Option<String>,
}

/// Cross-thread interrupt state: the worker parks the live body here so
/// `cancel` (and a superseding prompt) can abort a blocked read. A generation
/// bump lets the worker tell an abort apart from a transport failure.
struct Shared {
    body: Mutex<Option<Box<dyn StreamBody>>>,
    generation: AtomicU64,
    busy: AtomicBool,
}

impl Shared {
    fn abort_active_stream(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        if let Some(body) = self.body.lock().as_mut() {
            body.cancel();
        }
    }
}

pub struct ChatGptDriver {
    commands: Sender<CommandMessage>,
    shared: Arc<Shared>,
    options: Arc<Mutex<LiveOptions>>,
}

impl ChatGptDriver {
    pub fn start(options: DriverStartOptions, events: DriverEventSender) -> anyhow::Result<Self> {
        let manager = Arc::new(default_session_manager());
        Self::start_with_manager(options, events, manager, Arc::new(CurlStreamTransport))
    }

    fn start_with_manager(
        options: DriverStartOptions,
        events: DriverEventSender,
        manager: Arc<ChatGptSessionManager>,
        transport: Arc<dyn ResponsesStreamTransport>,
    ) -> anyhow::Result<Self> {
        // Continuity is history-based (see module docs): a persisted server
        // thread id would be invented, so any non-empty cursor is refused.
        if let Some(cursor) = options.provider_cursor
            && !cursor.native_id().is_empty()
        {
            anyhow::bail!(
                "cannot resume ChatGPT from a {} cursor",
                cursor.provider().display_name()
            );
        }
        let shared = Arc::new(Shared {
            body: Mutex::new(None),
            generation: AtomicU64::new(0),
            busy: AtomicBool::new(false),
        });
        let live = Arc::new(Mutex::new(LiveOptions {
            model: options.model.filter(|model| !model.trim().is_empty()),
            reasoning_effort: options.reasoning_effort,
            service_tier: options.service_tier,
        }));
        let (commands, incoming) = unbounded();
        // Restart resume: rebuild the resend-history a live worker would
        // have carried from the persisted Waku transcript seed. Post-seed
        // behavior is identical to a worker that had remained alive — the
        // next prompt still appends exactly once via `execute_turn`.
        let (history, turn_starts) =
            Worker::seed_history_from_transcript(options.chatgpt_history.unwrap_or_default());
        let mut worker = Worker {
            manager,
            transport,
            events,
            shared: shared.clone(),
            options: live.clone(),
            history,
            turn_starts,
        };
        worker.enforce_history_cap();
        std::thread::Builder::new()
            .name("waku-chatgpt-driver".into())
            .spawn(move || worker.run(incoming))
            .map_err(|error| anyhow::anyhow!("could not start the ChatGPT driver: {error}"))?;
        Ok(Self {
            commands,
            shared,
            options: live,
        })
    }

    fn send(&self, message: CommandMessage) {
        let _ = self.commands.send(message);
    }
}

impl Drop for ChatGptDriver {
    fn drop(&mut self) {
        // Best-effort: the worker exits on its own; the detached thread never
        // blocks teardown, and the live body is reaped on drop.
        self.send(CommandMessage::Shutdown);
    }
}

impl DriverControl for ChatGptDriver {
    fn prompt(&self, prompt: String) {
        // HTTP turns are unary: a prompt into a running turn aborts it (its
        // tail settles as stopped) and the new turn starts next.
        if self.shared.busy.load(Ordering::SeqCst) {
            self.shared.abort_active_stream();
        }
        self.send(CommandMessage::Prompt(prompt));
    }

    fn generate_title(&self, prompt: String) {
        self.send(CommandMessage::GenerateTitle(prompt));
    }

    fn cancel(&self) {
        self.shared.abort_active_stream();
        self.send(CommandMessage::Cancel);
    }

    fn respond(&self, _request_id: String, _option_id: String) {
        // Text chat raises no permissions; anything arriving here is stale.
    }

    fn apply_options(&self, options: SessionOptions) -> bool {
        // Model, effort, and tier are per-turn request fields: absorb in
        // place, no restart needed.
        *self.options.lock() = LiveOptions {
            model: options.model.filter(|model| !model.trim().is_empty()),
            reasoning_effort: options.reasoning_effort,
            service_tier: options.service_tier,
        };
        true
    }

    fn rollback(&self, turns: usize) -> anyhow::Result<Option<ProviderResumeCursor>> {
        let (response, answer) = unbounded();
        self.send(CommandMessage::Rollback { turns, response });
        // Rollback is unreachable through the UI (the provider reports no
        // rollback support); never block the daemon on a wedged worker.
        answer
            .recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| anyhow::anyhow!("ChatGPT rollback timed out"))?
            .map_err(anyhow::Error::msg)
    }

    // `fork` keeps the default "not supported" bail: no conversations exist
    // server-side to branch.
}

impl std::fmt::Debug for ChatGptDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatGptDriver").finish_non_exhaustive()
    }
}

struct Worker {
    manager: Arc<ChatGptSessionManager>,
    transport: Arc<dyn ResponsesStreamTransport>,
    events: DriverEventSender,
    shared: Arc<Shared>,
    options: Arc<Mutex<LiveOptions>>,
    history: Vec<Value>,
    turn_starts: Vec<usize>,
}

impl Worker {
    fn run(mut self, incoming: Receiver<CommandMessage>) {
        while let Ok(message) = incoming.recv() {
            match message {
                CommandMessage::Prompt(prompt) => self.run_turn(prompt),
                CommandMessage::GenerateTitle(prompt) => self.generate_title(prompt),
                CommandMessage::Cancel => {
                    // The abort already settled (or will settle) the turn via
                    // the interrupted read; nothing more to do.
                }
                CommandMessage::Rollback { turns, response } => {
                    let _ = response.send(self.rollback_history(turns));
                }
                CommandMessage::Shutdown => break,
            }
        }
    }

    fn emit(&self, event: DriverEvent) {
        let _ = self.events.send(event);
    }

    fn fail_turn(&self, message: &str) {
        self.emit(DriverEvent::Error(message.to_owned()));
        self.emit(DriverEvent::TurnFinished {
            success: false,
            summary: None,
        });
    }

    fn generate_title(&mut self, prompt: String) {
        let options = self.options.lock().clone();
        let Some(model) = options.model.filter(|model| !model.is_empty()) else {
            return;
        };
        let Ok(auth) = self.manager.ensure_fresh_auth() else {
            return;
        };
        let title_prompt = format!(
            "Create a concise chat title for this request. Return only the title, no quotes or markdown, at most 6 words.\n\n{prompt}"
        );
        let input = vec![
            serde_json::json!({"role":"user","content":[{"type":"input_text","text":title_prompt}]}),
        ];
        let Ok((_, body)) = build_responses_body(&model, input, None, None) else {
            return;
        };
        let config = self.manager.config();
        let headers =
            render_header_config(auth.access_token(), auth.account_id(), &config.originator);
        let Ok((head, mut stream)) =
            self.transport
                .start_stream(&config.responses_url(), &headers, &body)
        else {
            return;
        };
        if !(200..300).contains(&head.status) {
            stream.finish();
            return;
        }
        let mut parser = ResponsesSseParser::new();
        let mut decoder = Utf8StreamDecoder::new();
        let mut title = String::new();
        let mut buf = [0_u8; 4096];
        loop {
            match stream.read_chunk(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    for event in parser.push(&decoder.push(&buf[..n])) {
                        match event {
                            ResponsesStreamEvent::TextDelta(delta) => title.push_str(&delta),
                            ResponsesStreamEvent::Completed(_) => break,
                            _ => {}
                        }
                    }
                }
                Err(_) => break,
            }
        }
        stream.finish();
        let title = title.trim().trim_matches('"').trim().to_owned();
        if !title.is_empty() && title.chars().count() <= 80 {
            self.emit(DriverEvent::AutoTitleUpdated(Some(title)));
        }
    }

    fn run_turn(&mut self, prompt: String) {
        self.shared.busy.store(true, Ordering::SeqCst);
        self.emit(DriverEvent::TurnStarted);
        // Interrupts that land before the stream starts (a fast cancel, a
        // superseding prompt) must still settle the turn: attempts re-check
        // this generation after parking the live body.
        let turn_generation = self.shared.generation.load(Ordering::SeqCst);
        if self.execute_turn(&prompt, turn_generation) == TurnEnd::Completed {
            self.emit(DriverEvent::TurnFinished {
                success: true,
                summary: None,
            });
        }
        self.shared.busy.store(false, Ordering::SeqCst);
    }

    fn execute_turn(&mut self, prompt: &str, turn_generation: u64) -> TurnEnd {
        let options = self.options.lock().clone();
        let Some(model) = options.model.filter(|model| !model.is_empty()) else {
            self.fail_turn(NO_MODEL_MESSAGE);
            return TurnEnd::Settled;
        };
        let auth = match self.manager.ensure_fresh_auth() {
            Ok(auth) => auth,
            Err(error) => {
                self.fail_turn(&auth_failure_message(&error));
                return TurnEnd::Settled;
            }
        };

        let mut input = self.history.clone();
        let user_message =
            serde_json::json!({"role":"user","content":[{"type":"input_text","text":prompt}]});
        input.push(user_message.clone());

        let mut auth = auth;
        let mut tier = options.service_tier.clone();
        let mut retried_auth = false;
        let mut retried_tier = false;
        loop {
            let (_, body) = match build_responses_body(
                &model,
                input.clone(),
                options.reasoning_effort.as_deref(),
                tier.as_deref(),
            ) {
                Ok(built) => built,
                Err(_) => {
                    self.fail_turn(INVALID_REQUEST_MESSAGE);
                    return TurnEnd::Settled;
                }
            };
            match self.post_once(&auth, &body, tier.as_deref(), turn_generation) {
                PostOutcome::Done { output, text } => {
                    self.commit_history(user_message.clone(), output, text);
                    return TurnEnd::Completed;
                }
                PostOutcome::Interrupted => {
                    self.emit(DriverEvent::TurnFinished {
                        success: false,
                        summary: None,
                    });
                    return TurnEnd::Settled;
                }
                PostOutcome::Fatal(message) => {
                    self.fail_turn(&message);
                    return TurnEnd::Settled;
                }
                PostOutcome::RetryAuth if !retried_auth => {
                    retried_auth = true;
                    match self.manager.ensure_fresh_auth() {
                        Ok(fresh) => auth = fresh,
                        Err(error) => {
                            self.fail_turn(&auth_failure_message(&error));
                            return TurnEnd::Settled;
                        }
                    }
                }
                PostOutcome::RetryTier if !retried_tier => {
                    retried_tier = true;
                    tier = None;
                }
                // An exhausted retry means the second attempt failed the same
                // way: revoked credentials, or a tier the account truly
                // cannot use.
                PostOutcome::RetryAuth => {
                    self.fail_turn(SESSION_EXPIRED_MESSAGE);
                    return TurnEnd::Settled;
                }
                PostOutcome::RetryTier => {
                    self.fail_turn(RESPONSE_FAILED_MESSAGE);
                    return TurnEnd::Settled;
                }
            }
        }
    }

    /// One POST attempt: stream SSE to events, or classify the failure.
    /// `tier` is the tier the body carried (for the `fast` fallback check).
    /// `turn_generation` settles attempts superseded before they started.
    fn post_once(
        &mut self,
        auth: &FreshAuth,
        body: &str,
        tier: Option<&str>,
        turn_generation: u64,
    ) -> PostOutcome {
        let config = self.manager.config();
        let url = config.responses_url();
        let header_config =
            render_header_config(auth.access_token(), auth.account_id(), &config.originator);
        let generation = self.shared.generation.load(Ordering::SeqCst);
        let (head, stream) = match self.transport.start_stream(&url, &header_config, body) {
            Ok(started) => started,
            Err(error) => {
                return PostOutcome::Fatal(match error.code {
                    ChatGptErrorCode::NetworkError => NETWORK_MESSAGE.to_owned(),
                    _ => SERVICE_UNAVAILABLE_MESSAGE.to_owned(),
                });
            }
        };
        if !(200..300).contains(&head.status) {
            let mut stream = stream;
            let error_body = read_error_body(&mut *stream);
            stream.finish();
            return classify_http_error(head.status, head.retry_after_secs, tier, &error_body);
        }
        // Park the live body where `cancel` can abort it; every path below
        // takes it back and reaps it. An interrupt that landed while the
        // request was in flight settles quietly instead of streaming.
        *self.shared.body.lock() = Some(stream);
        if self.shared.generation.load(Ordering::SeqCst) != turn_generation {
            if let Some(mut stream) = self.shared.body.lock().take() {
                stream.finish();
            }
            return PostOutcome::Interrupted;
        }
        let outcome = self.consume_stream(generation);
        if let Some(mut stream) = self.shared.body.lock().take() {
            stream.finish();
        }
        outcome
    }

    /// Reads SSE chunks to completion, emitting deltas as they arrive.
    fn consume_stream(&mut self, generation: u64) -> PostOutcome {
        let mut parser = ResponsesSseParser::new();
        let mut decoder = Utf8StreamDecoder::new();
        let mut text = String::new();
        let mut buf = [0_u8; 8192];
        loop {
            let read = match self.shared.body.lock().as_mut() {
                Some(body) => body.read_chunk(&mut buf),
                None => {
                    return PostOutcome::Fatal(TRUNCATED_MESSAGE.to_owned());
                }
            };
            match read {
                Ok(0) => {
                    // Clean EOF mid-turn: an abort settles quietly, anything
                    // else truncated the response.
                    if self.shared.generation.load(Ordering::SeqCst) != generation {
                        return PostOutcome::Interrupted;
                    }
                    return PostOutcome::Fatal(TRUNCATED_MESSAGE.to_owned());
                }
                Ok(bytes) => {
                    let chunk = decoder.push(&buf[..bytes]);
                    for event in parser.push(&chunk) {
                        match event {
                            ResponsesStreamEvent::TextDelta(delta) => {
                                text.push_str(&delta);
                                self.emit(DriverEvent::TextDelta(delta));
                            }
                            ResponsesStreamEvent::ReasoningDelta(delta) => {
                                self.emit(DriverEvent::ReasoningDelta(delta));
                            }
                            ResponsesStreamEvent::Completed(payload) => {
                                return PostOutcome::Done {
                                    output: completed_output_items(&payload),
                                    text: std::mem::take(&mut text),
                                };
                            }
                            ResponsesStreamEvent::Failed => {
                                return PostOutcome::Fatal(RESPONSE_FAILED_MESSAGE.to_owned());
                            }
                            ResponsesStreamEvent::WebSearchCall(_) => {
                                // Phase 1: recognized at the protocol layer so
                                // the search item is no longer dropped.
                                // History capture lands in the activation
                                // phase; no behavior change yet.
                            }
                        }
                    }
                }
                Err(_) => {
                    if self.shared.generation.load(Ordering::SeqCst) != generation {
                        return PostOutcome::Interrupted;
                    }
                    return PostOutcome::Fatal(NETWORK_MESSAGE.to_owned());
                }
            }
        }
    }

    /// Rebuilds the worker's resend-history from the persisted-transcript
    /// seed carried on the start request. Item shapes match
    /// [`Worker::commit_history`] exactly — user items open turns recorded
    /// in `turn_starts`, assistant items take the rebuilt-text shape a live
    /// worker keeps — so the next turn's request is identical to one from a
    /// worker that had never restarted. No text is logged here; the seed
    /// only flows into the request body like live-turn history does.
    fn seed_history_from_transcript(seeds: Vec<ChatGptHistorySeed>) -> (Vec<Value>, Vec<usize>) {
        let mut history = Vec::with_capacity(seeds.len());
        let mut turn_starts = Vec::new();
        for seed in seeds {
            match seed.role {
                ChatGptHistoryRole::User => {
                    turn_starts.push(history.len());
                    history.push(serde_json::json!({
                        "role": "user",
                        "content": [{"type": "input_text", "text": seed.text}],
                    }));
                }
                ChatGptHistoryRole::Assistant => {
                    history.push(serde_json::json!({
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": seed.text}],
                    }));
                }
            }
        }
        (history, turn_starts)
    }

    /// Appends a completed turn (user message plus endpoint output items) to
    /// the client-side history. When the completed payload carries no output
    /// items, the streamed text rebuilds the assistant message so the next
    /// turn still sees it. Ids are stripped: the stateless endpoint rejects
    /// server-side ids on resubmission.
    fn commit_history(&mut self, user_message: Value, output: Vec<Value>, text: String) {
        let items = if output.is_empty() && !text.is_empty() {
            vec![
                serde_json::json!({"role":"assistant","content":[{"type":"output_text","text":text}]}),
            ]
        } else {
            filter_codex_input(&output)
        };
        self.turn_starts.push(self.history.len());
        self.history.push(user_message);
        self.history.extend(items);
        self.enforce_history_cap();
    }

    fn enforce_history_cap(&mut self) {
        if self.history.len() <= MAX_HISTORY_ITEMS {
            return;
        }
        let overflow = self.history.len() - MAX_HISTORY_ITEMS;
        self.history.drain(..overflow);
        for start in &mut self.turn_starts {
            *start = start.saturating_sub(overflow);
        }
        while self.turn_starts.len() > 1 && self.turn_starts[1] == 0 {
            self.turn_starts.remove(0);
        }
    }

    fn rollback_history(&mut self, turns: usize) -> Result<Option<ProviderResumeCursor>, String> {
        let keep = self.turn_starts.len().saturating_sub(turns);
        let index = self.turn_starts.get(keep).copied().unwrap_or(0);
        self.history.truncate(index);
        self.turn_starts.truncate(keep);
        Ok(None)
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum TurnEnd {
    Completed,
    Settled,
}

enum PostOutcome {
    Done { output: Vec<Value>, text: String },
    Interrupted,
    Fatal(String),
    RetryAuth,
    RetryTier,
}

/// Reads a non-2xx body up to the classification cap. Content is only sniffed
/// for known shapes — never forwarded.
fn read_error_body(stream: &mut dyn StreamBody) -> String {
    let mut body = Vec::new();
    let mut buf = [0_u8; 4096];
    while body.len() < ERROR_BODY_CAP_BYTES {
        match stream.read_chunk(&mut buf) {
            Ok(0) => break,
            Ok(bytes) => body.extend_from_slice(&buf[..bytes]),
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&body).into_owned()
}

fn classify_http_error(
    status: u16,
    retry_after_secs: Option<u64>,
    tier: Option<&str>,
    body: &str,
) -> PostOutcome {
    if status == 401 {
        // Fresh credentials were just minted: a 401 means revoked or rotated
        // elsewhere. One re-read heals a cross-instance rotation; a second
        // 401 settles as expired.
        return PostOutcome::RetryAuth;
    }
    if status == 429 {
        return PostOutcome::Fatal(rate_limited_message(retry_after_secs));
    }
    // Upstream proxy contract: a `fast` tier the account cannot use is
    // retried once with the field removed.
    if tier == Some("fast") && is_unsupported_service_tier_error(body) {
        return PostOutcome::RetryTier;
    }
    if status == 404 || ((status == 400 || status == 422) && is_model_unavailable_body(body)) {
        return PostOutcome::Fatal(MODEL_UNAVAILABLE_MESSAGE.to_owned());
    }
    if (500..600).contains(&status) {
        return PostOutcome::Fatal(SERVICE_UNAVAILABLE_MESSAGE.to_owned());
    }
    PostOutcome::Fatal(format!("{INVALID_REQUEST_MESSAGE} (status {status})"))
}

/// Incremental UTF-8 decoder for network chunks: complete characters decode
/// immediately, an incomplete tail carries over, and genuinely invalid bytes
/// become U+FFFD without losing the stream.
struct Utf8StreamDecoder {
    carry: Vec<u8>,
}

impl Utf8StreamDecoder {
    fn new() -> Self {
        Self { carry: Vec::new() }
    }

    fn push(&mut self, bytes: &[u8]) -> String {
        self.carry.extend_from_slice(bytes);
        let mut out = String::new();
        let mut rest = std::mem::take(&mut self.carry);
        loop {
            match std::str::from_utf8(&rest) {
                Ok(valid) => {
                    out.push_str(valid);
                    break;
                }
                Err(error) => {
                    let up_to = error.valid_up_to();
                    out.push_str(std::str::from_utf8(&rest[..up_to]).unwrap_or_default());
                    match error.error_len() {
                        // Incomplete sequence at the tail: wait for more.
                        None => {
                            self.carry = rest[up_to..].to_vec();
                            break;
                        }
                        // Invalid byte: replace and continue past it.
                        Some(len) => {
                            out.push('\u{FFFD}');
                            rest = rest[up_to + len..].to_vec();
                        }
                    }
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Tests (mock transports only; never real credentials or the network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chatgpt_protocol::{AUTH_CLAIM, DeviceAuthConfig, DevicePollOutcome, TokenSet};
    use crate::chatgpt_session::{DeviceAuthTransport, ManualClock, MemoryStore, RawDeviceCode};
    use crate::driver::test_event_channel;
    use std::collections::VecDeque;

    /// Distinctly fake bearer material: tests assert its *absence* from every
    /// retained capture, serialized event, and error string.
    const TEST_ACCESS_TOKEN: &str = "test-access-token-AAA";
    const TEST_ACCOUNT_ID: &str = "acct-test-driver";

    fn test_jwt() -> String {
        use base64::Engine as _;
        let encode =
            |value: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.as_bytes());
        format!(
            "{}.{}.{}",
            encode(r#"{"alg":"none"}"#),
            encode(
                &serde_json::json!({
                    AUTH_CLAIM: {"chatgpt_account_id": TEST_ACCOUNT_ID},
                })
                .to_string()
            ),
            encode("sig")
        )
    }

    fn token_response() -> Value {
        serde_json::json!({
            "access_token": test_jwt(),
            "refresh_token": "test-refresh-token-BBB",
            "expires_in": 3600,
        })
    }

    /// Guards the seeding path the driver tests depend on: the stub exchange
    /// normalizes into the expected account.
    #[test]
    fn stub_exchange_normalizes_account() {
        let tokens = TokenSet::from_token_response(&token_response(), None, 1_000_000).unwrap();
        assert_eq!(tokens.account_id(), Some(TEST_ACCOUNT_ID));
    }

    /// Device transport that signs in once, then fails refreshes as dead.
    struct StubDeviceTransport;

    impl DeviceAuthTransport for StubDeviceTransport {
        fn request_device_code(
            &self,
            _config: &DeviceAuthConfig,
        ) -> Result<RawDeviceCode, ChatGptError> {
            Ok(RawDeviceCode {
                device_auth_id: "device-test".to_owned(),
                user_code: "AAAA-BBBB".to_owned(),
                interval_secs: 1,
            })
        }

        fn poll_device_code(
            &self,
            _config: &DeviceAuthConfig,
            _device_auth_id: &str,
            _user_code: &str,
        ) -> Result<DevicePollOutcome, ChatGptError> {
            Ok(DevicePollOutcome::Authorized {
                authorization_code: "auth-code".to_owned(),
                code_verifier: "verifier".to_owned(),
            })
        }

        fn exchange_code(
            &self,
            _config: &DeviceAuthConfig,
            _authorization_code: &str,
            _code_verifier: &str,
        ) -> Result<Value, ChatGptError> {
            Ok(token_response())
        }

        fn refresh(
            &self,
            _config: &DeviceAuthConfig,
            _refresh_token: &str,
        ) -> Result<Value, ChatGptError> {
            Err(ChatGptError::new(
                ChatGptErrorCode::RefreshTokenInvalid,
                "refresh token is no longer valid",
            ))
        }

        fn fetch_models(
            &self,
            _config: &DeviceAuthConfig,
            _access_token: &str,
            _account_id: &str,
        ) -> Result<Value, ChatGptError> {
            Ok(Value::Array(Vec::new()))
        }
    }

    /// Scripted stream: head plus byte-exact chunks (split frames welcome).
    struct MockStream {
        head: StreamHead,
        chunks: VecDeque<std::io::Result<Vec<u8>>>,
    }

    struct MockStreamBody {
        chunks: VecDeque<std::io::Result<Vec<u8>>>,
        cancelled: bool,
    }

    impl StreamBody for MockStreamBody {
        fn read_chunk(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.cancelled {
                return Err(std::io::Error::other("cancelled"));
            }
            match self.chunks.pop_front() {
                Some(Ok(bytes)) => {
                    let take = bytes.len().min(buf.len());
                    buf[..take].copy_from_slice(&bytes[..take]);
                    if take < bytes.len() {
                        self.chunks.push_front(Ok(bytes[take..].to_vec()));
                    }
                    Ok(take)
                }
                Some(Err(error)) => Err(std::io::Error::other(error.to_string())),
                None => Ok(0),
            }
        }

        fn cancel(&mut self) {
            self.cancelled = true;
        }

        fn finish(&mut self) {}
    }

    /// Captured request: URL and body verbatim, headers as presence flags —
    /// the bearer itself is never retained, even in tests.
    #[derive(Debug)]
    struct CapturedRequest {
        url: String,
        had_bearer: bool,
        had_account_id: bool,
        had_sse_accept: bool,
        body: Value,
    }

    struct MockTransport {
        streams: Mutex<VecDeque<Result<MockStream, ChatGptError>>>,
        requests: Mutex<Vec<CapturedRequest>>,
    }

    impl MockTransport {
        fn with_streams(streams: Vec<Result<MockStream, ChatGptError>>) -> Self {
            Self {
                streams: Mutex::new(streams.into()),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn ok(status: u16, chunks: Vec<&[u8]>) -> Result<MockStream, ChatGptError> {
            Ok(MockStream {
                head: StreamHead {
                    status,
                    retry_after_secs: None,
                },
                chunks: chunks.into_iter().map(|chunk| Ok(chunk.to_vec())).collect(),
            })
        }
    }

    impl ResponsesStreamTransport for MockTransport {
        fn start_stream(
            &self,
            url: &str,
            header_config: &str,
            body: &str,
        ) -> Result<(StreamHead, Box<dyn StreamBody>), ChatGptError> {
            // Presence, not values: the bearer must be provably absent from
            // everything this transport retains.
            let has_secret =
                header_config.contains(TEST_ACCESS_TOKEN) || body.contains(TEST_ACCESS_TOKEN);
            assert!(!has_secret, "test transport retained the bearer");
            self.requests.lock().push(CapturedRequest {
                url: url.to_owned(),
                had_bearer: header_config.contains("Authorization: Bearer "),
                had_account_id: header_config.contains("chatgpt-account-id: "),
                had_sse_accept: header_config.contains("text/event-stream"),
                body: serde_json::from_str(body).unwrap_or(Value::Null),
            });
            self.streams
                .lock()
                .pop_front()
                .unwrap_or(Err(network_error("mock streams exhausted")))
                .map(|stream| {
                    (
                        stream.head,
                        Box::new(MockStreamBody {
                            chunks: stream.chunks,
                            cancelled: false,
                        }) as Box<dyn StreamBody>,
                    )
                })
        }
    }

    fn authenticated_manager() -> Arc<ChatGptSessionManager> {
        let manager = Arc::new(ChatGptSessionManager::open(
            Arc::new(MemoryStore::default()),
            Arc::new(StubDeviceTransport),
            Arc::new(ManualClock::new(1_000_000)),
        ));
        manager.start_device_login().unwrap();
        manager.poll().unwrap();
        assert!(matches!(
            manager.public_session().unwrap().status,
            crate::chatgpt_session::LoginStatus::Authenticated
        ));
        manager
    }

    fn start_options(model: Option<&str>) -> DriverStartOptions {
        DriverStartOptions {
            binary: std::path::PathBuf::from("chatgpt"),
            cwd: std::path::PathBuf::from("/tmp"),
            mode: crate::model::RuntimeMode::default(),
            model: model.map(str::to_owned),
            reasoning_effort: None,
            service_tier: None,
            context_window: None,
            agent_preset: None,
            computer_use_enabled: false,
            provider_cursor: None,
            chatgpt_history: None,
        }
    }

    fn start_driver(
        manager: Arc<ChatGptSessionManager>,
        transport: Arc<MockTransport>,
        model: Option<&str>,
    ) -> (ChatGptDriver, Receiver<DriverEvent>) {
        let (events, receiver) = test_event_channel();
        let driver =
            ChatGptDriver::start_with_manager(start_options(model), events, manager, transport)
                .unwrap();
        (driver, receiver)
    }

    fn collect_until_finished(receiver: &Receiver<DriverEvent>) -> Vec<DriverEvent> {
        let mut events = Vec::new();
        loop {
            let event = receiver
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("driver turn settled");
            let finished = matches!(event, DriverEvent::TurnFinished { .. });
            events.push(event);
            if finished {
                return events;
            }
        }
    }

    fn sse_text_frame(delta: &str) -> String {
        format!(
            "data: {{\"type\":\"response.output_text.delta\",\"delta\":{}}}\n\n",
            serde_json::to_string(delta).unwrap()
        )
    }

    fn sse_completed_frame() -> String {
        "data: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n".to_owned()
    }

    #[test]
    fn request_carries_model_headers_and_normalized_body() {
        let completed = sse_completed_frame();
        let transport = Arc::new(MockTransport::with_streams(vec![MockTransport::ok(
            200,
            vec![completed.as_bytes()],
        )]));
        let (driver, receiver) =
            start_driver(authenticated_manager(), transport.clone(), Some("gpt-5.5"));
        driver.prompt("Hello".to_owned());
        let events = collect_until_finished(&receiver);
        assert!(matches!(events.first(), Some(DriverEvent::TurnStarted)));
        assert!(matches!(
            events.last(),
            Some(DriverEvent::TurnFinished { success: true, .. })
        ));

        let requests = transport.requests.lock();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert!(request.url.contains("/responses?"));
        assert!(request.url.contains("client_version="));
        assert!(request.had_bearer);
        assert!(request.had_account_id);
        assert!(request.had_sse_accept);
        assert_eq!(request.body["model"], "gpt-5.5");
        assert_eq!(request.body["stream"], true);
        assert_eq!(request.body["store"], false);
        assert_eq!(request.body["reasoning"]["effort"], "medium");
        assert!(request.body["include"].as_array().is_some_and(|include| {
            include
                .iter()
                .any(|item| item == "reasoning.encrypted_content")
        }));
        assert!(request.body.get("max_output_tokens").is_none());
        let input = request.body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "user");
    }

    #[test]
    fn text_streams_progressively_and_history_carries_forward() {
        let first = format!("{}{}", sse_text_frame("Hel"), sse_text_frame("lo"));
        let completed = sse_completed_frame();
        let second_text = sse_text_frame("Again");
        let transport = Arc::new(MockTransport::with_streams(vec![
            MockTransport::ok(200, vec![first.as_bytes(), completed.as_bytes()]),
            MockTransport::ok(200, vec![second_text.as_bytes(), completed.as_bytes()]),
        ]));
        let (driver, receiver) =
            start_driver(authenticated_manager(), transport.clone(), Some("gpt-5.5"));
        driver.prompt("Hello".to_owned());
        let first_events = collect_until_finished(&receiver);
        let deltas: Vec<_> = first_events
            .iter()
            .filter_map(|event| match event {
                DriverEvent::TextDelta(text) => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["Hel".to_owned(), "lo".to_owned()]);

        // Second turn resends the first turn's messages (continuity).
        driver.prompt("And again".to_owned());
        collect_until_finished(&receiver);
        let requests = transport.requests.lock();
        assert_eq!(requests.len(), 2);
        let input = requests[1].body["input"].as_array().unwrap();
        assert!(input.len() >= 3, "history carries forward: {input:?}");
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["text"], "Hello");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input.last().unwrap()["content"][0]["text"], "And again");
        // Stateless resubmission carries no server-side ids.
        for item in input {
            assert!(
                item.get("id").is_none(),
                "history item carries id: {item:?}"
            );
        }
    }

    #[test]
    fn split_chunks_parse_as_one_stream() {
        // One SSE frame split across three network chunks.
        let frame = sse_text_frame("chunked");
        let bytes = frame.as_bytes();
        let (a, rest) = bytes.split_at(11);
        let (b, c) = rest.split_at(17);
        let completed = sse_completed_frame();
        let transport = Arc::new(MockTransport::with_streams(vec![MockTransport::ok(
            200,
            vec![a, b, c, completed.as_bytes()],
        )]));
        let (driver, receiver) =
            start_driver(authenticated_manager(), transport.clone(), Some("gpt-5.5"));
        driver.prompt("hi".to_owned());
        let events = collect_until_finished(&receiver);
        assert!(events.iter().any(|event| matches!(
            event,
            DriverEvent::TextDelta(text) if text == "chunked"
        )));
    }

    #[test]
    fn multibyte_text_split_across_chunks_decodes() {
        // "日本語" framed as SSE, split mid-character across two chunks.
        let frame = sse_text_frame("日本語");
        let bytes = frame.as_bytes();
        // Split inside the first CJK character's UTF-8 sequence.
        let prefix_len = frame.find("日本語").unwrap() + 1;
        let (a, b) = bytes.split_at(prefix_len);
        let completed = sse_completed_frame();
        let transport = Arc::new(MockTransport::with_streams(vec![MockTransport::ok(
            200,
            vec![a, b, completed.as_bytes()],
        )]));
        let (driver, receiver) =
            start_driver(authenticated_manager(), transport.clone(), Some("gpt-5.5"));
        driver.prompt("hi".to_owned());
        let events = collect_until_finished(&receiver);
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                DriverEvent::TextDelta(delta) => Some(delta.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "日本語");
        assert!(matches!(
            events.last(),
            Some(DriverEvent::TurnFinished { success: true, .. })
        ));
    }

    #[test]
    fn stream_error_event_is_safe_and_settles() {
        let partial = sse_text_frame("partial");
        let transport = Arc::new(MockTransport::with_streams(vec![MockTransport::ok(
            200,
            vec![
                partial.as_bytes(),
                b"data: {\"type\":\"response.failed\",\"response\":{\"error\":\"SECRET-BODY-MARKER\"}}\n\n"
                    .as_slice(),
            ],
        )]));
        let (driver, receiver) =
            start_driver(authenticated_manager(), transport.clone(), Some("gpt-5.5"));
        driver.prompt("hi".to_owned());
        let events = collect_until_finished(&receiver);
        assert!(matches!(
            events.last(),
            Some(DriverEvent::TurnFinished { success: false, .. })
        ));
        let text = format!("{events:?}");
        assert!(!text.contains("SECRET-BODY-MARKER"));
        assert!(!text.contains(TEST_ACCESS_TOKEN));
    }

    #[test]
    fn unauthenticated_manager_reports_without_network() {
        let manager = Arc::new(ChatGptSessionManager::open(
            Arc::new(MemoryStore::default()),
            Arc::new(StubDeviceTransport),
            Arc::new(ManualClock::new(1_000_000)),
        ));
        let transport = Arc::new(MockTransport::with_streams(Vec::new()));
        let (driver, receiver) = start_driver(manager, transport.clone(), Some("gpt-5.5"));
        driver.prompt("hi".to_owned());
        let events = collect_until_finished(&receiver);
        assert!(events.iter().any(|event| matches!(
            event,
            DriverEvent::Error(message) if message.contains("not connected")
        )));
        assert!(transport.requests.lock().is_empty());
    }

    #[test]
    fn dead_refresh_settles_as_expired() {
        struct ExpiringDeviceTransport;
        impl DeviceAuthTransport for ExpiringDeviceTransport {
            fn request_device_code(
                &self,
                _config: &DeviceAuthConfig,
            ) -> Result<RawDeviceCode, ChatGptError> {
                Ok(RawDeviceCode {
                    device_auth_id: "d".to_owned(),
                    user_code: "U".to_owned(),
                    interval_secs: 1,
                })
            }
            fn poll_device_code(
                &self,
                _config: &DeviceAuthConfig,
                _device_auth_id: &str,
                _user_code: &str,
            ) -> Result<DevicePollOutcome, ChatGptError> {
                Ok(DevicePollOutcome::Authorized {
                    authorization_code: "c".to_owned(),
                    code_verifier: "v".to_owned(),
                })
            }
            fn exchange_code(
                &self,
                _config: &DeviceAuthConfig,
                _authorization_code: &str,
                _code_verifier: &str,
            ) -> Result<Value, ChatGptError> {
                Ok(serde_json::json!({
                    "access_token": test_jwt(),
                    "refresh_token": "dead-refresh",
                    "expires_in": 0,
                }))
            }
            fn refresh(
                &self,
                _config: &DeviceAuthConfig,
                _refresh_token: &str,
            ) -> Result<Value, ChatGptError> {
                Err(ChatGptError::new(
                    ChatGptErrorCode::RefreshTokenInvalid,
                    "gone",
                ))
            }
            fn fetch_models(
                &self,
                _config: &DeviceAuthConfig,
                _access_token: &str,
                _account_id: &str,
            ) -> Result<Value, ChatGptError> {
                Ok(Value::Array(Vec::new()))
            }
        }
        let manager = Arc::new(ChatGptSessionManager::open(
            Arc::new(MemoryStore::default()),
            Arc::new(ExpiringDeviceTransport),
            Arc::new(ManualClock::new(1_000_000)),
        ));
        manager.start_device_login().unwrap();
        manager.poll().unwrap();
        let transport = Arc::new(MockTransport::with_streams(Vec::new()));
        let (driver, receiver) = start_driver(manager, transport.clone(), Some("gpt-5.5"));
        driver.prompt("hi".to_owned());
        let events = collect_until_finished(&receiver);
        assert!(events.iter().any(|event| matches!(
            event,
            DriverEvent::Error(message) if message.contains("expired")
        )));
        assert!(transport.requests.lock().is_empty());
    }

    #[test]
    fn http_401_retries_once_then_expires() {
        let error_body: &[u8] = b"{}";
        let transport = Arc::new(MockTransport::with_streams(vec![
            Ok(MockStream {
                head: StreamHead {
                    status: 401,
                    retry_after_secs: None,
                },
                chunks: VecDeque::from([Ok(error_body.to_vec())]),
            }),
            Ok(MockStream {
                head: StreamHead {
                    status: 401,
                    retry_after_secs: None,
                },
                chunks: VecDeque::from([Ok(error_body.to_vec())]),
            }),
        ]));
        let (driver, receiver) =
            start_driver(authenticated_manager(), transport.clone(), Some("gpt-5.5"));
        driver.prompt("hi".to_owned());
        let events = collect_until_finished(&receiver);
        // Exactly one retry, then the expired reading — never a loop.
        assert_eq!(transport.requests.lock().len(), 2);
        assert!(events.iter().any(|event| matches!(
            event,
            DriverEvent::Error(message) if message.contains("expired")
        )));
    }

    #[test]
    fn http_429_reports_retry_after_without_retrying() {
        let transport = Arc::new(MockTransport::with_streams(vec![Ok(MockStream {
            head: StreamHead {
                status: 429,
                retry_after_secs: Some(42),
            },
            chunks: VecDeque::from([Ok(b"{}".to_vec())]),
        })]));
        let (driver, receiver) =
            start_driver(authenticated_manager(), transport.clone(), Some("gpt-5.5"));
        driver.prompt("hi".to_owned());
        let events = collect_until_finished(&receiver);
        assert_eq!(transport.requests.lock().len(), 1);
        assert!(events.iter().any(|event| matches!(
            event,
            DriverEvent::Error(message) if message.contains("42s")
        )));
    }

    #[test]
    fn fast_tier_rejection_retries_once_without_tier() {
        let mut options = start_options(Some("gpt-5.5"));
        options.service_tier = Some("fast".to_owned());
        let (events, receiver) = test_event_channel();
        let completed = sse_completed_frame();
        let transport = Arc::new(MockTransport::with_streams(vec![
            Ok(MockStream {
                head: StreamHead {
                    status: 400,
                    retry_after_secs: None,
                },
                chunks: VecDeque::from([Ok(
                    b"{\"error\":{\"message\":\"Unsupported service_tier: fast\"}}".to_vec(),
                )]),
            }),
            MockTransport::ok(200, vec![completed.as_bytes()]),
        ]));
        let driver = ChatGptDriver::start_with_manager(
            options,
            events,
            authenticated_manager(),
            transport.clone(),
        )
        .unwrap();
        driver.prompt("hi".to_owned());
        let events = collect_until_finished(&receiver);
        assert!(matches!(
            events.last(),
            Some(DriverEvent::TurnFinished { success: true, .. })
        ));
        let requests = transport.requests.lock();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].body["service_tier"], "fast");
        assert!(requests[1].body.get("service_tier").is_none());
    }

    #[test]
    fn unknown_model_status_maps_to_unavailable() {
        for status in [400_u16, 404] {
            let body: &[u8] = if status == 404 {
                b"{}"
            } else {
                b"{\"error\":{\"message\":\"The model `gpt-9` does not exist\"}}"
            };
            let transport = Arc::new(MockTransport::with_streams(vec![Ok(MockStream {
                head: StreamHead {
                    status,
                    retry_after_secs: None,
                },
                chunks: VecDeque::from([Ok(body.to_vec())]),
            })]));
            let (driver, receiver) =
                start_driver(authenticated_manager(), transport.clone(), Some("gpt-9"));
            driver.prompt("hi".to_owned());
            let events = collect_until_finished(&receiver);
            assert!(
                events.iter().any(|event| matches!(
                    event,
                    DriverEvent::Error(message) if message.contains("unavailable")
                )),
                "status {status}"
            );
        }
    }

    #[test]
    fn transport_failure_is_a_network_error() {
        let transport = Arc::new(MockTransport::with_streams(vec![Err(network_error(
            "down",
        ))]));
        let (driver, receiver) =
            start_driver(authenticated_manager(), transport.clone(), Some("gpt-5.5"));
        driver.prompt("hi".to_owned());
        let events = collect_until_finished(&receiver);
        assert!(events.iter().any(|event| matches!(
            event,
            DriverEvent::Error(message) if message.contains("Could not reach")
        )));
    }

    #[test]
    fn missing_model_sends_nothing() {
        let transport = Arc::new(MockTransport::with_streams(Vec::new()));
        let (driver, receiver) = start_driver(authenticated_manager(), transport.clone(), None);
        driver.prompt("hi".to_owned());
        let events = collect_until_finished(&receiver);
        assert!(transport.requests.lock().is_empty());
        assert!(events.iter().any(|event| matches!(
            event,
            DriverEvent::Error(message) if message.contains("No ChatGPT model")
        )));
    }

    #[test]
    fn resume_cursor_with_id_is_refused() {
        let (events, _receiver) = test_event_channel();
        let mut options = start_options(Some("gpt-5.5"));
        options.provider_cursor = Some(ProviderResumeCursor::ChatGpt {
            session_id: "server-thread".to_owned(),
        });
        let result = ChatGptDriver::start_with_manager(
            options,
            events,
            authenticated_manager(),
            Arc::new(MockTransport::with_streams(Vec::new())),
        );
        assert!(result.is_err());
    }

    #[test]
    fn cancel_settles_the_turn_as_stopped() {
        // A stream that never completes: cancel must settle it without an
        // error event leaking transport internals.
        struct HangingBody {
            cancelled: Arc<AtomicBool>,
        }
        impl StreamBody for HangingBody {
            fn read_chunk(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                for _ in 0..300 {
                    if self.cancelled.load(Ordering::SeqCst) {
                        return Err(std::io::Error::other("cancelled"));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Ok(0)
            }
            fn cancel(&mut self) {
                self.cancelled.store(true, Ordering::SeqCst);
            }
            fn finish(&mut self) {}
        }
        struct HangingTransport {
            started: Arc<AtomicBool>,
        }
        impl ResponsesStreamTransport for HangingTransport {
            fn start_stream(
                &self,
                _url: &str,
                _header_config: &str,
                _body: &str,
            ) -> Result<(StreamHead, Box<dyn StreamBody>), ChatGptError> {
                self.started.store(true, Ordering::SeqCst);
                Ok((
                    StreamHead {
                        status: 200,
                        retry_after_secs: None,
                    },
                    Box::new(HangingBody {
                        cancelled: Arc::new(AtomicBool::new(false)),
                    }),
                ))
            }
        }
        let (events, receiver) = test_event_channel();
        let hanging = Arc::new(HangingTransport {
            started: Arc::new(AtomicBool::new(false)),
        });
        let driver = ChatGptDriver::start_with_manager(
            start_options(Some("gpt-5.5")),
            events,
            authenticated_manager(),
            hanging.clone(),
        )
        .unwrap();
        driver.prompt("hi".to_owned());
        // Wait for the turn to start streaming, then cancel.
        assert!(matches!(
            receiver.recv_timeout(std::time::Duration::from_secs(10)),
            Ok(DriverEvent::TurnStarted)
        ));
        for _ in 0..100 {
            if hanging.started.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(hanging.started.load(Ordering::SeqCst));
        driver.cancel();
        let mut saw_finish = false;
        for _ in 0..100 {
            match receiver.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(DriverEvent::TurnFinished { success: false, .. }) => {
                    saw_finish = true;
                    break;
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        assert!(saw_finish, "cancel settled the turn as stopped");
    }

    fn persisted_transcript(turns: &[(&str, &str)]) -> Vec<crate::model::Message> {
        let mut messages = Vec::new();
        for (user, assistant) in turns {
            messages.push(crate::model::Message::new(
                crate::model::MessageRole::User,
                (*user).to_owned(),
            ));
            messages.push(crate::model::Message::new(
                crate::model::MessageRole::Assistant,
                (*assistant).to_owned(),
            ));
        }
        messages
    }

    fn live_worker_history(turns: &[(&str, &str)]) -> (Vec<Value>, Vec<usize>) {
        // A worker that stayed alive through these turns: each completed turn
        // commits its user message plus the streamed-text assistant rebuild
        // (empty endpoint output items, like the mock `response.completed`
        // frames produce).
        let (events, _receiver) = test_event_channel();
        let mut worker = Worker {
            manager: authenticated_manager(),
            transport: Arc::new(MockTransport::with_streams(Vec::new())),
            events,
            shared: Arc::new(Shared {
                body: Mutex::new(None),
                generation: AtomicU64::new(0),
                busy: AtomicBool::new(false),
            }),
            options: Arc::new(Mutex::new(LiveOptions::default())),
            history: Vec::new(),
            turn_starts: Vec::new(),
        };
        for (user, assistant) in turns {
            let user_message = serde_json::json!({
                "role": "user",
                "content": [{"type": "input_text", "text": *user}],
            });
            worker.commit_history(user_message, Vec::new(), (*assistant).to_owned());
        }
        (worker.history, worker.turn_starts)
    }

    #[test]
    fn empty_seed_starts_with_empty_history() {
        let (history, turn_starts) = Worker::seed_history_from_transcript(Vec::new());
        assert!(history.is_empty());
        assert!(turn_starts.is_empty());
    }

    #[test]
    fn seed_item_shapes_match_commit_history() {
        let seeds = vec![
            ChatGptHistorySeed {
                role: ChatGptHistoryRole::User,
                text: "u1".to_owned(),
            },
            ChatGptHistorySeed {
                role: ChatGptHistoryRole::Assistant,
                text: "a1".to_owned(),
            },
        ];
        let (history, turn_starts) = Worker::seed_history_from_transcript(seeds);
        assert_eq!(turn_starts, vec![0]);
        assert_eq!(
            history[0],
            serde_json::json!({
                "role": "user",
                "content": [{"type": "input_text", "text": "u1"}],
            })
        );
        assert_eq!(
            history[1],
            serde_json::json!({
                "role": "assistant",
                "content": [{"type": "output_text", "text": "a1"}],
            })
        );
    }

    #[test]
    fn restarted_worker_resends_the_live_workers_history() {
        // Worker A lives through three turns; Worker B is seeded from the
        // persisted transcript of those same turns. The next turn's request
        // input must be identical — this is the restart bug, proven fixed.
        let turns = [("u1", "a1"), ("u2", "a2"), ("u3", "a3")];
        let (live_history, live_starts) = live_worker_history(&turns);
        let seeds = ChatGptHistorySeed::from_messages(&persisted_transcript(&turns));
        let (seeded_history, seeded_starts) = Worker::seed_history_from_transcript(seeds);
        assert_eq!(seeded_history, live_history);
        assert_eq!(seeded_starts, live_starts);
        assert_eq!(seeded_starts, vec![0, 2, 4]);
    }

    #[test]
    fn seeded_worker_appends_the_new_prompt_exactly_once() {
        let seeds = ChatGptHistorySeed::from_messages(&persisted_transcript(&[("u1", "a1")]));
        let completed = sse_completed_frame();
        let transport = Arc::new(MockTransport::with_streams(vec![MockTransport::ok(
            200,
            vec![completed.as_bytes()],
        )]));
        let (events, receiver) = test_event_channel();
        let mut options = start_options(Some("gpt-5.5"));
        options.chatgpt_history = Some(seeds);
        let driver = ChatGptDriver::start_with_manager(
            options,
            events,
            authenticated_manager(),
            transport.clone(),
        )
        .unwrap();
        driver.prompt("u2".to_owned());
        let events = collect_until_finished(&receiver);
        assert!(matches!(
            events.last(),
            Some(DriverEvent::TurnFinished { success: true, .. })
        ));
        let requests = transport.requests.lock();
        assert_eq!(requests.len(), 1);
        let input = requests[0].body["input"].as_array().unwrap();
        assert_eq!(
            input.len(),
            3,
            "seed plus exactly one new prompt: {input:?}"
        );
        assert_eq!(input[0]["content"][0]["text"], "u1");
        assert_eq!(input[1]["content"][0]["text"], "a1");
        assert_eq!(input[2]["content"][0]["text"], "u2");
    }

    #[test]
    fn seeded_history_reuses_the_live_history_cap() {
        // 250 single-message turns seed 250 items; the existing cap keeps the
        // newest 200 with turn indices still pointing at user items.
        let turns = (0..250)
            .map(|index| (format!("u{index}"), format!("a{index}")))
            .collect::<Vec<_>>();
        let refs = turns
            .iter()
            .map(|(user, assistant)| (user.as_str(), assistant.as_str()))
            .collect::<Vec<_>>();
        let seeds = ChatGptHistorySeed::from_messages(&persisted_transcript(&refs));
        assert_eq!(seeds.len(), CHATGPT_HISTORY_SEED_LIMIT);
        assert_eq!(seeds.first().unwrap().role, ChatGptHistoryRole::User);
        let (mut history, mut turn_starts) = Worker::seed_history_from_transcript(seeds);
        // Seed shape mirrors commit_history exactly, so enforcing the live
        // cap on it is the same operation a long-lived worker performs.
        let (events, _receiver) = test_event_channel();
        let mut worker = Worker {
            manager: authenticated_manager(),
            transport: Arc::new(MockTransport::with_streams(Vec::new())),
            events,
            shared: Arc::new(Shared {
                body: Mutex::new(None),
                generation: AtomicU64::new(0),
                busy: AtomicBool::new(false),
            }),
            options: Arc::new(Mutex::new(LiveOptions::default())),
            history: std::mem::take(&mut history),
            turn_starts: std::mem::take(&mut turn_starts),
        };
        worker.enforce_history_cap();
        assert!(worker.history.len() <= MAX_HISTORY_ITEMS);
        assert!(!worker.turn_starts.is_empty());
        assert_eq!(worker.turn_starts[0], 0);
        for start in &worker.turn_starts {
            assert_eq!(worker.history[*start]["role"], "user");
        }
    }

    #[test]
    fn security_no_bearer_in_debug_wire_or_errors() {
        let greeting = sse_text_frame("hello");
        let completed = sse_completed_frame();
        let transport = Arc::new(MockTransport::with_streams(vec![MockTransport::ok(
            200,
            vec![greeting.as_bytes(), completed.as_bytes()],
        )]));
        let manager = authenticated_manager();
        let (driver, receiver) = start_driver(manager, transport.clone(), Some("gpt-5.5"));
        // The driver handle exposes no session material in Debug.
        assert_eq!(format!("{driver:?}"), "ChatGptDriver { .. }");
        driver.prompt("hi".to_owned());
        let events = collect_until_finished(&receiver);
        // `DriverEvent` reaches the wire through the daemon's `event_to_wire`;
        // its Debug shape carries the same strings, so absence here covers
        // the wire payload too.
        let wire = format!("{events:?}");
        for secret in [
            TEST_ACCESS_TOKEN,
            "test-refresh-token-BBB",
            "auth-code",
            "verifier",
        ] {
            assert!(!wire.contains(secret), "wire leaks {secret}");
        }
        for request in transport.requests.lock().iter() {
            let captured = format!("{request:?}");
            assert!(!captured.contains(TEST_ACCESS_TOKEN));
        }
        // FreshAuth redacts by construction.
        let fresh = authenticated_manager().ensure_fresh_auth().unwrap();
        assert_eq!(
            format!("{fresh:?}"),
            format!(
                "FreshAuth {{ account_id: {:?}, access_token: \"<redacted>\" }}",
                fresh.account_id()
            )
        );
    }
}
