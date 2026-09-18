use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;
use uuid::Uuid;

use crate::attachments::{AttachmentUpload, StoredAttachment};
use crate::computer_use::ComputerPermissions;
use crate::model::{
    AgentSession, ChatGptHistorySeed, ClaudeHistorySeed, GoalOperation, Project, ProviderKind,
    ProviderProbe, ProviderResumeCursor, ProviderSessionHistory, ProviderSessionSummary,
    UserInputAnswer,
};
use crate::persistence::{ComposerDraftChange, ComposerDrafts, SessionMessageMatch, StoredMemory};
use crate::provider_session::{ProviderSessionFork, ProviderSessionForkRequest};
use crate::settings::DaemonSettings;
use crate::skills::SkillsCatalog;
use crate::usage::PlanUsage;
use crate::usage_history::MackUsageTotals;
use crate::workspace::{WorkspaceOperation, WorkspaceResult};

pub const PROTOCOL_VERSION: u32 = 8;
pub const MAX_WIRE_MESSAGE_BYTES: usize = 48 * 1024 * 1024;
pub const DAEMON_TOKEN_ENV: &str = "MACK_DAEMON_TOKEN";
pub const DAEMON_ADDRESS_ENV: &str = "MACK_DAEMON_ADDRESS";
pub const APP_EXECUTABLE_ENV: &str = "MACK_APP_EXECUTABLE";

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DaemonReady {
    pub address: String,
    pub protocol_version: u32,
    pub pid: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ClientMessage {
    Hello {
        protocol_version: u32,
        token: String,
        client_id: Uuid,
        #[serde(default)]
        resume_from: Vec<ReplayCursor>,
    },
    Request(Request),
    Shutdown,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub request_id: Uuid,
    pub session_id: Uuid,
    pub runtime_id: Uuid,
    pub command: Command,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ReplayCursor {
    pub session_id: Uuid,
    pub runtime_id: Uuid,
    /// Identifies the daemon process that assigned `sequence`.
    pub epoch: Uuid,
    pub sequence: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Command {
    /// Resolve the daemon-owned provider runtime for an existing task.
    ///
    /// Clients use this after reconnecting or opening the same daemon from a
    /// second app. It observes the session actor without starting, replacing,
    /// or otherwise mutating the provider process.
    AttachSession,
    Start {
        options: WireDriverStartOptions,
    },
    Prompt {
        prompt: String,
        /// The ids the submitting client already gave this turn and its user
        /// message. The daemon republishes them with the submission so every
        /// other client attached to the runtime mirrors the same rows instead
        /// of minting its own; older clients omit them and the daemon mints.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<Uuid>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<Uuid>,
    },
    GenerateTitle {
        prompt: String,
    },
    Steer {
        prompt: String,
    },
    Cancel,
    CancelComputerUse,
    RefreshBackgroundWork,
    StopBackgroundWork {
        key: Value,
        control_id: String,
    },
    Respond {
        request_id: String,
        option_id: String,
    },
    RespondUserInput {
        request_id: String,
        answers: Vec<UserInputAnswer>,
    },
    /// Ask the live provider runtime to read or mutate its persisted thread
    /// goal. Fire-and-forget: the outcome arrives as a `goalUpdated` driver
    /// event, or an `error` event when the provider refuses.
    Goal {
        operation: GoalOperation,
    },
    RunComputerTool {
        request: WireComputerToolRequest,
    },
    RejectComputerTool {
        request: WireComputerToolRequest,
        reason: String,
    },
    ApplyOptions {
        options: WireSessionOptions,
    },
    Rollback {
        turns: usize,
    },
    Fork {
        turns_to_remove: usize,
    },
    GetSettings,
    UpdateSettings {
        settings: DaemonSettings,
    },
    ProbeProvider {
        provider: ProviderKind,
        binary_override: Option<String>,
        discover_models: bool,
        probe_version: bool,
    },
    FetchPlanUsage {
        provider: ProviderKind,
        binary_override: Option<String>,
        cli_version: Option<String>,
    },
    ProbeComputerPermissions {
        prompt: bool,
    },
    /// Lifetime token totals for turns executed inside Mack, summed over
    /// every stored session. No parameters: the answer is a single row of
    /// sums, cheap enough to compute on every call.
    LoadMackUsageTotals,
    LoadSkills {
        projects: Vec<(String, PathBuf)>,
    },
    SetSkillsEnabled {
        dirs: Vec<PathBuf>,
        enabled: bool,
    },
    TrashSkills {
        dirs: Vec<PathBuf>,
    },
    LoadTaskState,
    SaveTaskState {
        projects: Vec<Project>,
        live_session_ids: Vec<Uuid>,
        sessions: Vec<AgentSession>,
    },
    /// Explicitly remove one daemon-owned task. Ordinary state saves are
    /// merge-only so a stale client snapshot cannot delete tasks another
    /// client just created.
    RemoveSession,
    HydrateSession {
        session_id: Uuid,
    },
    SearchSessionMessages {
        query: String,
        limit: usize,
    },
    /// List one provider's resumable CLI conversations on the daemon host.
    ListProviderSessions {
        provider: ProviderKind,
        limit: usize,
    },
    /// Load the user-visible transcript for one provider-native conversation.
    LoadProviderSession {
        cursor: ProviderResumeCursor,
        cwd: PathBuf,
    },
    LoadComposerDrafts,
    SaveComposerDrafts {
        drafts: ComposerDrafts,
        generation: u64,
    },
    ApplyComposerDraftChanges {
        changes: Vec<ComposerDraftChange>,
    },
    StoreBlob {
        mime_type: String,
        #[serde(with = "base64_bytes")]
        #[ts(type = "string")]
        bytes: Vec<u8>,
    },
    ImportAttachment {
        name: String,
        upload: AttachmentUpload,
    },
    ImportPathAttachment {
        #[ts(type = "string")]
        path: PathBuf,
    },
    ReadBlob {
        reference: String,
    },
    ReadAttachment {
        reference: String,
        path: PathBuf,
    },
    SweepBlobs,
    /// Fork a persisted task through one completed provider turn.
    ///
    /// This is intentionally a daemon-owned operation: provider-native
    /// conversation state, Git checkpoint refs, and SQLite all live on the
    /// daemon host and must move together for remote clients.
    ForkSessionFromResponse {
        turn_count: usize,
    },
    /// Restore a task and its provider conversation to immediately before a
    /// prior user message. The client can then submit the edited replacement
    /// as an ordinary new turn.
    RewindSessionToMessage {
        turn_count: usize,
    },
    ForkProviderSession {
        request: ProviderSessionForkRequest,
    },
    Workspace {
        operation: WorkspaceOperation,
    },
    OpenTerminal {
        #[ts(type = "string")]
        cwd: PathBuf,
        cols: u16,
        rows: u16,
    },
    WriteTerminal {
        #[serde(with = "base64_bytes")]
        #[ts(type = "string")]
        data: Vec<u8>,
    },
    ResizeTerminal {
        cols: u16,
        rows: u16,
    },
    CloseTerminal,
    CloseSession,
    /// Start a ChatGPT device login. Returns the user code and verification
    /// URL the user completes in their external browser.
    ChatGptConnect,
    /// Advance the ChatGPT login by one poll/refresh, or report current state.
    ChatGptPoll,
    /// Delete the stored ChatGPT session.
    ChatGptLogout,
    /// Read ChatGPT session state without touching the network.
    ChatGptSession,
    /// Discover the signed-in account's ChatGPT models (refreshing first).
    ChatGptDiscoverModels,
    /// Start a Claude PKCE login. Returns the authorize URL the user
    /// completes in their external browser; the pasted code goes to
    /// `ClaudeCompleteLogin`.
    ClaudeStartLogin,
    /// Complete the Claude login with the user-pasted `CODE`/`CODE#STATE`.
    /// Single-use and short-lived; tokens stay daemon-side.
    ClaudeCompleteLogin {
        code: String,
    },
    /// Delete the stored Claude session.
    ClaudeLogout,
    /// Read Claude session state without touching the network.
    ClaudeSession,
    /// Discover the signed-in account's Claude models (refreshing first,
    /// with a curated fallback when the catalog rejects the token).
    ClaudeDiscoverModels,
    /// List cross-chat memories for the signed-in ChatGPT account. Empty
    /// when signed out.
    ListMemories,
    /// Soft-delete one cross-chat memory by id. No-op when unknown or the
    /// account does not match.
    DeleteMemory {
        id: Uuid,
    },
    /// Soft-delete every cross-chat memory for the signed-in account.
    ClearMemories,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct WireDriverStartOptions {
    pub provider: String,
    pub binary: PathBuf,
    pub cwd: PathBuf,
    pub mode: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    pub context_window: Option<String>,
    pub agent_preset: Option<String>,
    pub computer_use_enabled: bool,
    pub provider_cursor: Option<Value>,
    /// ChatGPT-only resume history, seeded client-side from the persisted
    /// Mack transcript. Every other provider ignores it and keeps its native
    /// resume path. `None` (and empty) means a fresh conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chatgpt_history: Option<Vec<ChatGptHistorySeed>>,
    /// Claude-only resume history, seeded client-side from the persisted
    /// Mack transcript. Same contract as `chatgpt_history`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_history: Option<Vec<ClaudeHistorySeed>>,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct WireSessionOptions {
    pub mode: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    pub context_window: Option<String>,
    pub memory_enabled: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct WireComputerToolRequest {
    pub call_id: String,
    pub tool: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct WireDriverEvent {
    pub kind: String,
    #[serde(default)]
    pub payload: Value,
}

impl WireDriverEvent {
    pub fn new(kind: impl Into<String>, payload: Value) -> Self {
        Self {
            kind: kind.into(),
            payload,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SequencedEvent {
    pub session_id: Uuid,
    pub runtime_id: Uuid,
    /// Changes whenever the daemon restarts, so a reused runtime id can begin
    /// again at sequence one without being mistaken for an old event.
    pub epoch: Uuid,
    pub sequence: u64,
    pub event: WireDriverEvent,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ServerMessage {
    Hello {
        protocol_version: u32,
        daemon_version: String,
    },
    Rejected {
        message: String,
    },
    Response {
        request_id: Uuid,
        outcome: ResponseOutcome,
    },
    Event(SequencedEvent),
    /// The daemon-owned project/task catalog changed through another client.
    /// Clients should invalidate their lightweight task-state snapshot; live
    /// runtime events continue through [`Self::Event`].
    TaskStateChanged {
        revision: u64,
    },
    ShuttingDown,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ResponseOutcome {
    Ok { payload: ResponsePayload },
    Error { error: RpcError },
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ResponsePayload {
    Ack,
    SessionRuntime {
        runtime_id: Option<Uuid>,
        supports_steer: bool,
    },
    Started {
        supports_steer: bool,
    },
    OptionsApplied {
        applied: bool,
    },
    Cursor {
        cursor: Option<Value>,
    },
    Settings {
        settings: DaemonSettings,
    },
    ProviderProbe {
        probe: ProviderProbe,
        version: Option<String>,
    },
    PlanUsage {
        usage: Option<PlanUsage>,
    },
    ComputerPermissions {
        permissions: ComputerPermissions,
    },
    MackUsageTotals {
        totals: MackUsageTotals,
    },
    SkillsCatalog {
        catalog: SkillsCatalog,
    },
    TaskState {
        projects: Vec<Project>,
        sessions: Vec<AgentSession>,
        default_cwd: PathBuf,
        projectless_root: Option<PathBuf>,
    },
    TaskStateSaved {
        sessions: Vec<AgentSession>,
    },
    Session {
        session: Option<AgentSession>,
    },
    SessionMessageMatches {
        matches: Vec<SessionMessageMatch>,
    },
    Memories {
        memories: Vec<StoredMemory>,
    },
    ProviderSessions {
        sessions: Vec<ProviderSessionSummary>,
    },
    ProviderSessionHistory {
        history: ProviderSessionHistory,
    },
    ComposerDrafts {
        drafts: ComposerDrafts,
    },
    BlobStored {
        reference: String,
        path: PathBuf,
    },
    AttachmentStored {
        attachment: StoredAttachment,
    },
    BlobData {
        #[serde(with = "base64_bytes")]
        #[ts(type = "string")]
        bytes: Vec<u8>,
    },
    ProviderSessionForked {
        result: ProviderSessionFork,
    },
    SessionForked {
        session: AgentSession,
        checkpoint_warning: Option<String>,
    },
    SessionRewound {
        session: AgentSession,
        cleanup_warning: Option<String>,
    },
    Workspace {
        result: WorkspaceResult,
    },
    ChatGptSession {
        session: crate::chatgpt::ChatGptPublicSession,
    },
    ChatGptModels {
        models: Vec<crate::model::ProviderModel>,
    },
    ClaudeSession {
        session: crate::claude::ClaudePublicSession,
    },
    ClaudeModels {
        models: Vec<crate::model::ProviderModel>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
pub struct RpcError {
    pub message: String,
}

impl From<anyhow::Error> for RpcError {
    fn from(error: anyhow::Error) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}

mod base64_bytes {
    use base64::Engine as _;
    use serde::{Deserialize as _, Deserializer, Serializer};

    pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_commands_use_stable_camel_case_fields() {
        let list = serde_json::to_value(Command::ListMemories).unwrap();
        assert_eq!(list["type"], "listMemories");

        let id = Uuid::new_v4();
        let delete = serde_json::to_value(Command::DeleteMemory { id }).unwrap();
        assert_eq!(delete["type"], "deleteMemory");
        assert_eq!(delete["id"], id.to_string());
        let Command::DeleteMemory { id: back } = serde_json::from_value(delete).unwrap() else {
            panic!("unexpected command variant");
        };
        assert_eq!(back, id);

        let clear = serde_json::to_value(Command::ClearMemories).unwrap();
        assert_eq!(clear["type"], "clearMemories");

        let payload = serde_json::to_value(ResponsePayload::Memories {
            memories: vec![StoredMemory {
                id,
                content: "The user likes noodles".to_owned(),
                account_id: "acct-1".to_owned(),
                source_session_id: None,
                created_at: 1,
                updated_at: 2,
            }],
        })
        .unwrap();
        assert_eq!(payload["memories"][0]["content"], "The user likes noodles");
        assert_eq!(payload["memories"][0]["accountId"], "acct-1");
    }

    #[test]
    fn binary_payloads_use_base64_json_strings() {
        let payload = ResponsePayload::BlobData {
            bytes: vec![0, 1, 2, 255],
        };
        let json = serde_json::to_value(&payload).unwrap();

        assert_eq!(json["bytes"], "AAEC/w==");
        let ResponsePayload::BlobData { bytes } = serde_json::from_value(json).unwrap() else {
            panic!("unexpected payload variant");
        };
        assert_eq!(bytes, vec![0, 1, 2, 255]);

        let command = Command::WriteTerminal {
            data: vec![0, 1, 2, 255],
        };
        let json = serde_json::to_value(&command).unwrap();
        assert_eq!(json["type"], "writeTerminal");
        assert_eq!(json["data"], "AAEC/w==");
        let Command::WriteTerminal { data } = serde_json::from_value(json).unwrap() else {
            panic!("unexpected command variant");
        };
        assert_eq!(data, vec![0, 1, 2, 255]);
    }

    #[test]
    fn response_fork_command_uses_stable_camel_case_fields() {
        let json =
            serde_json::to_value(Command::ForkSessionFromResponse { turn_count: 7 }).unwrap();

        assert_eq!(json["type"], "forkSessionFromResponse");
        assert_eq!(json["turnCount"], 7);
        assert_eq!(PROTOCOL_VERSION, 8);
    }

    #[test]
    fn message_rewind_command_uses_stable_camel_case_fields() {
        let json = serde_json::to_value(Command::RewindSessionToMessage { turn_count: 4 }).unwrap();

        assert_eq!(json["type"], "rewindSessionToMessage");
        assert_eq!(json["turnCount"], 4);
        assert_eq!(PROTOCOL_VERSION, 8);
    }

    #[test]
    fn provider_session_commands_use_stable_wire_fields() {
        let list = serde_json::to_value(Command::ListProviderSessions {
            provider: ProviderKind::ChatGpt,
            limit: 250,
        })
        .unwrap();
        assert_eq!(list["type"], "listProviderSessions");
        assert_eq!(list["provider"], "chatGpt");
        assert_eq!(list["limit"], 250);

        let load = serde_json::to_value(Command::LoadProviderSession {
            cursor: ProviderResumeCursor::ChatGpt {
                session_id: "01900000-0000-7000-8000-000000000001".into(),
            },
            cwd: PathBuf::from("/tmp/project"),
        })
        .unwrap();
        assert_eq!(load["type"], "loadProviderSession");
        assert_eq!(load["cursor"]["provider"], "chatGpt");
        assert_eq!(
            load["cursor"]["sessionId"],
            "01900000-0000-7000-8000-000000000001"
        );
        assert_eq!(load["cwd"], "/tmp/project");
    }

    #[test]
    fn chatgpt_commands_carry_no_credential_material() {
        // Connect/Poll/Logout/Session/DiscoverModels take no arguments, so by
        // construction the client can send no secret. Assert the wire shapes
        // stay argument-free and name no credential field.
        let forbidden = [
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
        ];
        let commands = [
            Command::ChatGptConnect,
            Command::ChatGptPoll,
            Command::ChatGptLogout,
            Command::ChatGptSession,
            Command::ChatGptDiscoverModels,
        ];
        for command in commands {
            let json = serde_json::to_value(&command).unwrap();
            let text = json.to_string();
            for field in forbidden {
                assert!(!text.contains(field), "command wire leaks {field}");
            }
            // Round-trips keep the daemon/client dispatch aligned.
            let back: Command = serde_json::from_value(json).unwrap();
            assert_eq!(format!("{back:?}"), format!("{command:?}"));
        }
        // The session payload is the public shape only; models carry slugs.
        let session = ResponsePayload::ChatGptSession {
            session: crate::chatgpt::ChatGptPublicSession::default(),
        };
        let text = serde_json::to_value(&session).unwrap().to_string();
        for field in forbidden {
            assert!(!text.contains(field), "session payload leaks {field}");
        }
    }

    #[test]
    fn claude_commands_carry_no_token_material() {
        // Start/Logout/Session/DiscoverModels take no arguments, so by
        // construction the client can send no secret. CompleteLogin carries
        // the single-use pasted code only — never a token or verifier.
        // Assert the wire shapes name no credential field.
        let forbidden = [
            "access_token",
            "accessToken",
            "refresh_token",
            "refreshToken",
            "authorizationCode",
            "code_verifier",
            "codeVerifier",
            "verifier",
        ];
        let commands = [
            Command::ClaudeStartLogin,
            Command::ClaudeCompleteLogin {
                code: "paste-me".to_owned(),
            },
            Command::ClaudeLogout,
            Command::ClaudeSession,
            Command::ClaudeDiscoverModels,
        ];
        for command in commands {
            let json = serde_json::to_value(&command).unwrap();
            let text = json.to_string();
            for field in forbidden {
                assert!(!text.contains(field), "command wire leaks {field}");
            }
            // Round-trips keep the daemon/client dispatch aligned.
            let back: Command = serde_json::from_value(json).unwrap();
            assert_eq!(format!("{back:?}"), format!("{command:?}"));
        }
        // The session payload is the public shape only; models carry slugs.
        let session = ResponsePayload::ClaudeSession {
            session: crate::claude::ClaudePublicSession::default(),
        };
        let text = serde_json::to_value(&session).unwrap().to_string();
        for field in forbidden {
            assert!(!text.contains(field), "session payload leaks {field}");
        }
    }

    #[test]
    fn handshake_and_replay_field_names_are_stable() {
        let session_id = Uuid::nil();
        let runtime_id = Uuid::from_u128(1);
        let message = ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            token: "secret".into(),
            client_id: Uuid::from_u128(2),
            resume_from: vec![ReplayCursor {
                session_id,
                runtime_id,
                epoch: Uuid::from_u128(3),
                sequence: 9,
            }],
        };
        let json = serde_json::to_value(message).unwrap();

        assert_eq!(json["type"], "hello");
        assert_eq!(json["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(json["resumeFrom"][0]["sessionId"], session_id.to_string());
        assert_eq!(json["resumeFrom"][0]["runtimeId"], runtime_id.to_string());
        assert_eq!(
            json["resumeFrom"][0]["epoch"],
            Uuid::from_u128(3).to_string()
        );
        assert!(json.get("protocol_version").is_none());
    }

    #[test]
    fn composer_draft_changes_have_stable_wire_keys() {
        let project_id = Uuid::from_u128(7);
        let command = Command::ApplyComposerDraftChanges {
            changes: vec![ComposerDraftChange {
                target: crate::persistence::ComposerDraftTarget::NewSession { project_id },
                draft: Some(crate::persistence::ComposerDraft {
                    text: "unfinished".into(),
                    attachments: Vec::new(),
                }),
            }],
        };
        let json = serde_json::to_value(command).unwrap();

        assert_eq!(json["type"], "applyComposerDraftChanges");
        assert_eq!(json["changes"][0]["target"]["type"], "newSession");
        assert_eq!(
            json["changes"][0]["target"]["projectId"],
            project_id.to_string()
        );
        assert_eq!(json["changes"][0]["draft"]["text"], "unfinished");
    }
}
