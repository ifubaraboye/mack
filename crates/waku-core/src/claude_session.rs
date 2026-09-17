//! Daemon-owned Claude subscription session manager.
//!
//! Owns the PKCE copy-paste state machine — `unauthenticated`, `pending`,
//! `authenticated`, `expired` — plus token refresh and encrypted credential
//! storage. Mirrors [`crate::chatgpt_session`] but for the Claude Code OAuth
//! flow, which has no device-code grant: `start_login` renders an
//! `https://claude.ai/oauth/authorize?code=true` URL, the user pastes the
//! `CODE` (or `CODE#STATE`) back, and `complete_login` exchanges it.
//!
//! Architecture rules (same as the ChatGPT manager):
//! - Raw tokens never leave this module except through [`FreshAuth`], whose
//!   access token is `pub(crate)`-gated for the future daemon driver. The
//!   renderer and the daemon WebSocket protocol only ever see
//!   [`PublicSession`] (status + public profile).
//! - All blocking work (subprocesses, network, filesystem) runs on the
//!   caller's thread: invoke from the background executor, never from render.
//! - HTTPS follows the house convention from `usage.rs`: the system `curl`
//!   binary with headers on `-H` argv (non-secret only) and request bodies on
//!   stdin (`--data-binary @-`), so bearer material never appears in argv.
//! - Credentials at rest are AES-256-GCM encrypted. The 32-byte data key
//!   lives in the macOS keychain via the same `/usr/bin/security` pattern
//!   `usage.rs` reads through, or in a `0600` file inside a `0700` directory
//!   elsewhere. There is no plaintext credential file and no raw-token API.
//!   The store directory (`~/.waku/claude`) is isolated from the Claude Code
//!   CLI's own `~/.claude/.credentials.json`: Waku never reads or writes the
//!   CLI file, so the two sign-ins stay independent.
//! - Exactly one refresh may be in flight per manager (refresh-token rotation
//!   would otherwise invalidate a concurrent refresh and force a re-login).
//!
//! No real credentials appear anywhere here or in tests: every test drives a
//! mock transport and an in-memory store.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use aes_gcm::{Aes256Gcm, Key, KeyInit as _, Nonce, aead::Aead as _};
use base64::Engine as _;
use parking_lot::Mutex;
use rand::TryRngCore as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::claude_protocol::{
    ClaudeAuthConfig, ClaudeError, ClaudeErrorCode, ClaudeRequestHeaders, ClaudeUser,
    FALLBACK_MODEL_SLUGS, PENDING_LOGIN_TTL_MS, TokenSet, build_authorize_url, exchange_body,
    extract_error_code, extract_model_ids, generate_pkce, is_dead_refresh_error,
    parse_authorization_input, parse_claude_user, refresh_body,
};

/// Absolute path keeps a shadowed `curl` on `PATH` out of the credential
/// exchange, matching `usage.rs`. Windows 10 build 17063+ ships the same
/// tool in System32.
#[cfg(not(windows))]
pub(crate) const CURL_PATH: &str = "/usr/bin/curl";
#[cfg(windows)]
pub(crate) const CURL_PATH: &str = r"C:\Windows\System32\curl.exe";

const CURL_TIMEOUT_SECS: &str = "20";
/// macOS keychain identity for the session data-encryption key.
#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "Waku Claude";
#[cfg(target_os = "macos")]
const KEYCHAIN_ACCOUNT: &str = "session-encryption-key";
const SESSION_FILE_NAME: &str = "session.json.enc";
const KEY_FILE_NAME: &str = "session.key";
const ENVELOPE_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Clock (injectable for tests)
// ---------------------------------------------------------------------------

/// Millisecond clock. [`SystemClock`] for production, [`ManualClock`] for tests.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

/// Wall-clock time.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64)
            .unwrap_or(0)
    }
}

/// Manually advanced clock for deterministic tests.
pub struct ManualClock {
    now_ms: AtomicU64,
}

impl ManualClock {
    pub fn new(now_ms: u64) -> Self {
        Self {
            now_ms: AtomicU64::new(now_ms),
        }
    }

    pub fn set(&self, now_ms: u64) {
        self.now_ms.store(now_ms, Ordering::SeqCst);
    }

    pub fn advance(&self, delta_ms: u64) {
        self.now_ms.fetch_add(delta_ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.now_ms.load(Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------------------
// Login status + public session (the only shape UI/IPC may see)
// ---------------------------------------------------------------------------

/// Daemon-side login state. Serialized camelCase to match the ChatGPT
/// manager's vocabulary.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LoginStatus {
    #[default]
    Unauthenticated,
    Pending,
    Authenticated,
    Expired,
}

/// UI/IPC-safe session view: status plus public profile. No bearer material.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicSession {
    pub status: LoginStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<ClaudeUser>,
}

// ---------------------------------------------------------------------------
// Pending login display (the only shape the login UI may see)
// ---------------------------------------------------------------------------

/// Display material for an in-progress PKCE login: the authorize URL to open
/// plus the state half for reference. The verifier never leaves the daemon.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingLogin {
    pub authorize_url: String,
    pub state: String,
    pub expires_at_ms: u64,
}

// ---------------------------------------------------------------------------
// FreshAuth: the single sanctioned carrier of an access token
// ---------------------------------------------------------------------------

/// Fresh credentials for one API call. The access token is `pub(crate)` so
/// only the daemon driver (a later stage, same crate) can render it into an
/// `Authorization` header. There is no path from this type to the renderer
/// or the WebSocket protocol.
pub struct FreshAuth {
    access_token: String,
}

impl FreshAuth {
    /// Stage 2 daemon driver renders this into the `Authorization` header.
    /// `pub(crate)` so no UI, IPC, or external crate can reach bearer material.
    #[allow(dead_code)]
    pub(crate) fn access_token(&self) -> &str {
        &self.access_token
    }
}

impl std::fmt::Debug for FreshAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FreshAuth")
            .field("access_token", &"<redacted>")
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Persisted session
// ---------------------------------------------------------------------------

/// Pending PKCE material as persisted between start and code completion.
/// The verifier is as secret as a password until exchanged — it lives only
/// in the encrypted store and redacts in `Debug`.
#[derive(Clone, Deserialize, Serialize)]
struct PersistedPending {
    verifier: String,
    state: String,
    authorize_url: String,
    expires_at_ms: u64,
}

impl std::fmt::Debug for PersistedPending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PersistedPending")
            .field("verifier", &"<redacted>")
            .field("state", &self.state)
            .field("authorize_url", &self.authorize_url)
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

/// The stored session envelope. `Debug` redacts via the field types.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct StoredSession {
    status: LoginStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending: Option<PersistedPending>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tokens: Option<TokenSet>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user: Option<ClaudeUser>,
    #[serde(default)]
    created_at_ms: u64,
    #[serde(default)]
    updated_at_ms: u64,
}

// ---------------------------------------------------------------------------
// CredentialStore
// ---------------------------------------------------------------------------

/// Persistence for one desktop user's Claude session. Production uses
/// [`EncryptedFileStore`]; tests use [`MemoryStore`].
pub trait CredentialStore: Send + Sync {
    fn load(&self) -> anyhow::Result<Option<StoredSessionEnvelope>>;
    fn save(&self, session: &StoredSessionEnvelope) -> anyhow::Result<()>;
    fn delete(&self) -> anyhow::Result<()>;
}

/// The persistable session shape. Re-exported field-light: UI layers use
/// [`PublicSession`] instead; only the manager reads/writes this.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct StoredSessionEnvelope {
    pub status: LoginStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<ClaudeUser>,
    #[serde(skip)]
    persisted: Option<StoredSession>,
}

impl StoredSessionEnvelope {
    fn stored(&self) -> Option<&StoredSession> {
        self.persisted.as_ref()
    }
}

/// In-memory store for tests. Never touches disk or keychain.
#[derive(Clone, Default)]
pub struct MemoryStore {
    inner: Arc<Mutex<Option<StoredSession>>>,
}

impl CredentialStore for MemoryStore {
    fn load(&self) -> anyhow::Result<Option<StoredSessionEnvelope>> {
        Ok(self
            .inner
            .lock()
            .clone()
            .map(|stored| StoredSessionEnvelope {
                status: stored.status,
                user: stored.user.clone(),
                persisted: Some(stored),
            }))
    }

    fn save(&self, session: &StoredSessionEnvelope) -> anyhow::Result<()> {
        let Some(stored) = session.stored() else {
            anyhow::bail!("cannot save a session envelope without stored state");
        };
        *self.inner.lock() = Some(stored.clone());
        Ok(())
    }

    fn delete(&self) -> anyhow::Result<()> {
        *self.inner.lock() = None;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// EncryptedFileStore (production)
// ---------------------------------------------------------------------------

/// AES-256-GCM encrypted session file. Layout under `dir`:
/// - `session.json.enc`: `{v, nonce, ciphertext}` (base64), `0600`, atomic
///   write-then-rename so a crash mid-write cannot leave a torn file;
/// - key: macOS keychain (`security`, same pattern as `usage.rs`), otherwise
///   `session.key` (`0600`, base64) — the directory itself is `0700` via
///   `create_private_dir_all`, and on Windows lives under the user's profile
///   with its inherited owner-only ACL.
///
/// Known limitation: the macOS keychain write passes the random data key on
/// argv (`security add-generic-password -w`), briefly visible to same-user
/// process listings. Tokens themselves never touch argv anywhere.
pub struct EncryptedFileStore {
    dir: PathBuf,
}

impl EncryptedFileStore {
    /// `~/.waku/claude` (or the platform temp dir when no home exists).
    /// Isolated from the Claude Code CLI's `~/.claude` directory by design.
    pub fn default_dir() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(".waku")
            .join("claude")
    }

    pub fn open(dir: PathBuf) -> std::io::Result<Self> {
        crate::fs_ext::create_private_dir_all(&dir)?;
        Ok(Self { dir })
    }

    fn session_path(&self) -> PathBuf {
        self.dir.join(SESSION_FILE_NAME)
    }

    fn key_path(&self) -> PathBuf {
        self.dir.join(KEY_FILE_NAME)
    }

    fn cipher(&self) -> anyhow::Result<Aes256Gcm> {
        let key_bytes = load_or_create_data_key(&self.key_path())?;
        Ok(Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_bytes)))
    }
}

impl CredentialStore for EncryptedFileStore {
    fn load(&self) -> anyhow::Result<Option<StoredSessionEnvelope>> {
        let bytes = match std::fs::read(self.session_path()) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        // A torn, corrupt, version-skewed, or undecryptable file self-heals
        // to signed out: credentials that cannot be read are useless, and
        // erroring forever would strand the user with no path back to login.
        let stored = match self.decrypt_session(&bytes) {
            Ok(stored) => stored,
            Err(_) => {
                let _ = std::fs::remove_file(self.session_path());
                return Ok(None);
            }
        };
        Ok(Some(StoredSessionEnvelope {
            status: stored.status,
            user: stored.user.clone(),
            persisted: Some(stored),
        }))
    }

    fn save(&self, session: &StoredSessionEnvelope) -> anyhow::Result<()> {
        let Some(stored) = session.stored() else {
            anyhow::bail!("cannot save a session envelope without stored state");
        };
        let plaintext = serde_json::to_vec(stored)?;
        let mut nonce_bytes = [0u8; 12];
        rand::rngs::OsRng
            .try_fill_bytes(&mut nonce_bytes)
            .map_err(|_| anyhow::anyhow!("claude session encryption failed"))?;
        let ciphertext = self
            .cipher()?
            .encrypt(Nonce::from_slice(&nonce_bytes), plaintext.as_slice())
            .map_err(|_| anyhow::anyhow!("claude session encryption failed"))?;
        let envelope = EncryptedEnvelope {
            v: ENVELOPE_VERSION,
            nonce: base64::engine::general_purpose::STANDARD.encode(nonce_bytes),
            ciphertext: base64::engine::general_purpose::STANDARD.encode(ciphertext),
        };
        let data = serde_json::to_vec(&envelope)?;
        write_private_file(&self.session_path(), &data)?;
        Ok(())
    }

    fn delete(&self) -> anyhow::Result<()> {
        match std::fs::remove_file(self.session_path()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[derive(Deserialize, Serialize)]
struct EncryptedEnvelope {
    v: u32,
    nonce: String,
    ciphertext: String,
}

impl EncryptedFileStore {
    fn decrypt_session(&self, bytes: &[u8]) -> anyhow::Result<StoredSession> {
        let envelope: EncryptedEnvelope = serde_json::from_slice(bytes)
            .map_err(|_| anyhow::anyhow!("claude session file is corrupt"))?;
        if envelope.v != ENVELOPE_VERSION {
            anyhow::bail!("claude session file has an unknown version");
        }
        let nonce_bytes = base64::engine::general_purpose::STANDARD
            .decode(&envelope.nonce)
            .map_err(|_| anyhow::anyhow!("claude session file is corrupt"))?;
        let ciphertext = base64::engine::general_purpose::STANDARD
            .decode(&envelope.ciphertext)
            .map_err(|_| anyhow::anyhow!("claude session file is corrupt"))?;
        let nonce = Nonce::from_slice(&nonce_bytes);
        let plaintext = self
            .cipher()?
            .decrypt(nonce, ciphertext.as_slice())
            .map_err(|_| anyhow::anyhow!("claude session file failed authentication"))?;
        serde_json::from_slice(&plaintext)
            .map_err(|_| anyhow::anyhow!("claude session file is corrupt"))
    }
}

/// Loads the 32-byte data key, generating and persisting it on first use.
fn load_or_create_data_key(key_path: &Path) -> anyhow::Result<[u8; 32]> {
    #[cfg(target_os = "macos")]
    if let Ok(key) = read_keychain_key() {
        return Ok(key);
    }
    if let Ok(key) = read_key_file(key_path) {
        #[cfg(target_os = "macos")]
        {
            // Prefer the keychain going forward, but keep working if it is
            // unreachable — never strand a signed-in user on key storage.
            let _ = write_keychain_key(&key);
        }
        return Ok(key);
    }
    let mut key = [0u8; 32];
    rand::rngs::OsRng
        .try_fill_bytes(&mut key)
        .map_err(|_| anyhow::anyhow!("could not generate the claude data key"))?;
    #[cfg(target_os = "macos")]
    {
        if write_keychain_key(&key).is_err() {
            write_key_file(key_path, &key)?;
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        write_key_file(key_path, &key)?;
    }
    Ok(key)
}

fn read_key_file(path: &Path) -> anyhow::Result<[u8; 32]> {
    let raw = std::fs::read(path)?;
    let decoded = base64::engine::general_purpose::STANDARD.decode(raw.trim_ascii())?;
    decoded
        .try_into()
        .map_err(|_| anyhow::anyhow!("claude key file has an unexpected length"))
}

fn write_key_file(path: &Path, key: &[u8; 32]) -> anyhow::Result<()> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(key);
    write_private_file(path, encoded.as_bytes())
}

/// Writes `data` atomically (write-then-rename) with owner-only permissions:
/// `0600` on unix, inherited profile ACL on Windows.
fn write_private_file(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true).mode(0o600);
            options.open(&temporary)?.write_all(data)?;
        }
        #[cfg(not(unix))]
        {
            std::fs::write(&temporary, data)?;
        }
    }
    std::fs::rename(&temporary, path)?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn read_keychain_key() -> anyhow::Result<[u8; 32]> {
    let output = std::process::Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            KEYCHAIN_ACCOUNT,
            "-w",
        ])
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        anyhow::bail!("claude keychain item is missing");
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(output.stdout.trim_ascii())
        .map_err(|_| anyhow::anyhow!("claude keychain item is malformed"))?;
    decoded
        .try_into()
        .map_err(|_| anyhow::anyhow!("claude keychain item has an unexpected length"))
}

#[cfg(target_os = "macos")]
fn write_keychain_key(key: &[u8; 32]) -> anyhow::Result<()> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(key);
    let status = std::process::Command::new("/usr/bin/security")
        .args([
            "add-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            KEYCHAIN_ACCOUNT,
            "-w",
            &encoded,
            "-U",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        anyhow::bail!("could not store the claude key in the keychain");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// OAuthTransport (blocking; background executor only)
// ---------------------------------------------------------------------------

/// Blocking PKCE transport. Implementations must never log or persist bearer
/// material; bodies travel on stdin, never argv.
pub trait OAuthTransport: Send + Sync {
    /// Returns the raw token JSON for an authorization-code exchange.
    fn exchange_code(
        &self,
        config: &ClaudeAuthConfig,
        code: &str,
        state: Option<&str>,
        verifier: &str,
    ) -> Result<Value, ClaudeError>;
    /// Returns the raw token JSON for a refresh-token rotation.
    fn refresh(&self, config: &ClaudeAuthConfig, refresh_token: &str)
    -> Result<Value, ClaudeError>;
    /// Returns the raw `/api/oauth/profile` JSON for the authenticated
    /// account. Only public profile metadata ever leaves the daemon.
    fn fetch_profile(
        &self,
        config: &ClaudeAuthConfig,
        access_token: &str,
    ) -> Result<Value, ClaudeError>;
    /// Returns the raw `/v1/models` JSON. May reject subscription tokens;
    /// the manager falls back to curated slugs then.
    fn fetch_models(
        &self,
        config: &ClaudeAuthConfig,
        access_token: &str,
    ) -> Result<Value, ClaudeError>;
}

/// Production transport over the system `curl`, following the `usage.rs`
/// convention. Blocking — call only from the background executor.
pub struct CurlOAuthTransport;

impl OAuthTransport for CurlOAuthTransport {
    fn exchange_code(
        &self,
        config: &ClaudeAuthConfig,
        code: &str,
        state: Option<&str>,
        verifier: &str,
    ) -> Result<Value, ClaudeError> {
        let body = exchange_body(config, code, state, verifier);
        let (status, text) = curl_post_json(&config.token_url, &body)?;
        if !(200..300).contains(&status) {
            return Err(ClaudeError::new(
                ClaudeErrorCode::TokenExchangeFailed,
                format!("Authorization code exchange failed ({status})"),
            )
            .with_status(status));
        }
        serde_json::from_str(&text).map_err(|_| {
            ClaudeError::new(
                ClaudeErrorCode::TokenExchangeFailed,
                "Token response was not valid JSON",
            )
            .with_status(status)
        })
    }

    fn refresh(
        &self,
        config: &ClaudeAuthConfig,
        refresh_token: &str,
    ) -> Result<Value, ClaudeError> {
        let body = refresh_body(config, refresh_token);
        let (status, text) = curl_post_json(&config.token_url, &body)?;
        if !(200..300).contains(&status) {
            if let Some(code) = extract_error_code(&text)
                && is_dead_refresh_error(&code)
            {
                return Err(ClaudeError::new(
                    ClaudeErrorCode::RefreshTokenInvalid,
                    "Refresh token is no longer valid. The user must sign in again",
                )
                .with_status(status));
            }
            return Err(ClaudeError::new(
                ClaudeErrorCode::TokenRefreshFailed,
                format!("Token refresh failed ({status})"),
            )
            .with_status(status));
        }
        serde_json::from_str(&text).map_err(|_| {
            ClaudeError::new(
                ClaudeErrorCode::TokenRefreshFailed,
                "Token response was not valid JSON",
            )
            .with_status(status)
        })
    }

    fn fetch_profile(
        &self,
        config: &ClaudeAuthConfig,
        access_token: &str,
    ) -> Result<Value, ClaudeError> {
        let headers = ClaudeRequestHeaders::new(access_token, config);
        let (status, text) = curl_get(&config.profile_url, &headers.curl_header_config())?;
        if !(200..300).contains(&status) {
            return Err(ClaudeError::new(
                ClaudeErrorCode::ProfileRequestFailed,
                format!("Profile request failed ({status})"),
            )
            .with_status(status));
        }
        serde_json::from_str(&text).map_err(|_| {
            ClaudeError::new(
                ClaudeErrorCode::ProfileRequestFailed,
                "Profile response was not valid JSON",
            )
            .with_status(status)
        })
    }

    fn fetch_models(
        &self,
        config: &ClaudeAuthConfig,
        access_token: &str,
    ) -> Result<Value, ClaudeError> {
        let headers = ClaudeRequestHeaders::new(access_token, config);
        let (status, text) = curl_get(&config.models_url, &headers.curl_header_config())?;
        if !(200..300).contains(&status) {
            return Err(ClaudeError::new(
                ClaudeErrorCode::ModelsRequestFailed,
                format!("Model list request failed ({status})"),
            )
            .with_status(status));
        }
        let value: Value = serde_json::from_str(&text).map_err(|_| {
            ClaudeError::new(
                ClaudeErrorCode::ModelsRequestFailed,
                "Model list response was not valid JSON",
            )
            .with_status(status)
        })?;
        // Shape only — never values.
        eprintln!(
            "claude models response: status={status} shape={} ids={}",
            crate::claude_protocol::describe_json_shape(&value),
            crate::claude_protocol::extract_model_ids(&value).len(),
        );
        Ok(value)
    }
}

/// POSTs a JSON body read from stdin (`--data-binary @-`). Only non-secret
/// `-H` flags touch argv; the body (which may carry codes, verifiers, or
/// refresh tokens) travels on stdin like the `usage.rs` headers do.
fn curl_post_json(url: &str, body: &str) -> Result<(u16, String), ClaudeError> {
    let mut child = crate::command_env::plain_command(CURL_PATH)
        .args([
            "-sS",
            "--max-time",
            CURL_TIMEOUT_SECS,
            "-D",
            "-",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-H",
            "Accept: application/json",
            "--data-binary",
            "@-",
            url,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| {
            ClaudeError::new(
                ClaudeErrorCode::NetworkError,
                "Could not start the Claude request",
            )
        })?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| {
            ClaudeError::new(
                ClaudeErrorCode::NetworkError,
                "Claude request stdin is unavailable",
            )
        })?
        .write_all(body.as_bytes())
        .map_err(|_| {
            ClaudeError::new(
                ClaudeErrorCode::NetworkError,
                "Failed to send the Claude request",
            )
        })?;
    let output = child.wait_with_output().map_err(|_| {
        ClaudeError::new(
            ClaudeErrorCode::NetworkError,
            "Claude request did not finish",
        )
    })?;
    if !output.status.success() {
        return Err(ClaudeError::new(
            ClaudeErrorCode::NetworkError,
            "Failed to reach the Claude service",
        ));
    }
    split_status_and_body(&String::from_utf8_lossy(&output.stdout))
}

/// GETs `url` with headers supplied as a curl `-K -` stdin config (the
/// `usage.rs` convention: bearer material travels on stdin, never argv).
fn curl_get(url: &str, curl_config: &str) -> Result<(u16, String), ClaudeError> {
    let mut child = crate::command_env::plain_command(CURL_PATH)
        .args([
            "-sS",
            "--max-time",
            CURL_TIMEOUT_SECS,
            "-D",
            "-",
            "-K",
            "-",
            url,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| {
            ClaudeError::new(
                ClaudeErrorCode::NetworkError,
                "Could not start the Claude request",
            )
        })?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| {
            ClaudeError::new(
                ClaudeErrorCode::NetworkError,
                "Claude request stdin is unavailable",
            )
        })?
        .write_all(curl_config.as_bytes())
        .map_err(|_| {
            ClaudeError::new(
                ClaudeErrorCode::NetworkError,
                "Failed to send the Claude request",
            )
        })?;
    let output = child.wait_with_output().map_err(|_| {
        ClaudeError::new(
            ClaudeErrorCode::NetworkError,
            "Claude request did not finish",
        )
    })?;
    if !output.status.success() {
        return Err(ClaudeError::new(
            ClaudeErrorCode::NetworkError,
            "Failed to reach the Claude service",
        ));
    }
    split_status_and_body(&String::from_utf8_lossy(&output.stdout))
}

/// `-D -` prefixes the body with the response headers; the status code is on
/// the first line and the body follows the blank separator line.
fn split_status_and_body(raw: &str) -> Result<(u16, String), ClaudeError> {
    let status = raw
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| {
            ClaudeError::new(
                ClaudeErrorCode::NetworkError,
                "Claude response carried no status",
            )
        })?;
    let body = raw
        .split_once("\r\n\r\n")
        .or_else(|| raw.split_once("\n\n"))
        .map(|(_, body)| body)
        .unwrap_or_default()
        .to_owned();
    Ok((status, body))
}

// ---------------------------------------------------------------------------
// ClaudeSessionManager
// ---------------------------------------------------------------------------

/// Daemon-owned manager for one desktop user's Claude subscription session.
/// Construct with [`ClaudeSessionManager::open`]; drive from the background
/// executor.
pub struct ClaudeSessionManager {
    config: ClaudeAuthConfig,
    store: Arc<dyn CredentialStore>,
    transport: Arc<dyn OAuthTransport>,
    clock: Arc<dyn Clock>,
    /// Serializes refreshes so token rotation cannot invalidate a concurrent
    /// refresh. Holders always re-check expiry under the lock (double-checked).
    refresh_lock: Mutex<()>,
}

impl ClaudeSessionManager {
    pub fn open(
        store: Arc<dyn CredentialStore>,
        transport: Arc<dyn OAuthTransport>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self::open_with_config(ClaudeAuthConfig::default(), store, transport, clock)
    }

    pub fn open_with_config(
        config: ClaudeAuthConfig,
        store: Arc<dyn CredentialStore>,
        transport: Arc<dyn OAuthTransport>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            config,
            store,
            transport,
            clock,
            refresh_lock: Mutex::new(()),
        }
    }

    /// Endpoint/identity configuration backing this manager. The later
    /// subscription driver reads its URLs from here so protocol constants
    /// stay isolated in one place.
    pub fn config(&self) -> &ClaudeAuthConfig {
        &self.config
    }

    fn storage_error(error: anyhow::Error) -> ClaudeError {
        ClaudeError::new(
            ClaudeErrorCode::StorageError,
            format!("Claude session storage failed: {error:#}"),
        )
    }

    fn load_stored(&self) -> Result<Option<StoredSession>, ClaudeError> {
        Ok(self
            .store
            .load()
            .map_err(Self::storage_error)?
            .and_then(|envelope| envelope.persisted))
    }

    fn persist(&self, stored: &StoredSession) -> Result<(), ClaudeError> {
        self.store
            .save(&StoredSessionEnvelope {
                status: stored.status,
                user: stored.user.clone(),
                persisted: Some(stored.clone()),
            })
            .map_err(Self::storage_error)
    }

    /// Effective status without network: a `pending` login past its expiry
    /// reads as `expired`.
    fn effective_status(stored: Option<&StoredSession>, now_ms: u64) -> LoginStatus {
        match stored {
            None => LoginStatus::Unauthenticated,
            Some(stored) if stored.tokens.is_some() => LoginStatus::Authenticated,
            Some(stored) => match &stored.pending {
                Some(pending) if now_ms < pending.expires_at_ms => LoginStatus::Pending,
                _ => LoginStatus::Expired,
            },
        }
    }

    /// UI-safe view. Pure read plus expiry computation — never touches the
    /// network, safe to call from any thread (but still not from render;
    /// cache the result on the entity per house rules).
    pub fn public_session(&self) -> Result<PublicSession, ClaudeError> {
        let now_ms = self.clock.now_ms();
        let stored = self.load_stored()?;
        Ok(PublicSession {
            status: Self::effective_status(stored.as_ref(), now_ms),
            user: stored.and_then(|stored| stored.user),
        })
    }

    /// Starts (or reuses) a PKCE login. A still-valid pending login is
    /// returned again so repeated clicks do not invalidate the user's
    /// in-progress authorize URL. Returns display material for the UI layer;
    /// the verifier stays encrypted daemon-side.
    pub fn start_login(&self) -> Result<PendingLogin, ClaudeError> {
        let now_ms = self.clock.now_ms();
        if let Some(stored) = self.load_stored()?
            && let Some(pending) = &stored.pending
            && pending.expires_at_ms > now_ms
        {
            return Ok(PendingLogin {
                authorize_url: pending.authorize_url.clone(),
                state: pending.state.clone(),
                expires_at_ms: pending.expires_at_ms,
            });
        }
        let created_at_ms = self
            .load_stored()?
            .map(|stored| stored.created_at_ms)
            .unwrap_or(now_ms);
        let pkce = generate_pkce()?;
        let authorize_url = build_authorize_url(&self.config, &pkce);
        let pending = PersistedPending {
            verifier: pkce.verifier,
            state: pkce.state.clone(),
            authorize_url: authorize_url.clone(),
            expires_at_ms: now_ms.saturating_add(PENDING_LOGIN_TTL_MS),
        };
        self.persist(&StoredSession {
            status: LoginStatus::Pending,
            pending: Some(pending),
            tokens: None,
            user: None,
            created_at_ms,
            updated_at_ms: now_ms,
        })?;
        Ok(PendingLogin {
            authorize_url,
            state: pkce.state,
            expires_at_ms: now_ms.saturating_add(PENDING_LOGIN_TTL_MS),
        })
    }

    /// Completes the login with the user-pasted `CODE` or `CODE#STATE`.
    /// The profile fetch is best-effort: tokens persist even when the
    /// profile endpoint is unreachable, with `user` left empty.
    pub fn complete_login(&self, code_input: &str) -> Result<PublicSession, ClaudeError> {
        let now_ms = self.clock.now_ms();
        let Some(mut stored) = self.load_stored()? else {
            return Err(ClaudeError::new(
                ClaudeErrorCode::NotAuthenticated,
                "No Claude sign-in is in progress. Start sign-in first",
            ));
        };
        let Some(pending) = stored.pending.clone() else {
            return Err(ClaudeError::new(
                ClaudeErrorCode::NotAuthenticated,
                "No Claude sign-in is in progress. Start sign-in first",
            ));
        };
        if now_ms >= pending.expires_at_ms {
            stored.status = LoginStatus::Expired;
            stored.pending = None;
            stored.updated_at_ms = now_ms;
            self.persist(&stored)?;
            return Err(ClaudeError::new(
                ClaudeErrorCode::InvalidAuthorizationCode,
                "Claude sign-in expired before the code was entered. Start again",
            ));
        }
        let input = parse_authorization_input(code_input)?;
        let raw = self.transport.exchange_code(
            &self.config,
            &input.code,
            input.state.as_deref().or(Some(pending.state.as_str())),
            &pending.verifier,
        )?;
        let mut tokens = TokenSet::from_token_response(&raw, None, now_ms)?;
        // Best-effort identity: a profile failure must not strand a valid
        // token set. The account id only scopes future memory rows.
        let user = self
            .transport
            .fetch_profile(&self.config, tokens.access_token())
            .ok()
            .and_then(|profile| parse_claude_user(&profile));
        tokens.set_user(user.clone());
        stored.tokens = Some(tokens);
        stored.user = user.clone();
        stored.status = LoginStatus::Authenticated;
        stored.pending = None;
        stored.updated_at_ms = self.clock.now_ms();
        self.persist(&stored)?;
        Ok(PublicSession {
            status: LoginStatus::Authenticated,
            user,
        })
    }

    /// Fresh credentials for one API call, refreshing through the
    /// refresh token when within the expiry margin. Concurrent callers
    /// serialize; only the first performs the refresh.
    pub fn ensure_fresh_auth(&self) -> Result<FreshAuth, ClaudeError> {
        let now_ms = self.clock.now_ms();
        if let Some(stored) = self.load_stored()?
            && let Some(tokens) = &stored.tokens
            && !tokens.is_expired(now_ms)
        {
            return Ok(FreshAuth {
                access_token: tokens.access_token().to_owned(),
            });
        }
        let _guard = self.refresh_lock.lock();
        let now_ms = self.clock.now_ms();
        let Some(mut stored) = self.load_stored()? else {
            return Err(ClaudeError::new(
                ClaudeErrorCode::NotAuthenticated,
                "No Claude credentials available. The user must sign in",
            ));
        };
        let Some(tokens) = stored.tokens.clone() else {
            return Err(ClaudeError::new(
                ClaudeErrorCode::NotAuthenticated,
                "Claude sign-in is not complete",
            ));
        };
        if !tokens.is_expired(now_ms) {
            // A concurrent caller already refreshed under the lock.
            return Ok(FreshAuth {
                access_token: tokens.access_token().to_owned(),
            });
        }
        let Some(refresh_token) = tokens.refresh_token().map(str::to_owned) else {
            // No rotation possible; hand out the current token if it exists.
            return Ok(FreshAuth {
                access_token: tokens.access_token().to_owned(),
            });
        };
        match self.transport.refresh(&self.config, &refresh_token) {
            Ok(raw) => {
                let mut fresh = TokenSet::from_token_response(&raw, Some(&refresh_token), now_ms)?;
                if fresh.user().is_none() {
                    fresh.set_user(stored.user.clone());
                }
                let user = fresh.user().cloned();
                stored.tokens = Some(fresh.clone());
                stored.status = LoginStatus::Authenticated;
                if user.is_some() {
                    stored.user = user;
                }
                stored.updated_at_ms = now_ms;
                self.persist(&stored)?;
                Ok(FreshAuth {
                    access_token: fresh.access_token().to_owned(),
                })
            }
            Err(error) if error.is_refresh_token_invalid() => {
                // Keep a secret-free `Expired` marker instead of deleting:
                // the UI must distinguish "was signed in, needs re-auth"
                // from "never signed in". No bearer material survives.
                let now_ms = self.clock.now_ms();
                let created_at_ms = self
                    .load_stored()
                    .ok()
                    .flatten()
                    .map(|stored| stored.created_at_ms)
                    .unwrap_or(now_ms);
                let marker = StoredSession {
                    status: LoginStatus::Expired,
                    pending: None,
                    tokens: None,
                    user: None,
                    created_at_ms,
                    updated_at_ms: now_ms,
                };
                if self.persist(&marker).is_err() {
                    let _ = self.store.delete();
                }
                Err(error)
            }
            Err(error) => Err(error),
        }
    }

    /// Deletes the stored session. The most reliable credential cleanup: the
    /// app keeps nothing usable afterward.
    pub fn logout(&self) -> Result<(), ClaudeError> {
        self.store.delete().map_err(Self::storage_error)
    }

    /// Display material for an in-progress login, if one is still valid.
    /// Lets a restarted client re-render the authorize URL without starting
    /// over (the verifier never leaves the encrypted store).
    pub fn pending_login_display(&self) -> Result<Option<PendingLogin>, ClaudeError> {
        let now_ms = self.clock.now_ms();
        let stored = self.load_stored()?;
        let Some(pending) = stored.as_ref().and_then(|stored| stored.pending.as_ref()) else {
            return Ok(None);
        };
        if pending.expires_at_ms <= now_ms {
            return Ok(None);
        }
        Ok(Some(PendingLogin {
            authorize_url: pending.authorize_url.clone(),
            state: pending.state.clone(),
            expires_at_ms: pending.expires_at_ms,
        }))
    }

    /// Discovers the signed-in account's model ids: ensures fresh auth
    /// (refreshing first), tries `/v1/models`, and falls back to the curated
    /// slug list when the endpoint rejects the subscription token.
    /// Bearer material never leaves this call.
    pub fn discover_models(&self) -> Result<Vec<String>, ClaudeError> {
        let auth = self.ensure_fresh_auth()?;
        match self
            .transport
            .fetch_models(&self.config, auth.access_token())
        {
            Ok(value) => {
                let ids = extract_model_ids(&value);
                if ids.is_empty() {
                    Ok(FALLBACK_MODEL_SLUGS
                        .iter()
                        .map(|slug| slug.to_string())
                        .collect())
                } else {
                    Ok(ids)
                }
            }
            Err(_) => Ok(FALLBACK_MODEL_SLUGS
                .iter()
                .map(|slug| slug.to_string())
                .collect()),
        }
    }
}

/// Default daemon-owned session manager: encrypted file store under
/// `~/.waku/claude` (in-memory fallback when the directory is unusable),
/// curl transport, wall clock. Shared by the auth commands and the later
/// subscription driver so every daemon path coordinates through the one
/// credential directory instead of drifting into parallel constructions.
pub fn default_session_manager() -> ClaudeSessionManager {
    let store: Arc<dyn CredentialStore> =
        match EncryptedFileStore::open(EncryptedFileStore::default_dir()) {
            Ok(store) => Arc::new(store),
            Err(error) => {
                eprintln!("Waku Claude sessions will not persist across restarts: {error:#}");
                Arc::new(MemoryStore::default())
            }
        };
    ClaudeSessionManager::open(store, Arc::new(CurlOAuthTransport), Arc::new(SystemClock))
}

// ---------------------------------------------------------------------------
// Tests (mock transport + memory store; never real credentials/network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::VecDeque;

    struct MockTransport {
        inner: Mutex<MockState>,
    }

    struct MockState {
        exchanges: VecDeque<Result<Value, ClaudeError>>,
        refreshes: VecDeque<Result<Value, ClaudeError>>,
        profiles: VecDeque<Result<Value, ClaudeError>>,
        models: VecDeque<Result<Value, ClaudeError>>,
        refresh_calls: usize,
        exchanged: Vec<(String, String)>,
    }

    impl MockTransport {
        fn new() -> Self {
            Self {
                inner: Mutex::new(MockState {
                    exchanges: VecDeque::new(),
                    refreshes: VecDeque::new(),
                    profiles: VecDeque::new(),
                    models: VecDeque::new(),
                    refresh_calls: 0,
                    exchanged: Vec::new(),
                }),
            }
        }

        fn push_exchange(&self, result: Result<Value, ClaudeError>) {
            self.inner.lock().exchanges.push_back(result);
        }

        fn push_refresh(&self, result: Result<Value, ClaudeError>) {
            self.inner.lock().refreshes.push_back(result);
        }

        fn push_profile(&self, result: Result<Value, ClaudeError>) {
            self.inner.lock().profiles.push_back(result);
        }

        fn push_models(&self, result: Result<Value, ClaudeError>) {
            self.inner.lock().models.push_back(result);
        }
    }

    impl OAuthTransport for MockTransport {
        fn exchange_code(
            &self,
            _config: &ClaudeAuthConfig,
            code: &str,
            _state: Option<&str>,
            verifier: &str,
        ) -> Result<Value, ClaudeError> {
            let mut inner = self.inner.lock();
            inner.exchanged.push((code.to_owned(), verifier.to_owned()));
            inner.exchanges.pop_front().unwrap_or_else(|| {
                Err(ClaudeError::new(
                    ClaudeErrorCode::TokenExchangeFailed,
                    "no mock exchange queued",
                ))
            })
        }

        fn refresh(
            &self,
            _config: &ClaudeAuthConfig,
            _refresh_token: &str,
        ) -> Result<Value, ClaudeError> {
            let mut inner = self.inner.lock();
            inner.refresh_calls += 1;
            inner.refreshes.pop_front().unwrap_or_else(|| {
                Err(ClaudeError::new(
                    ClaudeErrorCode::TokenRefreshFailed,
                    "no mock refresh queued",
                ))
            })
        }

        fn fetch_profile(
            &self,
            _config: &ClaudeAuthConfig,
            _access_token: &str,
        ) -> Result<Value, ClaudeError> {
            self.inner.lock().profiles.pop_front().unwrap_or_else(|| {
                Err(ClaudeError::new(
                    ClaudeErrorCode::ProfileRequestFailed,
                    "no mock profile queued",
                ))
            })
        }

        fn fetch_models(
            &self,
            _config: &ClaudeAuthConfig,
            _access_token: &str,
        ) -> Result<Value, ClaudeError> {
            self.inner.lock().models.pop_front().unwrap_or_else(|| {
                Err(ClaudeError::new(
                    ClaudeErrorCode::ModelsRequestFailed,
                    "no mock models queued",
                ))
            })
        }
    }

    fn manager(
        transport: MockTransport,
        clock: ManualClock,
    ) -> (ClaudeSessionManager, Arc<MockTransport>, Arc<ManualClock>) {
        let transport = Arc::new(transport);
        let clock = Arc::new(clock);
        let manager = ClaudeSessionManager::open(
            Arc::new(MemoryStore::default()),
            transport.clone(),
            clock.clone(),
        );
        (manager, transport, clock)
    }

    fn token_response(access: &str, refresh: &str) -> Value {
        json!({
            "access_token": access,
            "refresh_token": refresh,
            "expires_in": 3600,
            "scope": "org:create_api_key user:profile user:inference",
        })
    }

    fn profile_response() -> Value {
        json!({
            "user": {"email": "dev@example.com"},
            "organization": {
                "uuid": "org-1",
                "organization_type": "claude_max",
                "rate_limit_tier": "default_claude_max_20x",
            },
        })
    }

    #[test]
    fn full_login_flow_persists_tokens_and_profile() {
        let (manager, transport, _) = manager(MockTransport::new(), ManualClock::new(1_000));
        let pending = manager.start_login().expect("login starts");
        assert!(pending.authorize_url.contains("claude.ai/oauth/authorize"));
        assert!(!pending.state.is_empty());

        transport.push_exchange(Ok(token_response("access-1", "refresh-1")));
        transport.push_profile(Ok(profile_response()));
        let public = manager.complete_login("code-abc").expect("login completes");
        assert_eq!(public.status, LoginStatus::Authenticated);
        assert_eq!(
            public.user.as_ref().and_then(|user| user.email.as_deref()),
            Some("dev@example.com")
        );

        // The verifier reached the exchange but never the UI surface.
        let exchanged = &transport.inner.lock().exchanged;
        assert_eq!(exchanged.len(), 1);
        assert_eq!(exchanged[0].0, "code-abc");
        assert!(!exchanged[0].1.is_empty());

        let public = manager.public_session().expect("session reads");
        assert_eq!(public.status, LoginStatus::Authenticated);
        assert!(manager.pending_login_display().unwrap().is_none());
    }

    #[test]
    fn start_login_reuses_valid_pending_and_restarts_after_expiry() {
        let (manager, _, clock) = manager(MockTransport::new(), ManualClock::new(0));
        let first = manager.start_login().expect("login starts");
        let second = manager.start_login().expect("login reuses");
        assert_eq!(first.authorize_url, second.authorize_url);

        // Past expiry, the next start mints a fresh authorize URL and the
        // session reads pending again — not expired.
        clock.advance(PENDING_LOGIN_TTL_MS + 1);
        let third = manager.start_login().expect("login restarts");
        assert_ne!(first.authorize_url, third.authorize_url);
        let public = manager.public_session().expect("session reads");
        assert_eq!(public.status, LoginStatus::Pending);

        // Without restarting, an expired pending reads as expired.
        clock.advance(PENDING_LOGIN_TTL_MS + 1);
        let public = manager.public_session().expect("session reads");
        assert_eq!(public.status, LoginStatus::Expired);
    }

    #[test]
    fn complete_login_rejects_empty_code_and_expired_pending() {
        let (manager, transport, clock) = manager(MockTransport::new(), ManualClock::new(0));
        manager.start_login().expect("login starts");
        assert!(manager.complete_login("   ").is_err());

        clock.advance(PENDING_LOGIN_TTL_MS + 1);
        transport.push_exchange(Ok(token_response("a", "r")));
        assert!(manager.complete_login("code").is_err());
    }

    #[test]
    fn profile_failure_still_persists_tokens() {
        let (manager, transport, _) = manager(MockTransport::new(), ManualClock::new(0));
        manager.start_login().expect("login starts");
        transport.push_exchange(Ok(token_response("access-1", "refresh-1")));
        transport.push_profile(Err(ClaudeError::new(
            ClaudeErrorCode::ProfileRequestFailed,
            "profile down",
        )));
        let public = manager.complete_login("code").expect("login completes");
        assert_eq!(public.status, LoginStatus::Authenticated);
        assert!(public.user.is_none());
    }

    #[test]
    fn ensure_fresh_auth_refreshes_once_and_rotates() {
        let (manager, transport, clock) = manager(MockTransport::new(), ManualClock::new(0));
        manager.start_login().expect("login starts");
        transport.push_exchange(Ok(token_response("access-1", "refresh-1")));
        transport.push_profile(Ok(profile_response()));
        manager.complete_login("code").expect("login completes");

        // Inside the margin: exactly one refresh, then the rotated token wins.
        clock.advance(3_600_000);
        transport.push_refresh(Ok(token_response("access-2", "refresh-2")));
        let auth = manager.ensure_fresh_auth().expect("auth refreshes");
        assert_eq!(auth.access_token(), "access-2");
        assert_eq!(transport.inner.lock().refresh_calls, 1);

        let auth = manager.ensure_fresh_auth().expect("auth reuses");
        assert_eq!(auth.access_token(), "access-2");
        assert_eq!(transport.inner.lock().refresh_calls, 1);
    }

    #[test]
    fn dead_refresh_marks_session_expired_without_secrets() {
        let (manager, transport, clock) = manager(MockTransport::new(), ManualClock::new(0));
        manager.start_login().expect("login starts");
        transport.push_exchange(Ok(token_response("access-1", "refresh-1")));
        transport.push_profile(Ok(profile_response()));
        manager.complete_login("code").expect("login completes");

        clock.advance(3_600_000 + 1);
        transport.push_refresh(Err(ClaudeError::new(
            ClaudeErrorCode::RefreshTokenInvalid,
            "dead",
        )));
        let error = manager.ensure_fresh_auth().expect_err("refresh dies");
        assert!(error.is_refresh_token_invalid());

        let public = manager.public_session().expect("session reads");
        assert_eq!(public.status, LoginStatus::Expired);
        assert!(public.user.is_none());
    }

    #[test]
    fn discover_models_prefers_server_list_then_fallback() {
        let (manager, transport, _) = manager(MockTransport::new(), ManualClock::new(0));
        manager.start_login().expect("login starts");
        transport.push_exchange(Ok(token_response("access-1", "refresh-1")));
        transport.push_profile(Ok(profile_response()));
        manager.complete_login("code").expect("login completes");

        transport.push_models(Ok(json!({"data": [{"id": "model-a"}, {"id": "model-b"}]})));
        assert_eq!(
            manager.discover_models().unwrap(),
            vec!["model-a", "model-b"]
        );

        transport.push_models(Err(ClaudeError::new(
            ClaudeErrorCode::ModelsRequestFailed,
            "rejected",
        )));
        let fallback = manager.discover_models().unwrap();
        assert!(!fallback.is_empty());
        assert!(fallback.iter().all(|slug| slug.starts_with("claude-")));
    }

    #[test]
    fn logout_clears_the_session() {
        let (manager, transport, _) = manager(MockTransport::new(), ManualClock::new(0));
        manager.start_login().expect("login starts");
        transport.push_exchange(Ok(token_response("access-1", "refresh-1")));
        transport.push_profile(Ok(profile_response()));
        manager.complete_login("code").expect("login completes");

        manager.logout().expect("logout succeeds");
        let public = manager.public_session().expect("session reads");
        assert_eq!(public.status, LoginStatus::Unauthenticated);
    }
}
