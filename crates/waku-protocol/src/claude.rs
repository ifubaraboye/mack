//! Claude subscription provider wire types: daemon-owned authentication state
//! and model metadata safe to send to clients.
//!
//! By construction these types carry no bearer material — no access tokens,
//! refresh tokens, authorization codes, or PKCE verifiers. The daemon maps
//! its internal session onto [`ClaudePublicSession`]; clients render only
//! what is here. The pasted authorization code travels daemon-bound inside
//! `Command::ClaudeCompleteLogin`, never back out.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Daemon-side Claude login state, mirroring the session manager.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ClaudeLoginStatus {
    #[default]
    Unauthenticated,
    Pending,
    Authenticated,
    Expired,
}

/// Public profile for the signed-in Claude account. Identity claims only.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeUserInfo {
    pub account_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
}

/// The complete client-visible Claude session. `authorize_url` and `state`
/// are present only while `status` is `pending`; `user` only when
/// authenticated. The PKCE verifier never crosses the wire in either
/// direction.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ClaudePublicSession {
    pub status: ClaudeLoginStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorize_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<ClaudeUserInfo>,
    /// Safe, user-facing error for the `expired`/`unauthenticated` states.
    /// Never a raw server response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Safe Claude error categories for the UI. The daemon maps its typed
/// errors onto these; raw HTTP/JSON never crosses the wire.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ClaudeErrorKind {
    AuthenticationFailed,
    AuthorizationExpired,
    SessionExpired,
    NetworkError,
    RateLimited,
    ModelDiscoveryFailed,
    ServiceUnavailable,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_session_carries_no_credential_fields() {
        let session = ClaudePublicSession {
            status: ClaudeLoginStatus::Pending,
            authorize_url: Some("https://claude.ai/oauth/authorize?code=true".to_owned()),
            state: Some("abc123".to_owned()),
            expires_at_ms: Some(1_000),
            user: None,
            error: None,
        };
        let json = serde_json::to_value(&session).unwrap();
        let text = json.to_string();
        for forbidden in [
            "access_token",
            "accessToken",
            "refresh_token",
            "refreshToken",
            "authorization_code",
            "authorizationCode",
            "code_verifier",
            "codeVerifier",
            "verifier",
        ] {
            assert!(!text.contains(forbidden), "wire leaks {forbidden}");
        }
        assert_eq!(json["status"], "pending");
        assert_eq!(
            json["authorizeUrl"],
            "https://claude.ai/oauth/authorize?code=true"
        );
    }

    #[test]
    fn login_status_round_trips() {
        for status in [
            ClaudeLoginStatus::Unauthenticated,
            ClaudeLoginStatus::Pending,
            ClaudeLoginStatus::Authenticated,
            ClaudeLoginStatus::Expired,
        ] {
            let back: ClaudeLoginStatus =
                serde_json::from_value(serde_json::to_value(status).unwrap()).unwrap();
            assert_eq!(back, status);
        }
    }
}
