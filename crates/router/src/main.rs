//! 单入口、多账号 Kimi ACP 路由器。

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Parser;
use fs2::FileExt;
use kimi_subscription_router::home::AccountHome;
use kimi_subscription_router::routing::{PoolExhausted, RouteSelector};
use kimi_subscription_router::state::{RouterState, StateStore};
use kimi_switch_core::paths::AppPaths;
use kimi_switch_core::{Account, AccountRegistry, CredentialStore, FileStore, KeyringStore};
use kimi_switch_kimi::KimiProvider;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

const INTERNAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Parser)]
#[command(
    name = "kimi-subscription-router",
    version,
    about = "Route one ACP connection across multiple Kimi Code subscriptions."
)]
struct Cli {
    /// Kimi Code executable.
    #[arg(long, default_value = "kimi")]
    kimi_binary: PathBuf,

    /// Log level (equivalent to RUST_LOG). Logs are written to stderr.
    #[arg(long, default_value = "warn")]
    log: String,
}

struct InstanceLock {
    file: File,
}

impl InstanceLock {
    fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        file.try_lock_exclusive()
            .context("another Kimi subscription router is already running")?;
        Ok(Self { file })
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

struct ChildPeer {
    account_id: String,
    generation: u64,
    home: AccountHome,
    stdin: ChildStdin,
    _process: Child,
}

impl ChildPeer {
    async fn send(&mut self, message: &Value) -> Result<()> {
        let mut raw = serde_json::to_vec(message)?;
        raw.push(b'\n');
        self.stdin.write_all(&raw).await?;
        self.stdin.flush().await?;
        Ok(())
    }
}

enum ChildEvent {
    Message {
        account_id: String,
        generation: u64,
        value: Value,
    },
    Closed {
        account_id: String,
        generation: u64,
    },
}

#[derive(Clone)]
struct SessionContext {
    resume_params: Map<String, Value>,
}

impl SessionContext {
    fn from_request(request: &Value, session_id: &str) -> Self {
        let mut params = request
            .get("params")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        params.insert("sessionId".into(), Value::String(session_id.to_string()));
        params
            .entry("mcpServers")
            .or_insert_with(|| Value::Array(Vec::new()));
        Self {
            resume_params: params,
        }
    }

    fn for_session(mut self, session_id: &str) -> Self {
        self.resume_params
            .insert("sessionId".into(), Value::String(session_id.to_string()));
        self
    }
}

struct PendingRequest {
    account_id: String,
    method: String,
    original: Value,
    session_id: Option<String>,
}

struct InitState {
    request_id: Value,
    waiting: HashSet<String>,
    primary: String,
    primary_response: Option<Value>,
    deadline: Instant,
}

struct ResumeAction {
    account_id: String,
    session_id: String,
    original_request: Value,
    deadline: Instant,
}

enum FailoverAction {
    CloseThenResume {
        action: ResumeAction,
        resume_request: Value,
        waiting_account_id: String,
    },
    Resume(ResumeAction),
}

impl FailoverAction {
    fn waiting_account_id(&self) -> &str {
        match self {
            Self::CloseThenResume {
                waiting_account_id, ..
            } => waiting_account_id,
            Self::Resume(action) => &action.account_id,
        }
    }

    fn deadline(&self) -> Instant {
        match self {
            Self::CloseThenResume { action, .. } | Self::Resume(action) => action.deadline,
        }
    }

    fn client_id(&self) -> Value {
        let action = match self {
            Self::CloseThenResume { action, .. } | Self::Resume(action) => action,
        };
        action
            .original_request
            .get("id")
            .cloned()
            .unwrap_or(Value::Null)
    }
}

struct Router {
    provider: Arc<KimiProvider>,
    registry: Arc<AccountRegistry>,
    peers: HashMap<String, ChildPeer>,
    selector: RouteSelector,
    state_store: StateStore,
    state: RouterState,
    contexts: HashMap<String, SessionContext>,
    pending: HashMap<String, PendingRequest>,
    reverse_requests: HashMap<String, (String, Value)>,
    failovers: HashMap<String, FailoverAction>,
    init: Option<InitState>,
    initialize_request: Option<Value>,
    warming: HashSet<String>,
    kimi_binary: PathBuf,
    router_root: PathBuf,
    shared_sessions: PathBuf,
    child_events: mpsc::UnboundedSender<ChildEvent>,
    child_sequence: u64,
    sequence: u64,
}

impl Router {
    async fn run(mut self, mut child_events: mpsc::UnboundedReceiver<ChildEvent>) -> Result<()> {
        let stdin = tokio::io::stdin();
        let mut input = BufReader::new(stdin).lines();
        let mut sync_tick = tokio::time::interval(Duration::from_secs(2));
        sync_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                line = input.next_line() => {
                    match line? {
                        Some(line) if !line.trim().is_empty() => {
                            match serde_json::from_str::<Value>(&line) {
                                Ok(message) => self.handle_client(message).await?,
                                Err(error) => {
                                    write_output(&json!({
                                        "jsonrpc": "2.0",
                                        "id": Value::Null,
                                        "error": {"code": -32700, "message": format!("Parse error: {error}")}
                                    })).await?;
                                }
                            }
                        }
                        Some(_) => {}
                        None => break,
                    }
                }
                event = child_events.recv() => {
                    match event {
                        Some(ChildEvent::Message { account_id, generation, value }) => {
                            self.handle_child(&account_id, generation, value).await?;
                        }
                        Some(ChildEvent::Closed { account_id, generation }) => {
                            self.handle_child_closed(&account_id, generation).await?;
                        }
                        None => break,
                    }
                }
                _ = sync_tick.tick() => {
                    self.sync_routing_accounts().await;
                    self.absorb_all_credentials();
                    self.expire_internal_requests().await?;
                },
            }
        }
        self.sync_routing_accounts().await;
        self.absorb_all_credentials();
        Ok(())
    }

    async fn handle_client(&mut self, message: Value) -> Result<()> {
        if message.get("method").is_none() {
            return self.handle_client_response(message).await;
        }
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if method == "initialize" {
            return self.initialize_children(message).await;
        }
        if self.init.is_some() {
            return self
                .respond_error(
                    message.get("id").cloned().unwrap_or(Value::Null),
                    -32001,
                    "Router initialization is still in progress.",
                    None,
                )
                .await;
        }
        if method == "logout" {
            return self
                .respond_error(
                    message.get("id").cloned().unwrap_or(Value::Null),
                    -32601,
                    "Logout is managed by Kimi Subscription Router.",
                    None,
                )
                .await;
        }

        let request_id = message.get("id").cloned();
        let session_id = request_session_id(&message);
        let persisted_owner = session_id
            .as_deref()
            .and_then(|session_id| self.state.owner(session_id))
            .map(str::to_string);
        let account_id = match method.as_str() {
            "session/new" => match self.select_account(&HashSet::new()) {
                Ok(account_id) => account_id,
                Err(pool) => return self.respond_pool_exhausted(request_id, pool).await,
            },
            "session/list" | "authenticate" => match self.primary_account() {
                Some(account) => account,
                None => return self.respond_no_accounts(request_id).await,
            },
            _ if method.starts_with("session/") => {
                let Some(session_id) = session_id.as_deref() else {
                    return self
                        .respond_error(
                            request_id.unwrap_or(Value::Null),
                            -32602,
                            "Missing sessionId.",
                            None,
                        )
                        .await;
                };
                match self.state.owner(session_id) {
                    Some(owner) if self.peers.contains_key(owner) => owner.to_string(),
                    _ => match self.select_account(&HashSet::new()) {
                        Ok(account_id) => account_id,
                        Err(pool) => return self.respond_pool_exhausted(request_id, pool).await,
                    },
                }
            }
            _ => match self.primary_account() {
                Some(account) => account,
                None => return self.respond_no_accounts(request_id).await,
            },
        };

        if method == "session/prompt" {
            let session_id = session_id.as_deref().expect("validated session id");
            if persisted_owner
                .as_deref()
                .is_some_and(|owner| owner != account_id)
            {
                return self
                    .begin_resume_without_close(session_id, &account_id, message)
                    .await;
            }
            if !self.selector.account_has_capacity(&account_id) {
                if self.selector.account_routing_enabled(&account_id) {
                    self.selector.mark_exhausted(&account_id);
                }
                return self.begin_failover(session_id, &account_id, message).await;
            }
        }

        if matches!(method.as_str(), "session/load" | "session/resume") {
            if let Some(session_id) = session_id.as_deref() {
                self.contexts.insert(
                    session_id.to_string(),
                    SessionContext::from_request(&message, session_id),
                );
            }
        }
        self.forward_request(&account_id, method, session_id, message)
            .await
    }

    async fn handle_client_response(&mut self, mut message: Value) -> Result<()> {
        let Some(id) = message.get("id").cloned() else {
            return Ok(());
        };
        let key = id_key(&id);
        let Some((account_id, original_id)) = self.reverse_requests.remove(&key) else {
            return Ok(());
        };
        if let Some(object) = message.as_object_mut() {
            object.insert("id".into(), original_id);
        }
        if let Err(error) = self.send_to(&account_id, &message).await {
            tracing::warn!(account = %account_id, err = %error, "discard response for unavailable Kimi ACP child");
        }
        Ok(())
    }

    async fn initialize_children(&mut self, message: Value) -> Result<()> {
        if self.peers.is_empty() {
            return self.respond_no_accounts(message.get("id").cloned()).await;
        }
        let request_id = message.get("id").cloned().unwrap_or(Value::Null);
        self.initialize_request = Some(message.clone());
        let primary = self.primary_account().expect("non-empty peer set");
        let waiting = self.peers.keys().cloned().collect::<HashSet<_>>();
        self.init = Some(InitState {
            request_id,
            waiting,
            primary,
            primary_response: None,
            deadline: Instant::now() + INTERNAL_REQUEST_TIMEOUT,
        });
        let accounts = self.peers.keys().cloned().collect::<Vec<_>>();
        for account_id in accounts {
            if let Err(error) = self.send_to(&account_id, &message).await {
                tracing::warn!(account = %account_id, err = %error, "initialize Kimi ACP child failed");
                if let Some(generation) = self.peers.get(&account_id).map(|peer| peer.generation) {
                    self.handle_child_closed(&account_id, generation).await?;
                }
            }
        }
        Ok(())
    }

    async fn handle_child(
        &mut self,
        account_id: &str,
        generation: u64,
        mut message: Value,
    ) -> Result<()> {
        if !matches!(
            self.peers.get(account_id),
            Some(peer) if peer.generation == generation
        ) {
            return Ok(());
        }
        if self.handle_warming_response(account_id, &message) {
            return Ok(());
        }
        if self
            .handle_initialize_response(account_id, &message)
            .await?
        {
            return Ok(());
        }

        if message.get("method").is_some() {
            if let Some(original_id) = message.get("id").cloned() {
                self.sequence = self.sequence.wrapping_add(1);
                let routed_id = Value::String(format!("router-reverse-{}", self.sequence));
                self.reverse_requests
                    .insert(id_key(&routed_id), (account_id.to_string(), original_id));
                if let Some(object) = message.as_object_mut() {
                    object.insert("id".into(), routed_id);
                }
            }
            return write_output(&message).await;
        }

        let Some(response_id) = message.get("id").cloned() else {
            return write_output(&message).await;
        };
        let key = id_key(&response_id);
        if let Some(action) = self.failovers.remove(&key) {
            return self.finish_failover_step(action, message).await;
        }
        let Some(pending) = self.pending.remove(&key) else {
            return write_output(&message).await;
        };

        if pending.method == "session/prompt" && response_is_quota_exhausted(&message) {
            self.selector.mark_exhausted(&pending.account_id);
            let session_id = pending.session_id.as_deref().unwrap_or_default();
            return self
                .begin_failover(session_id, &pending.account_id, pending.original)
                .await;
        }

        self.commit_response_side_effects(&pending, &message)?;
        write_output(&message).await
    }

    async fn handle_child_closed(&mut self, account_id: &str, generation: u64) -> Result<()> {
        if self
            .peers
            .get(account_id)
            .is_some_and(|peer| peer.generation != generation)
        {
            return Ok(());
        }
        tracing::warn!(account = %account_id, "Kimi ACP child exited");
        self.peers.remove(account_id);
        self.warming.remove(account_id);
        self.reverse_requests
            .retain(|_, (request_account, _)| request_account != account_id);

        let interrupted = self
            .failovers
            .iter()
            .filter(|(_, action)| action.waiting_account_id() == account_id)
            .map(|(key, action)| (key.clone(), action.client_id()))
            .collect::<Vec<_>>();
        for (key, id) in interrupted {
            self.failovers.remove(&key);
            self.respond_error(
                id,
                -32045,
                "A Kimi ACP child exited during session failover.",
                None,
            )
            .await?;
        }

        let failed = self
            .pending
            .iter()
            .filter(|(_, request)| request.account_id == account_id)
            .map(|(key, request)| {
                (
                    key.clone(),
                    request.original.get("id").cloned().unwrap_or(Value::Null),
                )
            })
            .collect::<Vec<_>>();
        for (key, id) in failed {
            self.pending.remove(&key);
            self.respond_error(
                id,
                -32045,
                "The assigned Kimi ACP child exited before completing the request.",
                None,
            )
            .await?;
        }

        let mut init_response = None;
        if let Some(init) = self.init.as_mut() {
            if init.waiting.remove(account_id) && account_id == init.primary {
                init.primary_response = Some(json!({
                    "jsonrpc": "2.0",
                    "id": init.request_id,
                    "error": {"code": -32001, "message": "Primary Kimi ACP child exited during initialization."}
                }));
            }
            if init.waiting.is_empty() {
                init_response = init.primary_response.take();
            }
        }
        if let Some(response) = init_response {
            self.init = None;
            write_output(&response).await?;
        }
        Ok(())
    }

    async fn handle_initialize_response(
        &mut self,
        account_id: &str,
        message: &Value,
    ) -> Result<bool> {
        let Some(init) = self.init.as_mut() else {
            return Ok(false);
        };
        if message.get("id") != Some(&init.request_id) || !init.waiting.contains(account_id) {
            return Ok(false);
        }
        init.waiting.remove(account_id);
        if account_id == init.primary {
            let mut response = message.clone();
            normalize_initialize_response(&mut response);
            init.primary_response = Some(response);
        }
        if init.waiting.is_empty() {
            let response = init.primary_response.take().unwrap_or_else(|| {
                json!({
                    "jsonrpc": "2.0",
                    "id": init.request_id,
                    "error": {"code": -32001, "message": "Kimi ACP initialization failed."}
                })
            });
            self.init = None;
            write_output(&response).await?;
        }
        Ok(true)
    }

    fn commit_response_side_effects(
        &mut self,
        pending: &PendingRequest,
        response: &Value,
    ) -> Result<()> {
        if response.get("error").is_some() {
            return Ok(());
        }
        match pending.method.as_str() {
            "session/new" => {
                if let Some(session_id) = response
                    .pointer("/result/sessionId")
                    .and_then(Value::as_str)
                {
                    self.state.assign(session_id, &pending.account_id);
                    self.contexts.insert(
                        session_id.to_string(),
                        SessionContext::from_request(&pending.original, session_id),
                    );
                    self.state_store.save(&self.state)?;
                }
            }
            "session/load" | "session/resume" => {
                if let Some(session_id) = pending.session_id.as_deref() {
                    self.state.assign(session_id, &pending.account_id);
                    self.state_store.save(&self.state)?;
                }
            }
            "session/fork" => {
                if let Some(new_session_id) = response
                    .pointer("/result/sessionId")
                    .and_then(Value::as_str)
                {
                    self.state.assign(new_session_id, &pending.account_id);
                    if let Some(source) = pending.session_id.as_deref() {
                        if let Some(context) = self.contexts.get(source).cloned() {
                            self.contexts.insert(
                                new_session_id.to_string(),
                                context.for_session(new_session_id),
                            );
                        }
                    }
                    self.state_store.save(&self.state)?;
                }
            }
            "session/delete" => {
                if let Some(session_id) = pending.session_id.as_deref() {
                    self.state.remove(session_id);
                    self.contexts.remove(session_id);
                    self.state_store.save(&self.state)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn begin_failover(
        &mut self,
        session_id: &str,
        exhausted_owner: &str,
        original_request: Value,
    ) -> Result<()> {
        let request_id = original_request.get("id").cloned();
        let Some(context) = self.contexts.get(session_id).cloned() else {
            return self.respond_error(
                request_id.unwrap_or(Value::Null),
                -32043,
                "Cannot move this session because its resume context is unavailable. Load the session and retry.",
                None,
            ).await;
        };
        let mut excluded = HashSet::new();
        excluded.insert(exhausted_owner.to_string());
        let replacement = match self.select_account(&excluded) {
            Ok(account_id) => account_id,
            Err(pool) => return self.respond_pool_exhausted(request_id, pool).await,
        };
        let Some(client_id) = original_request.get("id").cloned() else {
            return Ok(());
        };
        self.sequence = self.sequence.wrapping_add(1);
        let resume_id = Value::String(format!("router-resume-{}", self.sequence));
        let resume = json!({
            "jsonrpc": "2.0",
            "id": resume_id,
            "method": "session/resume",
            "params": context.resume_params,
        });
        let action = ResumeAction {
            account_id: replacement,
            session_id: session_id.to_string(),
            original_request,
            deadline: Instant::now() + INTERNAL_REQUEST_TIMEOUT,
        };
        self.sequence = self.sequence.wrapping_add(1);
        let close_id = Value::String(format!("router-close-{}", self.sequence));
        let close = json!({
            "jsonrpc": "2.0",
            "id": close_id,
            "method": "session/close",
            "params": {"sessionId": session_id},
        });
        self.failovers.insert(
            id_key(close.get("id").expect("close id")),
            FailoverAction::CloseThenResume {
                action,
                resume_request: resume,
                waiting_account_id: exhausted_owner.to_string(),
            },
        );
        if let Err(error) = self.send_to(exhausted_owner, &close).await {
            self.failovers
                .remove(&id_key(close.get("id").expect("close id")));
            return self
                .respond_error(
                    client_id,
                    -32045,
                    "The previous Kimi ACP child is unavailable during session failover.",
                    Some(json!({"cause": error.to_string()})),
                )
                .await;
        }
        Ok(())
    }

    async fn begin_resume_without_close(
        &mut self,
        session_id: &str,
        replacement: &str,
        original_request: Value,
    ) -> Result<()> {
        let request_id = original_request.get("id").cloned();
        let Some(context) = self.contexts.get(session_id).cloned() else {
            return self
                .respond_error(
                    request_id.unwrap_or(Value::Null),
                    -32043,
                    "Cannot move this session because its resume context is unavailable. Load the session and retry.",
                    None,
                )
                .await;
        };
        let Some(client_id) = original_request.get("id").cloned() else {
            return Ok(());
        };
        self.sequence = self.sequence.wrapping_add(1);
        let resume_id = Value::String(format!("router-resume-{}", self.sequence));
        let resume = json!({
            "jsonrpc": "2.0",
            "id": resume_id,
            "method": "session/resume",
            "params": context.resume_params,
        });
        let action = ResumeAction {
            account_id: replacement.to_string(),
            session_id: session_id.to_string(),
            original_request,
            deadline: Instant::now() + INTERNAL_REQUEST_TIMEOUT,
        };
        let key = id_key(resume.get("id").expect("resume id"));
        self.failovers
            .insert(key.clone(), FailoverAction::Resume(action));
        if let Err(error) = self.send_to(replacement, &resume).await {
            self.failovers.remove(&key);
            return self
                .respond_error(
                    client_id,
                    -32045,
                    "The replacement Kimi ACP child is unavailable during session recovery.",
                    Some(json!({"cause": error.to_string()})),
                )
                .await;
        }
        Ok(())
    }

    async fn finish_failover_step(&mut self, step: FailoverAction, response: Value) -> Result<()> {
        match step {
            FailoverAction::CloseThenResume {
                action,
                resume_request,
                ..
            } => {
                if let Some(error) = response.get("error") {
                    let id = action
                        .original_request
                        .get("id")
                        .cloned()
                        .unwrap_or(Value::Null);
                    return self
                        .respond_error(
                            id,
                            -32044,
                            "Kimi session failover could not close the previous owner.",
                            Some(error.clone()),
                        )
                        .await;
                }
                let key = id_key(resume_request.get("id").expect("resume id"));
                let account_id = action.account_id.clone();
                let client_id = action
                    .original_request
                    .get("id")
                    .cloned()
                    .unwrap_or(Value::Null);
                self.failovers.insert(key, FailoverAction::Resume(action));
                if let Err(error) = self.send_to(&account_id, &resume_request).await {
                    self.failovers
                        .remove(&id_key(resume_request.get("id").expect("resume id")));
                    return self
                        .respond_error(
                            client_id,
                            -32045,
                            "The replacement Kimi ACP child is unavailable during session failover.",
                            Some(json!({"cause": error.to_string()})),
                        )
                        .await;
                }
                Ok(())
            }
            FailoverAction::Resume(action) => self.finish_resume(action, response).await,
        }
    }

    async fn finish_resume(&mut self, action: ResumeAction, response: Value) -> Result<()> {
        if let Some(error) = response.get("error") {
            let id = action
                .original_request
                .get("id")
                .cloned()
                .unwrap_or(Value::Null);
            return self
                .respond_error(
                    id,
                    -32044,
                    "Kimi session failover could not resume the conversation.",
                    Some(error.clone()),
                )
                .await;
        }
        self.state.assign(&action.session_id, &action.account_id);
        self.state_store.save(&self.state)?;
        self.forward_request(
            &action.account_id,
            "session/prompt".into(),
            Some(action.session_id),
            action.original_request,
        )
        .await
    }

    async fn forward_request(
        &mut self,
        account_id: &str,
        method: String,
        session_id: Option<String>,
        message: Value,
    ) -> Result<()> {
        let pending_key = message.get("id").map(id_key);
        if let Some(id) = message.get("id") {
            self.pending.insert(
                id_key(id),
                PendingRequest {
                    account_id: account_id.to_string(),
                    method,
                    original: message.clone(),
                    session_id,
                },
            );
        }
        if let Err(error) = self.send_to(account_id, &message).await {
            if let Some(key) = pending_key {
                self.pending.remove(&key);
                return self
                    .respond_error(
                        message.get("id").cloned().unwrap_or(Value::Null),
                        -32045,
                        "The assigned Kimi ACP child is unavailable.",
                        Some(json!({"cause": error.to_string()})),
                    )
                    .await;
            }
            tracing::warn!(account = %account_id, err = %error, "discard notification for unavailable Kimi ACP child");
        }
        Ok(())
    }

    async fn send_to(&mut self, account_id: &str, message: &Value) -> Result<()> {
        self.peers
            .get_mut(account_id)
            .with_context(|| format!("Kimi ACP child unavailable for account {account_id}"))?
            .send(message)
            .await
    }

    fn primary_account(&self) -> Option<String> {
        self.selector
            .accounts()
            .iter()
            .find(|account| self.peers.contains_key(&account.id.0))
            .map(|account| account.id.0.clone())
    }

    fn select_account(
        &mut self,
        explicitly_excluded: &HashSet<String>,
    ) -> Result<String, PoolExhausted> {
        let mut excluded = explicitly_excluded.clone();
        for account in self.selector.accounts() {
            if !self.peers.contains_key(&account.id.0) {
                excluded.insert(account.id.0.clone());
            }
        }
        self.selector
            .select(&self.state, &excluded)
            .map(|selection| selection.account_id)
    }

    fn absorb_all_credentials(&self) {
        for peer in self.peers.values() {
            if let Err(error) = peer.home.absorb_credentials(&self.provider) {
                tracing::warn!(account = %peer.account_id, err = %error, "absorb child credentials failed");
            }
        }
    }

    fn handle_warming_response(&mut self, account_id: &str, message: &Value) -> bool {
        if !self.warming.contains(account_id) {
            return false;
        }
        let Some(request_id) = self
            .initialize_request
            .as_ref()
            .and_then(|request| request.get("id"))
        else {
            return false;
        };
        if message.get("id") != Some(request_id) {
            return false;
        }
        self.warming.remove(account_id);
        if message.get("error").is_some() {
            tracing::warn!(account = %account_id, "new Kimi ACP child rejected initialization");
            self.peers.remove(account_id);
        }
        self.refresh_selector_accounts();
        true
    }

    fn refresh_selector_accounts(&mut self) {
        match self.registry.list_by_provider("kimi") {
            Ok(accounts) => self.selector.replace_accounts(
                accounts
                    .into_iter()
                    .filter(|account| {
                        self.peers.contains_key(&account.id.0)
                            && !self.warming.contains(&account.id.0)
                    })
                    .collect(),
            ),
            Err(error) => tracing::warn!(err = %error, "reload routing accounts failed"),
        }
    }

    async fn sync_routing_accounts(&mut self) {
        let accounts = match self.registry.list_by_provider("kimi") {
            Ok(accounts) => accounts,
            Err(error) => {
                tracing::warn!(err = %error, "reload routing accounts failed");
                return;
            }
        };
        let desired = accounts
            .iter()
            .map(|account| account.id.0.clone())
            .collect::<HashSet<_>>();
        let removed = self
            .peers
            .keys()
            .filter(|account_id| !desired.contains(*account_id))
            .cloned()
            .collect::<Vec<_>>();
        for account_id in removed {
            if let Some(mut peer) = self.peers.remove(&account_id) {
                self.warming.remove(&account_id);
                if let Err(error) = peer.stdin.shutdown().await {
                    tracing::debug!(account = %account_id, err = %error, "close removed Kimi ACP stdin failed");
                }
                if let Err(error) = peer._process.start_kill() {
                    tracing::debug!(account = %account_id, err = %error, "stop removed Kimi ACP child failed");
                }
                if let Err(error) = peer._process.wait().await {
                    tracing::debug!(account = %account_id, err = %error, "wait for removed Kimi ACP child failed");
                }
                if let Err(error) = peer.home.purge_credentials() {
                    tracing::warn!(account = %account_id, err = %error, "purge removed account credentials failed");
                }
                if let Err(error) = self.handle_child_closed(&account_id, peer.generation).await {
                    tracing::warn!(account = %account_id, err = %error, "report removed Kimi ACP child failed");
                }
            }
        }

        for account in &accounts {
            if self.peers.contains_key(&account.id.0) {
                continue;
            }
            let home = match AccountHome::prepare(
                &self.router_root,
                &self.shared_sessions,
                account,
                &self.provider,
            ) {
                Ok(home) => home,
                Err(error) => {
                    tracing::warn!(account = %account.id, err = %error, "skip Kimi account without usable credentials");
                    continue;
                }
            };
            self.child_sequence = self.child_sequence.wrapping_add(1);
            match spawn_child(
                &self.kimi_binary,
                home,
                self.child_sequence,
                self.child_events.clone(),
            )
            .await
            {
                Ok(peer) => {
                    let account_id = account.id.0.clone();
                    self.peers.insert(account_id.clone(), peer);
                    if let Some(initialize) = self.initialize_request.clone() {
                        self.warming.insert(account_id.clone());
                        if let Err(error) = self.send_to(&account_id, &initialize).await {
                            tracing::warn!(account = %account_id, err = %error, "initialize new Kimi ACP child failed");
                            self.warming.remove(&account_id);
                            self.peers.remove(&account_id);
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(account = %account.id, err = %error, "skip unavailable Kimi account")
                }
            }
        }
        self.refresh_selector_accounts();
    }

    async fn expire_internal_requests(&mut self) -> Result<()> {
        let now = Instant::now();
        let expired = self
            .failovers
            .iter()
            .filter(|(_, action)| action.deadline() <= now)
            .map(|(key, action)| (key.clone(), action.client_id()))
            .collect::<Vec<_>>();
        for (key, id) in expired {
            self.failovers.remove(&key);
            self.respond_error(id, -32046, "Kimi session failover timed out.", None)
                .await?;
        }

        if self.init.as_ref().is_some_and(|init| init.deadline <= now) {
            let init = self.init.take().expect("checked initialization state");
            let response = init.primary_response.unwrap_or_else(|| {
                json!({
                    "jsonrpc": "2.0",
                    "id": init.request_id,
                    "error": {"code": -32001, "message": "Kimi ACP initialization timed out."}
                })
            });
            for account_id in init.waiting {
                self.peers.remove(&account_id);
            }
            self.refresh_selector_accounts();
            write_output(&response).await?;
        }
        Ok(())
    }

    async fn respond_no_accounts(&self, id: Option<Value>) -> Result<()> {
        self.respond_error(
            id.unwrap_or(Value::Null),
            -32000,
            "No routable Kimi accounts are configured.",
            None,
        )
        .await
    }

    async fn respond_pool_exhausted(&self, id: Option<Value>, pool: PoolExhausted) -> Result<()> {
        let data = json!({
            "nextReset": pool.next_reset.map(|value| value.to_rfc3339()),
        });
        self.respond_error(
            id.unwrap_or(Value::Null),
            -32042,
            "All configured Kimi accounts are depleted.",
            Some(data),
        )
        .await
    }

    async fn respond_error(
        &self,
        id: Value,
        code: i64,
        message: &str,
        data: Option<Value>,
    ) -> Result<()> {
        let mut error = json!({"code": code, "message": message});
        if let Some(data) = data {
            error
                .as_object_mut()
                .expect("error object")
                .insert("data".into(), data);
        }
        write_output(&json!({"jsonrpc": "2.0", "id": id, "error": error})).await
    }
}

fn request_session_id(message: &Value) -> Option<String> {
    message
        .pointer("/params/sessionId")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn id_key(id: &Value) -> String {
    serde_json::to_string(id).unwrap_or_else(|_| "null".into())
}

fn response_is_quota_exhausted(response: &Value) -> bool {
    let Some(error) = response.get("error") else {
        return false;
    };
    let text = error.to_string().to_ascii_lowercase();
    [
        "usage limit",
        "quota exhausted",
        "quota_exhausted",
        "resource_exhausted",
        "rate limit exceeded",
        "status 429",
        "http 429",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn normalize_initialize_response(response: &mut Value) {
    let Some(result) = response.get_mut("result").and_then(Value::as_object_mut) else {
        return;
    };
    result.insert("authMethods".into(), Value::Array(Vec::new()));
    result.insert(
        "agentInfo".into(),
        json!({
            "name": "Kimi Subscription Router",
            "version": env!("CARGO_PKG_VERSION"),
        }),
    );
}

async fn write_output(message: &Value) -> Result<()> {
    let mut stdout = tokio::io::stdout();
    let mut raw = serde_json::to_vec(message)?;
    raw.push(b'\n');
    stdout.write_all(&raw).await?;
    stdout.flush().await?;
    Ok(())
}

async fn spawn_child(
    kimi_binary: &Path,
    home: AccountHome,
    generation: u64,
    events: mpsc::UnboundedSender<ChildEvent>,
) -> Result<ChildPeer> {
    let account_id = home.account_id.clone();
    let mut process = Command::new(kimi_binary)
        .arg("acp")
        .env("KIMI_CODE_HOME", &home.path)
        .env(
            "KIMI_CODE_EXPERIMENTAL_PERSISTENCE_MINIDB_READMODEL",
            "false",
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("start {} acp", kimi_binary.display()))?;
    let stdin = process.stdin.take().context("capture Kimi ACP stdin")?;
    let stdout = process.stdout.take().context("capture Kimi ACP stdout")?;
    let reader_account = account_id.clone();
    tokio::spawn(async move {
        read_child_lines(
            reader_account,
            generation,
            BufReader::new(stdout).lines(),
            events,
        )
        .await;
    });
    Ok(ChildPeer {
        account_id,
        generation,
        home,
        stdin,
        _process: process,
    })
}

async fn read_child_lines<R>(
    account_id: String,
    generation: u64,
    mut lines: Lines<BufReader<R>>,
    events: mpsc::UnboundedSender<ChildEvent>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => match serde_json::from_str::<Value>(&line) {
                Ok(value) => {
                    if events
                        .send(ChildEvent::Message {
                            account_id: account_id.clone(),
                            generation,
                            value,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                Err(error) => {
                    tracing::warn!(account = %account_id, err = %error, "invalid JSON from Kimi ACP child")
                }
            },
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(account = %account_id, err = %error, "read Kimi ACP child failed");
                break;
            }
        }
    }
    let _ = events.send(ChildEvent::Closed {
        account_id,
        generation,
    });
}

fn build_provider(
    paths: &AppPaths,
) -> Result<(Arc<KimiProvider>, Arc<AccountRegistry>, Vec<Account>)> {
    let store: Arc<dyn CredentialStore> = Arc::new(FileStore::with_legacy_keyring(
        paths.credentials_file(),
        KeyringStore::new(),
    ));
    let registry = Arc::new(AccountRegistry::new(paths.registry_file()));
    let accounts = registry.list_by_provider("kimi")?;
    let provider = Arc::new(kimi_switch_kimi::new(store, registry.clone()));
    Ok((provider, registry, accounts))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(cli.log));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    let paths = AppPaths::resolve()?;
    let _instance_lock = InstanceLock::acquire(&paths.router_lock_file())?;
    let (provider, registry, accounts) = build_provider(&paths)?;
    if accounts.is_empty() {
        bail!("no Kimi accounts; add an account in Kimi Subscription Router first");
    }

    let router_root = paths.router_data_dir();
    let shared_sessions = router_root.join("sessions");
    fs::create_dir_all(&router_root)?;
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let mut peers = HashMap::new();
    let mut available_accounts = Vec::new();
    let mut child_sequence = 0_u64;
    for account in accounts {
        match AccountHome::prepare(&router_root, &shared_sessions, &account, &provider) {
            Ok(home) => {
                child_sequence = child_sequence.wrapping_add(1);
                match spawn_child(&cli.kimi_binary, home, child_sequence, events_tx.clone()).await {
                    Ok(peer) => {
                        peers.insert(account.id.0.clone(), peer);
                        available_accounts.push(account);
                    }
                    Err(error) => {
                        tracing::warn!(account = %account.id, err = %error, "skip unavailable Kimi account")
                    }
                }
            }
            Err(error) => {
                tracing::warn!(account = %account.id, err = %error, "skip Kimi account without usable credentials")
            }
        }
    }
    if peers.is_empty() {
        bail!("no Kimi account could start an ACP child");
    }

    let state_store = StateStore::new(paths.router_state_file());
    let state = state_store.load()?;
    let selector = RouteSelector::new(available_accounts, paths.quota_cache_file());
    Router {
        provider,
        registry,
        peers,
        selector,
        state_store,
        state,
        contexts: HashMap::new(),
        pending: HashMap::new(),
        reverse_requests: HashMap::new(),
        failovers: HashMap::new(),
        init: None,
        initialize_request: None,
        warming: HashSet::new(),
        kimi_binary: cli.kimi_binary,
        router_root,
        shared_sessions,
        child_events: events_tx,
        child_sequence,
        sequence: 0,
    }
    .run(events_rx)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_explicit_quota_errors() {
        assert!(response_is_quota_exhausted(&json!({
            "error": {"code": -32000, "message": "Usage limit reached"}
        })));
        assert!(!response_is_quota_exhausted(&json!({
            "error": {"code": -32000, "message": "tool failed"}
        })));
        assert!(!response_is_quota_exhausted(
            &json!({"result": {"stopReason": "end_turn"}})
        ));
    }

    #[test]
    fn initialize_response_does_not_expose_account_login_home() {
        let mut response = json!({
            "result": {
                "authMethods": [{"env": {"KIMI_CODE_HOME": "/private/account"}}],
                "agentInfo": {"name": "Kimi Code CLI", "version": "0.36.1"}
            }
        });
        normalize_initialize_response(&mut response);
        assert_eq!(response.pointer("/result/authMethods"), Some(&json!([])));
        assert_eq!(
            response.pointer("/result/agentInfo/name"),
            Some(&json!("Kimi Subscription Router"))
        );
        assert!(!response.to_string().contains("/private/account"));
    }

    #[test]
    fn forked_context_resumes_the_new_session() {
        let request = json!({
            "params": {
                "sessionId": "source-session",
                "cwd": "/workspace",
                "mcpServers": []
            }
        });
        let context =
            SessionContext::from_request(&request, "source-session").for_session("forked-session");

        assert_eq!(
            context.resume_params.get("sessionId"),
            Some(&json!("forked-session"))
        );
        assert_eq!(context.resume_params.get("cwd"), Some(&json!("/workspace")));
    }
}
