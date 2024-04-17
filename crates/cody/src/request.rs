use serde::{Deserialize, Serialize};

pub enum CheckStatus {}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckStatusParams {
    pub local_checks_only: bool,
}

impl lsp::request::Request for CheckStatus {
    type Params = CheckStatusParams;
    type Result = SignInStatus;
    const METHOD: &'static str = "checkStatus";
}

pub enum Initialize {}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionConfiguration {
    pub server_endpoint: String,
    pub access_token: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    /// The process Id of the parent process that started
    /// the server. Is null if the process has not been started by another process.
    /// If the parent process is not alive then the server should exit (see exit notification) its process.
    pub process_id: Option<u32>,

    /// The rootPath of the workspace. Is null
    /// if no folder is open.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[deprecated(note = "Use `root_uri` instead when possible")]
    pub root_path: Option<String>,

    /// The rootUri of the workspace. Is null if no
    /// folder is open. If both `rootPath` and `rootUri` are set
    /// `rootUri` wins.
    ///
    /// Deprecated in favour of `workspaceFolders`
    #[serde(default)]
    pub root_uri: Option<lsp::Url>,

    /// User provided initialization options.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initialization_options: Option<serde_json::value::Value>,

    /// The capabilities provided by the client (editor or tool)
    pub capabilities: lsp::ClientCapabilities,

    /// The initial trace setting. If omitted trace is disabled ('off').
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace: Option<lsp::TraceValue>,

    /// The workspace folders configured in the client when the server starts.
    /// This property is only available if the client supports workspace folders.
    /// It can be `null` if the client supports workspace folders but none are
    /// configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_folders: Option<Vec<lsp::WorkspaceFolder>>,

    /// Information about the client.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_info: Option<lsp::ClientInfo>,

    /// The locale the client is currently showing the user interface
    /// in. This must not necessarily be the locale of the operating
    /// system.
    ///
    /// Uses IETF language tags as the value's syntax
    /// (See <https://en.wikipedia.org/wiki/IETF_language_tag>)
    ///
    /// @since 3.16.0
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub extension_configuration: Option<ExtensionConfiguration>,
}

#[derive(Debug, PartialEq, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    /// The capabilities the language server provides.
    pub capabilities: lsp::ServerCapabilities,

    /// Information about the server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_info: Option<lsp::ServerInfo>,

    /// Unofficial UT8-offsets extension.
    ///
    /// See https://clangd.llvm.org/extensions.html#utf-8-offsets.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg(feature = "proposed")]
    pub offset_encoding: Option<String>,
}

impl lsp::request::Request for Initialize {
    type Params = InitializeParams;
    type Result = InitializeResult;
    const METHOD: &'static str = "initialize";
}

pub enum SignInInitiate {}

#[derive(Debug, Serialize, Deserialize)]
pub struct SignInInitiateParams {}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status")]
pub enum SignInInitiateResult {
    AlreadySignedIn { user: String },
    PromptUserDeviceFlow(PromptUserDeviceFlow),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptUserDeviceFlow {
    pub user_code: String,
    pub verification_uri: String,
}

impl lsp::request::Request for SignInInitiate {
    type Params = SignInInitiateParams;
    type Result = SignInInitiateResult;
    const METHOD: &'static str = "signInInitiate";
}

pub enum SignInConfirm {}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignInConfirmParams {
    pub user_code: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status")]
pub enum SignInStatus {
    #[serde(rename = "OK")]
    Ok {
        user: Option<String>,
    },
    MaybeOk {
        user: String,
    },
    AlreadySignedIn {
        user: String,
    },
    NotAuthorized {
        user: String,
    },
    NotSignedIn,
}

impl lsp::request::Request for SignInConfirm {
    type Params = SignInConfirmParams;
    type Result = SignInStatus;
    const METHOD: &'static str = "signInConfirm";
}

pub enum SignOut {}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignOutParams {}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignOutResult {}

impl lsp::request::Request for SignOut {
    type Params = SignOutParams;
    type Result = SignOutResult;
    const METHOD: &'static str = "signOut";
}

pub enum GetCompletions {}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetCompletionsParams {
    pub uri: String,
    pub position: lsp::Position,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetCompletionsDocument {
    pub tab_size: u32,
    pub indent_size: u32,
    pub insert_spaces: bool,
    pub uri: lsp::Url,
    pub relative_path: String,
    pub position: lsp::Position,
    pub version: usize,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetCompletionsResult {
    #[serde(rename(deserialize = "items"))]
    pub completions: Vec<Completion>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Completion {
    pub insert_text: String,
    pub id: String,
    pub range: lsp::Range,
}

impl lsp::request::Request for GetCompletions {
    type Params = GetCompletionsParams;
    type Result = GetCompletionsResult;
    const METHOD: &'static str = "autocomplete/execute";
}

pub enum GetCompletionsCycling {}

impl lsp::request::Request for GetCompletionsCycling {
    type Params = GetCompletionsParams;
    type Result = GetCompletionsResult;
    const METHOD: &'static str = "getCompletionsCycling";
}

pub enum DidOpenTextDocument {}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DidOpenTextDocumentParams {
    pub uri: String,
    pub content: String,
}

impl lsp::notification::Notification for DidOpenTextDocument {
    type Params = DidOpenTextDocumentParams;
    const METHOD: &'static str = "textDocument/didOpen";
}

pub enum DidChangeTextDocument {}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DidChangeTextDocumentParams {
    pub uri: String,
    pub content: String,
}

impl lsp::notification::Notification for DidChangeTextDocument {
    type Params = DidChangeTextDocumentParams;
    const METHOD: &'static str = "textDocument/didChange";
}

pub enum LogMessage {}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogMessageParams {
    pub level: u8,
    pub message: String,
    pub metadata_str: String,
    pub extra: Vec<String>,
}

impl lsp::notification::Notification for LogMessage {
    type Params = LogMessageParams;
    const METHOD: &'static str = "LogMessage";
}

pub enum StatusNotification {}

#[derive(Debug, Serialize, Deserialize)]
pub struct StatusNotificationParams {
    pub message: String,
    pub status: String, // One of Normal/InProgress
}

impl lsp::notification::Notification for StatusNotification {
    type Params = StatusNotificationParams;
    const METHOD: &'static str = "statusNotification";
}

pub enum SetEditorInfo {}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetEditorInfoParams {
    pub editor_info: EditorInfo,
    pub editor_plugin_info: EditorPluginInfo,
}

impl lsp::request::Request for SetEditorInfo {
    type Params = SetEditorInfoParams;
    type Result = String;
    const METHOD: &'static str = "setEditorInfo";
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorPluginInfo {
    pub name: String,
    pub version: String,
}

pub enum NotifyAccepted {}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotifyAcceptedParams {
    pub uuid: String,
}

impl lsp::request::Request for NotifyAccepted {
    type Params = NotifyAcceptedParams;
    type Result = String;
    const METHOD: &'static str = "notifyAccepted";
}

pub enum NotifyRejected {}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotifyRejectedParams {
    pub uuids: Vec<String>,
}

impl lsp::request::Request for NotifyRejected {
    type Params = NotifyRejectedParams;
    type Result = String;
    const METHOD: &'static str = "notifyRejected";
}
