//! ChatGPT provider wire protocol, ported from `opencoredev/login-with-chatgpt`.
//!
//! This module is pure: URL construction, request headers, request-body
//! normalization, model-slug extraction, JWT claim reads, and the typed error
//! vocabulary. It performs no I/O and owns no credentials — session state,
//! storage, and HTTPS live in [`crate::chatgpt_session`].
//!
//! Everything here mirrors behavior implemented by that SDK (which itself
//! mirrors the Codex CLI's private OAuth client and the ChatGPT-backed Codex
//! endpoints). None of it is a documented public OpenAI API: endpoints,
//! client identifiers, and the `client_version` model gate can move without
//! notice, so every value stays overridable through [`DeviceAuthConfig`].
//!
//! Security: token-bearing structs redact in `Debug`. There is deliberately
//! no API that hands raw tokens to UI or IPC layers; the daemon driver (a
//! later stage) will consume them through crate-scoped accessors only.

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

// ---------------------------------------------------------------------------
// Constants (mirror `packages/core/src/constants.ts`)
// ---------------------------------------------------------------------------

/// Public OAuth client id used by the Codex CLI. Reused — not Waku's own.
pub const DEFAULT_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// OAuth issuer / authorization server origin.
pub const DEFAULT_ISSUER: &str = "https://auth.openai.com";
/// OAuth scopes required to obtain a refreshable ChatGPT session.
pub const DEFAULT_SCOPE: &str = "openid profile email offline_access";
/// Base URL of the ChatGPT-backed Codex model API.
pub const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
/// `originator` value identifying the client to OpenAI.
pub const DEFAULT_ORIGINATOR: &str = "codex_cli_rs";
/// JWT claim namespace carrying ChatGPT account/plan metadata.
pub const AUTH_CLAIM: &str = "https://api.openai.com/auth";
/// Device codes expire server-side ~15 minutes after issue.
pub const DEVICE_CODE_TTL_MS: u64 = 15 * 60 * 1000;
/// Default model used when a request omits one.
pub const DEFAULT_MODEL: &str = "gpt-5.5";
/// Codex client version sent as `client_version`. The backend gates the
/// available model set on this — a stale value makes models report as
/// unsupported. Bump toward the current Codex CLI release if models vanish.
pub const DEFAULT_CLIENT_VERSION: &str = "0.142.5";
/// Default system instructions for `/responses` calls.
pub const DEFAULT_CODEX_INSTRUCTIONS: &str = "You are a helpful assistant powered by the user's ChatGPT account. Answer the user's request directly and helpfully.";
/// The stateless backend requires encrypted reasoning content to be requested.
pub const REASONING_ENCRYPTED_CONTENT: &str = "reasoning.encrypted_content";
/// Refresh while the access token is within this window of expiring.
pub const EXPIRY_MARGIN_MS: u64 = 60 * 1000;
/// Error codes OpenAI returns when a refresh token can no longer be used.
const DEAD_REFRESH_ERRORS: [&str; 4] = [
    "refresh_token_expired",
    "refresh_token_reused",
    "refresh_token_invalidated",
    "invalid_grant",
];

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Stable error codes mirroring `ChatGPTAuthErrorCode`. Callers branch on
/// `code`, never on message text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatGptErrorCode {
    DeviceCodeRequestFailed,
    DeviceCodeDisabled,
    AuthorizationExpired,
    TokenExchangeFailed,
    TokenRefreshFailed,
    RefreshTokenInvalid,
    NotAuthenticated,
    NetworkError,
    ModelsRequestFailed,
    ResponsesRequestFailed,
    InvalidRequest,
    InvalidResponse,
    /// Local credential-store I/O failed (never a network or auth problem).
    StorageError,
}

/// Typed protocol/session error. Carries the upstream HTTP status when one is
/// available. Server response bodies are deliberately not retained: they add
/// no branching value beyond `code` + `status`, and dropping them keeps
/// credential-adjacent material out of logs and crash reports.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatGptError {
    pub code: ChatGptErrorCode,
    pub message: String,
    pub status: Option<u16>,
}

impl ChatGptError {
    pub fn new(code: ChatGptErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            status: None,
        }
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    /// `true` when the refresh token is dead and the user must sign in again.
    pub fn is_refresh_token_invalid(&self) -> bool {
        self.code == ChatGptErrorCode::RefreshTokenInvalid
    }
}

impl std::fmt::Display for ChatGptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(status) => write!(f, "{} (http {status})", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for ChatGptError {}

/// Maps a token-endpoint `error` field to a refresh outcome.
pub fn is_dead_refresh_error(code: &str) -> bool {
    DEAD_REFRESH_ERRORS.contains(&code)
}

/// Extracts a top-level string `error` field from a JSON body, if present.
pub fn extract_error_code(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body)
        .ok()?
        .get("error")?
        .as_str()
        .map(str::to_owned)
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Overridable endpoints and client identifiers. Mirrors `ChatGPTConfig` +
/// `resolveConfig`: every default below tracks the SDK so Waku keeps working
/// if OpenAI moves an endpoint — override the field instead of forking code.
#[derive(Clone, Debug)]
pub struct DeviceAuthConfig {
    pub client_id: String,
    pub issuer: String,
    pub scope: String,
    pub codex_base_url: String,
    pub originator: String,
    pub client_version: String,
}

impl Default for DeviceAuthConfig {
    fn default() -> Self {
        Self {
            client_id: DEFAULT_CLIENT_ID.to_owned(),
            issuer: DEFAULT_ISSUER.to_owned(),
            scope: DEFAULT_SCOPE.to_owned(),
            codex_base_url: DEFAULT_CODEX_BASE_URL.to_owned(),
            originator: DEFAULT_ORIGINATOR.to_owned(),
            client_version: DEFAULT_CLIENT_VERSION.to_owned(),
        }
    }
}

fn strip_trailing_slash(value: &str) -> &str {
    value.trim_end_matches('/')
}

impl DeviceAuthConfig {
    fn issuer(&self) -> String {
        strip_trailing_slash(&self.issuer).to_owned()
    }

    fn codex_base(&self) -> String {
        strip_trailing_slash(&self.codex_base_url).to_owned()
    }

    /// `{issuer}/oauth/token`.
    pub fn token_url(&self) -> String {
        format!("{}/oauth/token", self.issuer())
    }

    /// `{issuer}/oauth/authorize` (loopback PKCE flow; kept for completeness —
    /// Stage 1 implements the device flow, which needs no local listener).
    pub fn authorize_url(&self) -> String {
        format!("{}/oauth/authorize", self.issuer())
    }

    /// `{issuer}/api/accounts/deviceauth/usercode`.
    pub fn device_usercode_url(&self) -> String {
        format!("{}/api/accounts/deviceauth/usercode", self.issuer())
    }

    /// `{issuer}/api/accounts/deviceauth/token`.
    pub fn device_token_url(&self) -> String {
        format!("{}/api/accounts/deviceauth/token", self.issuer())
    }

    /// User-facing device verification page (`{issuer}/codex/device`).
    pub fn device_verification_url(&self) -> String {
        format!("{}/codex/device", self.issuer())
    }

    /// Redirect URI used to exchange a device authorization code.
    pub fn device_redirect_uri(&self) -> String {
        format!("{}/deviceauth/callback", self.issuer())
    }

    /// `{codexBase}/models` with the `client_version` model gate applied.
    pub fn models_url(&self) -> String {
        with_client_version(
            &format!("{}/models", self.codex_base()),
            &self.client_version,
        )
    }

    /// `{codexBase}/responses` with the `client_version` model gate applied.
    pub fn responses_url(&self) -> String {
        with_client_version(
            &format!("{}/responses", self.codex_base()),
            &self.client_version,
        )
    }
}

// ---------------------------------------------------------------------------
// Target URL + client version (mirror `resolveTargetUrl`/`withClientVersion`)
// ---------------------------------------------------------------------------

/// Maps an incoming URL onto the Codex base URL, tolerating absolute URLs and
/// bare paths and stripping a redundant `/v1` segment an OpenAI-style
/// provider wrapper may add.
pub fn resolve_target_url(input: &str, codex_base_url: &str) -> Result<String, ChatGptError> {
    let base = url::Url::parse(codex_base_url).map_err(|_| {
        ChatGptError::new(
            ChatGptErrorCode::InvalidRequest,
            "ChatGPT base URL is invalid",
        )
    })?;
    let base_path = base.path().trim_end_matches('/').to_owned();
    let parsed = if input.starts_with("http://") || input.starts_with("https://") {
        url::Url::parse(input)
    } else {
        url::Url::options()
            .base_url(Some(
                &url::Url::parse("https://placeholder.invalid").expect("placeholder URL parses"),
            ))
            .parse(input)
    }
    .map_err(|_| {
        ChatGptError::new(
            ChatGptErrorCode::InvalidRequest,
            "ChatGPT request URL is invalid",
        )
    })?;

    let mut pathname = parsed.path().to_owned();
    if !base_path.is_empty() && pathname.starts_with(&format!("{base_path}/")) {
        pathname = pathname[base_path.len()..].to_owned();
    }
    if pathname == "/v1" {
        pathname = "/".to_owned();
    } else if let Some(rest) = pathname.strip_prefix("/v1/") {
        pathname = format!("/{rest}");
    }
    if !pathname.starts_with('/') {
        pathname = format!("/{pathname}");
    }

    let origin = base.origin().ascii_serialization();
    let path = if base_path == "/" {
        String::new()
    } else {
        base_path
    };
    let query = match parsed.query() {
        Some(query) if !query.is_empty() => format!("?{query}"),
        _ => String::new(),
    };
    Ok(format!("{origin}{path}{pathname}{query}"))
}

/// Ensures the `client_version` query param is present (the model gate
/// depends on it). A caller-supplied value always wins.
pub fn with_client_version(target_url: &str, client_version: &str) -> String {
    if client_version.is_empty() {
        return target_url.to_owned();
    }
    let Ok(mut url) = url::Url::parse(target_url) else {
        return target_url.to_owned();
    };
    if url.query_pairs().any(|(key, _)| key == "client_version") {
        return target_url.to_owned();
    }
    url.query_pairs_mut()
        .append_pair("client_version", client_version);
    url.into()
}

// ---------------------------------------------------------------------------
// Request headers
// ---------------------------------------------------------------------------

/// Headers every ChatGPT-backed Codex request carries. The access token is
/// redacted in `Debug` — log the struct freely, never the token.
pub struct CodexRequestHeaders {
    access_token: String,
    pub chatgpt_account_id: String,
    pub originator: String,
}

impl CodexRequestHeaders {
    pub fn new(
        access_token: impl Into<String>,
        account_id: impl Into<String>,
        originator: impl Into<String>,
    ) -> Self {
        Self {
            access_token: access_token.into(),
            chatgpt_account_id: account_id.into(),
            originator: originator.into(),
        }
    }

    /// Header lines in `Name: value` form, ready for the curl `-K -` stdin
    /// convention used by `usage.rs` (never argv — no process-table leak).
    /// Crate-scoped: only the daemon transport renders these. Stage 2
    /// consumes this; until then it is intentionally unused.
    #[allow(dead_code)]
    pub(crate) fn header_lines(&self) -> Vec<String> {
        vec![
            format!("Authorization: Bearer {}", self.access_token),
            format!("chatgpt-account-id: {}", self.chatgpt_account_id),
            "OpenAI-Beta: responses=experimental".to_owned(),
            format!("originator: {}", self.originator),
            "Accept: application/json".to_owned(),
        ]
    }

    /// Renders [`Self::header_lines`] as a curl `-K -` stdin config body, so
    /// bearer material travels on stdin exactly like the `usage.rs` headers.
    /// `"` and `\` are escaped; header values in this flow (JWTs, account
    /// ids, fixed tokens) never contain them, but the escaping keeps the
    /// config well-formed regardless.
    pub(crate) fn curl_header_config(&self) -> String {
        self.header_lines()
            .iter()
            .map(|line| {
                let escaped = line.replace('\\', "\\\\").replace('"', "\\\"");
                format!("header = \"{escaped}\"")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl std::fmt::Debug for CodexRequestHeaders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexRequestHeaders")
            .field("access_token", &"<redacted>")
            .field("chatgpt_account_id", &self.chatgpt_account_id)
            .field("originator", &self.originator)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tokens + JWT claims (mirror `tokens.ts` normalization + `jwt.ts` reads)
// ---------------------------------------------------------------------------

/// Normalized OAuth credential set. All bearer material redacts in `Debug`.
/// Fields are private; the session manager exposes only `pub(crate)`
/// accessors, and the UI layer never sees this type at all.
#[derive(Clone, Deserialize, Serialize)]
pub struct TokenSet {
    access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at_ms: Option<u64>,
    /// Public profile parsed from the id token at exchange/refresh time.
    /// Safe for UI: identity claims only, no bearer material.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user: Option<ChatGptUser>,
}

impl TokenSet {
    /// Normalizes a token-endpoint payload. `previous_refresh` is kept when
    /// the response rotates no new refresh token; `now_ms` stamps `expires_in`.
    pub fn from_token_response(
        raw: &Value,
        previous_refresh: Option<&str>,
        now_ms: u64,
    ) -> Result<Self, ChatGptError> {
        let access_token = raw
            .get("access_token")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                ChatGptError::new(
                    ChatGptErrorCode::TokenExchangeFailed,
                    "Token response missing access_token",
                )
            })?
            .to_owned();
        let refresh_token = raw
            .get("refresh_token")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                previous_refresh
                    .filter(|token| !token.is_empty())
                    .map(str::to_owned)
            });
        let id_token = raw
            .get("id_token")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(str::to_owned);
        let account_id = id_token
            .as_deref()
            .and_then(derive_account_id)
            .or_else(|| derive_account_id(&access_token));
        let expires_at_ms = raw
            .get("expires_in")
            .and_then(Value::as_u64)
            .map(|expires_in| now_ms.saturating_add(expires_in.saturating_mul(1000)))
            .or_else(|| token_expiry_ms(&access_token));
        let user = id_token.as_deref().and_then(parse_chatgpt_user);
        Ok(Self {
            access_token,
            refresh_token,
            id_token,
            account_id,
            expires_at_ms,
            user,
        })
    }

    pub(crate) fn access_token(&self) -> &str {
        &self.access_token
    }

    pub(crate) fn refresh_token(&self) -> Option<&str> {
        self.refresh_token.as_deref()
    }

    pub fn account_id(&self) -> Option<&str> {
        self.account_id.as_deref()
    }

    pub fn expires_at_ms(&self) -> Option<u64> {
        self.expires_at_ms
    }

    pub fn user(&self) -> Option<&ChatGptUser> {
        self.user.as_ref()
    }

    /// `true` when the access token is missing, expired, or within the
    /// refresh margin — mirroring `isAccessTokenExpired`.
    pub fn is_expired(&self, now_ms: u64) -> bool {
        if self.access_token.is_empty() {
            return true;
        }
        match self
            .expires_at_ms
            .or_else(|| token_expiry_ms(&self.access_token))
        {
            Some(expires_at) => expires_at <= now_ms.saturating_add(EXPIRY_MARGIN_MS),
            None => false,
        }
    }

    #[cfg(test)]
    pub(crate) fn test_token(access_token: &str) -> Self {
        Self {
            access_token: access_token.to_owned(),
            refresh_token: None,
            id_token: None,
            account_id: None,
            expires_at_ms: None,
            user: None,
        }
    }
}

impl std::fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenSet")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("id_token", &self.id_token.as_ref().map(|_| "<redacted>"))
            .field("account_id", &self.account_id)
            .field("expires_at_ms", &self.expires_at_ms)
            .field("user", &self.user)
            .finish()
    }
}

/// Public profile derived from the id token. Safe for UI/IPC: no bearer
/// material, only identity claims.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChatGptUser {
    pub account_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
}

/// Decodes a JWT payload without verifying its signature. These tokens arrive
/// straight from OpenAI's token endpoint over TLS, so locally reading claims
/// is the documented SDK behavior — never use this to validate a token from
/// an untrusted source.
pub fn decode_jwt_payload(token: &str) -> Option<Map<String, Value>> {
    let mut parts = token.split('.');
    let (_header, payload, _signature) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice::<Value>(&bytes)
        .ok()?
        .as_object()
        .cloned()
}

/// Reads the `exp` claim as epoch milliseconds.
pub fn token_expiry_ms(token: &str) -> Option<u64> {
    decode_jwt_payload(token)?
        .get("exp")?
        .as_u64()
        .map(|exp| exp.saturating_mul(1000))
}

/// Reads the ChatGPT account id from an id (or access) token.
pub fn derive_account_id(token: &str) -> Option<String> {
    decode_jwt_payload(token)?
        .get(AUTH_CLAIM)?
        .as_object()?
        .get("chatgpt_account_id")?
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

/// Builds a public profile from an id token. `None` when the token carries
/// no account id — an account-less session is not usable.
pub fn parse_chatgpt_user(id_token: &str) -> Option<ChatGptUser> {
    let claims = decode_jwt_payload(id_token)?;
    let account_id = derive_account_id(id_token)?;
    let auth = claims.get(AUTH_CLAIM).and_then(Value::as_object);
    let text = |object: &Map<String, Value>, key: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    Some(ChatGptUser {
        account_id,
        email: text(&claims, "email"),
        name: text(&claims, "name"),
        plan: auth.and_then(|auth| text(auth, "chatgpt_plan_type")),
    })
}

// ---------------------------------------------------------------------------
// Model discovery (mirror `listCodexModels` + `extractCodexModelSlugs`)
// ---------------------------------------------------------------------------

/// Extracts model slugs from the shapes the ChatGPT backend has used for
/// model lists. Unknown entries are ignored; duplicates collapse.
pub fn extract_model_slugs(value: &Value) -> Vec<String> {
    fn candidate(item: &Value) -> Option<&str> {
        match item {
            Value::String(slug) => Some(slug),
            Value::Object(_) => item
                .get("slug")
                .or_else(|| item.get("id"))
                .or_else(|| item.get("model"))
                .or_else(|| item.get("name"))
                .and_then(Value::as_str),
            _ => None,
        }
    }

    let lists: Vec<&Vec<Value>> = match value {
        Value::Array(items) => vec![items],
        Value::Object(_) => ["models", "data", "items", "available_models"]
            .into_iter()
            .filter_map(|key| value.get(key).and_then(Value::as_array))
            .collect(),
        _ => Vec::new(),
    };

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for items in lists {
        for item in items {
            if let Some(slug) = candidate(item)
                .map(str::trim)
                .filter(|slug| !slug.is_empty())
                && seen.insert(slug.to_owned())
            {
                out.push(slug.to_owned());
            }
        }
    }
    out
}

/// One-line structural summary of a JSON value: object key names (sorted),
/// array lengths, scalar type names — never values. Safe for daemon logs
/// even for authenticated payloads: it shows *which shape* the backend sent
/// (e.g. a new response key the slug extractor does not cover) without
/// leaking content. Output is capped so a huge payload cannot flood stderr.
pub fn describe_json_shape(value: &Value) -> String {
    const MAX_LEN: usize = 500;
    const MAX_DEPTH: usize = 3;
    fn shape(value: &Value, depth: usize, out: &mut String) {
        if out.len() > MAX_LEN || depth > MAX_DEPTH {
            out.push('…');
            return;
        }
        match value {
            Value::Null => out.push_str("null"),
            Value::Bool(_) => out.push_str("bool"),
            Value::Number(_) => out.push_str("number"),
            Value::String(_) => out.push_str("string"),
            Value::Array(items) => {
                out.push_str(&format!("[len={}", items.len()));
                if let Some(first) = items.first() {
                    out.push(',');
                    shape(first, depth + 1, out);
                }
                out.push(']');
            }
            Value::Object(map) => {
                out.push('{');
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                for (index, key) in keys.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    out.push_str(key);
                    out.push(':');
                    shape(&map[*key], depth + 1, out);
                    if out.len() > MAX_LEN {
                        break;
                    }
                }
                out.push('}');
            }
        }
    }
    let mut out = String::new();
    shape(value, 0, &mut out);
    out
}

// ---------------------------------------------------------------------------
// Responses body normalization (mirror `normalizeResponsesBody`)
// ---------------------------------------------------------------------------

/// Caller-controlled defaults applied under caller-provided values.
#[derive(Clone, Debug, Default)]
pub struct ResponsesDefaults {
    pub instructions: Option<String>,
    pub reasoning_effort: Option<String>,
    pub reasoning_summary: Option<String>,
    pub text_verbosity: Option<String>,
    pub service_tier: Option<String>,
}

/// Adapts a standard OpenAI responses payload for the ChatGPT-backed Codex
/// endpoint, which runs **stateless** (`store: false`). Omitting any of these
/// yields a stream with no assistant text:
/// - `reasoning` must be configured (Codex models always reason);
/// - `include` must request `reasoning.encrypted_content` so reasoning
///   carries across turns without server-side storage;
/// - input items must not carry server-side ids; `item_reference` items
///   (an AI-SDK construct) are removed;
/// - `max_output_tokens` / `max_completion_tokens` are rejected.
pub fn normalize_responses_body(
    mut body: Map<String, Value>,
    defaults: &ResponsesDefaults,
) -> Map<String, Value> {
    if !body
        .get("instructions")
        .is_some_and(|value| value.is_string())
    {
        body.insert(
            "instructions".to_owned(),
            Value::String(
                defaults
                    .instructions
                    .clone()
                    .unwrap_or_else(|| DEFAULT_CODEX_INSTRUCTIONS.to_owned()),
            ),
        );
    }

    body.insert("store".to_owned(), Value::Bool(false));

    let mut reasoning = body
        .get("reasoning")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    reasoning.entry("effort".to_owned()).or_insert_with(|| {
        Value::String(
            defaults
                .reasoning_effort
                .clone()
                .unwrap_or_else(|| "medium".to_owned()),
        )
    });
    reasoning.entry("summary".to_owned()).or_insert_with(|| {
        Value::String(
            defaults
                .reasoning_summary
                .clone()
                .unwrap_or_else(|| "auto".to_owned()),
        )
    });
    body.insert("reasoning".to_owned(), Value::Object(reasoning));

    let mut text = body
        .get("text")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    text.entry("verbosity".to_owned()).or_insert_with(|| {
        Value::String(
            defaults
                .text_verbosity
                .clone()
                .unwrap_or_else(|| "medium".to_owned()),
        )
    });
    body.insert("text".to_owned(), Value::Object(text));

    if body.get("service_tier").is_none()
        && let Some(tier) = defaults.service_tier.as_deref()
    {
        body.insert("service_tier".to_owned(), Value::String(tier.to_owned()));
    }

    let mut include: Vec<Value> = body
        .get("include")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| item.is_string())
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if !include
        .iter()
        .any(|item| item.as_str() == Some(REASONING_ENCRYPTED_CONTENT))
    {
        include.push(Value::String(REASONING_ENCRYPTED_CONTENT.to_owned()));
    }
    body.insert("include".to_owned(), Value::Array(include));

    if let Some(input) = body.get("input").and_then(Value::as_array).cloned() {
        body.insert("input".to_owned(), Value::Array(filter_codex_input(&input)));
    }

    body.remove("max_output_tokens");
    body.remove("max_completion_tokens");
    body
}

/// Strips server-side ids from input items and removes `item_reference`
/// entries, which the stateless Codex API does not accept.
pub fn filter_codex_input(input: &[Value]) -> Vec<Value> {
    input
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) != Some("item_reference"))
        .map(|item| {
            if let Some(object) = item.as_object() {
                let mut rest = object.clone();
                rest.remove("id");
                Value::Object(rest)
            } else {
                item.clone()
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Responses request validation (policy half of `prepareResponsesPayload`)
// ---------------------------------------------------------------------------

/// Validates a `/responses` request body before it touches the network:
/// object shape, non-empty string `model` (defaulted when absent), and an
/// optional allowlist. Returns the resolved model slug.
pub fn validate_responses_request(
    body: &mut Map<String, Value>,
    default_model: &str,
    allowed_models: Option<&[String]>,
) -> Result<String, ChatGptError> {
    if body.get("model").is_none() {
        body.insert("model".to_owned(), Value::String(default_model.to_owned()));
    }
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| {
            ChatGptError::new(
                ChatGptErrorCode::InvalidRequest,
                "`model` must be a non-empty string",
            )
        })?
        .to_owned();
    if let Some(allowed) = allowed_models
        && !allowed.iter().any(|entry| entry == &model)
    {
        return Err(ChatGptError::new(
            ChatGptErrorCode::InvalidRequest,
            format!("model `{model}` is not allowed"),
        ));
    }
    Ok(model)
}

// ---------------------------------------------------------------------------
// Device-flow DTOs (wire shapes only; I/O lives in `chatgpt_session`)
// ---------------------------------------------------------------------------

/// Device code returned by `POST .../deviceauth/usercode`. `user_code` and
/// `verification_url` are display material (the user types the code into the
/// page); `device_auth_id` stays inside the daemon. Redacted accordingly.
#[derive(Clone, Deserialize, Serialize)]
pub struct DeviceCode {
    device_auth_id: String,
    pub user_code: String,
    pub verification_url: String,
    pub interval_secs: u64,
    pub expires_at_ms: u64,
}

impl DeviceCode {
    pub fn new(
        device_auth_id: String,
        user_code: String,
        verification_url: String,
        interval_secs: u64,
        expires_at_ms: u64,
    ) -> Result<Self, ChatGptError> {
        if device_auth_id.is_empty() || user_code.is_empty() {
            return Err(ChatGptError::new(
                ChatGptErrorCode::DeviceCodeRequestFailed,
                "Device code response was missing required fields",
            ));
        }
        Ok(Self {
            device_auth_id,
            user_code,
            verification_url,
            interval_secs: interval_secs.max(1),
            expires_at_ms,
        })
    }

    /// Stage 2 daemon wiring reuses the in-progress code across calls;
    /// until then this accessor is intentionally unused.
    #[allow(dead_code)]
    pub(crate) fn device_auth_id(&self) -> &str {
        &self.device_auth_id
    }
}

impl std::fmt::Debug for DeviceCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceCode")
            .field("device_auth_id", &"<redacted>")
            .field("user_code", &self.user_code)
            .field("verification_url", &self.verification_url)
            .field("interval_secs", &self.interval_secs)
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

/// One poll of `POST .../deviceauth/token`. `403/404/429` mean "keep
/// waiting" (documented retryable); `200` with a code triple authorizes;
/// anything else is a failure. A `200` without a code means still binding.
/// Manual `Debug`: authorization material redacts like every other bearer
/// carrier in this module.
pub enum DevicePollOutcome {
    Pending,
    Authorized {
        authorization_code: String,
        code_verifier: String,
    },
}

impl std::fmt::Debug for DevicePollOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "Pending"),
            Self::Authorized { .. } => f
                .debug_struct("Authorized")
                .field("authorization_code", &"<redacted>")
                .field("code_verifier", &"<redacted>")
                .finish(),
        }
    }
}

impl DevicePollOutcome {
    pub fn from_status_and_body(status: u16, body: &Value) -> Result<Self, ChatGptError> {
        if status == 403 || status == 404 || status == 429 {
            return Ok(Self::Pending);
        }
        if !(200..300).contains(&status) {
            return Err(ChatGptError::new(
                ChatGptErrorCode::TokenExchangeFailed,
                format!("Device authorization failed ({status})"),
            )
            .with_status(status));
        }
        let code = body.get("authorization_code").and_then(Value::as_str);
        let verifier = body.get("code_verifier").and_then(Value::as_str);
        match (code, verifier) {
            (Some(code), Some(verifier)) if !code.is_empty() && !verifier.is_empty() => {
                Ok(Self::Authorized {
                    authorization_code: code.to_owned(),
                    code_verifier: verifier.to_owned(),
                })
            }
            _ => Ok(Self::Pending),
        }
    }
}

// ---------------------------------------------------------------------------
// Form encoding (token exchange posts `application/x-www-form-urlencoded`)
// ---------------------------------------------------------------------------

/// Percent-encodes a form field value (RFC 3986 unreserved set passes
/// through). Kept local to avoid a new dependency for one call site.
pub fn form_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{byte:02X}"));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Responses SSE parsing (mirrors the upstream proxy contract)
// ---------------------------------------------------------------------------

/// The upstream `/responses` handler forwards the endpoint's
/// `text/event-stream` untouched to an OpenAI Responses parser, so the event
/// vocabulary is the standard Responses streaming one: `response.created`,
/// `response.output_text.delta` (`delta` string), `response.completed`, and
/// `response.failed` / `response.incomplete` / `error`. Anything else is
/// ignored — unknown events must never crash the stream.
///
/// The one lifecycle exception is `response.output_item.done` carrying a
/// `web_search_call` item: the observed backend leaves
/// `response.completed.response.output` empty (`[]`), so the streaming
/// `done` event is the authoritative source for the provider-managed search
/// item. The full item JSON is preserved (including its `ws_...` id) for the
/// later activation phase; search progress events carry only `item_id` and
/// stay ignored.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResponsesStreamEvent {
    TextDelta(String),
    ReasoningDelta(String),
    Completed(Value),
    Failed,
    WebSearchCall(Value),
}

/// Incremental SSE frame parser. Feed network chunks to [`Self::push`] as
/// they arrive and forward the returned events immediately — the full stream
/// is never buffered. Tolerant of `\n` / `\r\n`, split frames across chunks,
/// several frames per chunk, `:` keepalive comments, and `[DONE]`.
#[derive(Debug, Default)]
pub struct ResponsesSseParser {
    buffer: String,
}

impl ResponsesSseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses complete frames out of `chunk` plus any carry-over, keeping a
    /// partial tail buffered for the next call.
    pub fn push(&mut self, chunk: &str) -> Vec<ResponsesStreamEvent> {
        self.buffer.push_str(chunk);
        let mut events = Vec::new();
        while let Some(end) = frame_end(&self.buffer) {
            let frame: String = self.buffer.drain(..end).collect();
            // Consume the blank separator line itself.
            if self.buffer.starts_with("\r\n") {
                self.buffer.drain(..2);
            } else if self.buffer.starts_with('\n') {
                self.buffer.drain(..1);
            }
            events.extend(frame_events(&frame));
        }
        events
    }
}

/// Byte length of the first complete `frame` (exclusive of its blank
/// separator), if one is buffered. A frame ends at the first empty line.
fn frame_end(buffer: &str) -> Option<usize> {
    let bytes = buffer.as_bytes();
    let mut line_start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let end = bytes
            .iter()
            .skip(i)
            .position(|byte| *byte == b'\n')
            .map(|offset| i + offset)?;
        let line = &buffer[line_start..end];
        if line.trim_end_matches('\r').is_empty() {
            return Some(line_start);
        }
        i = end + 1;
        line_start = i;
    }
    None
}

/// Dispatches one complete SSE frame (no trailing blank line) into stream
/// events. Multiple `data:` lines join with `\n`, mirroring `EventSource`.
fn frame_events(frame: &str) -> Vec<ResponsesStreamEvent> {
    let mut data_lines = Vec::new();
    for line in frame.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with(':') || line.is_empty() {
            continue;
        }
        let Some(payload) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = payload.strip_prefix(' ').unwrap_or(payload);
        if payload == "[DONE]" {
            continue;
        }
        data_lines.push(payload);
    }
    if data_lines.is_empty() {
        return Vec::new();
    }
    let body = data_lines.join("\n");
    let value: Value = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(_) => return Vec::new(),
    };
    stream_event(&value).into_iter().collect()
}

/// Maps one parsed SSE JSON body onto a stream event. Only the documented
/// Responses families produce output; everything else (other item lifecycle
/// events, annotations, future additions) is ignored without failing.
///
/// The single item-lifecycle exception is `response.output_item.done` with
/// `item.type == "web_search_call"`, which yields
/// [`ResponsesStreamEvent::WebSearchCall`] carrying the full item JSON. All
/// other `output_item.*` events (including `added` and message-type `done`)
/// and all `response.web_search_call.*` progress events stay ignored: the
/// progress events carry only `item_id`, which the `done` event supersedes.
fn stream_event(value: &Value) -> Option<ResponsesStreamEvent> {
    let event_type = value.get("type").and_then(Value::as_str)?;
    if event_type == "response.completed" {
        return Some(ResponsesStreamEvent::Completed(value.clone()));
    }
    if matches!(
        event_type,
        "response.failed" | "response.incomplete" | "error"
    ) {
        return Some(ResponsesStreamEvent::Failed);
    }
    if event_type == "response.output_item.done" {
        let item = value.get("item")?;
        if item.get("type").and_then(Value::as_str) == Some("web_search_call") {
            return Some(ResponsesStreamEvent::WebSearchCall(item.clone()));
        }
        return None;
    }
    let delta = value
        .get("delta")
        .and_then(Value::as_str)
        .filter(|delta| !delta.is_empty())?;
    if event_type.contains("reasoning") {
        return Some(ResponsesStreamEvent::ReasoningDelta(delta.to_owned()));
    }
    if event_type.contains("output_text") {
        return Some(ResponsesStreamEvent::TextDelta(delta.to_owned()));
    }
    None
}

/// Output items of a `response.completed` payload, for stateless continuity:
/// the next turn resends these (ids stripped by [`filter_codex_input`]) so
/// encrypted reasoning round-trips without server-side storage.
pub fn completed_output_items(completed: &Value) -> Vec<Value> {
    completed
        .get("response")
        .and_then(|response| response.get("output"))
        .or_else(|| completed.get("output"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Mirrors the upstream proxy's service-tier fallback trigger: when the
/// request carried `service_tier: "fast"` and the rejection names an
/// unsupported tier, retry once with the field removed.
pub fn is_unsupported_service_tier_error(body: &str) -> bool {
    body.to_lowercase().contains("unsupported service_tier")
}

/// Reads `Retry-After` (delta-seconds form) from a raw response header block.
/// HTTP-date form is ignored — the daemon never waits on a clock reading it
/// cannot verify cheaply. Clamped to 1–300 seconds.
pub fn parse_retry_after_secs(header_block: &str) -> Option<u64> {
    for line in header_block.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("retry-after") {
            let secs: u64 = value.trim().parse().ok()?;
            if secs == 0 {
                return None;
            }
            return Some(secs.min(300));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn unsigned_jwt(payload: &Value) -> String {
        let encode =
            |value: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.as_bytes());
        format!(
            "{}.{}.{}",
            encode(r#"{"alg":"none"}"#),
            encode(&serde_json::to_string(payload).unwrap()),
            encode("sig")
        )
    }

    #[test]
    fn config_derives_all_endpoints_from_issuer() {
        let config = DeviceAuthConfig::default();
        assert_eq!(config.token_url(), "https://auth.openai.com/oauth/token");
        assert_eq!(
            config.device_usercode_url(),
            "https://auth.openai.com/api/accounts/deviceauth/usercode"
        );
        assert_eq!(
            config.device_token_url(),
            "https://auth.openai.com/api/accounts/deviceauth/token"
        );
        assert_eq!(
            config.device_verification_url(),
            "https://auth.openai.com/codex/device"
        );
        assert_eq!(
            config.device_redirect_uri(),
            "https://auth.openai.com/deviceauth/callback"
        );
        assert!(
            config
                .models_url()
                .starts_with("https://chatgpt.com/backend-api/codex/models?"),
            "{}",
            config.models_url()
        );
        assert!(config.models_url().contains("client_version="));
    }

    #[test]
    fn config_strips_trailing_slashes() {
        let config = DeviceAuthConfig {
            issuer: "https://auth.example.com///".to_owned(),
            codex_base_url: "https://example.com/api/".to_owned(),
            ..DeviceAuthConfig::default()
        };
        assert_eq!(config.token_url(), "https://auth.example.com/oauth/token");
        assert!(
            config
                .models_url()
                .starts_with("https://example.com/api/models?")
        );
    }

    #[test]
    fn client_version_is_added_once_and_never_overwritten() {
        let plain = with_client_version("https://example.com/models", "0.142.5");
        assert!(plain.contains("client_version=0.142.5"));
        let kept =
            with_client_version("https://example.com/models?client_version=9.9.9", "0.142.5");
        assert!(kept.contains("client_version=9.9.9"));
        assert!(!kept.contains("0.142.5"));
        assert_eq!(
            with_client_version("https://example.com/models", ""),
            "https://example.com/models"
        );
    }

    #[test]
    fn target_url_remaps_paths_and_strips_v1() {
        let base = "https://chatgpt.com/backend-api/codex";
        assert_eq!(
            resolve_target_url("/responses", base).unwrap(),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            resolve_target_url("/v1/responses", base).unwrap(),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            resolve_target_url("https://api.openai.com/v1/responses?stream=true", base).unwrap(),
            "https://chatgpt.com/backend-api/codex/responses?stream=true"
        );
        // A redundant base-path prefix is not doubled.
        assert_eq!(
            resolve_target_url("/backend-api/codex/models", base).unwrap(),
            "https://chatgpt.com/backend-api/codex/models"
        );
    }

    #[test]
    fn target_url_rejects_garbage() {
        // Relative inputs resolve against the base like the SDK's
        // `new URL(input, placeholder)` — only absolute garbage fails.
        assert_eq!(
            resolve_target_url("verify", DEFAULT_CODEX_BASE_URL).unwrap(),
            "https://chatgpt.com/backend-api/codex/verify"
        );
        assert!(resolve_target_url("http://exa mple.com/", DEFAULT_CODEX_BASE_URL).is_err());
        assert!(resolve_target_url("/ok", "not a url").is_err());
    }

    #[test]
    fn headers_redact_the_bearer_token() {
        let headers = CodexRequestHeaders::new("secret-token", "acct-1", "codex_cli_rs");
        let debug = format!("{headers:?}");
        assert!(!debug.contains("secret-token"));
        assert!(debug.contains("acct-1"));
        let lines = headers.header_lines();
        assert!(
            lines
                .iter()
                .any(|line| line == "Authorization: Bearer secret-token")
        );
        assert!(
            lines
                .iter()
                .any(|line| line == "chatgpt-account-id: acct-1")
        );
        assert!(
            lines
                .iter()
                .any(|line| line == "OpenAI-Beta: responses=experimental")
        );
        // The curl config carries the same headers on stdin, quoted.
        let config = headers.curl_header_config();
        assert!(config.contains("header = \"Authorization: Bearer secret-token\""));
        assert!(config.contains("header = \"chatgpt-account-id: acct-1\""));
    }

    #[test]
    fn token_set_requires_access_token_and_keeps_previous_refresh() {
        let raw = json!({"access_token": "a", "expires_in": 3600});
        let tokens = TokenSet::from_token_response(&raw, Some("old-refresh"), 1_000).unwrap();
        assert_eq!(tokens.access_token(), "a");
        assert_eq!(tokens.refresh_token(), Some("old-refresh"));
        assert_eq!(tokens.expires_at_ms(), Some(1_000 + 3_600_000));

        let rotated = json!({"access_token": "b", "refresh_token": "new"});
        let tokens = TokenSet::from_token_response(&rotated, Some("old-refresh"), 0).unwrap();
        assert_eq!(tokens.refresh_token(), Some("new"));

        assert!(TokenSet::from_token_response(&json!({}), None, 0).is_err());
        assert!(TokenSet::from_token_response(&json!({"access_token": ""}), None, 0).is_err());
    }

    #[test]
    fn token_set_expiry_uses_margin_and_jwt_fallback() {
        let raw = json!({"access_token": "a", "expires_in": 3_600});
        let tokens = TokenSet::from_token_response(&raw, None, 0).unwrap();
        assert!(!tokens.is_expired(0));
        // Within the 60 s margin counts as expired.
        assert!(tokens.is_expired(3_600_000 - EXPIRY_MARGIN_MS + 1));
        assert!(tokens.is_expired(u64::MAX));

        let jwt = unsigned_jwt(&json!({"exp": 100}));
        let raw = json!({"access_token": jwt});
        let tokens = TokenSet::from_token_response(&raw, None, 0).unwrap();
        assert_eq!(tokens.expires_at_ms(), Some(100_000));
    }

    #[test]
    fn token_debug_never_contains_secrets() {
        let tokens = TokenSet {
            access_token: "access-123".to_owned(),
            refresh_token: Some("refresh-456".to_owned()),
            id_token: Some("id-789".to_owned()),
            account_id: Some("acct".to_owned()),
            expires_at_ms: Some(1),
            user: None,
        };
        let debug = format!("{tokens:?}");
        assert!(!debug.contains("access-123"));
        assert!(!debug.contains("refresh-456"));
        assert!(!debug.contains("id-789"));
        assert!(debug.contains("acct"));
    }

    #[test]
    fn jwt_reads_account_user_and_expiry() {
        let token = unsigned_jwt(&json!({
            "exp": 1_700_000_000u64,
            "email": "user@example.com",
            "name": "User",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct-42",
                "chatgpt_plan_type": "plus",
            },
        }));
        assert_eq!(derive_account_id(&token), Some("acct-42".to_owned()));
        assert_eq!(token_expiry_ms(&token), Some(1_700_000_000_000));
        let user = parse_chatgpt_user(&token).unwrap();
        assert_eq!(user.account_id, "acct-42");
        assert_eq!(user.email.as_deref(), Some("user@example.com"));
        assert_eq!(user.plan.as_deref(), Some("plus"));

        assert_eq!(derive_account_id("not-a-jwt"), None);
        assert_eq!(derive_account_id("a.b"), None);
        assert!(parse_chatgpt_user(&unsigned_jwt(&json!({"email": "x"}))).is_none());
    }

    #[test]
    fn model_slugs_cover_all_documented_shapes() {
        assert_eq!(
            extract_model_slugs(&json!(["gpt-5.5", "gpt-5.5", " gpt-5.4 ", ""])),
            vec!["gpt-5.5".to_owned(), "gpt-5.4".to_owned()]
        );
        assert_eq!(
            extract_model_slugs(
                &json!({"data": [{"slug": "a"}, {"id": "b"}, {"model": "c"}, {"name": "d"}, {"nope": 1}, 42, Value::Null] })
            ),
            vec![
                "a".to_owned(),
                "b".to_owned(),
                "c".to_owned(),
                "d".to_owned()
            ]
        );
        assert_eq!(
            extract_model_slugs(&json!({"models": [{"slug": "x"}]})),
            vec!["x".to_owned()]
        );
        assert!(extract_model_slugs(&json!({"unexpected": []})).is_empty());
        assert!(extract_model_slugs(&json!(42)).is_empty());
        assert!(extract_model_slugs(&Value::Null).is_empty());
    }

    #[test]
    fn shape_summary_shows_structure_never_values() {
        assert_eq!(
            describe_json_shape(&json!({"data": [{"slug": "gpt-5.5", "secret": "s3cr3t"}]})),
            "{data:[len=1,{secret:string,slug:string}]}"
        );
        assert_eq!(describe_json_shape(&json!([])), "[len=0]");
        assert_eq!(describe_json_shape(&json!(42)), "number");
        assert_eq!(describe_json_shape(&Value::Null), "null");
        // A genuinely empty model set is distinguishable from an unknown one.
        assert_eq!(describe_json_shape(&json!({"data": []})), "{data:[len=0]}");
    }

    #[test]
    fn normalization_applies_stateless_requirements() {
        let body = normalize_responses_body(Map::new(), &ResponsesDefaults::default());
        assert_eq!(body.get("store"), Some(&Value::Bool(false)));
        let field = |body: &Map<String, Value>, group: &str, key: &str| {
            body.get(group).and_then(|group| group.get(key)).cloned()
        };
        assert_eq!(
            field(&body, "reasoning", "effort"),
            Some(Value::String("medium".to_owned()))
        );
        assert_eq!(
            field(&body, "reasoning", "summary"),
            Some(Value::String("auto".to_owned()))
        );
        assert_eq!(
            field(&body, "text", "verbosity"),
            Some(Value::String("medium".to_owned()))
        );
        assert_eq!(
            body.get("instructions"),
            Some(&Value::String(DEFAULT_CODEX_INSTRUCTIONS.to_owned()))
        );
        assert_eq!(
            body.get("include"),
            Some(&json!([REASONING_ENCRYPTED_CONTENT]))
        );

        // Caller values win; conflicting token caps are dropped.
        let mut custom = Map::new();
        custom.insert("instructions".to_owned(), json!("custom"));
        custom.insert("reasoning".to_owned(), json!({"effort": "high"}));
        custom.insert("include".to_owned(), json!(["other"]));
        custom.insert("max_output_tokens".to_owned(), json!(100));
        custom.insert("max_completion_tokens".to_owned(), json!(100));
        custom.insert(
            "input".to_owned(),
            json!([
                {"type": "message", "id": "server-1", "role": "user"},
                {"type": "item_reference", "id": "ref-1"},
            ]),
        );
        let body = normalize_responses_body(
            custom,
            &ResponsesDefaults {
                service_tier: Some("fast".to_owned()),
                ..ResponsesDefaults::default()
            },
        );
        assert_eq!(body.get("instructions"), Some(&json!("custom")));
        assert_eq!(
            body.get("reasoning")
                .and_then(|reasoning| reasoning.get("effort")),
            Some(&json!("high"))
        );
        assert_eq!(body.get("service_tier"), Some(&json!("fast")));
        assert!(!body.contains_key("max_output_tokens"));
        assert!(!body.contains_key("max_completion_tokens"));
        assert_eq!(
            body.get("input"),
            Some(&json!([{"type": "message", "role": "user"}]))
        );
        let include = body.get("include").and_then(Value::as_array).unwrap();
        assert!(include.contains(&json!("other")));
        assert!(include.contains(&json!(REASONING_ENCRYPTED_CONTENT)));
        // No duplicates when already present.
        let mut again = Map::new();
        again.insert("include".to_owned(), json!([REASONING_ENCRYPTED_CONTENT]));
        let body = normalize_responses_body(again, &ResponsesDefaults::default());
        assert_eq!(
            body.get("include"),
            Some(&json!([REASONING_ENCRYPTED_CONTENT]))
        );
    }

    #[test]
    fn input_filtering_drops_references_and_ids_only() {
        let input = vec![
            json!({"type": "message", "id": "a", "role": "user"}),
            json!({"type": "item_reference", "id": "b"}),
            json!("plain"),
            json!(42),
        ];
        assert_eq!(
            filter_codex_input(&input),
            vec![
                json!({"type": "message", "role": "user"}),
                json!("plain"),
                json!(42),
            ]
        );
    }

    #[test]
    fn request_validation_defaults_and_rejects() {
        let mut body = Map::new();
        let model = validate_responses_request(&mut body, DEFAULT_MODEL, None).unwrap();
        assert_eq!(model, DEFAULT_MODEL);
        assert_eq!(body.get("model"), Some(&json!(DEFAULT_MODEL)));

        let mut body = Map::new();
        body.insert("model".to_owned(), json!("gpt-5.5"));
        let allowed = ["gpt-5.5".to_owned()];
        assert!(validate_responses_request(&mut body, DEFAULT_MODEL, Some(&allowed)).is_ok());

        let mut body = Map::new();
        body.insert("model".to_owned(), json!("evil-model"));
        assert!(validate_responses_request(&mut body, DEFAULT_MODEL, Some(&allowed)).is_err());

        let mut body = Map::new();
        body.insert("model".to_owned(), json!(""));
        assert!(validate_responses_request(&mut body, DEFAULT_MODEL, None).is_err());

        let mut body = Map::new();
        body.insert("model".to_owned(), json!(42));
        assert!(validate_responses_request(&mut body, DEFAULT_MODEL, None).is_err());
    }

    #[test]
    fn device_code_requires_fields_and_clamps_interval() {
        assert!(DeviceCode::new(String::new(), "X".into(), "u".into(), 5, 9).is_err());
        assert!(DeviceCode::new("d".into(), String::new(), "u".into(), 5, 9).is_err());
        let code = DeviceCode::new(
            "secret-device-id-9".into(),
            "ABCD-1234".into(),
            "u".into(),
            0,
            9,
        )
        .unwrap();
        assert_eq!(code.interval_secs, 1);
        let debug = format!("{code:?}");
        assert!(!debug.contains("secret-device-id-9"));
        assert!(debug.contains("ABCD-1234"));
    }

    #[test]
    fn device_poll_maps_retryable_statuses_and_partial_bodies() {
        assert!(matches!(
            DevicePollOutcome::from_status_and_body(403, &json!({})).unwrap(),
            DevicePollOutcome::Pending
        ));
        assert!(matches!(
            DevicePollOutcome::from_status_and_body(404, &json!({})).unwrap(),
            DevicePollOutcome::Pending
        ));
        assert!(matches!(
            DevicePollOutcome::from_status_and_body(429, &json!({})).unwrap(),
            DevicePollOutcome::Pending
        ));
        // 200 without a code triple is still binding.
        assert!(matches!(
            DevicePollOutcome::from_status_and_body(200, &json!({})).unwrap(),
            DevicePollOutcome::Pending
        ));
        assert!(matches!(
            DevicePollOutcome::from_status_and_body(
                200,
                &json!({"authorization_code": "c", "code_verifier": "v", "code_challenge": "h"})
            )
            .unwrap(),
            DevicePollOutcome::Authorized { .. }
        ));
        let error = DevicePollOutcome::from_status_and_body(500, &json!({})).unwrap_err();
        assert_eq!(error.code, ChatGptErrorCode::TokenExchangeFailed);
        assert_eq!(error.status, Some(500));
    }

    #[test]
    fn dead_refresh_errors_match_sdk_set() {
        for code in [
            "refresh_token_expired",
            "refresh_token_reused",
            "refresh_token_invalidated",
            "invalid_grant",
        ] {
            assert!(is_dead_refresh_error(code), "{code}");
        }
        assert!(!is_dead_refresh_error("temporarily_unavailable"));
        assert_eq!(
            extract_error_code(r#"{"error":"invalid_grant"}"#),
            Some("invalid_grant".to_owned())
        );
        assert_eq!(extract_error_code("not json"), None);
    }

    #[test]
    fn form_encoding_covers_unreserved_set() {
        assert_eq!(form_encode("abcXYZ019-_.~"), "abcXYZ019-_.~");
        assert_eq!(form_encode("a b+c&d=e"), "a%20b%2Bc%26d%3De");
    }

    #[test]
    fn sse_text_delta_streams_incrementally() {
        let mut parser = ResponsesSseParser::new();
        let events = parser.push("event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\n");
        assert_eq!(
            events,
            vec![ResponsesStreamEvent::TextDelta("Hel".to_owned())]
        );
        // A second delta in the same parser continues the stream.
        let events =
            parser.push("data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n");
        assert_eq!(
            events,
            vec![ResponsesStreamEvent::TextDelta("lo".to_owned())]
        );
    }

    #[test]
    fn sse_completion_and_reasoning_shapes() {
        let mut parser = ResponsesSseParser::new();
        let events = parser.push("data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"thinking\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n");
        assert_eq!(
            events,
            vec![
                ResponsesStreamEvent::ReasoningDelta("thinking".to_owned()),
                ResponsesStreamEvent::Completed(
                    json!({"type":"response.completed","response":{"output":[]}})
                ),
            ]
        );
        for event_type in ["response.failed", "response.incomplete", "error"] {
            let mut parser = ResponsesSseParser::new();
            let frame = format!("data: {{\"type\":\"{event_type}\"}}\n\n");
            assert_eq!(parser.push(&frame), vec![ResponsesStreamEvent::Failed]);
        }
    }

    #[test]
    fn sse_web_search_call_item_survives_output_item_done() {
        // Live-verified sequence: search item arrives via
        // `response.output_item.done` while `response.completed` carries
        // `output: []`. Progress events stay ignored; text flows unchanged.
        let mut parser = ResponsesSseParser::new();
        let events = parser.push(
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"ws_live\",\"type\":\"web_search_call\",\"status\":\"in_progress\",\"action\":{\"type\":\"search\",\"queries\":[\"current president of Nigeria 2026\"]}}}\n\n\
             data: {\"type\":\"response.web_search_call.in_progress\",\"item_id\":\"ws_live\"}\n\n\
             data: {\"type\":\"response.web_search_call.searching\",\"item_id\":\"ws_live\"}\n\n\
             data: {\"type\":\"response.web_search_call.completed\",\"item_id\":\"ws_live\"}\n\n\
             data: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"ws_live\",\"type\":\"web_search_call\",\"status\":\"completed\",\"action\":{\"type\":\"search\",\"queries\":[\"current president of Nigeria 2026\"]}}}\n\n\
             data: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"msg_1\",\"type\":\"message\"}}\n\n\
             data: {\"type\":\"response.content_part.added\",\"item_id\":\"msg_1\"}\n\n\
             data: {\"type\":\"response.output_text.delta\",\"delta\":\"Bola \"}\n\n\
             data: {\"type\":\"response.output_text.delta\",\"delta\":\"Tinubu\"}\n\n\
             data: {\"type\":\"response.output_text.done\"}\n\n\
             data: {\"type\":\"response.content_part.done\",\"item_id\":\"msg_1\"}\n\n\
             data: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"msg_1\",\"type\":\"message\"}}\n\n\
             data: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n",
        );
        assert_eq!(events.len(), 4);
        let ResponsesStreamEvent::WebSearchCall(item) = &events[0] else {
            panic!("expected WebSearchCall first, got {:?}", events[0]);
        };
        assert_eq!(item.get("id").and_then(Value::as_str), Some("ws_live"));
        assert_eq!(
            item.get("type").and_then(Value::as_str),
            Some("web_search_call")
        );
        assert_eq!(
            item.get("status").and_then(Value::as_str),
            Some("completed")
        );
        assert_eq!(
            item.get("action")
                .and_then(|action| action.get("queries"))
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );
        assert_eq!(
            events[1],
            ResponsesStreamEvent::TextDelta("Bola ".to_owned())
        );
        assert_eq!(
            events[2],
            ResponsesStreamEvent::TextDelta("Tinubu".to_owned())
        );
        assert_eq!(
            events[3],
            ResponsesStreamEvent::Completed(
                json!({"type":"response.completed","response":{"output":[]}})
            )
        );
    }

    #[test]
    fn sse_web_search_call_preserves_ws_12345_id() {
        // The `ws_...` id is the Phase-2 history linkage key; it must
        // survive parsing byte-identical.
        let mut parser = ResponsesSseParser::new();
        let events = parser.push(
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"ws_12345\",\"type\":\"web_search_call\",\"status\":\"completed\",\"action\":{\"type\":\"search\",\"queries\":[\"q\"]}}}\n\n",
        );
        assert_eq!(events.len(), 1);
        let ResponsesStreamEvent::WebSearchCall(item) = &events[0] else {
            panic!("expected WebSearchCall, got {:?}", events[0]);
        };
        assert_eq!(item.get("id").and_then(Value::as_str), Some("ws_12345"));
    }

    #[test]
    fn sse_normal_response_without_search_is_unchanged() {
        // No search events: the pre-change event vector must be exact, with
        // zero WebSearchCall entries.
        let mut parser = ResponsesSseParser::new();
        let events = parser.push(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n\
             data: {\"type\":\"response.output_text.done\"}\n\n\
             data: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n",
        );
        assert_eq!(
            events,
            vec![
                ResponsesStreamEvent::TextDelta("hi".to_owned()),
                ResponsesStreamEvent::Completed(
                    json!({"type":"response.completed","response":{"output":[]}})
                ),
            ]
        );
    }

    #[test]
    fn sse_search_progress_and_message_done_stay_ignored() {
        // Progress events alone, message-type `done`, and future unknowns
        // are ignored; the parser continues normally afterwards.
        let mut parser = ResponsesSseParser::new();
        let events = parser.push(
            "data: {\"type\":\"response.web_search_call.in_progress\",\"item_id\":\"ws_x\"}\n\n\
             data: {\"type\":\"response.web_search_call.searching\",\"item_id\":\"ws_x\"}\n\n\
             data: {\"type\":\"response.web_search_call.completed\",\"item_id\":\"ws_x\"}\n\n\
             data: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"msg_1\",\"type\":\"message\"}}\n\n\
             data: {\"type\":\"unknown.future.event\",\"foo\":1}\n\n",
        );
        assert!(events.is_empty());
        let events =
            parser.push("data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n");
        assert_eq!(
            events,
            vec![ResponsesStreamEvent::TextDelta("ok".to_owned())]
        );
    }

    #[test]
    fn sse_unknown_and_malformed_events_never_crash() {
        let mut parser = ResponsesSseParser::new();
        // Unknown families, empty deltas, and non-JSON bodies are ignored.
        let events = parser.push(
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"x\"}}\n\n\
             data: {\"type\":\"response.output_text.delta\",\"delta\":\"\"}\n\n\
             data: not json at all\n\n\
             : keepalive comment\n\n\
             data: [DONE]\n\n",
        );
        assert!(events.is_empty());
        // The parser still works afterwards.
        let events =
            parser.push("data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n");
        assert_eq!(
            events,
            vec![ResponsesStreamEvent::TextDelta("ok".to_owned())]
        );
    }

    #[test]
    fn sse_split_frame_across_chunks_parses() {
        let mut parser = ResponsesSseParser::new();
        assert!(
            parser
                .push("data: {\"type\":\"response.output_text.del")
                .is_empty()
        );
        assert!(parser.push("ta\",\"delta\":\"Hel").is_empty());
        let events = parser.push("lo\"}\n\n");
        assert_eq!(
            events,
            vec![ResponsesStreamEvent::TextDelta("Hello".to_owned())]
        );
    }

    #[test]
    fn sse_crlf_and_multiline_data_frames() {
        let mut parser = ResponsesSseParser::new();
        // CRLF separators and multi-line data (joined like EventSource).
        let events = parser
            .push("data: {\"type\":\"response.output_text.delta\",\r\ndata: \"broken\"}\r\n\r\n");
        assert!(events.is_empty());
        let events =
            parser.push("data: {\"type\":\"response.output_text.delta\",\"delta\":\"d\"}\r\n\r\n");
        assert_eq!(
            events,
            vec![ResponsesStreamEvent::TextDelta("d".to_owned())]
        );
    }

    #[test]
    fn service_tier_fallback_trigger_matches_upstream() {
        assert!(is_unsupported_service_tier_error(
            "{\"error\":{\"message\":\"Unsupported service_tier: fast\"}}"
        ));
        assert!(is_unsupported_service_tier_error(
            "UNSUPPORTED SERVICE_TIER"
        ));
        assert!(!is_unsupported_service_tier_error("model_not_found"));
        assert!(!is_unsupported_service_tier_error(""));
    }

    #[test]
    fn retry_after_parses_seconds_and_clamps() {
        let headers = "HTTP/2 429\r\nretry-after: 7\r\ncontent-type: application/json\r\n";
        assert_eq!(parse_retry_after_secs(headers), Some(7));
        assert_eq!(
            parse_retry_after_secs("HTTP/2 429\nRetry-After: 9999\n"),
            Some(300)
        );
        assert_eq!(parse_retry_after_secs("HTTP/2 200\n"), None);
        assert_eq!(
            parse_retry_after_secs("HTTP/2 429\nretry-after: soon\n"),
            None
        );
    }

    #[test]
    fn completed_output_items_cover_response_shapes() {
        let completed =
            json!({"type":"response.completed","response":{"output":[{"type":"message"}]}});
        assert_eq!(
            completed_output_items(&completed),
            vec![json!({"type":"message"})]
        );
        assert!(completed_output_items(&json!({"type":"response.completed"})).is_empty());
    }
}
