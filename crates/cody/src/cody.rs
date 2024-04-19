pub mod request;
use anyhow::{anyhow, Result};
use collections::{HashMap, HashSet};
use command_palette_hooks::CommandPaletteFilter;
use futures::{channel::oneshot, future::Shared, Future, FutureExt, TryFutureExt};
use gpui::{
    actions, AppContext, AsyncAppContext, Context, Entity, EntityId, EventEmitter, Global, Model,
    ModelContext, Task, WeakModel,
};
use language::{
    language_settings::{all_language_settings, language_settings},
    point_from_lsp, point_to_lsp, Anchor, Bias, Buffer, BufferSnapshot, Language,
    LanguageServerName, PointUtf16, ToPointUtf16,
};
use lsp::{LanguageServer, LanguageServerBinary, LanguageServerId};
use node_runtime::NodeRuntime;
use parking_lot::Mutex;
use request::StatusNotification;
use serde_json::json;
use settings::SettingsStore;
use smol::{fs, stream::StreamExt};
use std::{
    any::TypeId,
    ffi::OsString,
    mem,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};
use util::{fs::remove_matching, http::HttpClient, maybe, paths, ResultExt};

actions!(
    cody,
    [
        Suggest,
        NextSuggestion,
        PreviousSuggestion,
        Reinstall,
        SignIn,
        SignOut
    ]
);

pub fn init(
    new_server_id: LanguageServerId,
    http: Arc<dyn HttpClient>,
    node_runtime: Arc<dyn NodeRuntime>,
    cx: &mut AppContext,
) {
    let cody = cx.new_model({
        let node_runtime = node_runtime.clone();
        move |cx| Cody::start(new_server_id, http, node_runtime, cx)
    });
    Cody::set_global(cody.clone(), cx);
    cx.observe(&cody, |handle, cx| {
        let cody_action_types = [
            TypeId::of::<Suggest>(),
            TypeId::of::<NextSuggestion>(),
            TypeId::of::<PreviousSuggestion>(),
            TypeId::of::<Reinstall>(),
        ];
        let cody_auth_action_types = [TypeId::of::<SignOut>()];
        let cody_no_auth_action_types = [TypeId::of::<SignIn>()];
        let status = handle.read(cx).status();
        let filter = CommandPaletteFilter::global_mut(cx);

        match status {
            Status::Disabled => {
                filter.hide_action_types(&cody_action_types);
                filter.hide_action_types(&cody_auth_action_types);
                filter.hide_action_types(&cody_no_auth_action_types);
            }
            Status::Authorized => {
                filter.hide_action_types(&cody_no_auth_action_types);
                filter.show_action_types(cody_action_types.iter().chain(&cody_auth_action_types));
            }
            _ => {
                filter.hide_action_types(&cody_action_types);
                filter.hide_action_types(&cody_auth_action_types);
                filter.show_action_types(cody_no_auth_action_types.iter());
            }
        }
    })
    .detach();

    cx.on_action(|_: &SignIn, cx| {
        if let Some(cody) = Cody::global(cx) {
            cody.update(cx, |cody, cx| cody.sign_in(cx))
                .detach_and_log_err(cx);
        }
    });
    cx.on_action(|_: &SignOut, cx| {
        if let Some(cody) = Cody::global(cx) {
            cody.update(cx, |cody, cx| cody.sign_out(cx))
                .detach_and_log_err(cx);
        }
    });
    cx.on_action(|_: &Reinstall, cx| {
        if let Some(cody) = Cody::global(cx) {
            cody.update(cx, |cody, cx| cody.reinstall(cx)).detach();
        }
    });
}

enum CodyServer {
    Disabled,
    Starting { task: Shared<Task<()>> },
    Error(Arc<str>),
    Running(RunningCodyServer),
}

impl CodyServer {
    fn as_authenticated(&mut self) -> Result<&mut RunningCodyServer> {
        let server = self.as_running()?;
        if matches!(server.sign_in_status, SignInStatus::Authorized { .. }) {
            Ok(server)
        } else {
            Err(anyhow!("must sign in before using cody"))
        }
    }

    fn as_running(&mut self) -> Result<&mut RunningCodyServer> {
        match self {
            CodyServer::Starting { .. } => Err(anyhow!("cody is still starting")),
            CodyServer::Disabled => Err(anyhow!("cody is disabled")),
            CodyServer::Error(error) => Err(anyhow!(
                "cody was not started because of an error: {}",
                error
            )),
            CodyServer::Running(server) => Ok(server),
        }
    }
}

struct RunningCodyServer {
    name: LanguageServerName,
    lsp: Arc<LanguageServer>,
    sign_in_status: SignInStatus,
    registered_buffers: HashMap<EntityId, RegisteredBuffer>,
}

#[derive(Clone, Debug)]
enum SignInStatus {
    Authorized,
    Unauthorized,
    SigningIn {
        prompt: Option<request::PromptUserDeviceFlow>,
        task: Shared<Task<Result<(), Arc<anyhow::Error>>>>,
    },
    SignedOut,
}

#[derive(Debug, Clone)]
pub enum Status {
    Starting {
        task: Shared<Task<()>>,
    },
    Error(Arc<str>),
    Disabled,
    SignedOut,
    SigningIn {
        prompt: Option<request::PromptUserDeviceFlow>,
    },
    Unauthorized,
    Authorized,
}

impl Status {
    pub fn is_authorized(&self) -> bool {
        matches!(self, Status::Authorized)
    }
}

struct RegisteredBuffer {
    uri: lsp::Url,
    language_id: String,
    snapshot: BufferSnapshot,
    snapshot_version: i32,
    _subscriptions: [gpui::Subscription; 2],
    pending_buffer_change: Task<Option<()>>,
}

impl RegisteredBuffer {
    fn report_changes(
        &mut self,
        buffer: &Model<Buffer>,
        cx: &mut ModelContext<Cody>,
    ) -> oneshot::Receiver<(i32, BufferSnapshot)> {
        let (done_tx, done_rx) = oneshot::channel();

        if buffer.read(cx).version() == self.snapshot.version {
            let _ = done_tx.send((self.snapshot_version, self.snapshot.clone()));
        } else {
            let buffer = buffer.downgrade();
            let id = buffer.entity_id();
            let prev_pending_change =
                mem::replace(&mut self.pending_buffer_change, Task::ready(None));
            self.pending_buffer_change = cx.spawn(move |cody, mut cx| async move {
                prev_pending_change.await;

                let old_version = cody
                    .update(&mut cx, |cody, _| {
                        let server = cody.server.as_authenticated().log_err()?;
                        let buffer = server.registered_buffers.get_mut(&id)?;
                        Some(buffer.snapshot.version.clone())
                    })
                    .ok()??;
                let new_snapshot = buffer.update(&mut cx, |buffer, _| buffer.snapshot()).ok()?;

                let content_changes = cx
                    .background_executor()
                    .spawn({
                        let new_snapshot = new_snapshot.clone();
                        async move {
                            new_snapshot
                                .edits_since::<(PointUtf16, usize)>(&old_version)
                                .map(|edit| {
                                    let edit_start = edit.new.start.0;
                                    let edit_end = edit_start + (edit.old.end.0 - edit.old.start.0);
                                    let new_text = new_snapshot
                                        .text_for_range(edit.new.start.1..edit.new.end.1)
                                        .collect();
                                    lsp::TextDocumentContentChangeEvent {
                                        range: Some(lsp::Range::new(
                                            point_to_lsp(edit_start),
                                            point_to_lsp(edit_end),
                                        )),
                                        range_length: None,
                                        text: new_text,
                                    }
                                })
                                .collect::<Vec<_>>()
                        }
                    })
                    .await;

                cody.update(&mut cx, |cody, _| {
                    let server = cody.server.as_authenticated().log_err()?;
                    let buffer = server.registered_buffers.get_mut(&id)?;
                    if !content_changes.is_empty() {
                        buffer.snapshot_version += 1;
                        buffer.snapshot = new_snapshot;
                        server
                            .lsp
                            .notify::<request::DidChangeTextDocument>(
                                request::DidChangeTextDocumentParams {
                                    uri: buffer.uri.clone().to_string(),
                                    content: buffer.snapshot.text(),
                                },
                            )
                            .log_err();
                    }
                    let _ = done_tx.send((buffer.snapshot_version, buffer.snapshot.clone()));
                    Some(())
                })
                .ok()?;

                Some(())
            });
        }

        done_rx
    }
}

#[derive(Debug)]
pub struct Completion {
    pub uuid: String,
    pub range: Range<Anchor>,
    pub text: String,
}

pub struct Cody {
    http: Arc<dyn HttpClient>,
    node_runtime: Arc<dyn NodeRuntime>,
    server: CodyServer,
    buffers: HashSet<WeakModel<Buffer>>,
    server_id: LanguageServerId,
    _subscription: gpui::Subscription,
}

pub enum Event {
    CodyLanguageServerStarted,
}

impl EventEmitter<Event> for Cody {}

struct GlobalCody(Model<Cody>);

impl Global for GlobalCody {}

impl Cody {
    pub fn global(cx: &AppContext) -> Option<Model<Self>> {
        cx.try_global::<GlobalCody>().map(|model| model.0.clone())
    }

    pub fn set_global(cody: Model<Self>, cx: &mut AppContext) {
        cx.set_global(GlobalCody(cody));
    }

    fn start(
        new_server_id: LanguageServerId,
        http: Arc<dyn HttpClient>,
        node_runtime: Arc<dyn NodeRuntime>,
        cx: &mut ModelContext<Self>,
    ) -> Self {
        let mut this = Self {
            server_id: new_server_id,
            http,
            node_runtime,
            server: CodyServer::Disabled,
            buffers: Default::default(),
            _subscription: cx.on_app_quit(Self::shutdown_language_server),
        };
        this.enable_or_disable_cody(cx);
        cx.observe_global::<SettingsStore>(move |this, cx| this.enable_or_disable_cody(cx))
            .detach();
        this
    }

    fn shutdown_language_server(
        &mut self,
        _cx: &mut ModelContext<Self>,
    ) -> impl Future<Output = ()> {
        let shutdown = match mem::replace(&mut self.server, CodyServer::Disabled) {
            CodyServer::Running(server) => Some(Box::pin(async move { server.lsp.shutdown() })),
            _ => None,
        };

        async move {
            if let Some(shutdown) = shutdown {
                shutdown.await;
            }
        }
    }

    fn enable_or_disable_cody(&mut self, cx: &mut ModelContext<Self>) {
        let server_id = self.server_id;
        let http = self.http.clone();
        let node_runtime = self.node_runtime.clone();
        if all_language_settings(None, cx).copilot_enabled(None, None) {
            if matches!(self.server, CodyServer::Disabled) {
                let start_task = cx
                    .spawn(move |this, cx| {
                        Self::start_language_server(server_id, http, node_runtime, this, cx)
                    })
                    .shared();
                self.server = CodyServer::Starting { task: start_task };
                cx.notify();
            }
        } else {
            self.server = CodyServer::Disabled;
            cx.notify();
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn fake(cx: &mut gpui::TestAppContext) -> (Model<Self>, lsp::FakeLanguageServer) {
        use lsp::FakeLanguageServer;
        use node_runtime::FakeNodeRuntime;

        let (server, fake_server) = FakeLanguageServer::new(
            LanguageServerBinary {
                path: "path/to/cody".into(),
                arguments: vec![],
                env: None,
            },
            "cody".into(),
            Default::default(),
            cx.to_async(),
        );
        let http = util::http::FakeHttpClient::create(|_| async { unreachable!() });
        let node_runtime = FakeNodeRuntime::new();
        let this = cx.new_model(|cx| Self {
            server_id: LanguageServerId(0),
            http: http.clone(),
            node_runtime,
            server: CodyServer::Running(RunningCodyServer {
                name: LanguageServerName(Arc::from("cody")),
                lsp: Arc::new(server),
                sign_in_status: SignInStatus::Authorized,
                registered_buffers: Default::default(),
            }),
            _subscription: cx.on_app_quit(Self::shutdown_language_server),
            buffers: Default::default(),
        });
        (this, fake_server)
    }

    fn start_language_server(
        new_server_id: LanguageServerId,
        http: Arc<dyn HttpClient>,
        node_runtime: Arc<dyn NodeRuntime>,
        this: WeakModel<Self>,
        mut cx: AsyncAppContext,
    ) -> impl Future<Output = ()> {
        async move {
            let start_language_server = async {
                let server_path = get_cody_lsp(http).await?;
                let node_path = node_runtime.binary_path().await?;
                let arguments: Vec<OsString> = vec![server_path.into()];
                let mut env = HashMap::default();
                env.insert(
                    String::from("SRC_ENDPOINT"),
                    String::from("https://sourcegraph.com"),
                );
                env.insert(
                    String::from("SRC_ACCESS_TOKEN"),
                    std::env::var("SRC_ACCESS_TOKEN").unwrap(),
                );
                env.insert(
                    String::from("CODY_AGENT_TRACE_PATH"),
                    String::from("/Users/pjlast/workspace/pjlast/zed/codyagent.json"),
                );
                let binary = LanguageServerBinary {
                    path: node_path,
                    arguments,
                    // TODO: We could set HTTP_PROXY etc here and fix the cody issue.
                    env: Some(env),
                };

                let server = LanguageServer::new(
                    Arc::new(Mutex::new(None)),
                    new_server_id,
                    binary,
                    Path::new("/"),
                    None,
                    cx.clone(),
                )?;

                server
                    .on_notification::<StatusNotification, _>(
                        |_, _| { /* Silence the notification */ },
                    )
                    .detach();
                // let server = cx.update(|cx| server.initialize(None, cx))?.await?;

                let server = cx
                    .update(|cx| {
                        let root_uri = lsp::Url::from_file_path(&server.root_path()).unwrap();
                        #[allow(deprecated)]
                        let params = request::InitializeParams {
                            process_id: None,
                            root_path: None,
                            root_uri: Some(root_uri.clone()),
                            initialization_options: None,
                            extension_configuration: Some(request::ExtensionConfiguration {
                                server_endpoint: String::from("https://sourcegraph.com/"),
                                access_token: std::env::var("SRC_ACCESS_TOKEN").unwrap(),
                            }),
                            capabilities: lsp::ClientCapabilities {
                                workspace: Some(lsp::WorkspaceClientCapabilities {
                                    configuration: Some(true),
                                    did_change_watched_files: Some(
                                        lsp::DidChangeWatchedFilesClientCapabilities {
                                            dynamic_registration: Some(true),
                                            relative_pattern_support: Some(true),
                                        },
                                    ),
                                    did_change_configuration: Some(
                                        lsp::DynamicRegistrationClientCapabilities {
                                            dynamic_registration: Some(true),
                                        },
                                    ),
                                    workspace_folders: Some(true),
                                    symbol: Some(lsp::WorkspaceSymbolClientCapabilities {
                                        resolve_support: None,
                                        ..lsp::WorkspaceSymbolClientCapabilities::default()
                                    }),
                                    inlay_hint: Some(lsp::InlayHintWorkspaceClientCapabilities {
                                        refresh_support: Some(true),
                                    }),
                                    diagnostic: Some(lsp::DiagnosticWorkspaceClientCapabilities {
                                        refresh_support: None,
                                    }),
                                    workspace_edit: Some(lsp::WorkspaceEditClientCapabilities {
                                        resource_operations: Some(vec![
                                            lsp::ResourceOperationKind::Create,
                                            lsp::ResourceOperationKind::Rename,
                                            lsp::ResourceOperationKind::Delete,
                                        ]),
                                        document_changes: Some(true),
                                        ..lsp::WorkspaceEditClientCapabilities::default()
                                    }),
                                    ..Default::default()
                                }),
                                text_document: Some(lsp::TextDocumentClientCapabilities {
                                    definition: Some(lsp::GotoCapability {
                                        link_support: Some(true),
                                        dynamic_registration: None,
                                    }),
                                    code_action: Some(lsp::CodeActionClientCapabilities {
                                        code_action_literal_support: Some(
                                            lsp::CodeActionLiteralSupport {
                                                code_action_kind:
                                                    lsp::CodeActionKindLiteralSupport {
                                                        value_set: vec![
                                                            lsp::CodeActionKind::REFACTOR
                                                                .as_str()
                                                                .into(),
                                                            lsp::CodeActionKind::QUICKFIX
                                                                .as_str()
                                                                .into(),
                                                            lsp::CodeActionKind::SOURCE
                                                                .as_str()
                                                                .into(),
                                                        ],
                                                    },
                                            },
                                        ),
                                        data_support: Some(true),
                                        resolve_support: Some(
                                            lsp::CodeActionCapabilityResolveSupport {
                                                properties: vec![
                                                    "kind".to_string(),
                                                    "diagnostics".to_string(),
                                                    "isPreferred".to_string(),
                                                    "disabled".to_string(),
                                                    "edit".to_string(),
                                                    "command".to_string(),
                                                ],
                                            },
                                        ),
                                        ..Default::default()
                                    }),
                                    completion: Some(lsp::CompletionClientCapabilities {
                                        completion_item: Some(lsp::CompletionItemCapability {
                                            snippet_support: Some(true),
                                            resolve_support: Some(
                                                lsp::CompletionItemCapabilityResolveSupport {
                                                    properties: vec![
                                                        "documentation".to_string(),
                                                        "additionalTextEdits".to_string(),
                                                    ],
                                                },
                                            ),
                                            insert_replace_support: Some(true),
                                            ..Default::default()
                                        }),
                                        completion_list: Some(lsp::CompletionListCapability {
                                            item_defaults: Some(vec![
                                                "commitCharacters".to_owned(),
                                                "editRange".to_owned(),
                                                "insertTextMode".to_owned(),
                                                "data".to_owned(),
                                            ]),
                                        }),
                                        ..Default::default()
                                    }),
                                    rename: Some(lsp::RenameClientCapabilities {
                                        prepare_support: Some(true),
                                        ..Default::default()
                                    }),
                                    hover: Some(lsp::HoverClientCapabilities {
                                        content_format: Some(vec![lsp::MarkupKind::Markdown]),
                                        dynamic_registration: None,
                                    }),
                                    inlay_hint: Some(lsp::InlayHintClientCapabilities {
                                        resolve_support: Some(
                                            lsp::InlayHintResolveClientCapabilities {
                                                properties: vec![
                                                    "textEdits".to_string(),
                                                    "tooltip".to_string(),
                                                    "label.tooltip".to_string(),
                                                    "label.location".to_string(),
                                                    "label.command".to_string(),
                                                ],
                                            },
                                        ),
                                        dynamic_registration: Some(false),
                                    }),
                                    publish_diagnostics: Some(
                                        lsp::PublishDiagnosticsClientCapabilities {
                                            related_information: Some(true),
                                            ..Default::default()
                                        },
                                    ),
                                    formatting: Some(lsp::DynamicRegistrationClientCapabilities {
                                        dynamic_registration: None,
                                    }),
                                    on_type_formatting: Some(
                                        lsp::DynamicRegistrationClientCapabilities {
                                            dynamic_registration: None,
                                        },
                                    ),
                                    diagnostic: Some(lsp::DiagnosticClientCapabilities {
                                        related_document_support: Some(true),
                                        dynamic_registration: None,
                                    }),
                                    ..Default::default()
                                }),
                                experimental: Some(json!({
                                    "serverStatusNotification": true,
                                })),
                                window: Some(lsp::WindowClientCapabilities {
                                    work_done_progress: Some(true),
                                    ..Default::default()
                                }),
                                general: None,
                            },
                            trace: None,
                            workspace_folders: Some(vec![lsp::WorkspaceFolder {
                                uri: root_uri,
                                name: Default::default(),
                            }]),
                            client_info: release_channel::ReleaseChannel::try_global(cx).map(
                                |release_channel| lsp::ClientInfo {
                                    name: release_channel.display_name().to_string(),
                                    version: Some(
                                        release_channel::AppVersion::global(cx).to_string(),
                                    ),
                                },
                            ),
                            locale: None,
                        };
                        // server.request::<request::Initialize>(params)
                        cx.spawn(|_| async move {
                            let response = server.request::<request::Initialize>(params).await?;
                            let server = if let Some(info) = response.server_info {
                                server.set_name(info.name)
                            } else {
                                server
                            };
                            let server = server.set_capabilities(response.capabilities);

                            server.notify::<lsp::notification::Initialized>(
                                lsp::InitializedParams {},
                            )?;
                            Ok::<std::sync::Arc<LanguageServer>, anyhow::Error>(Arc::new(server))
                        })
                    })?
                    .await?;

                // println!("Get status");
                // let status = server
                //     .request::<request::CheckStatus>(request::CheckStatusParams {
                //         local_checks_only: false,
                //     })
                //     .await?;

                // println!("Set editor info");
                // server
                //     .request::<request::SetEditorInfo>(request::SetEditorInfoParams {
                //         editor_info: request::EditorInfo {
                //             name: "zed".into(),
                //             version: env!("CARGO_PKG_VERSION").into(),
                //         },
                //         editor_plugin_info: request::EditorPluginInfo {
                //             name: "zed-cody".into(),
                //             version: "0.0.1".into(),
                //         },
                //     })
                //     .await?;

                anyhow::Ok((
                    server,
                    request::SignInStatus::Ok {
                        user: Some("pjlast".to_string()),
                    },
                ))
            };

            let server = start_language_server.await;
            this.update(&mut cx, |this, cx| {
                cx.notify();
                match server {
                    Ok((server, status)) => {
                        this.server = CodyServer::Running(RunningCodyServer {
                            name: LanguageServerName(Arc::from("cody")),
                            lsp: server,
                            sign_in_status: SignInStatus::SignedOut,
                            registered_buffers: Default::default(),
                        });
                        cx.emit(Event::CodyLanguageServerStarted);
                        this.update_sign_in_status(status, cx);
                    }
                    Err(error) => {
                        this.server = CodyServer::Error(error.to_string().into());
                        cx.notify()
                    }
                }
            })
            .ok();
        }
    }

    pub fn sign_in(&mut self, cx: &mut ModelContext<Self>) -> Task<Result<()>> {
        if let CodyServer::Running(server) = &mut self.server {
            let task = match &server.sign_in_status {
                SignInStatus::Authorized { .. } => Task::ready(Ok(())).shared(),
                SignInStatus::SigningIn { task, .. } => {
                    cx.notify();
                    task.clone()
                }
                SignInStatus::SignedOut | SignInStatus::Unauthorized { .. } => {
                    let lsp = server.lsp.clone();
                    let task = cx
                        .spawn(|this, mut cx| async move {
                            let sign_in = async {
                                let sign_in = lsp
                                    .request::<request::SignInInitiate>(
                                        request::SignInInitiateParams {},
                                    )
                                    .await?;
                                match sign_in {
                                    request::SignInInitiateResult::AlreadySignedIn { user } => {
                                        Ok(request::SignInStatus::Ok { user: Some(user) })
                                    }
                                    request::SignInInitiateResult::PromptUserDeviceFlow(flow) => {
                                        this.update(&mut cx, |this, cx| {
                                            if let CodyServer::Running(RunningCodyServer {
                                                sign_in_status: status,
                                                ..
                                            }) = &mut this.server
                                            {
                                                if let SignInStatus::SigningIn {
                                                    prompt: prompt_flow,
                                                    ..
                                                } = status
                                                {
                                                    *prompt_flow = Some(flow.clone());
                                                    cx.notify();
                                                }
                                            }
                                        })?;
                                        let response = lsp
                                            .request::<request::SignInConfirm>(
                                                request::SignInConfirmParams {
                                                    user_code: flow.user_code,
                                                },
                                            )
                                            .await?;
                                        Ok(response)
                                    }
                                }
                            };

                            let sign_in = sign_in.await;
                            this.update(&mut cx, |this, cx| match sign_in {
                                Ok(status) => {
                                    this.update_sign_in_status(status, cx);
                                    Ok(())
                                }
                                Err(error) => {
                                    this.update_sign_in_status(
                                        request::SignInStatus::NotSignedIn,
                                        cx,
                                    );
                                    Err(Arc::new(error))
                                }
                            })?
                        })
                        .shared();
                    server.sign_in_status = SignInStatus::SigningIn {
                        prompt: None,
                        task: task.clone(),
                    };
                    cx.notify();
                    task
                }
            };

            cx.background_executor()
                .spawn(task.map_err(|err| anyhow!("{:?}", err)))
        } else {
            // If we're downloading, wait until download is finished
            // If we're in a stuck state, display to the user
            Task::ready(Err(anyhow!("cody hasn't started yet")))
        }
    }

    fn sign_out(&mut self, cx: &mut ModelContext<Self>) -> Task<Result<()>> {
        self.update_sign_in_status(request::SignInStatus::NotSignedIn, cx);
        if let CodyServer::Running(RunningCodyServer { lsp: server, .. }) = &self.server {
            let server = server.clone();
            cx.background_executor().spawn(async move {
                server
                    .request::<request::SignOut>(request::SignOutParams {})
                    .await?;
                anyhow::Ok(())
            })
        } else {
            Task::ready(Err(anyhow!("cody hasn't started yet")))
        }
    }

    pub fn reinstall(&mut self, cx: &mut ModelContext<Self>) -> Task<()> {
        let start_task = cx
            .spawn({
                let http = self.http.clone();
                let node_runtime = self.node_runtime.clone();
                let server_id = self.server_id;
                move |this, cx| async move {
                    clear_cody_dir().await;
                    Self::start_language_server(server_id, http, node_runtime, this, cx).await
                }
            })
            .shared();

        self.server = CodyServer::Starting {
            task: start_task.clone(),
        };

        cx.notify();

        cx.background_executor().spawn(start_task)
    }

    pub fn language_server(&self) -> Option<(&LanguageServerName, &Arc<LanguageServer>)> {
        if let CodyServer::Running(server) = &self.server {
            Some((&server.name, &server.lsp))
        } else {
            None
        }
    }

    pub fn register_buffer(&mut self, buffer: &Model<Buffer>, cx: &mut ModelContext<Self>) {
        let weak_buffer = buffer.downgrade();
        self.buffers.insert(weak_buffer.clone());

        if let CodyServer::Running(RunningCodyServer {
            lsp: server,
            sign_in_status: status,
            registered_buffers,
            ..
        }) = &mut self.server
        {
            if !matches!(status, SignInStatus::Authorized { .. }) {
                return;
            }

            registered_buffers
                .entry(buffer.entity_id())
                .or_insert_with(|| {
                    let uri: lsp::Url = uri_for_buffer(buffer, cx);
                    let language_id = id_for_language(buffer.read(cx).language());
                    let snapshot = buffer.read(cx).snapshot();
                    server
                        .notify::<request::DidOpenTextDocument>(
                            request::DidOpenTextDocumentParams {
                                uri: uri.clone().to_string(),
                                content: snapshot.text(),
                            },
                        )
                        .log_err();

                    RegisteredBuffer {
                        uri,
                        language_id,
                        snapshot,
                        snapshot_version: 0,
                        pending_buffer_change: Task::ready(Some(())),
                        _subscriptions: [
                            cx.subscribe(buffer, |this, buffer, event, cx| {
                                this.handle_buffer_event(buffer, event, cx).log_err();
                            }),
                            cx.observe_release(buffer, move |this, _buffer, _cx| {
                                this.buffers.remove(&weak_buffer);
                                this.unregister_buffer(&weak_buffer);
                            }),
                        ],
                    }
                });
        }
    }

    fn handle_buffer_event(
        &mut self,
        buffer: Model<Buffer>,
        event: &language::Event,
        cx: &mut ModelContext<Self>,
    ) -> Result<()> {
        if let Ok(server) = self.server.as_running() {
            if let Some(registered_buffer) = server.registered_buffers.get_mut(&buffer.entity_id())
            {
                match event {
                    language::Event::Edited => {
                        let _ = registered_buffer.report_changes(&buffer, cx);
                    }
                    language::Event::Saved => {
                        server
                            .lsp
                            .notify::<lsp::notification::DidSaveTextDocument>(
                                lsp::DidSaveTextDocumentParams {
                                    text_document: lsp::TextDocumentIdentifier::new(
                                        registered_buffer.uri.clone(),
                                    ),
                                    text: None,
                                },
                            )?;
                    }
                    language::Event::FileHandleChanged | language::Event::LanguageChanged => {
                        let new_language_id = id_for_language(buffer.read(cx).language());
                        let new_uri = uri_for_buffer(&buffer, cx);
                        if new_uri != registered_buffer.uri
                            || new_language_id != registered_buffer.language_id
                        {
                            let old_uri = mem::replace(&mut registered_buffer.uri, new_uri);
                            registered_buffer.language_id = new_language_id;
                            server
                                .lsp
                                .notify::<lsp::notification::DidCloseTextDocument>(
                                    lsp::DidCloseTextDocumentParams {
                                        text_document: lsp::TextDocumentIdentifier::new(old_uri),
                                    },
                                )?;
                            server
                                .lsp
                                .notify::<lsp::notification::DidOpenTextDocument>(
                                    lsp::DidOpenTextDocumentParams {
                                        text_document: lsp::TextDocumentItem::new(
                                            registered_buffer.uri.clone(),
                                            registered_buffer.language_id.clone(),
                                            registered_buffer.snapshot_version,
                                            registered_buffer.snapshot.text(),
                                        ),
                                    },
                                )?;
                        }
                    }
                    _ => {}
                }
            }
        }

        Ok(())
    }

    fn unregister_buffer(&mut self, buffer: &WeakModel<Buffer>) {
        if let Ok(server) = self.server.as_running() {
            if let Some(buffer) = server.registered_buffers.remove(&buffer.entity_id()) {
                server
                    .lsp
                    .notify::<lsp::notification::DidCloseTextDocument>(
                        lsp::DidCloseTextDocumentParams {
                            text_document: lsp::TextDocumentIdentifier::new(buffer.uri),
                        },
                    )
                    .log_err();
            }
        }
    }

    pub fn completions<T>(
        &mut self,
        buffer: &Model<Buffer>,
        position: T,
        cx: &mut ModelContext<Self>,
    ) -> Task<Result<Vec<Completion>>>
    where
        T: ToPointUtf16,
    {
        self.request_completions::<request::GetCompletions, _>(buffer, position, cx)
    }

    pub fn completions_cycling<T>(
        &mut self,
        buffer: &Model<Buffer>,
        position: T,
        cx: &mut ModelContext<Self>,
    ) -> Task<Result<Vec<Completion>>>
    where
        T: ToPointUtf16,
    {
        self.request_completions::<request::GetCompletionsCycling, _>(buffer, position, cx)
    }

    pub fn accept_completion(
        &mut self,
        completion: &Completion,
        cx: &mut ModelContext<Self>,
    ) -> Task<Result<()>> {
        let server = match self.server.as_authenticated() {
            Ok(server) => server,
            Err(error) => return Task::ready(Err(error)),
        };
        let request =
            server
                .lsp
                .request::<request::NotifyAccepted>(request::NotifyAcceptedParams {
                    uuid: completion.uuid.clone(),
                });
        cx.background_executor().spawn(async move {
            request.await?;
            Ok(())
        })
    }

    pub fn discard_completions(
        &mut self,
        completions: &[Completion],
        cx: &mut ModelContext<Self>,
    ) -> Task<Result<()>> {
        let server = match self.server.as_authenticated() {
            Ok(server) => server,
            Err(error) => return Task::ready(Err(error)),
        };
        let request =
            server
                .lsp
                .request::<request::NotifyRejected>(request::NotifyRejectedParams {
                    uuids: completions
                        .iter()
                        .map(|completion| completion.uuid.clone())
                        .collect(),
                });
        cx.background_executor().spawn(async move {
            request.await?;
            Ok(())
        })
    }

    fn request_completions<R, T>(
        &mut self,
        buffer: &Model<Buffer>,
        position: T,
        cx: &mut ModelContext<Self>,
    ) -> Task<Result<Vec<Completion>>>
    where
        R: 'static
            + lsp::request::Request<
                Params = request::GetCompletionsParams,
                Result = request::GetCompletionsResult,
            >,
        T: ToPointUtf16,
    {
        self.register_buffer(buffer, cx);

        let server = match self.server.as_authenticated() {
            Ok(server) => server,
            Err(error) => return Task::ready(Err(error)),
        };
        let lsp = server.lsp.clone();
        let registered_buffer = server
            .registered_buffers
            .get_mut(&buffer.entity_id())
            .unwrap();
        let snapshot = registered_buffer.report_changes(buffer, cx);
        let buffer = buffer.read(cx);
        let uri = registered_buffer.uri.clone();
        let position = position.to_point_utf16(buffer);
        let settings = language_settings(buffer.language_at(position).as_ref(), buffer.file(), cx);
        let tab_size = settings.tab_size;
        let hard_tabs = settings.hard_tabs;
        let relative_path = buffer
            .file()
            .map(|file| file.path().to_path_buf())
            .unwrap_or_default();

        cx.background_executor().spawn(async move {
            let (version, snapshot) = snapshot.await?;
            let result = lsp
                .request::<R>(request::GetCompletionsParams {
                    uri: uri.to_string(),
                    position: point_to_lsp(position),
                })
                .await?;
            let completions = result
                .completions
                .into_iter()
                .map(|completion| {
                    let start = snapshot
                        .clip_point_utf16(point_from_lsp(completion.range.start), Bias::Left);
                    let end =
                        snapshot.clip_point_utf16(point_from_lsp(completion.range.end), Bias::Left);
                    Completion {
                        uuid: completion.id,
                        range: snapshot.anchor_before(start)..snapshot.anchor_after(end),
                        text: completion.insert_text,
                    }
                })
                .collect();
            anyhow::Ok(completions)
        })
    }

    pub fn status(&self) -> Status {
        match &self.server {
            CodyServer::Starting { task } => Status::Starting { task: task.clone() },
            CodyServer::Disabled => Status::Disabled,
            CodyServer::Error(error) => Status::Error(error.clone()),
            CodyServer::Running(RunningCodyServer { sign_in_status, .. }) => match sign_in_status {
                SignInStatus::Authorized { .. } => Status::Authorized,
                SignInStatus::Unauthorized { .. } => Status::Unauthorized,
                SignInStatus::SigningIn { prompt, .. } => Status::SigningIn {
                    prompt: prompt.clone(),
                },
                SignInStatus::SignedOut => Status::SignedOut,
            },
        }
    }

    fn update_sign_in_status(
        &mut self,
        lsp_status: request::SignInStatus,
        cx: &mut ModelContext<Self>,
    ) {
        self.buffers.retain(|buffer| buffer.is_upgradable());

        if let Ok(server) = self.server.as_running() {
            match lsp_status {
                request::SignInStatus::Ok { user: Some(_) }
                | request::SignInStatus::MaybeOk { .. }
                | request::SignInStatus::AlreadySignedIn { .. } => {
                    server.sign_in_status = SignInStatus::Authorized;
                    for buffer in self.buffers.iter().cloned().collect::<Vec<_>>() {
                        if let Some(buffer) = buffer.upgrade() {
                            self.register_buffer(&buffer, cx);
                        }
                    }
                }
                request::SignInStatus::NotAuthorized { .. } => {
                    server.sign_in_status = SignInStatus::Unauthorized;
                    for buffer in self.buffers.iter().cloned().collect::<Vec<_>>() {
                        self.unregister_buffer(&buffer);
                    }
                }
                request::SignInStatus::Ok { user: None } | request::SignInStatus::NotSignedIn => {
                    server.sign_in_status = SignInStatus::SignedOut;
                    for buffer in self.buffers.iter().cloned().collect::<Vec<_>>() {
                        self.unregister_buffer(&buffer);
                    }
                }
            }

            cx.notify();
        }
    }
}

fn id_for_language(language: Option<&Arc<Language>>) -> String {
    let language_name = language.map(|language| language.name());
    match language_name.as_deref() {
        Some("Plain Text") => "plaintext".to_string(),
        Some(language_name) => language_name.to_lowercase(),
        None => "plaintext".to_string(),
    }
}

fn uri_for_buffer(buffer: &Model<Buffer>, cx: &AppContext) -> lsp::Url {
    if let Some(file) = buffer.read(cx).file().and_then(|file| file.as_local()) {
        lsp::Url::from_file_path(file.abs_path(cx)).unwrap()
    } else {
        format!("buffer://{}", buffer.entity_id()).parse().unwrap()
    }
}

async fn clear_cody_dir() {
    remove_matching(&paths::COPILOT_DIR, |_| true).await
}

async fn get_cody_lsp(http: Arc<dyn HttpClient>) -> anyhow::Result<PathBuf> {
    const SERVER_PATH: &str = "dist/agent.js";

    // TODO: Fetch latest cody agent from somewhere

    ///Check for the latest cody language server and download it if we haven't already
    async fn fetch_latest(_http: Arc<dyn HttpClient>) -> anyhow::Result<PathBuf> {
        let server_path = &*paths::CODY_DIR.join(SERVER_PATH);

        Ok(server_path.to_path_buf())
    }

    match fetch_latest(http).await {
        ok @ Result::Ok(..) => ok,
        e @ Err(..) => {
            e.log_err();
            // Fetch a cached binary, if it exists
            maybe!(async {
                let mut last_version_dir = None;
                let mut entries = fs::read_dir(paths::COPILOT_DIR.as_path()).await?;
                while let Some(entry) = entries.next().await {
                    let entry = entry?;
                    if entry.file_type().await?.is_dir() {
                        last_version_dir = Some(entry.path());
                    }
                }
                let last_version_dir =
                    last_version_dir.ok_or_else(|| anyhow!("no cached binary"))?;
                let server_path = last_version_dir.join(SERVER_PATH);
                if server_path.exists() {
                    Ok(server_path)
                } else {
                    Err(anyhow!(
                        "missing executable in directory {:?}",
                        last_version_dir
                    ))
                }
            })
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use language::BufferId;

    #[gpui::test(iterations = 10)]
    async fn test_buffer_management(cx: &mut TestAppContext) {
        let (cody, mut lsp) = Cody::fake(cx);

        let buffer_1 = cx.new_model(|cx| {
            Buffer::new(0, BufferId::new(cx.entity_id().as_u64()).unwrap(), "Hello")
        });
        let buffer_1_uri: lsp::Url = format!("buffer://{}", buffer_1.entity_id().as_u64())
            .parse()
            .unwrap();
        cody.update(cx, |cody, cx| cody.register_buffer(&buffer_1, cx));
        assert_eq!(
            lsp.receive_notification::<lsp::notification::DidOpenTextDocument>()
                .await,
            lsp::DidOpenTextDocumentParams {
                text_document: lsp::TextDocumentItem::new(
                    buffer_1_uri.clone(),
                    "plaintext".into(),
                    0,
                    "Hello".into()
                ),
            }
        );

        let buffer_2 = cx.new_model(|cx| {
            Buffer::new(
                0,
                BufferId::new(cx.entity_id().as_u64()).unwrap(),
                "Goodbye",
            )
        });
        let buffer_2_uri: lsp::Url = format!("buffer://{}", buffer_2.entity_id().as_u64())
            .parse()
            .unwrap();
        cody.update(cx, |cody, cx| cody.register_buffer(&buffer_2, cx));
        assert_eq!(
            lsp.receive_notification::<lsp::notification::DidOpenTextDocument>()
                .await,
            lsp::DidOpenTextDocumentParams {
                text_document: lsp::TextDocumentItem::new(
                    buffer_2_uri.clone(),
                    "plaintext".into(),
                    0,
                    "Goodbye".into()
                ),
            }
        );

        buffer_1.update(cx, |buffer, cx| buffer.edit([(5..5, " world")], None, cx));
        assert_eq!(
            lsp.receive_notification::<lsp::notification::DidChangeTextDocument>()
                .await,
            lsp::DidChangeTextDocumentParams {
                text_document: lsp::VersionedTextDocumentIdentifier::new(buffer_1_uri.clone(), 1),
                content_changes: vec![lsp::TextDocumentContentChangeEvent {
                    range: Some(lsp::Range::new(
                        lsp::Position::new(0, 5),
                        lsp::Position::new(0, 5)
                    )),
                    range_length: None,
                    text: " world".into(),
                }],
            }
        );

        // Ensure updates to the file are reflected in the LSP.
        buffer_1.update(cx, |buffer, cx| {
            buffer.file_updated(
                Arc::new(File {
                    abs_path: "/root/child/buffer-1".into(),
                    path: Path::new("child/buffer-1").into(),
                }),
                cx,
            )
        });
        assert_eq!(
            lsp.receive_notification::<lsp::notification::DidCloseTextDocument>()
                .await,
            lsp::DidCloseTextDocumentParams {
                text_document: lsp::TextDocumentIdentifier::new(buffer_1_uri),
            }
        );
        let buffer_1_uri = lsp::Url::from_file_path("/root/child/buffer-1").unwrap();
        assert_eq!(
            lsp.receive_notification::<lsp::notification::DidOpenTextDocument>()
                .await,
            lsp::DidOpenTextDocumentParams {
                text_document: lsp::TextDocumentItem::new(
                    buffer_1_uri.clone(),
                    "plaintext".into(),
                    1,
                    "Hello world".into()
                ),
            }
        );

        // Ensure all previously-registered buffers are closed when signing out.
        lsp.handle_request::<request::SignOut, _, _>(|_, _| async {
            Ok(request::SignOutResult {})
        });
        cody.update(cx, |cody, cx| cody.sign_out(cx)).await.unwrap();
        assert_eq!(
            lsp.receive_notification::<lsp::notification::DidCloseTextDocument>()
                .await,
            lsp::DidCloseTextDocumentParams {
                text_document: lsp::TextDocumentIdentifier::new(buffer_1_uri.clone()),
            }
        );
        assert_eq!(
            lsp.receive_notification::<lsp::notification::DidCloseTextDocument>()
                .await,
            lsp::DidCloseTextDocumentParams {
                text_document: lsp::TextDocumentIdentifier::new(buffer_2_uri.clone()),
            }
        );

        // Ensure all previously-registered buffers are re-opened when signing in.
        lsp.handle_request::<request::SignInInitiate, _, _>(|_, _| async {
            Ok(request::SignInInitiateResult::AlreadySignedIn {
                user: "user-1".into(),
            })
        });
        cody.update(cx, |cody, cx| cody.sign_in(cx)).await.unwrap();

        assert_eq!(
            lsp.receive_notification::<lsp::notification::DidOpenTextDocument>()
                .await,
            lsp::DidOpenTextDocumentParams {
                text_document: lsp::TextDocumentItem::new(
                    buffer_1_uri.clone(),
                    "plaintext".into(),
                    0,
                    "Hello world".into()
                ),
            }
        );
        assert_eq!(
            lsp.receive_notification::<lsp::notification::DidOpenTextDocument>()
                .await,
            lsp::DidOpenTextDocumentParams {
                text_document: lsp::TextDocumentItem::new(
                    buffer_2_uri.clone(),
                    "plaintext".into(),
                    0,
                    "Goodbye".into()
                ),
            }
        );
        // Dropping a buffer causes it to be closed on the LSP side as well.
        cx.update(|_| drop(buffer_2));
        assert_eq!(
            lsp.receive_notification::<lsp::notification::DidCloseTextDocument>()
                .await,
            lsp::DidCloseTextDocumentParams {
                text_document: lsp::TextDocumentIdentifier::new(buffer_2_uri),
            }
        );
    }

    struct File {
        abs_path: PathBuf,
        path: Arc<Path>,
    }

    impl language::File for File {
        fn as_local(&self) -> Option<&dyn language::LocalFile> {
            Some(self)
        }

        fn mtime(&self) -> Option<std::time::SystemTime> {
            unimplemented!()
        }

        fn path(&self) -> &Arc<Path> {
            &self.path
        }

        fn full_path(&self, _: &AppContext) -> PathBuf {
            unimplemented!()
        }

        fn file_name<'a>(&'a self, _: &'a AppContext) -> &'a std::ffi::OsStr {
            unimplemented!()
        }

        fn is_deleted(&self) -> bool {
            unimplemented!()
        }

        fn as_any(&self) -> &dyn std::any::Any {
            unimplemented!()
        }

        fn to_proto(&self) -> rpc::proto::File {
            unimplemented!()
        }

        fn worktree_id(&self) -> usize {
            0
        }

        fn is_private(&self) -> bool {
            false
        }
    }

    impl language::LocalFile for File {
        fn abs_path(&self, _: &AppContext) -> PathBuf {
            self.abs_path.clone()
        }

        fn load(&self, _: &AppContext) -> Task<Result<String>> {
            unimplemented!()
        }

        fn buffer_reloaded(
            &self,
            _: BufferId,
            _: &clock::Global,
            _: language::RopeFingerprint,
            _: language::LineEnding,
            _: Option<std::time::SystemTime>,
            _: &mut AppContext,
        ) {
            unimplemented!()
        }
    }
}