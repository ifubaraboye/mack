//! Daemon-owned ChatGPT session manager (Stage 1 foundation).
//!
//! Owns the device-authorization state machine — `unauthenticated`, `pending`,
//! `authenticated`, `expired` — plus token refresh and encrypted credential
//! storage. There is intentionally **no UI and no chat** here; a later stage
//! adds the Settings surface and the `/responses` streaming driver on top of
//! the [`ChatGptSessionManager`] API.
//!
//! Architecture rules (see also `crate::chatgpt_protocol`):
//! - Raw tokens never leave this module except through [`FreshAuth`], whose
//!   access token is `pub(crate)`-gated for the future daemon driver. The
//!   GPUI renderer and the daemon WebSocket protocol only ever see
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
//! - Exactly one refresh may be in flight per manager (refresh-token rotation
//!   would otherwise invalidate a concurrent refresh and force a re-login).
//!   Concurrent [`ChatGptSessionManager::ensure_fresh_auth`] callers serialize
//!   on a lock and re-check expiry before refreshing.
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

use crate::chatgpt_protocol::{
    ChatGptError, ChatGptErrorCode, ChatGptUser, DEVICE_CODE_TTL_MS, DeviceAuthConfig, DeviceCode,
    DevicePollOutcome, TokenSet, extract_error_code, extract_model_slugs, form_encode,
    is_dead_refresh_error,
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
const KEYCHAIN_SERVICE: &str = "Waku ChatGPT";
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

/// Daemon-side login state. Serialized camelCase to match the SDK's
/// `LoginStatus` vocabulary for future wire compatibility.
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
    pub user: Option<ChatGptUser>,
}

// ---------------------------------------------------------------------------
// FreshAuth: the single sanctioned carrier of an access token
// ---------------------------------------------------------------------------

/// Fresh credentials for one API call. The account id is public; the access
/// token is `pub(crate)` so only the daemon driver (a later stage, same
/// crate) can render it into an `Authorization` header. There is no path
/// from this type to the renderer or the WebSocket protocol.
pub struct FreshAuth {
    account_id: String,
    access_token: String,
}

impl FreshAuth {
    pub fn account_id(&self) -> &str {
        &self.account_id
    }

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
            .field("account_id", &self.account_id)
            .field("access_token", &"<redacted>")
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Persisted session
// ---------------------------------------------------------------------------

/// Pending device-login material as persisted between polls.
#[derive(Clone, Deserialize, Serialize)]
struct PersistedDevice {
    device_auth_id: String,
    user_code: String,
    verification_url: String,
    interval_secs: u64,
    expires_at_ms: u64,
    last_polled_at_ms: u64,
}

impl std::fmt::Debug for PersistedDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PersistedDevice")
            .field("device_auth_id", &"<redacted>")
            .field("user_code", &self.user_code)
            .field("verification_url", &self.verification_url)
            .field("interval_secs", &self.interval_secs)
            .field("expires_at_ms", &self.expires_at_ms)
            .field("last_polled_at_ms", &self.last_polled_at_ms)
            .finish()
    }
}

/// The stored session envelope. `Debug` redacts via the field types.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct StoredSession {
    status: LoginStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device: Option<PersistedDevice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tokens: Option<TokenSet>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user: Option<ChatGptUser>,
    #[serde(default)]
    created_at_ms: u64,
    #[serde(default)]
    updated_at_ms: u64,
}

// ---------------------------------------------------------------------------
// CredentialStore
// ---------------------------------------------------------------------------

/// Persistence for one desktop user's ChatGPT session. Production uses
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
    pub user: Option<ChatGptUser>,
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
    /// `~/.waku/chatgpt` (or the platform temp dir when no home exists).
    pub fn default_dir() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(".waku")
            .join("chatgpt")
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
            .map_err(|_| anyhow::anyhow!("chatgpt session encryption failed"))?;
        let ciphertext = self
            .cipher()?
            .encrypt(Nonce::from_slice(&nonce_bytes), plaintext.as_slice())
            .map_err(|_| anyhow::anyhow!("chatgpt session encryption failed"))?;
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
            .map_err(|_| anyhow::anyhow!("chatgpt session file is corrupt"))?;
        if envelope.v != ENVELOPE_VERSION {
            anyhow::bail!("chatgpt session file has an unknown version");
        }
        let nonce_bytes = base64::engine::general_purpose::STANDARD
            .decode(&envelope.nonce)
            .map_err(|_| anyhow::anyhow!("chatgpt session file is corrupt"))?;
        let ciphertext = base64::engine::general_purpose::STANDARD
            .decode(&envelope.ciphertext)
            .map_err(|_| anyhow::anyhow!("chatgpt session file is corrupt"))?;
        let nonce = Nonce::from_slice(&nonce_bytes);
        let plaintext = self
            .cipher()?
            .decrypt(nonce, ciphertext.as_slice())
            .map_err(|_| anyhow::anyhow!("chatgpt session file failed authentication"))?;
        serde_json::from_slice(&plaintext)
            .map_err(|_| anyhow::anyhow!("chatgpt session file is corrupt"))
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
        .map_err(|_| anyhow::anyhow!("could not generate the chatgpt data key"))?;
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
        .map_err(|_| anyhow::anyhow!("chatgpt key file has an unexpected length"))
}

fn write_key_file(path: &Path, key: &[u8; 32]) -> anyhow::Result<()> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(key);
    write_private_file(path, encoded.as_bytes())
}

/// Writes `data` atomically (write-then-rename) with owner-only permissions:
/// `0600` on unix, inherited profile ACL on Windows (same reasoning as
/// `fs_ext`: those locations already grant owner + administrators alone).
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
        anyhow::bail!("chatgpt keychain item is missing");
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(output.stdout.trim_ascii())
        .map_err(|_| anyhow::anyhow!("chatgpt keychain item is malformed"))?;
    decoded
        .try_into()
        .map_err(|_| anyhow::anyhow!("chatgpt keychain item has an unexpected length"))
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
        anyhow::bail!("could not store the chatgpt key in the keychain");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// DeviceAuthTransport (blocking; background executor only)
// ---------------------------------------------------------------------------

/// Device-code material as returned by the usercode endpoint (before the
/// manager stamps expiry). Redacted in `Debug`.
#[derive(Clone)]
pub struct RawDeviceCode {
    pub(crate) device_auth_id: String,
    pub(crate) user_code: String,
    pub(crate) interval_secs: u64,
}

impl std::fmt::Debug for RawDeviceCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawDeviceCode")
            .field("device_auth_id", &"<redacted>")
            .field("user_code", &self.user_code)
            .field("interval_secs", &self.interval_secs)
            .finish()
    }
}

/// Blocking device-flow transport. Implementations must never log or persist
/// bearer material; bodies travel on stdin, never argv.
pub trait DeviceAuthTransport: Send + Sync {
    fn request_device_code(&self, config: &DeviceAuthConfig)
    -> Result<RawDeviceCode, ChatGptError>;
    fn poll_device_code(
        &self,
        config: &DeviceAuthConfig,
        device_auth_id: &str,
        user_code: &str,
    ) -> Result<DevicePollOutcome, ChatGptError>;
    /// Returns the raw token JSON; the manager normalizes it into `TokenSet`.
    fn exchange_code(
        &self,
        config: &DeviceAuthConfig,
        authorization_code: &str,
        code_verifier: &str,
    ) -> Result<Value, ChatGptError>;
    /// Returns the raw token JSON; the manager normalizes it into `TokenSet`.
    fn refresh(
        &self,
        config: &DeviceAuthConfig,
        refresh_token: &str,
    ) -> Result<Value, ChatGptError>;
    /// Returns the raw `/models` JSON for the signed-in account. The manager
    /// supplies fresh auth; only public model metadata ever leaves the daemon.
    fn fetch_models(
        &self,
        config: &DeviceAuthConfig,
        access_token: &str,
        account_id: &str,
    ) -> Result<Value, ChatGptError>;
}

/// Production transport over the system `curl`, following the `usage.rs`
/// convention. Blocking — call only from the background executor.
pub struct CurlDeviceTransport;

impl DeviceAuthTransport for CurlDeviceTransport {
    fn request_device_code(
        &self,
        config: &DeviceAuthConfig,
    ) -> Result<RawDeviceCode, ChatGptError> {
        let body = serde_json::json!({"client_id": config.client_id}).to_string();
        let (status, text) = curl_post_json(&config.device_usercode_url(), &body)?;
        if status == 404 {
            return Err(ChatGptError::new(
                ChatGptErrorCode::DeviceCodeDisabled,
                "Device-code login is not enabled for this server",
            )
            .with_status(status));
        }
        if !(200..300).contains(&status) {
            return Err(ChatGptError::new(
                ChatGptErrorCode::DeviceCodeRequestFailed,
                format!("Device code request failed ({status})"),
            )
            .with_status(status));
        }
        let value: Value = serde_json::from_str(&text).map_err(|_| {
            ChatGptError::new(
                ChatGptErrorCode::DeviceCodeRequestFailed,
                "Device code response was not valid JSON",
            )
            .with_status(status)
        })?;
        let device_auth_id = value
            .get("device_auth_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                ChatGptError::new(
                    ChatGptErrorCode::DeviceCodeRequestFailed,
                    "Device code response was missing required fields",
                )
                .with_status(status)
            })?
            .to_owned();
        let user_code = value
            .get("user_code")
            .or_else(|| value.get("usercode"))
            .and_then(Value::as_str)
            .filter(|code| !code.is_empty())
            .ok_or_else(|| {
                ChatGptError::new(
                    ChatGptErrorCode::DeviceCodeRequestFailed,
                    "Device code response was missing required fields",
                )
                .with_status(status)
            })?
            .to_owned();
        let interval_secs = value
            .get("interval")
            .and_then(|interval| {
                interval
                    .as_u64()
                    .or_else(|| interval.as_str()?.trim().parse().ok())
            })
            .filter(|interval| *interval > 0)
            .unwrap_or(5);
        Ok(RawDeviceCode {
            device_auth_id,
            user_code,
            interval_secs,
        })
    }

    fn poll_device_code(
        &self,
        config: &DeviceAuthConfig,
        device_auth_id: &str,
        user_code: &str,
    ) -> Result<DevicePollOutcome, ChatGptError> {
        let body = serde_json::json!({
            "device_auth_id": device_auth_id,
            "user_code": user_code,
        })
        .to_string();
        let (status, text) = curl_post_json(&config.device_token_url(), &body)?;
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        DevicePollOutcome::from_status_and_body(status, &value)
    }

    fn exchange_code(
        &self,
        config: &DeviceAuthConfig,
        authorization_code: &str,
        code_verifier: &str,
    ) -> Result<Value, ChatGptError> {
        let redirect_uri = config.device_redirect_uri();
        let body = encode_form(&[
            ("grant_type", "authorization_code"),
            ("client_id", config.client_id.as_str()),
            ("code", authorization_code),
            ("code_verifier", code_verifier),
            ("redirect_uri", redirect_uri.as_str()),
        ]);
        let (status, text) = curl_post_form(&config.token_url(), &body)?;
        if !(200..300).contains(&status) {
            return Err(ChatGptError::new(
                ChatGptErrorCode::TokenExchangeFailed,
                format!("Authorization code exchange failed ({status})"),
            )
            .with_status(status));
        }
        serde_json::from_str(&text).map_err(|_| {
            ChatGptError::new(
                ChatGptErrorCode::TokenExchangeFailed,
                "Token response was not valid JSON",
            )
            .with_status(status)
        })
    }

    fn refresh(
        &self,
        config: &DeviceAuthConfig,
        refresh_token: &str,
    ) -> Result<Value, ChatGptError> {
        let body = serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": config.client_id,
            "scope": config.scope,
        })
        .to_string();
        let (status, text) = curl_post_json(&config.token_url(), &body)?;
        if !(200..300).contains(&status) {
            if let Some(code) = extract_error_code(&text)
                && is_dead_refresh_error(&code)
            {
                return Err(ChatGptError::new(
                    ChatGptErrorCode::RefreshTokenInvalid,
                    "Refresh token is no longer valid. The user must sign in again",
                )
                .with_status(status));
            }
            return Err(ChatGptError::new(
                ChatGptErrorCode::TokenRefreshFailed,
                format!("Token refresh failed ({status})"),
            )
            .with_status(status));
        }
        serde_json::from_str(&text).map_err(|_| {
            ChatGptError::new(
                ChatGptErrorCode::TokenRefreshFailed,
                "Token response was not valid JSON",
            )
            .with_status(status)
        })
    }

    fn fetch_models(
        &self,
        config: &DeviceAuthConfig,
        access_token: &str,
        account_id: &str,
    ) -> Result<Value, ChatGptError> {
        let headers = crate::chatgpt_protocol::CodexRequestHeaders::new(
            access_token,
            account_id,
            config.originator.as_str(),
        );
        let (status, text) = curl_get(&config.models_url(), &headers.curl_header_config())?;
        if !(200..300).contains(&status) {
            return Err(ChatGptError::new(
                ChatGptErrorCode::ModelsRequestFailed,
                format!("Model list request failed ({status})"),
            )
            .with_status(status));
        }
        let value: Value = serde_json::from_str(&text).map_err(|_| {
            ChatGptError::new(
                ChatGptErrorCode::ModelsRequestFailed,
                "Model list response was not valid JSON",
            )
            .with_status(status)
        })?;
        // Shape only — never values. Tells a genuinely empty model set apart
        // from a response shape the slug extractor does not cover, without
        // leaking anything into daemon logs.
        eprintln!(
            "chatgpt models response: status={status} shape={} slugs={}",
            crate::chatgpt_protocol::describe_json_shape(&value),
            crate::chatgpt_protocol::extract_model_slugs(&value).len(),
        );
        Ok(value)
    }
}

fn encode_form(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(key, value)| format!("{}={}", form_encode(key), form_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// POSTs a JSON body read from stdin (`--data-binary @-`). Only non-secret
/// `-H` flags touch argv; the body (which may carry refresh tokens or
/// authorization codes) travels on stdin like the `usage.rs` headers do.
fn curl_post_json(url: &str, body: &str) -> Result<(u16, String), ChatGptError> {
    curl_post(url, body, "application/json")
}

fn curl_post_form(url: &str, body: &str) -> Result<(u16, String), ChatGptError> {
    curl_post(url, body, "application/x-www-form-urlencoded")
}

fn curl_post(url: &str, body: &str, content_type: &str) -> Result<(u16, String), ChatGptError> {
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
            &format!("Content-Type: {content_type}"),
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
            ChatGptError::new(
                ChatGptErrorCode::NetworkError,
                "Could not start the ChatGPT request",
            )
        })?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| {
            ChatGptError::new(
                ChatGptErrorCode::NetworkError,
                "ChatGPT request stdin is unavailable",
            )
        })?
        .write_all(body.as_bytes())
        .map_err(|_| {
            ChatGptError::new(
                ChatGptErrorCode::NetworkError,
                "Failed to send the ChatGPT request",
            )
        })?;
    let output = child.wait_with_output().map_err(|_| {
        ChatGptError::new(
            ChatGptErrorCode::NetworkError,
            "ChatGPT request did not finish",
        )
    })?;
    if !output.status.success() {
        return Err(ChatGptError::new(
            ChatGptErrorCode::NetworkError,
            "Failed to reach the ChatGPT service",
        ));
    }
    split_status_and_body(&String::from_utf8_lossy(&output.stdout))
}

/// GETs `url` with headers supplied as a curl `-K -` stdin config (the
/// `usage.rs` convention: bearer material travels on stdin, never argv).
fn curl_get(url: &str, curl_config: &str) -> Result<(u16, String), ChatGptError> {
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
            ChatGptError::new(
                ChatGptErrorCode::NetworkError,
                "Could not start the ChatGPT request",
            )
        })?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| {
            ChatGptError::new(
                ChatGptErrorCode::NetworkError,
                "ChatGPT request stdin is unavailable",
            )
        })?
        .write_all(curl_config.as_bytes())
        .map_err(|_| {
            ChatGptError::new(
                ChatGptErrorCode::NetworkError,
                "Failed to send the ChatGPT request",
            )
        })?;
    let output = child.wait_with_output().map_err(|_| {
        ChatGptError::new(
            ChatGptErrorCode::NetworkError,
            "ChatGPT request did not finish",
        )
    })?;
    if !output.status.success() {
        return Err(ChatGptError::new(
            ChatGptErrorCode::NetworkError,
            "Failed to reach the ChatGPT service",
        ));
    }
    split_status_and_body(&String::from_utf8_lossy(&output.stdout))
}

/// `-D -` prefixes the body with the response headers; the status code is on
/// the first line and the body follows the blank separator line.
fn split_status_and_body(raw: &str) -> Result<(u16, String), ChatGptError> {
    let status = raw
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| {
            ChatGptError::new(
                ChatGptErrorCode::NetworkError,
                "ChatGPT response carried no status",
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
// ChatGptSessionManager
// ---------------------------------------------------------------------------

/// Daemon-owned manager for one desktop user's ChatGPT session. Construct
/// with [`ChatGptSessionManager::open`]; drive from the background executor.
pub struct ChatGptSessionManager {
    config: DeviceAuthConfig,
    store: Arc<dyn CredentialStore>,
    transport: Arc<dyn DeviceAuthTransport>,
    clock: Arc<dyn Clock>,
    /// Serializes refreshes so token rotation cannot invalidate a concurrent
    /// refresh. Holders always re-check expiry under the lock (double-checked).
    refresh_lock: Mutex<()>,
}

impl ChatGptSessionManager {
    pub fn open(
        store: Arc<dyn CredentialStore>,
        transport: Arc<dyn DeviceAuthTransport>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self::open_with_config(DeviceAuthConfig::default(), store, transport, clock)
    }

    pub fn open_with_config(
        config: DeviceAuthConfig,
        store: Arc<dyn CredentialStore>,
        transport: Arc<dyn DeviceAuthTransport>,
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

    /// Endpoint/identity configuration backing this manager. The Stage 3
    /// `/responses` driver reads its URLs and originator from here so protocol
    /// constants stay isolated in one place.
    pub fn config(&self) -> &DeviceAuthConfig {
        &self.config
    }

    fn storage_error(error: anyhow::Error) -> ChatGptError {
        ChatGptError::new(
            ChatGptErrorCode::StorageError,
            format!("ChatGPT session storage failed: {error:#}"),
        )
    }

    fn load_stored(&self) -> Result<Option<StoredSession>, ChatGptError> {
        Ok(self
            .store
            .load()
            .map_err(Self::storage_error)?
            .and_then(|envelope| envelope.persisted))
    }

    fn persist(&self, stored: &StoredSession) -> Result<(), ChatGptError> {
        self.store
            .save(&StoredSessionEnvelope {
                status: stored.status,
                user: stored.user.clone(),
                persisted: Some(stored.clone()),
            })
            .map_err(Self::storage_error)
    }

    /// Effective status without network: a `pending` login past its expiry
    /// reads as `expired` (the transition is persisted on the next `poll`).
    fn effective_status(stored: Option<&StoredSession>, now_ms: u64) -> LoginStatus {
        match stored {
            None => LoginStatus::Unauthenticated,
            Some(stored) if stored.tokens.is_some() => LoginStatus::Authenticated,
            Some(stored) => match &stored.device {
                Some(device) if now_ms < device.expires_at_ms => LoginStatus::Pending,
                _ => LoginStatus::Expired,
            },
        }
    }

    /// UI-safe view. Pure read plus expiry computation — never touches the
    /// network, safe to call from any thread (but still not from render;
    /// cache the result on the entity per house rules).
    pub fn public_session(&self) -> Result<PublicSession, ChatGptError> {
        let now_ms = self.clock.now_ms();
        let stored = self.load_stored()?;
        Ok(PublicSession {
            status: Self::effective_status(stored.as_ref(), now_ms),
            user: stored.and_then(|stored| stored.user),
        })
    }

    /// Starts (or reuses) a device login. A still-valid pending code is
    /// returned again so repeated clicks do not invalidate the user's
    /// in-progress code. Returns display material for the later UI layer.
    pub fn start_device_login(&self) -> Result<DeviceCode, ChatGptError> {
        let now_ms = self.clock.now_ms();
        if let Some(stored) = self.load_stored()?
            && let Some(device) = &stored.device
            && device.expires_at_ms > now_ms
        {
            return DeviceCode::new(
                device.device_auth_id.clone(),
                device.user_code.clone(),
                device.verification_url.clone(),
                device.interval_secs,
                device.expires_at_ms,
            );
        }
        let created_at_ms = self
            .load_stored()?
            .map(|stored| stored.created_at_ms)
            .unwrap_or(now_ms);
        let raw = self.transport.request_device_code(&self.config)?;
        let device = PersistedDevice {
            device_auth_id: raw.device_auth_id.clone(),
            user_code: raw.user_code.clone(),
            verification_url: self.config.device_verification_url(),
            interval_secs: raw.interval_secs,
            expires_at_ms: now_ms.saturating_add(DEVICE_CODE_TTL_MS),
            last_polled_at_ms: 0,
        };
        self.persist(&StoredSession {
            status: LoginStatus::Pending,
            device: Some(device.clone()),
            tokens: None,
            user: None,
            created_at_ms,
            updated_at_ms: now_ms,
        })?;
        DeviceCode::new(
            device.device_auth_id,
            device.user_code,
            device.verification_url,
            device.interval_secs,
            device.expires_at_ms,
        )
    }

    /// Advances the session by at most one device poll (respecting the
    /// server-provided interval) or one token refresh. Safe on any cadence —
    /// the later UI layer polls this, never the upstream endpoints directly.
    pub fn poll(&self) -> Result<PublicSession, ChatGptError> {
        let now_ms = self.clock.now_ms();
        let Some(mut stored) = self.load_stored()? else {
            return Ok(PublicSession {
                status: LoginStatus::Unauthenticated,
                user: None,
            });
        };

        if stored.tokens.is_some() {
            return self.refresh_if_needed_into(stored);
        }

        let Some(device) = stored.device.clone() else {
            return Ok(PublicSession {
                status: LoginStatus::Expired,
                user: None,
            });
        };
        if now_ms >= device.expires_at_ms {
            stored.status = LoginStatus::Expired;
            stored.device = None;
            stored.updated_at_ms = now_ms;
            self.persist(&stored)?;
            return Ok(PublicSession {
                status: LoginStatus::Expired,
                user: None,
            });
        }
        if now_ms.saturating_sub(device.last_polled_at_ms)
            < device.interval_secs.saturating_mul(1000)
        {
            return Ok(PublicSession {
                status: LoginStatus::Pending,
                user: None,
            });
        }

        stored.device = Some(PersistedDevice {
            last_polled_at_ms: now_ms,
            ..device.clone()
        });
        self.persist(&stored)?;
        match self.transport.poll_device_code(
            &self.config,
            &device.device_auth_id,
            &device.user_code,
        )? {
            DevicePollOutcome::Pending => Ok(PublicSession {
                status: LoginStatus::Pending,
                user: None,
            }),
            DevicePollOutcome::Authorized {
                authorization_code,
                code_verifier,
            } => {
                let raw = self.transport.exchange_code(
                    &self.config,
                    &authorization_code,
                    &code_verifier,
                )?;
                let tokens = TokenSet::from_token_response(&raw, None, now_ms)?;
                let user = tokens.user().cloned();
                stored.tokens = Some(tokens);
                stored.user = user.clone();
                stored.status = LoginStatus::Authenticated;
                stored.device = None;
                stored.updated_at_ms = now_ms;
                self.persist(&stored)?;
                Ok(PublicSession {
                    status: LoginStatus::Authenticated,
                    user,
                })
            }
        }
    }

    /// Fresh credentials for one API call, refreshing through the
    /// refresh token when within the expiry margin. Concurrent callers
    /// serialize; only the first performs the refresh.
    pub fn ensure_fresh_auth(&self) -> Result<FreshAuth, ChatGptError> {
        let now_ms = self.clock.now_ms();
        if let Some(stored) = self.load_stored()?
            && let Some(tokens) = &stored.tokens
            && !tokens.is_expired(now_ms)
            && let Some(account_id) = tokens.account_id()
        {
            return Ok(FreshAuth {
                account_id: account_id.to_owned(),
                access_token: tokens.access_token().to_owned(),
            });
        }
        let _guard = self.refresh_lock.lock();
        let now_ms = self.clock.now_ms();
        let Some(mut stored) = self.load_stored()? else {
            return Err(ChatGptError::new(
                ChatGptErrorCode::NotAuthenticated,
                "No ChatGPT credentials available. The user must sign in",
            ));
        };
        let Some(tokens) = stored.tokens.clone() else {
            return Err(ChatGptError::new(
                ChatGptErrorCode::NotAuthenticated,
                "ChatGPT sign-in is not complete",
            ));
        };
        if !tokens.is_expired(now_ms)
            && let Some(account_id) = tokens.account_id()
        {
            // A concurrent caller already refreshed under the lock.
            return Ok(FreshAuth {
                account_id: account_id.to_owned(),
                access_token: tokens.access_token().to_owned(),
            });
        }
        let Some(refresh_token) = tokens.refresh_token().map(str::to_owned) else {
            // No rotation possible; hand out the current token if it exists.
            if let Some(account_id) = tokens.account_id() {
                return Ok(FreshAuth {
                    account_id: account_id.to_owned(),
                    access_token: tokens.access_token().to_owned(),
                });
            }
            return Err(ChatGptError::new(
                ChatGptErrorCode::NotAuthenticated,
                "No ChatGPT credentials available. The user must sign in",
            ));
        };
        match self.transport.refresh(&self.config, &refresh_token) {
            Ok(raw) => {
                let fresh = TokenSet::from_token_response(&raw, Some(&refresh_token), now_ms)?;
                let user = fresh.user().cloned();
                stored.tokens = Some(fresh.clone());
                stored.status = LoginStatus::Authenticated;
                if user.is_some() {
                    stored.user = user;
                }
                stored.updated_at_ms = now_ms;
                self.persist(&stored)?;
                let Some(account_id) = fresh.account_id() else {
                    return Err(ChatGptError::new(
                        ChatGptErrorCode::TokenRefreshFailed,
                        "Refreshed session carries no account",
                    ));
                };
                Ok(FreshAuth {
                    account_id: account_id.to_owned(),
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
                    device: None,
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

    /// Authenticated refresh path shared by `poll` on an authenticated
    /// session: transparent refresh, or session teardown on a dead refresh
    /// token so the UI falls back to `expired` instead of erroring forever.
    fn refresh_if_needed_into(&self, stored: StoredSession) -> Result<PublicSession, ChatGptError> {
        let now_ms = self.clock.now_ms();
        let needs_refresh = stored
            .tokens
            .as_ref()
            .is_some_and(|tokens| tokens.is_expired(now_ms));
        if !needs_refresh {
            return Ok(PublicSession {
                status: LoginStatus::Authenticated,
                user: stored.user,
            });
        }
        match self.ensure_fresh_auth() {
            Ok(_) => {
                let stored = self.load_stored()?.unwrap_or(stored);
                Ok(PublicSession {
                    status: LoginStatus::Authenticated,
                    user: stored.user,
                })
            }
            Err(error) if error.is_refresh_token_invalid() => Ok(PublicSession {
                status: LoginStatus::Expired,
                user: None,
            }),
            Err(error) => Err(error),
        }
    }

    /// Deletes the stored session. The most reliable credential cleanup: the
    /// app keeps nothing usable afterward.
    pub fn logout(&self) -> Result<(), ChatGptError> {
        self.store.delete().map_err(Self::storage_error)
    }

    /// Display material for an in-progress login, if one is still valid.
    /// Lets a restarted client re-render the code without starting over.
    pub fn pending_login_display(&self) -> Result<Option<DeviceCode>, ChatGptError> {
        let now_ms = self.clock.now_ms();
        let stored = self.load_stored()?;
        let Some(device) = stored.as_ref().and_then(|stored| stored.device.as_ref()) else {
            return Ok(None);
        };
        if device.expires_at_ms <= now_ms {
            return Ok(None);
        }
        Ok(Some(DeviceCode::new(
            device.device_auth_id.clone(),
            device.user_code.clone(),
            device.verification_url.clone(),
            device.interval_secs,
            device.expires_at_ms,
        )?))
    }

    /// Discovers the signed-in account's model slugs: ensures fresh auth
    /// (refreshing first), fetches `/models`, and returns public slugs only.
    /// Bearer material never leaves this call.
    pub fn discover_models(&self) -> Result<Vec<String>, ChatGptError> {
        let auth = self.ensure_fresh_auth()?;
        let value =
            self.transport
                .fetch_models(&self.config, auth.access_token(), auth.account_id())?;
        Ok(extract_model_slugs(&value))
    }
}

/// Default daemon-owned session manager: encrypted file store under
/// `~/.waku/chatgpt` (in-memory fallback when the directory is unusable),
/// curl transport, wall clock. Shared by the auth commands and the Stage 3
/// `/responses` driver so every daemon path coordinates through the one
/// credential directory instead of drifting into parallel constructions.
pub fn default_session_manager() -> ChatGptSessionManager {
    let store: Arc<dyn CredentialStore> =
        match EncryptedFileStore::open(EncryptedFileStore::default_dir()) {
            Ok(store) => Arc::new(store),
            Err(error) => {
                eprintln!("Waku ChatGPT sessions will not persist across restarts: {error:#}");
                Arc::new(MemoryStore::default())
            }
        };
    ChatGptSessionManager::open(store, Arc::new(CurlDeviceTransport), Arc::new(SystemClock))
}

// ---------------------------------------------------------------------------
// Tests (mock transport + memory store; never real credentials/network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chatgpt_protocol::{AUTH_CLAIM, ChatGptUser};
    use serde_json::json;
    use std::collections::VecDeque;

    struct MockTransport {
        inner: Mutex<MockState>,
    }

    struct MockState {
        device_codes: VecDeque<RawDeviceCode>,
        polls: VecDeque<Result<DevicePollOutcome, ChatGptError>>,
        exchanges: VecDeque<Result<Value, ChatGptError>>,
        refreshes: VecDeque<Result<Value, ChatGptError>>,
        models: VecDeque<Result<Value, ChatGptError>>,
        refresh_calls: usize,
        model_auth: Vec<(String, String)>,
    }

    impl MockTransport {
        fn new() -> Self {
            Self {
                inner: Mutex::new(MockState {
                    device_codes: VecDeque::new(),
                    polls: VecDeque::new(),
                    exchanges: VecDeque::new(),
                    refreshes: VecDeque::new(),
                    models: VecDeque::new(),
                    refresh_calls: 0,
                    model_auth: Vec::new(),
                }),
            }
        }

        fn push_device_code(&self, code: RawDeviceCode) {
            self.inner.lock().device_codes.push_back(code);
        }

        fn push_poll(&self, outcome: Result<DevicePollOutcome, ChatGptError>) {
            self.inner.lock().polls.push_back(outcome);
        }

        fn push_exchange(&self, result: Result<Value, ChatGptError>) {
            self.inner.lock().exchanges.push_back(result);
        }

        fn push_refresh(&self, result: Result<Value, ChatGptError>) {
            self.inner.lock().refreshes.push_back(result);
        }

        fn push_models(&self, result: Result<Value, ChatGptError>) {
            self.inner.lock().models.push_back(result);
        }

        fn refresh_calls(&self) -> usize {
            self.inner.lock().refresh_calls
        }
    }

    impl DeviceAuthTransport for MockTransport {
        fn request_device_code(
            &self,
            _config: &DeviceAuthConfig,
        ) -> Result<RawDeviceCode, ChatGptError> {
            self.inner.lock().device_codes.pop_front().ok_or_else(|| {
                ChatGptError::new(
                    ChatGptErrorCode::DeviceCodeRequestFailed,
                    "no mock device code",
                )
            })
        }

        fn poll_device_code(
            &self,
            _config: &DeviceAuthConfig,
            _device_auth_id: &str,
            _user_code: &str,
        ) -> Result<DevicePollOutcome, ChatGptError> {
            self.inner
                .lock()
                .polls
                .pop_front()
                .unwrap_or(Ok(DevicePollOutcome::Pending))
        }

        fn exchange_code(
            &self,
            _config: &DeviceAuthConfig,
            _authorization_code: &str,
            _code_verifier: &str,
        ) -> Result<Value, ChatGptError> {
            self.inner.lock().exchanges.pop_front().unwrap_or_else(|| {
                Err(ChatGptError::new(
                    ChatGptErrorCode::TokenExchangeFailed,
                    "no mock exchange",
                ))
            })
        }

        fn refresh(
            &self,
            _config: &DeviceAuthConfig,
            _refresh_token: &str,
        ) -> Result<Value, ChatGptError> {
            let mut state = self.inner.lock();
            state.refresh_calls += 1;
            state.refreshes.pop_front().unwrap_or_else(|| {
                Err(ChatGptError::new(
                    ChatGptErrorCode::TokenRefreshFailed,
                    "no mock refresh",
                ))
            })
        }

        fn fetch_models(
            &self,
            _config: &DeviceAuthConfig,
            access_token: &str,
            account_id: &str,
        ) -> Result<Value, ChatGptError> {
            let mut state = self.inner.lock();
            state
                .model_auth
                .push((access_token.to_owned(), account_id.to_owned()));
            state.models.pop_front().unwrap_or_else(|| {
                Err(ChatGptError::new(
                    ChatGptErrorCode::ModelsRequestFailed,
                    "no mock models",
                ))
            })
        }
    }

    // `Arc<MockTransport>` also implements the trait so tests can count calls.
    impl DeviceAuthTransport for Arc<MockTransport> {
        fn request_device_code(
            &self,
            config: &DeviceAuthConfig,
        ) -> Result<RawDeviceCode, ChatGptError> {
            (**self).request_device_code(config)
        }

        fn poll_device_code(
            &self,
            config: &DeviceAuthConfig,
            device_auth_id: &str,
            user_code: &str,
        ) -> Result<DevicePollOutcome, ChatGptError> {
            (**self).poll_device_code(config, device_auth_id, user_code)
        }

        fn exchange_code(
            &self,
            config: &DeviceAuthConfig,
            authorization_code: &str,
            code_verifier: &str,
        ) -> Result<Value, ChatGptError> {
            (**self).exchange_code(config, authorization_code, code_verifier)
        }

        fn refresh(
            &self,
            config: &DeviceAuthConfig,
            refresh_token: &str,
        ) -> Result<Value, ChatGptError> {
            (**self).refresh(config, refresh_token)
        }

        fn fetch_models(
            &self,
            config: &DeviceAuthConfig,
            access_token: &str,
            account_id: &str,
        ) -> Result<Value, ChatGptError> {
            (**self).fetch_models(config, access_token, account_id)
        }
    }

    fn harness() -> (
        Arc<MemoryStore>,
        Arc<MockTransport>,
        Arc<ManualClock>,
        ChatGptSessionManager,
    ) {
        harness_with_config(DeviceAuthConfig::default())
    }

    fn harness_with_config(
        config: DeviceAuthConfig,
    ) -> (
        Arc<MemoryStore>,
        Arc<MockTransport>,
        Arc<ManualClock>,
        ChatGptSessionManager,
    ) {
        let store = Arc::new(MemoryStore::default());
        let transport = Arc::new(MockTransport::new());
        let clock = Arc::new(ManualClock::new(1_000_000));
        let manager = ChatGptSessionManager::open_with_config(
            config,
            store.clone(),
            transport.clone(),
            clock.clone(),
        );
        (store, transport, clock, manager)
    }

    fn device_code(id: &str) -> RawDeviceCode {
        RawDeviceCode {
            device_auth_id: id.to_owned(),
            user_code: "ABCD-1234".to_owned(),
            interval_secs: 5,
        }
    }

    fn token_response(account_id: &str, refresh: &str) -> Value {
        json!({
            "access_token": test_access_token(account_id),
            "refresh_token": refresh,
            "expires_in": 3600,
        })
    }

    /// Opaque mock tokens carry no account id, which real sessions always
    /// have — and `ensure_fresh_auth` rightly refuses account-less tokens.
    /// Tests therefore mint unsigned JWTs carrying the account claim.
    fn test_access_token(account_id: &str) -> String {
        let encode =
            |value: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.as_bytes());
        let payload = serde_json::json!({ AUTH_CLAIM: { "chatgpt_account_id": account_id } });
        format!(
            "{}.{}.{}",
            encode(r#"{"alg":"none"}"#),
            encode(&payload.to_string()),
            encode("sig")
        )
    }

    #[test]
    fn unauthenticated_to_pending_to_authenticated() {
        let (_store, transport, clock, manager) = harness();
        assert_eq!(
            manager.public_session().unwrap().status,
            LoginStatus::Unauthenticated
        );

        transport.push_device_code(device_code("dev-1"));
        let code = manager.start_device_login().unwrap();
        assert_eq!(code.user_code, "ABCD-1234");
        assert_eq!(
            manager.public_session().unwrap().status,
            LoginStatus::Pending
        );

        // Second start reuses the still-valid code: no new upstream request.
        let again = manager.start_device_login().unwrap();
        assert_eq!(again.user_code, "ABCD-1234");

        // Interval gate: an immediate poll performs no upstream request.
        let session = manager.poll().unwrap();
        assert_eq!(session.status, LoginStatus::Pending);

        clock.advance(5_000);
        transport.push_poll(Ok(DevicePollOutcome::Pending));
        assert_eq!(manager.poll().unwrap().status, LoginStatus::Pending);

        clock.advance(5_000);
        transport.push_poll(Ok(DevicePollOutcome::Authorized {
            authorization_code: "auth-code".to_owned(),
            code_verifier: "verifier".to_owned(),
        }));
        transport.push_exchange(Ok(token_response("acct-1", "refresh-1")));
        let session = manager.poll().unwrap();
        assert_eq!(session.status, LoginStatus::Authenticated);

        // Fresh token within margin: no refresh performed.
        let auth = manager.ensure_fresh_auth().unwrap();
        assert_eq!(transport.refresh_calls(), 0);
        assert_eq!(auth.account_id(), "acct-1");
        let debug = format!("{auth:?}");
        assert!(!debug.contains("refresh-1"));
        assert!(debug.contains("acct-1"));
    }

    #[test]
    fn pending_expires_without_authorization() {
        let (_store, transport, clock, manager) = harness();
        transport.push_device_code(device_code("dev-1"));
        manager.start_device_login().unwrap();
        assert_eq!(
            manager.public_session().unwrap().status,
            LoginStatus::Pending
        );
        assert_eq!(manager.poll().unwrap().status, LoginStatus::Pending);

        // Past the 15-minute device TTL, reads report expiry…
        clock.advance(DEVICE_CODE_TTL_MS + 1);
        assert_eq!(
            manager.public_session().unwrap().status,
            LoginStatus::Expired
        );
        // …and the next poll persists the transition.
        assert_eq!(manager.poll().unwrap().status, LoginStatus::Expired);
        // A new login starts cleanly after expiry.
        transport.push_device_code(device_code("dev-2"));
        assert_eq!(manager.start_device_login().unwrap().user_code, "ABCD-1234");
        assert_eq!(
            manager.public_session().unwrap().status,
            LoginStatus::Pending
        );
    }

    #[test]
    fn logout_clears_the_session() {
        let (_store, transport, clock, manager) = harness();
        transport.push_device_code(device_code("dev-1"));
        manager.start_device_login().unwrap();
        clock.advance(5_000);
        transport.push_poll(Ok(DevicePollOutcome::Authorized {
            authorization_code: "c".to_owned(),
            code_verifier: "v".to_owned(),
        }));
        transport.push_exchange(Ok(token_response("acct-1", "r")));
        assert_eq!(manager.poll().unwrap().status, LoginStatus::Authenticated);

        manager.logout().unwrap();
        assert_eq!(
            manager.public_session().unwrap(),
            PublicSession {
                status: LoginStatus::Unauthenticated,
                user: None,
            }
        );
        assert!(manager.ensure_fresh_auth().is_err());
    }

    #[test]
    fn refresh_rotates_and_dead_refresh_expires() {
        let (_store, transport, clock, manager) = harness();
        transport.push_device_code(device_code("dev-1"));
        manager.start_device_login().unwrap();
        clock.advance(5_000);
        transport.push_poll(Ok(DevicePollOutcome::Authorized {
            authorization_code: "c".to_owned(),
            code_verifier: "v".to_owned(),
        }));
        transport.push_exchange(Ok(token_response("acct-1", "refresh-old")));
        assert_eq!(manager.poll().unwrap().status, LoginStatus::Authenticated);

        // Successful rotation inside the margin.
        clock.advance(3_600_000 - 30_000);
        let rotated = test_access_token("acct-1");
        transport.push_refresh(Ok(token_response("acct-1", "refresh-new")));
        let auth = manager.ensure_fresh_auth().unwrap();
        assert_eq!(transport.refresh_calls(), 1);
        assert_eq!(auth.access_token(), rotated);

        // Dead refresh token tears the session down to expired.
        clock.advance(3_600_000);
        transport.push_refresh(Err(ChatGptError::new(
            ChatGptErrorCode::RefreshTokenInvalid,
            "Refresh token is no longer valid",
        )));
        let error = manager.ensure_fresh_auth().unwrap_err();
        assert!(error.is_refresh_token_invalid());
        assert_eq!(
            manager.public_session().unwrap().status,
            LoginStatus::Expired
        );
    }

    #[test]
    fn concurrent_refresh_performs_a_single_rotation() {
        let (_store, transport, clock, manager) = harness();
        transport.push_device_code(device_code("dev-1"));
        manager.start_device_login().unwrap();
        clock.advance(5_000);
        transport.push_poll(Ok(DevicePollOutcome::Authorized {
            authorization_code: "c".to_owned(),
            code_verifier: "v".to_owned(),
        }));
        transport.push_exchange(Ok(token_response("acct-1", "refresh-old")));
        assert_eq!(manager.poll().unwrap().status, LoginStatus::Authenticated);

        // Expire the token, then hammer from N threads: rotation is single.
        clock.advance(3_600_000);
        let rotated = test_access_token("acct-1");
        transport.push_refresh(Ok(token_response("acct-1", "refresh-new")));
        let manager = Arc::new(manager);
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let manager = manager.clone();
                std::thread::spawn(move || {
                    manager
                        .ensure_fresh_auth()
                        .map(|auth| auth.access_token().to_owned())
                })
            })
            .collect();
        let mut tokens = Vec::new();
        for handle in handles {
            tokens.push(handle.join().expect("refresh thread panicked").unwrap());
        }
        assert!(tokens.iter().all(|token| token == &rotated));
        assert_eq!(transport.refresh_calls(), 1);
    }

    #[test]
    fn retryable_poll_failures_stay_pending() {
        let (_store, transport, clock, manager) = harness();
        transport.push_device_code(device_code("dev-1"));
        manager.start_device_login().unwrap();
        clock.advance(5_000);
        // 429-style retryable maps to Pending inside the outcome parser…
        transport.push_poll(Ok(DevicePollOutcome::Pending));
        assert_eq!(manager.poll().unwrap().status, LoginStatus::Pending);
        // …while a hard failure surfaces without killing the login.
        clock.advance(5_000);
        transport.push_poll(Err(ChatGptError::new(
            ChatGptErrorCode::TokenExchangeFailed,
            "Device authorization failed (500)",
        )));
        assert!(manager.poll().is_err());
        assert_eq!(
            manager.public_session().unwrap().status,
            LoginStatus::Pending
        );
    }

    #[test]
    fn public_session_never_contains_tokens() {
        let (_store, transport, clock, manager) = harness();
        transport.push_device_code(device_code("dev-1"));
        manager.start_device_login().unwrap();
        clock.advance(5_000);
        transport.push_poll(Ok(DevicePollOutcome::Authorized {
            authorization_code: "c".to_owned(),
            code_verifier: "v".to_owned(),
        }));
        let access = test_access_token("acct-secret");
        transport.push_exchange(Ok(token_response("acct-secret", "refresh-secret")));
        let session = manager.poll().unwrap();
        let serialized = serde_json::to_string(&session).unwrap();
        assert!(!serialized.contains(&access));
        assert!(!serialized.contains("refresh-secret"));
    }

    fn sign_in(
        transport: &Arc<MockTransport>,
        clock: &Arc<ManualClock>,
        manager: &ChatGptSessionManager,
    ) {
        transport.push_device_code(device_code("dev-1"));
        manager.start_device_login().unwrap();
        clock.advance(5_000);
        transport.push_poll(Ok(DevicePollOutcome::Authorized {
            authorization_code: "c".to_owned(),
            code_verifier: "v".to_owned(),
        }));
        transport.push_exchange(Ok(token_response("acct-1", "refresh-1")));
        assert_eq!(manager.poll().unwrap().status, LoginStatus::Authenticated);
    }

    #[test]
    fn discover_models_returns_account_slugs_with_fresh_auth() {
        let (_store, transport, clock, manager) = harness();
        sign_in(&transport, &clock, &manager);
        transport.push_models(Ok(
            json!({"data": [{"slug": "gpt-5.5"}, {"id": "gpt-5.4"}]}),
        ));
        let slugs = manager.discover_models().unwrap();
        assert_eq!(slugs, vec!["gpt-5.5".to_owned(), "gpt-5.4".to_owned()]);
        // The models call carried the fresh bearer material and account.
        let auth = transport.inner.lock().model_auth.clone();
        assert_eq!(auth.len(), 1);
        assert_eq!(auth[0].1, "acct-1");
        assert!(!auth[0].0.is_empty());
        assert_eq!(transport.refresh_calls(), 0);
    }

    #[test]
    fn discover_models_refreshes_expired_tokens_first() {
        let (_store, transport, clock, manager) = harness();
        sign_in(&transport, &clock, &manager);
        clock.advance(3_600_000);
        transport.push_refresh(Ok(token_response("acct-1", "refresh-2")));
        transport.push_models(Ok(json!({"models": ["gpt-5.5"]})));
        assert_eq!(
            manager.discover_models().unwrap(),
            vec!["gpt-5.5".to_owned()]
        );
        assert_eq!(transport.refresh_calls(), 1);
    }

    #[test]
    fn discover_models_reports_failures_safely() {
        let (_store, transport, clock, manager) = harness();
        // Signed out: no discovery without credentials.
        let error = manager.discover_models().unwrap_err();
        assert_eq!(error.code, ChatGptErrorCode::NotAuthenticated);

        sign_in(&transport, &clock, &manager);
        transport.push_models(Err(ChatGptError::new(
            ChatGptErrorCode::ModelsRequestFailed,
            "Model list request failed (500)",
        )));
        let error = manager.discover_models().unwrap_err();
        assert_eq!(error.code, ChatGptErrorCode::ModelsRequestFailed);

        // Unknown shapes degrade to an empty list, never an error.
        transport.push_models(Ok(json!({"unexpected": []})));
        assert!(manager.discover_models().unwrap().is_empty());

        // A dead refresh surfaces distinctly so callers show Expired.
        clock.advance(3_600_000);
        transport.push_refresh(Err(ChatGptError::new(
            ChatGptErrorCode::RefreshTokenInvalid,
            "Refresh token is no longer valid",
        )));
        let error = manager.discover_models().unwrap_err();
        assert!(error.is_refresh_token_invalid());
    }

    #[test]
    fn pending_login_survives_manager_rebuild() {
        // Restart simulation: a new manager over the same store resumes.
        let store = Arc::new(MemoryStore::default());
        let clock = Arc::new(ManualClock::new(1_000_000));
        let transport = Arc::new(MockTransport::new());
        let first = ChatGptSessionManager::open(store.clone(), transport.clone(), clock.clone());
        transport.push_device_code(device_code("dev-1"));
        first.start_device_login().unwrap();
        drop(first);

        let second = ChatGptSessionManager::open(
            store.clone(),
            Arc::new(MockTransport::new()),
            clock.clone(),
        );
        let display = second
            .pending_login_display()
            .unwrap()
            .expect("login resumes");
        assert_eq!(display.user_code, "ABCD-1234");
        assert_eq!(
            second.public_session().unwrap().status,
            LoginStatus::Pending
        );
    }

    #[test]
    fn encrypted_file_store_round_trips_without_plaintext() {
        let dir = std::env::temp_dir().join(format!("waku-chatgpt-test-{}", unique_suffix()));
        let store = EncryptedFileStore::open(dir.clone()).unwrap();
        let access = test_access_token("acct-xyz");
        let session = StoredSession {
            status: LoginStatus::Authenticated,
            device: None,
            tokens: Some(
                TokenSet::from_token_response(&token_response("acct-xyz", "refresh-xyz"), None, 0)
                    .unwrap(),
            ),
            user: Some(ChatGptUser {
                account_id: "acct-1".to_owned(),
                email: None,
                name: None,
                plan: None,
            }),
            created_at_ms: 1,
            updated_at_ms: 2,
        };
        let envelope = StoredSessionEnvelope {
            status: session.status,
            user: session.user.clone(),
            persisted: Some(session),
        };
        store.save(&envelope).unwrap();

        // No file under the directory contains the raw tokens.
        for entry in std::fs::read_dir(&dir).unwrap() {
            let contents = std::fs::read(entry.unwrap().path()).unwrap();
            assert!(!contains_subslice(&contents, access.as_bytes()));
            assert!(!contains_subslice(&contents, b"refresh-xyz"));
        }

        let loaded = store.load().unwrap().expect("session round-trips");
        assert_eq!(loaded.status, LoginStatus::Authenticated);
        let stored = loaded.persisted.expect("envelope carries state");
        assert_eq!(stored.tokens.unwrap().access_token(), access);

        store.delete().unwrap();
        assert!(store.load().unwrap().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_session_file_recovers_to_signed_out() {
        let dir = std::env::temp_dir().join(format!("waku-chatgpt-corrupt-{}", unique_suffix()));
        let store = EncryptedFileStore::open(dir.clone()).unwrap();
        std::fs::write(dir.join(SESSION_FILE_NAME), b"not a session").unwrap();
        // Corrupt state never strands the user: it reads as signed out.
        assert!(store.load().unwrap().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn curl_status_split_handles_crlf_and_lf() {
        let (status, body) =
            split_status_and_body("HTTP/1.1 200 OK\r\nA: b\r\n\r\n{\"ok\":true}").unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "{\"ok\":true}");
        let (status, body) = split_status_and_body("HTTP/2 429\n\nretry").unwrap();
        assert_eq!(status, 429);
        assert_eq!(body, "retry");
        assert!(split_status_and_body("garbage").is_err());
    }

    fn unique_suffix() -> String {
        use std::sync::atomic::AtomicU64;
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        format!("{}-{}", std::process::id(), id)
    }

    fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }
}
