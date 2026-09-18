//! Daemon-owned Claude subscription text-chat driver.
//!
//! One background worker thread owns the whole conversation: each `prompt`
//! becomes a single stateless `POST /v1/messages` (plus one bounded
//! re-authenticated retry), and the `text/event-stream` body is parsed
//! incrementally into the existing [`DriverEvent`] vocabulary the app pump
//! already renders. No chat UI or wire changes — events flow through the
//! standard daemon forwarder.
//!
//! Architecture rules:
//! - Bearer material lives only in [`FreshAuth`](crate::claude_session::FreshAuth)
//!   (memory) and the curl `-K -` stdin config. It never touches argv, disk,
//!   logs, events, or the wire. The JSON request body (model, messages)
//!   briefly touches a `0600` temp file because stdin is already claimed by
//!   the header config; the file holds no credentials and is removed when
//!   the turn ends.
//! - All blocking work (curl spawn, stdout reads, refresh) runs on the worker
//!   thread. Render and the UI pump only ever see [`DriverEvent`]s.
//! - Continuity is client-side: the worker resends its message history every
//!   turn. No server conversation id is invented.
//! - Subscription OAuth requests carry the byte-exact Claude Code identity
//!   as the first `system` block (see
//!   [`crate::claude_protocol::DEFAULT_SYSTEM_PREFIX`]); memory context rides
//!   as a later block. The driver sends no tools.
//! - Retries are bounded: at most one re-authenticated retry per 401. No
//!   other automatic retries; 429s are reported with their `Retry-After`
//!   wait, never retried.
//!
//! Mock transports back every test here; no test touches the network or real
//! credentials.

use std::io::{Read, Write};
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::Mutex;
use serde_json::Value;
use uuid::Uuid;

use super::{DriverControl, DriverEventSender, DriverStartOptions, SessionOptions};
use crate::claude_protocol::{
    ClaudeAuthConfig, ClaudeError, ClaudeErrorCode, ClaudeRequestHeaders, MessagesSseParser,
    MessagesStreamEvent, messages_request_body, parse_retry_after_secs,
};
use crate::claude_session::{CURL_PATH, ClaudeSessionManager, FreshAuth, default_session_manager};
use crate::memory;
use crate::model::CLAUDE_HISTORY_SEED_LIMIT;
use crate::model::{ClaudeHistoryRole, ClaudeHistorySeed, DriverEvent, ProviderResumeCursor};
use crate::persistence::StateStore;
use crate::usage_history::TokenTotals;

/// Total curl wall-clock budget per turn attempt. Streaming turns run for
/// minutes; unary auth calls keep their own 20s budget elsewhere.
const STREAM_MAX_TIME_SECS: &str = "600";
/// Upper bound on a non-2xx response body kept for error classification. Raw
/// bodies are never surfaced — only matched for known shapes — so this bounds
/// memory, not disclosure.
const ERROR_BODY_CAP_BYTES: usize = 64 * 1024;
/// Client-side message history cap (messages, not tokens). Oldest turns drain
/// first; indices stay consistent via [`Worker::enforce_history_cap`].
/// Single source of truth lives in the protocol crate so the restart seed
/// builder and the live worker can never drift apart.
const MAX_HISTORY_ITEMS: usize = CLAUDE_HISTORY_SEED_LIMIT;

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

/// Starts one authenticated `/v1/messages` stream. `header_config` carries the
/// bearer on a curl `-K` stdin config (never argv); `body` is the JSON
/// request. Implementations must never log either.
pub trait MessagesStreamTransport: Send + Sync {
    fn start_stream(
        &self,
        url: &str,
        header_config: &str,
        body: &str,
    ) -> Result<(StreamHead, Box<dyn StreamBody>), ClaudeError>;
}

/// Production transport over the system `curl`.
pub struct CurlStreamTransport;

impl MessagesStreamTransport for CurlStreamTransport {
    fn start_stream(
        &self,
        url: &str,
        header_config: &str,
        body: &str,
    ) -> Result<(StreamHead, Box<dyn StreamBody>), ClaudeError> {
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
            .map_err(|_| spawn_error("Could not start the Claude request"))?;
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
            return Err(spawn_error("Failed to send the Claude request"));
        }
        let mut stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(spawn_error("Claude request stdout is unavailable"));
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
/// the user's prompt (already stored in Mack's own session) but no
/// credentials; stdin is claimed by the `-K -` header config, so the body
/// cannot travel there too.
fn write_body_file(body: &str) -> Result<std::path::PathBuf, ClaudeError> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "mack-claude-msg-{}-{}.json",
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
            .map_err(|_| network_error("Could not stage the Claude request"))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&path, body.as_bytes())
            .map_err(|_| network_error("Could not stage the Claude request"))?;
    }
    Ok(path)
}

fn remove_body_file(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
}

/// Reads the `-D -` header block (through the blank separator line),
/// skipping `1xx` interim blocks. Returns status plus any `Retry-After` wait.
fn read_response_head(stdout: &mut impl Read) -> Result<StreamHead, ClaudeError> {
    let mut raw = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        match stdout.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                raw.push(byte[0]);
                if raw.len() > 32 * 1024 {
                    return Err(network_error("Claude response headers are too large"));
                }
                if raw.ends_with(b"\r\n\r\n") || raw.ends_with(b"\n\n") {
                    break;
                }
            }
            Err(_) => return Err(network_error("Could not read the Claude response")),
        }
    }
    let text = String::from_utf8_lossy(&raw);
    let mut lines = text.lines();
    let status_line = lines
        .next()
        .ok_or_else(|| network_error("Claude response carried no status"))?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| network_error("Claude response carried no status"))?;
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

fn network_error(message: &str) -> ClaudeError {
    ClaudeError::new(ClaudeErrorCode::NetworkError, message)
}

// ---------------------------------------------------------------------------
// Request construction
// ---------------------------------------------------------------------------

/// Builds one `/v1/messages` request body: selected model, full client-side
/// message history, `stream: true`, then the OAuth identity prefix plus an
/// optional memory `extra_system` block. Returns the model and the JSON text.
fn build_messages_body(
    config: &ClaudeAuthConfig,
    model: &str,
    messages: Vec<Value>,
    extra_system: Option<String>,
) -> Result<(String, String), ClaudeError> {
    messages_request_body(config, model, messages, extra_system, config.max_tokens)
}

/// Renders the `-K -` stdin config: bearer headers plus the SSE accept and
/// JSON content type. The caller holds the only copy; tests assert even
/// redacted captures retain no bearer bytes.
fn render_header_config(access_token: &str, config: &ClaudeAuthConfig) -> String {
    let headers = ClaudeRequestHeaders::new(access_token, config);
    let mut rendered = headers.curl_header_config();
    for line in [
        "Accept: text/event-stream",
        "Content-Type: application/json",
    ] {
        let escaped = line.replace('\\', "\\\\").replace('"', "\\\"");
        rendered.push_str(&format!("\nheader = \"{escaped}\""));
    }
    rendered
}

// ---------------------------------------------------------------------------
// Safe error messages (static text only; upstream bodies never surface)
// ---------------------------------------------------------------------------

const NO_MODEL_MESSAGE: &str =
    "No Claude model is selected. Discover models in Settings → Providers → Claude, then pick one.";
const SESSION_EXPIRED_MESSAGE: &str = "Claude session expired. Sign in again.";
const NOT_CONNECTED_MESSAGE: &str =
    "Claude is not connected. Connect it in Settings → Providers → Claude.";
const NETWORK_MESSAGE: &str = "Could not reach the Claude service.";
const MODEL_UNAVAILABLE_MESSAGE: &str = "The selected Claude model is unavailable. Discover models again in Settings → Providers → Claude.";
const RESPONSE_FAILED_MESSAGE: &str = "Claude could not complete the response.";
const TRUNCATED_MESSAGE: &str = "The Claude response ended before it completed.";
const INVALID_REQUEST_MESSAGE: &str = "Claude rejected the request.";
const SERVICE_UNAVAILABLE_MESSAGE: &str = "The Claude service is unavailable. Try again shortly.";

fn auth_failure_message(error: &ClaudeError) -> String {
    match error.code {
        ClaudeErrorCode::NotAuthenticated => NOT_CONNECTED_MESSAGE.to_owned(),
        ClaudeErrorCode::RefreshTokenInvalid => SESSION_EXPIRED_MESSAGE.to_owned(),
        ClaudeErrorCode::NetworkError => NETWORK_MESSAGE.to_owned(),
        _ => "Could not refresh the Claude session.".to_owned(),
    }
}

fn rate_limited_message(retry_after_secs: Option<u64>) -> String {
    match retry_after_secs {
        Some(secs) => format!("Claude is rate limited. Try again in {secs}s."),
        None => "Claude is rate limited. Try again shortly.".to_owned(),
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
    memory_enabled: bool,
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

pub struct ClaudeDriver {
    commands: Sender<CommandMessage>,
    shared: Arc<Shared>,
    options: Arc<Mutex<LiveOptions>>,
}

impl ClaudeDriver {
    pub fn start(options: DriverStartOptions, events: DriverEventSender) -> anyhow::Result<Self> {
        let manager = Arc::new(default_session_manager());
        Self::start_with_manager(options, events, manager, Arc::new(CurlStreamTransport))
    }

    fn start_with_manager(
        options: DriverStartOptions,
        events: DriverEventSender,
        manager: Arc<ClaudeSessionManager>,
        transport: Arc<dyn MessagesStreamTransport>,
    ) -> anyhow::Result<Self> {
        // Continuity is history-based (see module docs): a persisted server
        // conversation id would be invented, so any non-empty cursor is refused.
        if let Some(cursor) = options.provider_cursor
            && !cursor.native_id().is_empty()
        {
            anyhow::bail!(
                "cannot resume Claude from a {} cursor",
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
            memory_enabled: options.memory_enabled,
        }));
        let (commands, incoming) = unbounded();
        // Restart resume: rebuild the resend-history a live worker would
        // have carried from the persisted Mack transcript seed. Post-seed
        // behavior is identical to a worker that had remained alive — the
        // next prompt still appends exactly once via `execute_turn`.
        let (history, turn_starts) =
            Worker::seed_history_from_transcript(options.claude_history.unwrap_or_default());
        let mut worker = Worker {
            manager,
            transport,
            events,
            shared: shared.clone(),
            options: live.clone(),
            history,
            turn_starts,
            last_turn_user: String::new(),
            last_turn_assistant: String::new(),
            last_turn_account: String::new(),
            memory_db_path: None,
        };
        worker.enforce_history_cap();
        std::thread::Builder::new()
            .name("mack-claude-driver".into())
            .spawn(move || worker.run(incoming))
            .map_err(|error| anyhow::anyhow!("could not start the Claude driver: {error}"))?;
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

impl Drop for ClaudeDriver {
    fn drop(&mut self) {
        // Best-effort: the worker exits on its own; the detached thread never
        // blocks teardown, and the live body is reaped on drop.
        self.send(CommandMessage::Shutdown);
    }
}

impl DriverControl for ClaudeDriver {
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
        // Model and the memory toggle are per-turn request fields: absorb in
        // place, no restart needed. Effort/tier/window have no Claude
        // mapping yet and are accepted without effect.
        *self.options.lock() = LiveOptions {
            model: options.model.filter(|model| !model.trim().is_empty()),
            memory_enabled: options.memory_enabled,
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
            .map_err(|_| anyhow::anyhow!("Claude rollback timed out"))?
            .map_err(anyhow::Error::msg)
    }

    // `fork` keeps the default "not supported" bail: no conversations exist
    // server-side to branch.
}

impl std::fmt::Debug for ClaudeDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeDriver").finish_non_exhaustive()
    }
}

struct Worker {
    manager: Arc<ClaudeSessionManager>,
    transport: Arc<dyn MessagesStreamTransport>,
    events: DriverEventSender,
    shared: Arc<Shared>,
    options: Arc<Mutex<LiveOptions>>,
    history: Vec<Value>,
    turn_starts: Vec<usize>,
    /// Last successful turn, for background memory extraction. Written in
    /// `execute_turn`'s `Done` arm, read once in `run_turn` right after —
    /// never part of history, caps, or seeds.
    last_turn_user: String,
    last_turn_assistant: String,
    last_turn_account: String,
    /// Override for the memory database path. `None` (production) resolves
    /// to `StateStore::default_path()`; tests point it at a temp dir so
    /// extraction never touches the developer's real debug database.
    memory_db_path: Option<std::path::PathBuf>,
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
        let messages = vec![
            serde_json::json!({"role":"user","content":[{"type":"text","text":title_prompt}]}),
        ];
        let config = self.manager.config().clone();
        let Ok((_, body)) = messages_request_body(
            &config,
            &model,
            messages,
            None,
            crate::claude_protocol::TITLE_MAX_TOKENS,
        ) else {
            return;
        };
        let headers = render_header_config(auth.access_token(), &config);
        let Ok((head, mut stream)) =
            self.transport
                .start_stream(&config.messages_url, &headers, &body)
        else {
            return;
        };
        if !(200..300).contains(&head.status) {
            stream.finish();
            return;
        }
        let mut parser = MessagesSseParser::new();
        let mut decoder = Utf8StreamDecoder::new();
        let mut title = String::new();
        let mut buf = [0_u8; 4096];
        loop {
            match stream.read_chunk(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    for event in parser.push(&decoder.push(&buf[..n])) {
                        match event {
                            MessagesStreamEvent::TextDelta(delta) => title.push_str(&delta),
                            MessagesStreamEvent::Completed { .. } => break,
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

    /// Resolves the memory database handle: the daemon-plumbed path when
    /// present, otherwise the default location (same database either way).
    fn memory_store(&self) -> StateStore {
        let db_path = self
            .memory_db_path
            .clone()
            .unwrap_or_else(StateStore::default_path);
        StateStore::daemon(db_path)
    }

    /// Account id scoping this turn's memory rows, read from stored state
    /// without network. `None` when signed out or the profile never
    /// resolved — memory then degrades to disabled for the turn.
    fn memory_account_id(&self) -> Option<String> {
        let account_id = self.manager.public_session().ok()?.user?.account_id;
        (!account_id.is_empty()).then_some(account_id)
    }

    /// Retrieves the current turn's memory context, if any. Runs on the
    /// worker thread (never the UI thread) over a short-lived read-only
    /// connection. A disabled toggle or unknown account skips the lookup
    /// entirely; every other failure also degrades to `None`, so the normal
    /// request proceeds exactly as without memories.
    fn retrieve_memory_instructions(
        &self,
        prompt: &str,
        account_id: Option<&str>,
    ) -> Option<String> {
        if !self.options.lock().memory_enabled {
            return None;
        }
        let account_id = account_id?;
        let terms = memory::memory_search_terms(prompt);
        if terms.is_empty() {
            return None;
        }
        let memories = self.memory_store().search_memories_job(
            terms,
            account_id.to_owned(),
            memory::MEMORY_RETRIEVAL_LIMIT,
        )()
        .unwrap_or_default();
        memory::compose_memory_instructions(None, &memories)
    }

    /// Fires one background memory-extraction pass for the last successful
    /// turn. Gated synchronously (no thread for trivial turns or unknown
    /// accounts); the thread itself is fire-and-forget and emits no driver
    /// events, so the next prompt never waits on it.
    fn spawn_memory_extraction(&self) {
        if !self.options.lock().memory_enabled {
            return;
        }
        if self.last_turn_account.is_empty()
            || !memory::should_extract(&self.last_turn_user, &self.last_turn_assistant)
        {
            return;
        }
        let transport = Arc::clone(&self.transport);
        let manager = Arc::clone(&self.manager);
        let model = self.options.lock().model.clone().unwrap_or_default();
        let store = self.memory_store();
        let account_id = self.last_turn_account.clone();
        let user_text = self.last_turn_user.clone();
        let assistant_text = self.last_turn_assistant.clone();
        let _ = std::thread::Builder::new()
            .name("mack-memory-extraction".into())
            .spawn(move || {
                run_memory_extraction(
                    transport.as_ref(),
                    &manager,
                    &model,
                    &store,
                    &account_id,
                    None,
                    &user_text,
                    &assistant_text,
                );
            });
    }

    fn run_turn(&mut self, prompt: String) {
        self.shared.busy.store(true, Ordering::SeqCst);
        self.emit(DriverEvent::TurnStarted);
        // Interrupts that land before the stream starts (a fast cancel, a
        // superseding prompt) must still settle the turn: attempts re-check
        // this generation after parking the live body.
        let turn_generation = self.shared.generation.load(Ordering::SeqCst);
        if self.execute_turn(&prompt, turn_generation) == TurnEnd::Completed {
            // Memory extraction runs on its own thread and never delays the
            // turn's completion events below.
            self.spawn_memory_extraction();
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

        let mut messages = self.history.clone();
        let user_message =
            serde_json::json!({"role":"user","content":[{"type":"text","text":prompt}]});
        messages.push(user_message.clone());

        // Cross-chat memories ride in a later `system` block, never in
        // `messages`: the history below stays exactly what the conversation
        // produced, and the identity block stays first.
        let account_id = self.memory_account_id();
        let extra_system = self.retrieve_memory_instructions(prompt, account_id.as_deref());

        let mut auth = auth;
        let mut retried_auth = false;
        loop {
            let config = self.manager.config().clone();
            let (_, body) = match build_messages_body(
                &config,
                &model,
                messages.clone(),
                extra_system.clone(),
            ) {
                Ok(built) => built,
                Err(_) => {
                    self.fail_turn(INVALID_REQUEST_MESSAGE);
                    return TurnEnd::Settled;
                }
            };
            match self.post_once(&auth, &body, turn_generation) {
                PostOutcome::Done { text, usage } => {
                    self.commit_history(user_message.clone(), text.clone());
                    self.last_turn_user = prompt.to_owned();
                    self.last_turn_assistant = text;
                    self.last_turn_account = account_id.clone().unwrap_or_default();
                    // Cumulative turn usage, before the turn settles so the
                    // app records it even if the finish races a shutdown.
                    self.emit(DriverEvent::TurnUsage { totals: usage });
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
                // An exhausted retry means the second attempt failed the same
                // way: revoked credentials, or rotation elsewhere.
                PostOutcome::RetryAuth => {
                    self.fail_turn(SESSION_EXPIRED_MESSAGE);
                    return TurnEnd::Settled;
                }
            }
        }
    }

    /// One POST attempt: stream SSE to events, or classify the failure.
    /// `turn_generation` settles attempts superseded before they started.
    fn post_once(&mut self, auth: &FreshAuth, body: &str, turn_generation: u64) -> PostOutcome {
        let config = self.manager.config().clone();
        let url = config.messages_url.clone();
        let header_config = render_header_config(auth.access_token(), &config);
        let generation = self.shared.generation.load(Ordering::SeqCst);
        let (head, stream) = match self.transport.start_stream(&url, &header_config, body) {
            Ok(started) => started,
            Err(error) => {
                return PostOutcome::Fatal(match error.code {
                    ClaudeErrorCode::NetworkError => NETWORK_MESSAGE.to_owned(),
                    _ => SERVICE_UNAVAILABLE_MESSAGE.to_owned(),
                });
            }
        };
        if !(200..300).contains(&head.status) {
            let mut stream = stream;
            let error_body = read_error_body(&mut *stream);
            stream.finish();
            return classify_http_error(head.status, head.retry_after_secs, &error_body);
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
        let mut parser = MessagesSseParser::new();
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
                            MessagesStreamEvent::TextDelta(delta) => {
                                text.push_str(&delta);
                                self.emit(DriverEvent::TextDelta(delta));
                            }
                            MessagesStreamEvent::ReasoningDelta(delta) => {
                                self.emit(DriverEvent::ReasoningDelta(delta));
                            }
                            MessagesStreamEvent::Completed { stop_reason, usage } => {
                                match stop_reason.as_deref() {
                                    // Ran out of output budget: report the
                                    // partial text as truncated, not done.
                                    Some("max_tokens") => {
                                        return PostOutcome::Fatal(TRUNCATED_MESSAGE.to_owned());
                                    }
                                    _ => {
                                        return PostOutcome::Done {
                                            text: std::mem::take(&mut text),
                                            usage,
                                        };
                                    }
                                }
                            }
                            MessagesStreamEvent::Failed => {
                                return PostOutcome::Fatal(RESPONSE_FAILED_MESSAGE.to_owned());
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
    /// worker that had never restarted.
    fn seed_history_from_transcript(seeds: Vec<ClaudeHistorySeed>) -> (Vec<Value>, Vec<usize>) {
        let mut history = Vec::with_capacity(seeds.len());
        let mut turn_starts = Vec::new();
        for seed in seeds {
            match seed.role {
                ClaudeHistoryRole::User => {
                    turn_starts.push(history.len());
                    history.push(serde_json::json!({
                        "role": "user",
                        "content": [{"type": "text", "text": seed.text}],
                    }));
                }
                ClaudeHistoryRole::Assistant => {
                    history.push(serde_json::json!({
                        "role": "assistant",
                        "content": [{"type": "text", "text": seed.text}],
                    }));
                }
            }
        }
        (history, turn_starts)
    }

    /// Appends a completed turn (user message plus the streamed assistant
    /// text) to the client-side history. Empty assistant text still records
    /// the user turn; the next request simply carries no assistant reply.
    fn commit_history(&mut self, user_message: Value, text: String) {
        self.turn_starts.push(self.history.len());
        self.history.push(user_message);
        if !text.is_empty() {
            self.history.push(
                serde_json::json!({"role":"assistant","content":[{"type":"text","text":text}]}),
            );
        }
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

/// One memory-extraction pass: a single-turn request carrying only the just
/// completed exchange, parsed and persisted silently. Mirrors
/// `generate_title`'s one-off request shape (same manager, transport, and
/// streaming-collect loop) but never emits events and never touches history,
/// caps, seeds, or the transcript — extraction is invisible by design.
///
/// `source_session_id` is `None`: the worker never learns the Mack session
/// id (`DriverStartOptions` carries none, and widening the wire protocol is
/// out of scope), so provenance waits for a later phase.
#[allow(clippy::too_many_arguments)]
fn run_memory_extraction(
    transport: &dyn MessagesStreamTransport,
    manager: &Arc<ClaudeSessionManager>,
    model: &str,
    store: &StateStore,
    account_id: &str,
    source_session_id: Option<Uuid>,
    user_text: &str,
    assistant_text: &str,
) {
    if model.trim().is_empty() || !memory::should_extract(user_text, assistant_text) {
        return;
    }
    let auth = match manager.ensure_fresh_auth() {
        Ok(auth) => auth,
        Err(_) => return,
    };
    let messages = vec![
        serde_json::json!({"role":"user","content":[{"type":"text","text":memory::extraction_prompt(user_text, assistant_text)}]}),
    ];
    let config = manager.config().clone();
    let Ok((_, body)) = messages_request_body(&config, model, messages, None, config.max_tokens)
    else {
        return;
    };
    let headers = render_header_config(auth.access_token(), &config);
    let Ok((head, mut stream)) = transport.start_stream(&config.messages_url, &headers, &body)
    else {
        return;
    };
    if !(200..300).contains(&head.status) {
        stream.finish();
        return;
    }
    let mut parser = MessagesSseParser::new();
    let mut decoder = Utf8StreamDecoder::new();
    let mut text = String::new();
    let mut buf = [0_u8; 4096];
    loop {
        match stream.read_chunk(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                for event in parser.push(&decoder.push(&buf[..n])) {
                    match event {
                        MessagesStreamEvent::TextDelta(delta) => text.push_str(&delta),
                        MessagesStreamEvent::Completed { .. } => break,
                        _ => {}
                    }
                }
            }
            Err(_) => break,
        }
    }
    stream.finish();
    persist_extracted_memories(store, account_id, source_session_id, &text);
}

/// Parses one extraction answer and folds it into the store: secrets never
/// persist (second layer behind the prompt), normalized duplicates are
/// skipped, same-fact rewordings update the existing row, and everything
/// else inserts. Every failure path returns quietly — the user never sees
/// extraction work or fail, and memory contents are never logged.
fn persist_extracted_memories(
    store: &StateStore,
    account_id: &str,
    source_session_id: Option<Uuid>,
    text: &str,
) {
    let candidates = memory::parse_extraction_output(text);
    if candidates.is_empty() {
        return;
    }
    for candidate in candidates {
        let candidate = candidate.trim();
        if candidate.is_empty() || memory::is_secret_like(candidate) {
            continue;
        }
        let normalized = memory::normalize_memory(candidate);
        let existing = store.list_memories(account_id).unwrap_or_default();
        match memory::match_existing(&normalized, &existing) {
            memory::MemoryMatch::Duplicate => {}
            memory::MemoryMatch::Update(id) => {
                let _ = store.update_memory_content(id, candidate);
            }
            memory::MemoryMatch::New => {
                let _ = store.insert_memory(candidate, account_id, source_session_id);
            }
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum TurnEnd {
    Completed,
    Settled,
}

enum PostOutcome {
    Done { text: String, usage: TokenTotals },
    Interrupted,
    Fatal(String),
    RetryAuth,
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

fn classify_http_error(status: u16, retry_after_secs: Option<u64>, body: &str) -> PostOutcome {
    if status == 401 {
        // Fresh credentials were just minted: a 401 means revoked or rotated
        // elsewhere. One re-read heals a cross-instance rotation; a second
        // 401 settles as expired.
        return PostOutcome::RetryAuth;
    }
    if status == 429 {
        return PostOutcome::Fatal(rate_limited_message(retry_after_secs));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_protocol::ClaudeAuthConfig;
    use crate::claude_session::{ClaudeSessionManager, ManualClock, MemoryStore, OAuthTransport};
    use crate::driver::test_event_channel;
    use std::collections::VecDeque;

    /// Distinctly fake bearer material: tests assert its *absence* from every
    /// retained capture, serialized event, and error string.
    const TEST_ACCESS_TOKEN: &str = "test-access-token-AAA";
    const TEST_ACCOUNT_ID: &str = "acct-test-driver";

    fn token_response() -> Value {
        serde_json::json!({
            "access_token": TEST_ACCESS_TOKEN,
            "refresh_token": "test-refresh-token-BBB",
            "expires_in": 3600,
            "scope": "org:create_api_key user:profile user:inference",
        })
    }

    fn profile_response() -> Value {
        serde_json::json!({
            "user": {"email": "dev@example.com"},
            "organization": {
                "uuid": TEST_ACCOUNT_ID,
                "organization_type": "claude_max",
                "rate_limit_tier": "default_claude_max_20x",
            },
        })
    }

    /// OAuth transport that signs in once, then fails refreshes as dead.
    struct StubOAuthTransport;

    impl OAuthTransport for StubOAuthTransport {
        fn exchange_code(
            &self,
            _config: &ClaudeAuthConfig,
            _code: &str,
            _state: Option<&str>,
            _verifier: &str,
        ) -> Result<Value, ClaudeError> {
            Ok(token_response())
        }

        fn refresh(
            &self,
            _config: &ClaudeAuthConfig,
            _refresh_token: &str,
        ) -> Result<Value, ClaudeError> {
            Err(ClaudeError::new(
                ClaudeErrorCode::RefreshTokenInvalid,
                "refresh token is no longer valid",
            ))
        }

        fn fetch_profile(
            &self,
            _config: &ClaudeAuthConfig,
            _access_token: &str,
        ) -> Result<Value, ClaudeError> {
            Ok(profile_response())
        }

        fn fetch_models(
            &self,
            _config: &ClaudeAuthConfig,
            _access_token: &str,
        ) -> Result<Value, ClaudeError> {
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
        had_beta: bool,
        had_version: bool,
        had_sse_accept: bool,
        body: Value,
    }

    struct MockTransport {
        streams: Mutex<VecDeque<Result<MockStream, ClaudeError>>>,
        requests: Mutex<Vec<CapturedRequest>>,
    }

    impl MockTransport {
        fn with_streams(streams: Vec<Result<MockStream, ClaudeError>>) -> Self {
            Self {
                streams: Mutex::new(streams.into()),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn ok(status: u16, chunks: Vec<&[u8]>) -> Result<MockStream, ClaudeError> {
            Ok(MockStream {
                head: StreamHead {
                    status,
                    retry_after_secs: None,
                },
                chunks: chunks.into_iter().map(|chunk| Ok(chunk.to_vec())).collect(),
            })
        }

        fn retry_after(status: u16, secs: u64) -> Result<MockStream, ClaudeError> {
            Ok(MockStream {
                head: StreamHead {
                    status,
                    retry_after_secs: Some(secs),
                },
                chunks: VecDeque::new(),
            })
        }
    }

    impl MessagesStreamTransport for MockTransport {
        fn start_stream(
            &self,
            url: &str,
            header_config: &str,
            body: &str,
        ) -> Result<(StreamHead, Box<dyn StreamBody>), ClaudeError> {
            // The header config transiently carries the bearer by design
            // (that is how the daemon authenticates); what must never happen
            // is retaining it. Capture presence flags only, then prove the
            // retained log holds no bearer bytes.
            self.requests.lock().push(CapturedRequest {
                url: url.to_owned(),
                had_bearer: header_config.contains("Authorization: Bearer "),
                had_beta: header_config.contains("anthropic-beta: oauth-2025-04-20"),
                had_version: header_config.contains("anthropic-version: "),
                had_sse_accept: header_config.contains("text/event-stream"),
                body: serde_json::from_str(body).unwrap_or(Value::Null),
            });
            let retained = format!("{:?}", self.requests.lock());
            assert!(
                !retained.contains(TEST_ACCESS_TOKEN),
                "test transport retained the bearer"
            );
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

    fn authenticated_manager() -> Arc<ClaudeSessionManager> {
        let manager = Arc::new(ClaudeSessionManager::open(
            Arc::new(MemoryStore::default()),
            Arc::new(StubOAuthTransport),
            Arc::new(ManualClock::new(1_000_000)),
        ));
        manager.start_login().unwrap();
        let public = manager.complete_login("code-abc").unwrap();
        assert!(matches!(
            public.status,
            crate::claude_session::LoginStatus::Authenticated
        ));
        assert_eq!(
            public.user.as_ref().map(|user| user.account_id.as_str()),
            Some(TEST_ACCOUNT_ID)
        );
        manager
    }

    fn start_options(model: Option<&str>) -> DriverStartOptions {
        DriverStartOptions {
            binary: std::path::PathBuf::from("claude"),
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
            claude_history: None,
            memory_db_path: None,
            // Memory extraction spawns threads per turn; keep it off except
            // in the dedicated extraction test below.
            memory_enabled: false,
        }
    }

    fn start_driver(
        manager: Arc<ClaudeSessionManager>,
        transport: Arc<MockTransport>,
        model: Option<&str>,
    ) -> (ClaudeDriver, Receiver<DriverEvent>) {
        let (events, receiver) = test_event_channel();
        let driver =
            ClaudeDriver::start_with_manager(start_options(model), events, manager, transport)
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
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"text_delta\",\"text\":{}}}}}\n\n",
            serde_json::to_string(delta).unwrap()
        )
    }

    fn sse_thinking_frame(thinking: &str) -> String {
        format!(
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"thinking_delta\",\"thinking\":{}}}}}\n\n",
            serde_json::to_string(thinking).unwrap()
        )
    }

    fn sse_completed_frame(stop_reason: &str) -> String {
        format!(
            "event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{stop_reason}\"}},\"usage\":{{\"output_tokens\":5}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
        )
    }

    fn sse_error_frame() -> String {
        "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}\n\n"
            .to_owned()
    }

    fn assert_success(events: &[DriverEvent]) {
        assert!(matches!(events.first(), Some(DriverEvent::TurnStarted)));
        assert!(matches!(
            events.last(),
            Some(DriverEvent::TurnFinished { success: true, .. })
        ));
    }

    fn assert_failure(events: &[DriverEvent], needle: &str) {
        let errors: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                DriverEvent::Error(message) => Some(message.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            errors.iter().any(|message| message.contains(needle)),
            "expected error containing {needle:?}, got {errors:?}"
        );
        assert!(matches!(
            events.last(),
            Some(DriverEvent::TurnFinished { success: false, .. })
        ));
    }

    #[test]
    fn request_carries_model_headers_and_identity_body() {
        let completed = sse_completed_frame("end_turn");
        let transport = Arc::new(MockTransport::with_streams(vec![MockTransport::ok(
            200,
            vec![completed.as_bytes()],
        )]));
        let (driver, receiver) = start_driver(
            authenticated_manager(),
            transport.clone(),
            Some("claude-sonnet-4-5"),
        );
        driver.prompt("Hello".to_owned());
        let events = collect_until_finished(&receiver);
        assert_success(&events);

        let requests = transport.requests.lock();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.url, "https://api.anthropic.com/v1/messages");
        assert!(request.had_bearer);
        assert!(request.had_beta);
        assert!(request.had_version);
        assert!(request.had_sse_accept);
        assert_eq!(request.body["model"], "claude-sonnet-4-5");
        assert_eq!(request.body["stream"], true);
        assert_eq!(request.body["max_tokens"], 8192);
        let system = request.body["system"].as_array().unwrap();
        assert!(!system.is_empty());
        assert_eq!(
            system[0]["text"],
            "You are Claude Code, Anthropic's official CLI for Claude."
        );
        let messages = request.body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert!(request.body.get("tools").is_none());
    }

    #[test]
    fn text_streams_progressively_and_history_carries_forward() {
        let first = format!("{}{}", sse_text_frame("Hel"), sse_text_frame("lo"));
        let completed = sse_completed_frame("end_turn");
        let second_text = sse_text_frame("Again");
        let transport = Arc::new(MockTransport::with_streams(vec![
            MockTransport::ok(200, vec![first.as_bytes(), completed.as_bytes()]),
            MockTransport::ok(200, vec![second_text.as_bytes(), completed.as_bytes()]),
        ]));
        let (driver, receiver) =
            start_driver(authenticated_manager(), transport.clone(), Some("model"));
        driver.prompt("First".to_owned());
        let events = collect_until_finished(&receiver);
        assert_success(&events);
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                DriverEvent::TextDelta(delta) => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello");

        driver.prompt("Second".to_owned());
        let events = collect_until_finished(&receiver);
        assert_success(&events);

        let requests = transport.requests.lock();
        assert_eq!(requests.len(), 2);
        let messages = requests[1].body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["text"], "Hello");
        assert_eq!(messages[2]["role"], "user");
    }

    #[test]
    fn thinking_delta_maps_to_reasoning_events() {
        let stream = format!(
            "{}{}{}",
            sse_thinking_frame("hmm"),
            sse_text_frame("done"),
            sse_completed_frame("end_turn")
        );
        let transport = Arc::new(MockTransport::with_streams(vec![MockTransport::ok(
            200,
            vec![stream.as_bytes()],
        )]));
        let (driver, receiver) =
            start_driver(authenticated_manager(), transport.clone(), Some("model"));
        driver.prompt("Think".to_owned());
        let events = collect_until_finished(&receiver);
        assert_success(&events);
        assert!(events.iter().any(|event| matches!(
            event,
            DriverEvent::ReasoningDelta(delta) if delta == "hmm"
        )));
    }

    #[test]
    fn malformed_frames_are_ignored() {
        let stream = format!(
            "event: ping\ndata: {{\"type\":\"ping\"}}\n\n: keepalive\n\nevent: content_block_delta\ndata: nope\n\n{}{}",
            sse_text_frame("ok"),
            sse_completed_frame("end_turn")
        );
        let transport = Arc::new(MockTransport::with_streams(vec![MockTransport::ok(
            200,
            vec![stream.as_bytes()],
        )]));
        let (driver, receiver) =
            start_driver(authenticated_manager(), transport.clone(), Some("model"));
        driver.prompt("Hi".to_owned());
        let events = collect_until_finished(&receiver);
        assert_success(&events);
        assert!(events.iter().any(|event| matches!(
            event,
            DriverEvent::TextDelta(delta) if delta == "ok"
        )));
    }

    #[test]
    fn unauthorized_refreshes_once_then_succeeds() {
        let completed = sse_completed_frame("end_turn");
        let transport = Arc::new(MockTransport::with_streams(vec![
            MockTransport::ok(401, vec![]),
            MockTransport::ok(200, vec![completed.as_bytes()]),
        ]));
        let (driver, receiver) =
            start_driver(authenticated_manager(), transport.clone(), Some("model"));
        driver.prompt("Hello".to_owned());
        let events = collect_until_finished(&receiver);
        assert_success(&events);
        assert_eq!(transport.requests.lock().len(), 2);
    }

    #[test]
    fn double_unauthorized_expires_the_turn() {
        let transport = Arc::new(MockTransport::with_streams(vec![
            MockTransport::ok(401, vec![]),
            MockTransport::ok(401, vec![]),
        ]));
        let (driver, receiver) =
            start_driver(authenticated_manager(), transport.clone(), Some("model"));
        driver.prompt("Hello".to_owned());
        let events = collect_until_finished(&receiver);
        assert_failure(&events, "expired");
    }

    #[test]
    fn rate_limited_reports_the_wait() {
        let transport = Arc::new(MockTransport::with_streams(vec![
            MockTransport::retry_after(429, 7),
        ]));
        let (driver, receiver) =
            start_driver(authenticated_manager(), transport.clone(), Some("model"));
        driver.prompt("Hello".to_owned());
        let events = collect_until_finished(&receiver);
        assert_failure(&events, "Try again in 7s");
    }

    #[test]
    fn unknown_model_reports_unavailable() {
        let body = br#"{"type":"error","error":{"type":"not_found_error","message":"model: no-such-model"}}"#;
        let transport = Arc::new(MockTransport::with_streams(vec![MockTransport::ok(
            404,
            vec![body.as_slice()],
        )]));
        let (driver, receiver) = start_driver(
            authenticated_manager(),
            transport.clone(),
            Some("no-such-model"),
        );
        driver.prompt("Hello".to_owned());
        let events = collect_until_finished(&receiver);
        assert_failure(&events, "unavailable");
    }

    #[test]
    fn error_frame_fails_the_turn() {
        let stream = format!("{}{}", sse_text_frame("partial"), sse_error_frame());
        let transport = Arc::new(MockTransport::with_streams(vec![MockTransport::ok(
            200,
            vec![stream.as_bytes()],
        )]));
        let (driver, receiver) =
            start_driver(authenticated_manager(), transport.clone(), Some("model"));
        driver.prompt("Hello".to_owned());
        let events = collect_until_finished(&receiver);
        assert_failure(&events, "could not complete");
    }

    #[test]
    fn max_tokens_stop_reports_truncation() {
        let stream = format!(
            "{}{}",
            sse_text_frame("partial"),
            sse_completed_frame("max_tokens")
        );
        let transport = Arc::new(MockTransport::with_streams(vec![MockTransport::ok(
            200,
            vec![stream.as_bytes()],
        )]));
        let (driver, receiver) =
            start_driver(authenticated_manager(), transport.clone(), Some("model"));
        driver.prompt("Hello".to_owned());
        let events = collect_until_finished(&receiver);
        assert_failure(&events, "ended before it completed");
    }

    #[test]
    fn missing_model_fails_without_a_request() {
        let transport = Arc::new(MockTransport::with_streams(vec![]));
        let (driver, receiver) = start_driver(authenticated_manager(), transport.clone(), None);
        driver.prompt("Hello".to_owned());
        let events = collect_until_finished(&receiver);
        assert_failure(&events, "No Claude model");
        assert!(transport.requests.lock().is_empty());
    }

    #[test]
    fn interrupt_generation_distinguishes_abort_from_failure() {
        // Primitive check: bumping the generation is what lets the worker
        // tell a cancel apart from a transport failure.
        let shared = Arc::new(Shared {
            body: Mutex::new(None),
            generation: AtomicU64::new(0),
            busy: AtomicBool::new(false),
        });
        shared.abort_active_stream();
        assert_eq!(shared.generation.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn rollback_truncates_to_turn_boundaries() {
        let completed = sse_completed_frame("end_turn");
        let transport = Arc::new(MockTransport::with_streams(vec![
            MockTransport::ok(200, vec![completed.as_bytes()]),
            MockTransport::ok(200, vec![completed.as_bytes()]),
            MockTransport::ok(200, vec![completed.as_bytes()]),
        ]));
        let (driver, receiver) =
            start_driver(authenticated_manager(), transport.clone(), Some("model"));
        driver.prompt("One".to_owned());
        collect_until_finished(&receiver);
        driver.prompt("Two".to_owned());
        collect_until_finished(&receiver);
        assert!(driver.rollback(1).is_ok());
        // The empty-text turns above each recorded only their user message,
        // so dropping the last turn leaves [u1]; the next prompt resends
        // exactly [u1, u3].
        driver.prompt("Three".to_owned());
        collect_until_finished(&receiver);
        let requests = transport.requests.lock();
        assert_eq!(requests.len(), 3);
        let messages = requests[2].body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1]["content"][0]["text"], "Three");
    }
}
