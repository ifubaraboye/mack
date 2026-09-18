//! Claude subscription provider protocol, mirroring the community-observed
//! Claude Code OAuth flow the way `chatgpt_protocol` mirrors
//! `opencoredev/login-with-chatgpt`.
//!
//! This module is pure: endpoint configuration, PKCE generation, authorize-URL
//! construction, authorization-code parsing, token normalization, request
//! headers, model-slug extraction, profile parsing, and the typed error
//! vocabulary. It performs no I/O and owns no credentials — session state,
//! storage, and HTTPS live in [`crate::claude_session`].
//!
//! Everything here tracks behavior implemented by community clients (`ccauth`,
//! `claude-code-login`, `claude-code-oauth`) against Anthropic's first-party
//! OAuth endpoints. None of it is a documented public Anthropic API for
//! third-party subscription use: endpoints, the client identifier, scopes, and
//! the required system-prompt prefix can move without notice, so every value
//! stays overridable through [`ClaudeAuthConfig`].
//!
//! Endpoint variance (Phase 0 finding): the token endpoint is observed as both
//! `https://console.anthropic.com/v1/oauth/token` and
//! `https://platform.claude.com/v1/oauth/token` across client versions. The
//! default below pairs with the `console.anthropic.com` redirect URI (the
//! majority copy-paste shape); override `token_url` if the account's flow
//! expects the platform host.
//!
//! Security: token-bearing structs redact in `Debug`. There is deliberately
//! no API that hands raw tokens to UI or IPC layers; the daemon driver (a
//! later stage) consumes them through crate-scoped accessors only.

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::usage_history::TokenTotals;

// ---------------------------------------------------------------------------
// Constants (community-observed Claude Code OAuth values)
// ---------------------------------------------------------------------------

/// Public OAuth client id used by the Claude Code CLI. Reused — not Mack's own.
pub const DEFAULT_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
/// Origin of the copy-paste authorization page.
pub const DEFAULT_AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
/// OAuth token endpoint. See the module docs for the platform-host variant.
pub const DEFAULT_TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
/// Copy-paste redirect URI registered for the public client id.
pub const DEFAULT_REDIRECT_URI: &str = "https://console.anthropic.com/oauth/code/callback";
/// OAuth scopes the Claude Code flow requests.
pub const DEFAULT_SCOPE: &str = "org:create_api_key user:profile user:inference";
/// Authenticated profile endpoint (subscription/plan metadata).
pub const DEFAULT_PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
/// Authenticated plan-usage endpoint (same shape `usage.rs` already reads for
/// the CLI's own credentials).
pub const DEFAULT_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// Model catalog endpoint. It may reject subscription OAuth tokens; the
/// session manager falls back to [`FALLBACK_MODEL_SLUGS`] then.
pub const DEFAULT_MODELS_URL: &str = "https://api.anthropic.com/v1/models";
/// Anthropic Messages endpoint used by the later subscription driver.
pub const DEFAULT_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
/// API version header every `api.anthropic.com` request carries.
pub const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";
/// Beta header the OAuth-backed endpoints require.
pub const DEFAULT_OAUTH_BETA: &str = "oauth-2025-04-20";
/// First system block every subscription-OAuth `/v1/messages` request must
/// carry, byte-exact. The backend rejects Sonnet/Opus OAuth requests whose
/// first system block differs with a generic 400; Haiku is exempt but
/// accepts the block harmlessly. Observed independently across community
/// clients (promptfoo, modelbridge, Anima, minzique) — undocumented by
/// Anthropic, so this stays an overridable config field, not a literal.
pub const DEFAULT_SYSTEM_PREFIX: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
/// Default `max_tokens` per Messages request. Required by the endpoint and
/// universally supported; the account's plan governs the real ceiling.
pub const DEFAULT_MAX_TOKENS: u32 = 8192;
/// Small `max_tokens` for title-generation one-offs.
pub const TITLE_MAX_TOKENS: u32 = 100;
/// `User-Agent` when the CLI's probed version is not known yet.
pub const DEFAULT_CLI_VERSION: &str = "2.1.0";
/// Pending PKCE logins expire client-side after this long. PKCE has no
/// server-side device expiry, so this only bounds stale verifiers.
pub const PENDING_LOGIN_TTL_MS: u64 = 15 * 60 * 1000;
/// Refresh while the access token is within this window of expiring.
pub const EXPIRY_MARGIN_MS: u64 = 60 * 1000;
/// PKCE verifier entropy (32 random bytes, base64url-encoded).
pub const PKCE_VERIFIER_BYTES: usize = 32;
/// `state` entropy (32 random bytes, hex-encoded).
pub const OAUTH_STATE_BYTES: usize = 32;
/// Curated model fallback when `/v1/models` rejects the subscription token.
/// Stale by design — bump toward current Claude models when the picker
/// empties. Never invent slugs: these are last-known-real, not aspirational.
pub const FALLBACK_MODEL_SLUGS: [&str; 3] =
    ["claude-sonnet-4-5", "claude-opus-4-1", "claude-haiku-4-5"];
/// Error codes from the token endpoint that mean the refresh token is dead
/// and the user must sign in again.
const DEAD_REFRESH_ERRORS: [&str; 2] = ["invalid_grant", "invalid_request"];

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Stable error codes. Callers branch on `code`, never on message text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClaudeErrorCode {
    LoginStartFailed,
    InvalidAuthorizationCode,
    TokenExchangeFailed,
    TokenRefreshFailed,
    RefreshTokenInvalid,
    NotAuthenticated,
    NetworkError,
    ProfileRequestFailed,
    ModelsRequestFailed,
    MessagesRequestFailed,
    InvalidRequest,
    InvalidResponse,
    /// Local credential-store I/O failed (never a network or auth problem).
    StorageError,
}

/// Typed protocol/session error. Carries the upstream HTTP status when one is
/// available. Server response bodies are deliberately not retained.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaudeError {
    pub code: ClaudeErrorCode,
    pub message: String,
    pub status: Option<u16>,
}

impl ClaudeError {
    pub fn new(code: ClaudeErrorCode, message: impl Into<String>) -> Self {
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
        self.code == ClaudeErrorCode::RefreshTokenInvalid
    }
}

impl std::fmt::Display for ClaudeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(status) => write!(f, "{} (http {status})", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for ClaudeError {}

/// Maps a token-endpoint `error` field to a refresh outcome.
pub fn is_dead_refresh_error(code: &str) -> bool {
    DEAD_REFRESH_ERRORS.contains(&code)
}

/// Extracts the error code from either OAuth-style
/// (`{"error": "invalid_grant"}`) or Anthropic-style
/// (`{"error": {"type": "invalid_grant"}}`) bodies.
pub fn extract_error_code(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    match value.get("error") {
        Some(Value::String(code)) => Some(code.clone()),
        Some(Value::Object(error)) => error.get("type").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Overridable endpoints and client identifiers. Every default tracks the
/// observed Claude Code flow so Mack keeps working if Anthropic moves an
/// endpoint — override the field instead of forking code.
#[derive(Clone, Debug)]
pub struct ClaudeAuthConfig {
    pub client_id: String,
    pub authorize_url: String,
    pub token_url: String,
    pub redirect_uri: String,
    pub scope: String,
    pub profile_url: String,
    pub usage_url: String,
    pub models_url: String,
    pub messages_url: String,
    pub anthropic_version: String,
    pub oauth_beta: String,
    pub cli_version: String,
    /// Byte-exact first system block for subscription-OAuth requests.
    pub system_prefix: String,
    /// Default `max_tokens` for Messages requests.
    pub max_tokens: u32,
}

impl Default for ClaudeAuthConfig {
    fn default() -> Self {
        Self {
            client_id: DEFAULT_CLIENT_ID.to_owned(),
            authorize_url: DEFAULT_AUTHORIZE_URL.to_owned(),
            token_url: DEFAULT_TOKEN_URL.to_owned(),
            redirect_uri: DEFAULT_REDIRECT_URI.to_owned(),
            scope: DEFAULT_SCOPE.to_owned(),
            profile_url: DEFAULT_PROFILE_URL.to_owned(),
            usage_url: DEFAULT_USAGE_URL.to_owned(),
            models_url: DEFAULT_MODELS_URL.to_owned(),
            messages_url: DEFAULT_MESSAGES_URL.to_owned(),
            anthropic_version: DEFAULT_ANTHROPIC_VERSION.to_owned(),
            oauth_beta: DEFAULT_OAUTH_BETA.to_owned(),
            cli_version: DEFAULT_CLI_VERSION.to_owned(),
            system_prefix: DEFAULT_SYSTEM_PREFIX.to_owned(),
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }
}

impl ClaudeAuthConfig {
    pub fn user_agent(&self) -> String {
        format!("claude-code/{}", self.cli_version)
    }
}

// ---------------------------------------------------------------------------
// PKCE + authorize URL (pure; randomness is the only impure input)
// ---------------------------------------------------------------------------

/// Fresh PKCE material for one login: verifier (kept daemon-side, encrypted),
/// challenge (sent in the authorize URL), and state (round-tripped).
#[derive(Clone)]
pub struct PkceMaterial {
    pub(crate) verifier: String,
    pub(crate) challenge: String,
    pub(crate) state: String,
}

impl std::fmt::Debug for PkceMaterial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PkceMaterial")
            .field("verifier", &"<redacted>")
            .field("challenge", &self.challenge)
            .field("state", &self.state)
            .finish()
    }
}

/// Generates fresh PKCE material. The verifier uses 32 random bytes,
/// base64url-encoded without padding (43 characters, within the RFC 7636
/// 43–128 range).
pub fn generate_pkce() -> Result<PkceMaterial, ClaudeError> {
    use rand::TryRngCore as _;
    let mut verifier_bytes = [0u8; PKCE_VERIFIER_BYTES];
    rand::rngs::OsRng
        .try_fill_bytes(&mut verifier_bytes)
        .map_err(|_| {
            ClaudeError::new(
                ClaudeErrorCode::LoginStartFailed,
                "Could not generate the Claude sign-in secret",
            )
        })?;
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(verifier_bytes);
    let mut state_bytes = [0u8; OAUTH_STATE_BYTES];
    rand::rngs::OsRng
        .try_fill_bytes(&mut state_bytes)
        .map_err(|_| {
            ClaudeError::new(
                ClaudeErrorCode::LoginStartFailed,
                "Could not generate the Claude sign-in secret",
            )
        })?;
    let state = state_bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let challenge = pkce_challenge(&verifier);
    Ok(PkceMaterial {
        verifier,
        challenge,
        state,
    })
}

/// S256 code challenge for a verifier: `BASE64URL(SHA256(verifier))`.
pub fn pkce_challenge(verifier: &str) -> String {
    use sha2::{Digest as _, Sha256};
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Builds the copy-paste authorization URL: `code=true` mode, byte-stable
/// parameter order matching the observed Claude Code clients.
pub fn build_authorize_url(config: &ClaudeAuthConfig, pkce: &PkceMaterial) -> String {
    let params = [
        ("code", "true"),
        ("client_id", config.client_id.as_str()),
        ("response_type", "code"),
        ("redirect_uri", config.redirect_uri.as_str()),
        ("scope", config.scope.as_str()),
        ("code_challenge", pkce.challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", pkce.state.as_str()),
    ];
    let query = params
        .iter()
        .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{}?{query}", config.authorize_url)
}

/// Percent-encodes a query component (RFC 3986 unreserved set passes
/// through). Local so no new dependency is needed for one call site.
pub fn percent_encode(value: &str) -> String {
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

/// Authorization code as pasted by the user: either `CODE` or `CODE#STATE`
/// (some authorize pages render both). Trims whitespace and URL fragments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizationInput {
    pub code: String,
    pub state: Option<String>,
}

/// Parses user-pasted authorization material. Empty input is rejected; the
/// state half is optional and only forwarded to the token endpoint.
pub fn parse_authorization_input(raw: &str) -> Result<AuthorizationInput, ClaudeError> {
    let cleaned = raw
        .trim()
        .split(['#', '&'])
        .next()
        .unwrap_or_default()
        .trim();
    // A `#`-separated state survives the first split only when it arrived as
    // `CODE#STATE` with no `&`; recover it from the untrimmed input.
    let state = raw
        .trim()
        .split('&')
        .next()
        .unwrap_or_default()
        .split_once('#')
        .map(|(_, state)| state.trim().to_owned())
        .filter(|state| !state.is_empty());
    if cleaned.is_empty() {
        return Err(ClaudeError::new(
            ClaudeErrorCode::InvalidAuthorizationCode,
            "Authorization code is empty",
        ));
    }
    Ok(AuthorizationInput {
        code: cleaned.to_owned(),
        state,
    })
}

// ---------------------------------------------------------------------------
// Token exchange / refresh bodies (pure JSON; secrets travel on stdin)
// ---------------------------------------------------------------------------

/// JSON body for the authorization-code exchange. `state` falls back to the
/// verifier when the authorize page returned no state half.
pub fn exchange_body(
    config: &ClaudeAuthConfig,
    code: &str,
    state: Option<&str>,
    verifier: &str,
) -> String {
    serde_json::json!({
        "grant_type": "authorization_code",
        "client_id": config.client_id,
        "code": code,
        "redirect_uri": config.redirect_uri,
        "code_verifier": verifier,
        "state": state.unwrap_or(verifier),
    })
    .to_string()
}

/// JSON body for a refresh-token rotation.
pub fn refresh_body(config: &ClaudeAuthConfig, refresh_token: &str) -> String {
    serde_json::json!({
        "grant_type": "refresh_token",
        "client_id": config.client_id,
        "refresh_token": refresh_token,
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// Request headers
// ---------------------------------------------------------------------------

/// Headers every subscription-OAuth Anthropic request carries. The access
/// token is redacted in `Debug` — log the struct freely, never the token.
pub struct ClaudeRequestHeaders {
    access_token: String,
    pub anthropic_version: String,
    pub oauth_beta: String,
    pub user_agent: String,
}

impl ClaudeRequestHeaders {
    pub fn new(access_token: impl Into<String>, config: &ClaudeAuthConfig) -> Self {
        Self {
            access_token: access_token.into(),
            anthropic_version: config.anthropic_version.clone(),
            oauth_beta: config.oauth_beta.clone(),
            user_agent: config.user_agent(),
        }
    }

    /// Header lines in `Name: value` form, ready for the curl `-K -` stdin
    /// convention used by `usage.rs` (never argv — no process-table leak).
    pub(crate) fn header_lines(&self) -> Vec<String> {
        vec![
            format!("Authorization: Bearer {}", self.access_token),
            format!("anthropic-version: {}", self.anthropic_version),
            format!("anthropic-beta: {}", self.oauth_beta),
            "Accept: application/json".to_owned(),
            format!("User-Agent: {}", self.user_agent),
        ]
    }

    /// Renders [`Self::header_lines`] as a curl `-K -` stdin config body.
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

impl std::fmt::Debug for ClaudeRequestHeaders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeRequestHeaders")
            .field("access_token", &"<redacted>")
            .field("anthropic_version", &self.anthropic_version)
            .field("oauth_beta", &self.oauth_beta)
            .field("user_agent", &self.user_agent)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tokens + profile (mirror `claudeAiOauth` normalization + profile reads)
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
    expires_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scopes: Option<Vec<String>>,
    /// Public profile resolved from `/api/oauth/profile` at exchange time.
    /// Safe for UI: identity claims only, no bearer material.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user: Option<ClaudeUser>,
}

impl TokenSet {
    /// Normalizes a token-endpoint payload. `previous_refresh` is kept when
    /// the response rotates no new refresh token; `now_ms` stamps `expires_in`.
    /// Accepts both `expires_in` (seconds) and `expires_at` (seconds or
    /// milliseconds — magnitudes at or below `1e11` read as seconds).
    pub fn from_token_response(
        raw: &Value,
        previous_refresh: Option<&str>,
        now_ms: u64,
    ) -> Result<Self, ClaudeError> {
        let access_token = raw
            .get("access_token")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                ClaudeError::new(
                    ClaudeErrorCode::TokenExchangeFailed,
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
        let expires_at_ms = raw
            .get("expires_in")
            .and_then(Value::as_u64)
            .map(|expires_in| now_ms.saturating_add(expires_in.saturating_mul(1000)))
            .or_else(|| {
                raw.get("expires_at")
                    .and_then(Value::as_u64)
                    .map(|expires_at| {
                        if expires_at <= 100_000_000_000 {
                            expires_at.saturating_mul(1000)
                        } else {
                            expires_at
                        }
                    })
            });
        let scopes = raw
            .get("scope")
            .and_then(Value::as_str)
            .map(|scope| {
                scope
                    .split_whitespace()
                    .filter(|part| !part.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .filter(|scopes| !scopes.is_empty());
        Ok(Self {
            access_token,
            refresh_token,
            expires_at_ms,
            scopes,
            user: None,
        })
    }

    pub(crate) fn access_token(&self) -> &str {
        &self.access_token
    }

    pub(crate) fn refresh_token(&self) -> Option<&str> {
        self.refresh_token.as_deref()
    }

    pub fn expires_at_ms(&self) -> Option<u64> {
        self.expires_at_ms
    }

    pub fn user(&self) -> Option<&ClaudeUser> {
        self.user.as_ref()
    }

    pub(crate) fn set_user(&mut self, user: Option<ClaudeUser>) {
        self.user = user;
    }

    /// `true` when the access token is missing, expired, or within the
    /// refresh margin.
    pub fn is_expired(&self, now_ms: u64) -> bool {
        if self.access_token.is_empty() {
            return true;
        }
        match self.expires_at_ms {
            Some(expires_at) => expires_at <= now_ms.saturating_add(EXPIRY_MARGIN_MS),
            // No expiry claim: treat as usable until a 401 says otherwise.
            None => false,
        }
    }

    #[cfg(test)]
    pub(crate) fn test_token(access_token: &str) -> Self {
        Self {
            access_token: access_token.to_owned(),
            refresh_token: None,
            expires_at_ms: None,
            scopes: None,
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
            .field("expires_at_ms", &self.expires_at_ms)
            .field("scopes", &self.scopes)
            .field("user", &self.user)
            .finish()
    }
}

/// Public profile derived from `/api/oauth/profile`. Safe for UI/IPC: no
/// bearer material, only identity claims.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClaudeUser {
    pub account_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
}

/// Builds a public profile from the OAuth profile payload. Shapes observed:
/// `{"user": {"email": ...}, "organization": {"uuid"|"id",
/// "organization_type": "claude_max", "rate_limit_tier": ...}}`.
/// `None` when no account id is present — an account-less session is unusable.
pub fn parse_claude_user(profile: &Value) -> Option<ClaudeUser> {
    let organization = profile.get("organization")?;
    let account_id = organization
        .get("uuid")
        .or_else(|| organization.get("id"))
        .or_else(|| organization.get("organization_id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())?;
    let email = profile
        .get("user")
        .and_then(|user| user.get("email"))
        .or_else(|| profile.get("email"))
        .and_then(Value::as_str)
        .filter(|email| !email.is_empty())
        .map(str::to_owned);
    let tier = organization.get("rate_limit_tier").and_then(Value::as_str);
    let subscription = organization
        .get("organization_type")
        .and_then(Value::as_str)
        .and_then(|organization_type| organization_type.strip_prefix("claude_"));
    Some(ClaudeUser {
        account_id: account_id.to_owned(),
        email,
        plan: plan_label(subscription, tier),
    })
}

/// "Max (20x)" from the organization type (`claude_max`) and rate-limit tier
/// (`default_claude_max_20x`). Mirrors the `usage.rs` labeling so the panel
/// reads the same with either credential source.
fn plan_label(subscription: Option<&str>, tier: Option<&str>) -> Option<String> {
    let multiple = tier
        .and_then(|tier| tier.rsplit(['_', '-']).next())
        .and_then(|tail| tail.strip_suffix('x'))
        .filter(|multiple| !multiple.is_empty() && multiple.chars().all(|c| c.is_ascii_digit()));
    match (subscription, multiple) {
        (Some(subscription), Some(multiple)) => {
            let mut name = subscription
                .split('_')
                .map(|part| {
                    let mut chars = part.chars();
                    chars.next().map_or_else(String::new, |first| {
                        first.to_uppercase().collect::<String>() + chars.as_str()
                    })
                })
                .collect::<Vec<_>>()
                .join(" ");
            if name.is_empty() {
                name = subscription.to_owned();
            }
            Some(format!("{name} ({multiple}x)"))
        }
        (Some(subscription), None) => Some(subscription.to_owned()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Model discovery (mirror `/v1/models` extraction + curated fallback)
// ---------------------------------------------------------------------------

/// Extracts model ids from the shapes the Anthropic model catalog has used.
/// Unknown entries are ignored; duplicates collapse.
pub fn extract_model_ids(value: &Value) -> Vec<String> {
    fn candidate(item: &Value) -> Option<&str> {
        match item {
            Value::String(id) => Some(id),
            Value::Object(_) => item
                .get("id")
                .or_else(|| item.get("slug"))
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
            if let Some(id) = candidate(item).map(str::trim).filter(|id| !id.is_empty())
                && seen.insert(id.to_owned())
            {
                out.push(id.to_owned());
            }
        }
    }
    out
}

/// One-line structural summary of a JSON value: object key names (sorted),
/// array lengths, scalar type names — never values. Safe for daemon logs.
pub fn describe_json_shape(value: &Value) -> String {
    crate::chatgpt_protocol::describe_json_shape(value)
}

// ---------------------------------------------------------------------------
// Messages request construction (pure)
// ---------------------------------------------------------------------------

/// Builds one `/v1/messages` request body: selected model, the worker's
/// message history, `stream: true`, and `max_tokens`.
///
/// `system` is always an array whose first block is the byte-exact OAuth
/// identity prefix ([`ClaudeAuthConfig::system_prefix`]); `extra_system`
/// (memory context) rides as a later block so the model still sees it while
/// the backend still sees the identity first. The driver sends no tools, so
/// no `tool_choice` is set.
///
/// Returns the resolved model and the JSON text.
pub fn messages_request_body(
    config: &ClaudeAuthConfig,
    model: &str,
    messages: Vec<Value>,
    extra_system: Option<String>,
    max_tokens: u32,
) -> Result<(String, String), ClaudeError> {
    let model = model.trim();
    if model.is_empty() {
        return Err(ClaudeError::new(
            ClaudeErrorCode::InvalidRequest,
            "`model` must be a non-empty string",
        ));
    }
    let mut system = vec![serde_json::json!({
        "type": "text",
        "text": config.system_prefix,
    })];
    if let Some(extra) = extra_system.filter(|extra| !extra.trim().is_empty()) {
        system.push(serde_json::json!({
            "type": "text",
            "text": extra,
        }));
    }
    let body = serde_json::json!({
        "model": model,
        "messages": messages,
        "system": system,
        "stream": true,
        "max_tokens": max_tokens.max(1),
    });
    serde_json::to_string(&body)
        .map(|json| (model.to_owned(), json))
        .map_err(|_| {
            ClaudeError::new(
                ClaudeErrorCode::InvalidRequest,
                "Claude request could not be encoded",
            )
        })
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
// Messages SSE parsing (standard Anthropic streaming vocabulary)
// ---------------------------------------------------------------------------

/// Stream events the Claude driver renders. `content_block_start` /
/// `content_block_stop` / `ping` carry no driver signal and stay ignored;
/// `message_start` contributes the input-side usage, `message_delta` the stop
/// reason plus the output-side usage, all attached to the `Completed` emitted
/// at `message_stop`. Anything else — including `error` frames and malformed
/// JSON — settles the turn without crashing the stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MessagesStreamEvent {
    TextDelta(String),
    ReasoningDelta(String),
    Completed {
        stop_reason: Option<String>,
        usage: TokenTotals,
    },
    Failed,
}

/// Incremental SSE frame parser. Feed network chunks to [`Self::push`] as
/// they arrive and forward the returned events immediately — the full stream
/// is never buffered. Tolerant of `\n` / `\r\n`, split frames across chunks,
/// several frames per chunk, `:` keepalive comments, and `event:` lines
/// (dispatch reads the JSON `type` only).
#[derive(Debug, Default)]
pub struct MessagesSseParser {
    buffer: String,
    pending_stop_reason: Option<String>,
    pending_usage: TokenTotals,
}

/// Reads a token count the way the transcript scanner does: finite positive
/// numbers truncate, everything else (missing, negative, non-numeric) is zero.
fn usage_int(value: Option<&Value>) -> u64 {
    value
        .and_then(Value::as_u64)
        .or_else(|| {
            value
                .and_then(Value::as_f64)
                .filter(|value| value.is_finite() && *value > 0.0)
                .map(|value| value.trunc() as u64)
        })
        .unwrap_or(0)
}

impl MessagesSseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses complete frames out of `chunk` plus any carry-over, keeping a
    /// partial tail buffered for the next call.
    pub fn push(&mut self, chunk: &str) -> Vec<MessagesStreamEvent> {
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
            events.extend(self.frame_events(&frame));
        }
        events
    }

    /// Dispatches one complete SSE frame (no trailing blank line) into
    /// stream events. Multiple `data:` lines join with `\n`, mirroring
    /// `EventSource`.
    fn frame_events(&mut self, frame: &str) -> Vec<MessagesStreamEvent> {
        let mut data_lines = Vec::new();
        for line in frame.lines() {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.starts_with(':') || line.is_empty() || line.starts_with("event:") {
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
        self.stream_event(&value).into_iter().collect()
    }

    fn stream_event(&mut self, value: &Value) -> Option<MessagesStreamEvent> {
        match value.get("type").and_then(Value::as_str) {
            Some("content_block_delta") => {
                let delta = value.get("delta")?;
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => delta
                        .get("text")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                        .map(|text| MessagesStreamEvent::TextDelta(text.to_owned())),
                    Some("thinking_delta") => delta
                        .get("thinking")
                        .and_then(Value::as_str)
                        .filter(|thinking| !thinking.is_empty())
                        .map(|thinking| MessagesStreamEvent::ReasoningDelta(thinking.to_owned())),
                    // Signatures and partial JSON need no driver signal.
                    _ => None,
                }
            }
            Some("message_start") => {
                // The turn's input-side usage. One turn carries one message,
                // so the latest frame wins rather than accumulating.
                if let Some(usage) = value
                    .get("message")
                    .and_then(|message| message.get("usage"))
                {
                    self.pending_usage.uncached_input = usage_int(usage.get("input_tokens"));
                    self.pending_usage.cached_input =
                        usage_int(usage.get("cache_read_input_tokens"));
                    self.pending_usage.cache_creation =
                        usage_int(usage.get("cache_creation_input_tokens"));
                }
                None
            }
            Some("message_delta") => {
                if let Some(reason) = value
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(Value::as_str)
                    .filter(|reason| !reason.is_empty())
                {
                    self.pending_stop_reason = Some(reason.to_owned());
                }
                // The message's total output, not a delta — latest wins.
                if let Some(usage) = value.get("usage") {
                    self.pending_usage.output = usage_int(usage.get("output_tokens"));
                }
                None
            }
            Some("message_stop") => Some(MessagesStreamEvent::Completed {
                stop_reason: self.pending_stop_reason.take(),
                usage: std::mem::take(&mut self.pending_usage),
            }),
            Some("error") => Some(MessagesStreamEvent::Failed),
            // content_block_start/stop, ping, and unknown types carry
            // nothing the driver renders.
            _ => None,
        }
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

// ---------------------------------------------------------------------------
// Tests (pure; no network, no real credentials)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pkce_challenge_matches_rfc7636_vector() {
        // RFC 7636 Appendix B test vector (verified against OpenSSL and
        // Python hashlib — the challenge ends in "-cM").
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn generated_pkce_is_well_formed() {
        let pkce = generate_pkce().expect("generation succeeds");
        assert_eq!(pkce.verifier.len(), 43);
        assert_eq!(pkce.challenge.len(), 43);
        assert_eq!(pkce.state.len(), 64);
        assert_eq!(pkce_challenge(&pkce.verifier), pkce.challenge);
    }

    #[test]
    fn authorize_url_carries_copy_paste_flow_params() {
        let config = ClaudeAuthConfig::default();
        let pkce = PkceMaterial {
            verifier: "verifier".to_owned(),
            challenge: "challenge".to_owned(),
            state: "state".to_owned(),
        };
        let url = build_authorize_url(&config, &pkce);
        assert!(url.starts_with("https://claude.ai/oauth/authorize?"));
        assert!(url.contains("code=true"));
        assert!(url.contains("client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains(
            "redirect_uri=https%3A%2F%2Fconsole.anthropic.com%2Foauth%2Fcode%2Fcallback"
        ));
        assert!(url.contains("code_challenge=challenge"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=state"));
    }

    #[test]
    fn authorization_input_parses_code_and_state_shapes() {
        let parsed = parse_authorization_input("  abc123  ").unwrap();
        assert_eq!(parsed.code, "abc123");
        assert_eq!(parsed.state, None);

        let parsed = parse_authorization_input("abc123#state456").unwrap();
        assert_eq!(parsed.code, "abc123");
        assert_eq!(parsed.state.as_deref(), Some("state456"));

        assert!(parse_authorization_input("   ").is_err());
        assert!(parse_authorization_input("").is_err());
    }

    #[test]
    fn token_response_normalizes_expiry_and_keeps_previous_refresh() {
        let raw = json!({
            "access_token": "sk-ant-oat01-abc",
            "expires_in": 3600,
            "scope": "org:create_api_key user:profile user:inference",
        });
        let tokens = TokenSet::from_token_response(&raw, Some("old-refresh"), 1_000).unwrap();
        assert_eq!(tokens.expires_at_ms(), Some(1_000 + 3_600_000));
        assert_eq!(tokens.refresh_token(), Some("old-refresh"));
        assert!(!tokens.is_expired(1_000));
        assert!(tokens.is_expired(1_000 + 3_600_000));

        let rotated = json!({
            "access_token": "sk-ant-oat01-def",
            "refresh_token": "new-refresh",
            "expires_in": 3600,
        });
        let tokens = TokenSet::from_token_response(&rotated, Some("old-refresh"), 0).unwrap();
        assert_eq!(tokens.refresh_token(), Some("new-refresh"));
    }

    #[test]
    fn token_response_accepts_millis_expiry_and_rejects_missing_access() {
        let raw = json!({"access_token": "x", "expires_at": 1_700_000_000_000_u64});
        let tokens = TokenSet::from_token_response(&raw, None, 0).unwrap();
        assert_eq!(tokens.expires_at_ms(), Some(1_700_000_000_000));

        let raw = json!({"refresh_token": "y"});
        assert!(TokenSet::from_token_response(&raw, None, 0).is_err());
    }

    #[test]
    fn error_codes_extract_from_both_body_shapes() {
        assert_eq!(
            extract_error_code(r#"{"error":"invalid_grant"}"#).as_deref(),
            Some("invalid_grant")
        );
        assert_eq!(
            extract_error_code(r#"{"error":{"type":"invalid_grant"}}"#).as_deref(),
            Some("invalid_grant")
        );
        assert!(is_dead_refresh_error("invalid_grant"));
        assert!(!is_dead_refresh_error("authorization_pending"));
    }

    #[test]
    fn profile_parses_account_email_and_plan() {
        let profile = json!({
            "user": {"email": "dev@example.com"},
            "organization": {
                "uuid": "org-1",
                "organization_type": "claude_max",
                "rate_limit_tier": "default_claude_max_20x",
            },
        });
        let user = parse_claude_user(&profile).expect("profile parses");
        assert_eq!(user.account_id, "org-1");
        assert_eq!(user.email.as_deref(), Some("dev@example.com"));
        assert_eq!(user.plan.as_deref(), Some("Max (20x)"));

        assert!(parse_claude_user(&json!({"user": {}})).is_none());
    }

    #[test]
    fn model_ids_extract_and_dedupe() {
        let value = json!({"data": [{"id": "a"}, {"id": "a"}, {"id": "b"}]});
        assert_eq!(extract_model_ids(&value), vec!["a", "b"]);
        assert!(extract_model_ids(&json!({"unexpected": true})).is_empty());
    }

    #[test]
    fn debug_redacts_bearer_material() {
        let mut tokens = TokenSet::test_token("secret-access");
        tokens.refresh_token = Some("secret-refresh".to_owned());
        let text = format!("{tokens:?}");
        assert!(!text.contains("secret-access"));
        assert!(!text.contains("secret-refresh"));

        let headers = ClaudeRequestHeaders::new("secret-access", &ClaudeAuthConfig::default());
        let text = format!("{headers:?} {}", headers.curl_header_config());
        // The curl config necessarily carries the token for stdin transport;
        // the Debug struct itself must not.
        assert!(format!("{headers:?}").contains("<redacted>"));
        assert!(!format!("{headers:?}").contains("secret-access"));
        let _ = text;
    }

    #[test]
    fn exchange_and_refresh_bodies_carry_pkce_grants() {
        let config = ClaudeAuthConfig::default();
        let body: Value =
            serde_json::from_str(&exchange_body(&config, "code", Some("state"), "verifier"))
                .unwrap();
        assert_eq!(body["grant_type"], "authorization_code");
        assert_eq!(body["code_verifier"], "verifier");

        let body: Value = serde_json::from_str(&refresh_body(&config, "refresh")).unwrap();
        assert_eq!(body["grant_type"], "refresh_token");
    }

    #[test]
    fn messages_body_carries_identity_first_and_streams() {
        let config = ClaudeAuthConfig::default();
        let messages = vec![serde_json::json!({"role": "user", "content": "hi"})];
        let (model, body) = messages_request_body(
            &config,
            "claude-sonnet-4-5",
            messages,
            Some("Remember this".to_owned()),
            500,
        )
        .unwrap();
        assert_eq!(model, "claude-sonnet-4-5");
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["model"], "claude-sonnet-4-5");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 500);
        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 2);
        assert_eq!(
            system[0]["text"],
            "You are Claude Code, Anthropic's official CLI for Claude."
        );
        assert_eq!(system[1]["text"], "Remember this");

        // Empty model and empty extra degrade honestly.
        assert!(messages_request_body(&config, "  ", Vec::new(), None, 10).is_err());
        let (_, body) =
            messages_request_body(&config, "m", Vec::new(), Some("  ".to_owned()), 0).unwrap();
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["system"].as_array().unwrap().len(), 1);
        assert_eq!(body["max_tokens"], 1);
    }

    #[test]
    fn retry_after_parses_delta_seconds_only() {
        let block = "HTTP/1.1 429 Too Many Requests\r\nretry-after: 7\r\n\r\n";
        assert_eq!(parse_retry_after_secs(block), Some(7));
        assert_eq!(parse_retry_after_secs("HTTP/1.1 200 OK\n\n"), None);
        assert_eq!(parse_retry_after_secs("retry-after: 0"), None);
        assert_eq!(parse_retry_after_secs("retry-after: 9999"), Some(300));
        assert_eq!(parse_retry_after_secs("retry-after: soon"), None);
    }

    #[test]
    fn messages_parser_streams_deltas_and_stop_reason() {
        let mut parser = MessagesSseParser::new();
        // Split frames across chunks must still parse.
        let events = parser.push("event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel");
        assert!(events.is_empty());
        let events = parser.push("lo\"}}\n\nevent: message_delta\n");
        assert_eq!(
            events,
            vec![MessagesStreamEvent::TextDelta("Hello".to_owned())]
        );
        let events = parser.push(
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
        );
        assert!(events.is_empty());
        let events = parser.push("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
        assert_eq!(
            events,
            vec![MessagesStreamEvent::Completed {
                stop_reason: Some("end_turn".to_owned()),
                usage: TokenTotals::default(),
            }]
        );
    }

    #[test]
    fn messages_parser_maps_thinking_and_ignores_noise() {
        let mut parser = MessagesSseParser::new();
        let chunk = concat!(
            "event: ping\ndata: {\"type\":\"ping\"}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hmm\"}}\n\n",
            "event: content_block_delta\ndata: not-json\n\n",
            ": keepalive\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        assert_eq!(
            parser.push(chunk),
            vec![
                MessagesStreamEvent::ReasoningDelta("hmm".to_owned()),
                MessagesStreamEvent::Completed {
                    stop_reason: None,
                    usage: TokenTotals::default(),
                },
            ]
        );
    }

    #[test]
    fn messages_parser_marks_error_frames_failed() {
        let mut parser = MessagesSseParser::new();
        let events = parser.push("event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}\n\n");
        assert_eq!(events, vec![MessagesStreamEvent::Failed]);
    }

    #[test]
    fn messages_parser_collects_input_and_output_usage() {
        let mut parser = MessagesSseParser::new();
        let events = parser.push(concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{",
            "\"input_tokens\":1000,\"cache_read_input_tokens\":800,",
            "\"cache_creation_input_tokens\":50}}}\n\n",
        ));
        assert!(events.is_empty());
        let events = parser.push(concat!(
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},",
            "\"usage\":{\"output_tokens\":200}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ));
        assert_eq!(
            events,
            vec![MessagesStreamEvent::Completed {
                stop_reason: Some("end_turn".to_owned()),
                usage: TokenTotals {
                    uncached_input: 1000,
                    cached_input: 800,
                    cache_creation: 50,
                    output: 200,
                    reasoning: 0,
                },
            }]
        );
    }

    #[test]
    fn messages_parser_usage_defaults_to_zero_without_usage_frames() {
        let mut parser = MessagesSseParser::new();
        let events = parser.push("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
        assert_eq!(
            events,
            vec![MessagesStreamEvent::Completed {
                stop_reason: None,
                usage: TokenTotals::default(),
            }]
        );
    }
}
