//! ChatGPT provider wire types: daemon-owned authentication state and model
//! metadata safe to send to clients.
//!
//! By construction these types carry no bearer material — no access tokens,
//! refresh tokens, authorization codes, verifiers, or device ids. The daemon
//! maps its internal session onto [`ChatGptPublicSession`]; clients render
//! only what is here.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Daemon-side ChatGPT login state, mirroring the session manager.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ChatGptLoginStatus {
    #[default]
    Unauthenticated,
    Pending,
    Authenticated,
    Expired,
}

/// Public profile for the signed-in ChatGPT account. Identity claims only.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ChatGptUserInfo {
    pub account_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
}

/// The complete client-visible ChatGPT session. `user_code` and friends are
/// present only while `status` is `pending`; `user` only when authenticated.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ChatGptPublicSession {
    pub status: ChatGptLoginStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<ChatGptUserInfo>,
    /// Safe, user-facing error for the `expired`/`unauthenticated` states.
    /// Never a raw server response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Safe ChatGPT error categories for the UI. The daemon maps its typed
/// errors onto these; raw HTTP/JSON never crosses the wire.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ChatGptErrorKind {
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
        let session = ChatGptPublicSession {
            status: ChatGptLoginStatus::Pending,
            user_code: Some("ABCD-1234".to_owned()),
            verification_url: Some("https://auth.openai.com/codex/device".to_owned()),
            interval_secs: Some(5),
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
            "device_auth_id",
            "deviceAuthId",
            "id_token",
            "idToken",
        ] {
            assert!(!text.contains(forbidden), "wire leaks {forbidden}");
        }
        assert_eq!(json["status"], "pending");
        assert_eq!(json["userCode"], "ABCD-1234");
    }

    #[test]
    fn login_status_round_trips() {
        for status in [
            ChatGptLoginStatus::Unauthenticated,
            ChatGptLoginStatus::Pending,
            ChatGptLoginStatus::Authenticated,
            ChatGptLoginStatus::Expired,
        ] {
            let back: ChatGptLoginStatus =
                serde_json::from_value(serde_json::to_value(status).unwrap()).unwrap();
            assert_eq!(back, status);
        }
    }
}
