//! 团队协作的网络编排（原生侧）。
//!
//! 队长：在本机启动 WebSocket 服务承载团队数据，队员凭邀请码接入；队员：
//! 维持一条到队长的长连接并持续同步。所有数据都落在本机数据库
//! （见 `storage::services::team`），网络层只负责传输与在线状态。

pub mod client;
pub mod protocol;
pub mod server;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use napi::bindgen_prelude::{Promise, Status};
use napi::threadsafe_function::ThreadsafeFunction;
use napi_derive::napi;
use serde::Serialize;

use crate::storage::services::team as store;

use client::{ClientConfig, ClientHandle, MediaResponse};
use protocol::ClientMessage;
use server::HostHandle;

/// 推送给 Node 主进程的团队事件。
#[napi(object)]
pub struct TeamEventPayload {
    /// 事件类型：status / snapshot / members / record-upserted /
    /// record-deleted / media-uploaded / error / idle / joined / removed。
    pub event: String,
    pub team_id: String,
    /// 事件载荷（JSON 字符串，由渲染层按事件类型解析）。
    pub payload_json: String,
}

/// 事件回调：Node 侧返回 Promise，Rust 不等待其结果。
pub type TeamEventCallback =
    ThreadsafeFunction<TeamEventPayload, Promise<()>, TeamEventPayload, Status, false>;

/// 队长服务状态。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostState {
    pub running: bool,
    pub team_id: String,
    pub port: u16,
    pub endpoint: String,
}

impl HostState {
    fn stopped() -> Self {
        Self {
            running: false,
            team_id: String::new(),
            port: 0,
            endpoint: String::new(),
        }
    }
}

static EVENT_SINK: OnceLock<TeamEventCallback> = OnceLock::new();
static HOST: OnceLock<Mutex<Option<HostHandle>>> = OnceLock::new();
static CLIENTS: OnceLock<Mutex<HashMap<String, Arc<ClientHandle>>>> = OnceLock::new();

fn host_slot() -> &'static Mutex<Option<HostHandle>> {
    HOST.get_or_init(|| Mutex::new(None))
}

fn client_slot() -> &'static Mutex<HashMap<String, Arc<ClientHandle>>> {
    CLIENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 注册事件回调（重复注册时保留首次注册的回调）。
pub fn register_event_sink(callback: TeamEventCallback) {
    let _ = EVENT_SINK.set(callback);
}

/// 推送一条事件给 Node 主进程；未注册回调时静默丢弃。
pub async fn emit_event(event: &str, team_id: &str, payload_json: String) {
    let Some(sink) = EVENT_SINK.get() else {
        return;
    };
    let _ = sink
        .call_async_catch(TeamEventPayload {
            event: event.to_string(),
            team_id: team_id.to_string(),
            payload_json,
        })
        .await;
}

/// 在阻塞线程池中执行一次数据库操作（Rust 侧不做同步 I/O）。
pub(crate) async fn db<T: Send + 'static>(
    run: impl FnOnce() -> napi::Result<T> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(run)
        .await
        .map_err(|error| format!("team task failed: {error}"))?
        .map_err(|error| error.reason.clone())
}

pub(crate) fn new_peer_id() -> String {
    format!(
        "peer-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..20]
    )
}

/// 本机设备标识：加入申请去重与队长黑名单都以它为依据。
/// 首次使用时生成并持久化到系统设置，之后保持不变。
pub fn local_client_id(database_path: &std::path::Path) -> String {
    const SETTING_CODE: &str = "team_client_id";
    if let Ok(Some(value)) =
        crate::storage::services::system_settings::get_system_setting_value(
            database_path,
            SETTING_CODE,
        )
    {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    let generated = format!(
        "client-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..20]
    );
    let _ = crate::storage::services::system_settings::set_system_setting(
        database_path,
        "团队设备标识",
        SETTING_CODE,
        &generated,
    );
    generated
}

// ===== 队长服务 =====

/// 启动队长服务（`port` 为 0 时由系统分配）。
pub async fn start_host(
    database_path: PathBuf,
    team_id: String,
    port: u16,
) -> Result<HostState, String> {
    stop_host().await;
    let handle = server::start(database_path, team_id, port).await?;
    let state = HostState {
        running: true,
        team_id: handle.team_id.clone(),
        port: handle.port,
        endpoint: handle.endpoint.clone(),
    };
    *host_slot().lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
    Ok(state)
}

/// 停止队长服务。
pub async fn stop_host() {
    let handle = host_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if let Some(handle) = handle {
        server::stop(handle).await;
    }
}

/// 查询队长服务状态（同步、无 I/O）。
pub fn host_state() -> HostState {
    let slot = host_slot().lock().unwrap_or_else(|e| e.into_inner());
    match slot.as_ref() {
        Some(handle) => HostState {
            running: true,
            team_id: handle.team_id.clone(),
            port: handle.port,
            endpoint: handle.endpoint.clone(),
        },
        None => HostState::stopped(),
    }
}

/// 队长侧把一条变更广播给所有在线队员（本地写入后调用）。
pub fn host_broadcast(message: protocol::ServerMessage) {
    let slot = host_slot().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(handle) = slot.as_ref() {
        handle.hub.broadcast(None, message);
    }
}

/// 取出当前队长服务的连接表（未运行时返回 None）。
fn host_hub() -> Option<Arc<server::Hub>> {
    let slot = host_slot().lock().unwrap_or_else(|e| e.into_inner());
    slot.as_ref().map(|handle| handle.hub.clone())
}

/// 队长移除成员：删除其成员记录、断开连接并广播最新成员列表。
///
/// 只解除当前会话——被移除者仍可凭邀请链接重新加入，不做永久封禁。
pub async fn host_remove_member(
    database_path: PathBuf,
    team_id: String,
    peer_id: String,
) -> Result<(), String> {
    let hub = host_hub();
    if let Some(hub) = hub.as_ref() {
        if hub.team_id != team_id {
            return Err("该团队不由本机承载队长服务".to_string());
        }
    }
    let removed = db({
        let database_path = database_path.clone();
        let team_id = team_id.clone();
        let peer_id = peer_id.clone();
        move || store::delete_member(&database_path, &team_id, &peer_id)
    })
    .await?;
    if !removed {
        return Err("成员不存在".to_string());
    }
    if let Some(hub) = hub {
        hub.kick(
            &peer_id,
            protocol::ServerMessage::Removed {
                message: "你已被队长移出团队".to_string(),
            },
        );
        server::broadcast_members(&hub, None).await;
    }
    Ok(())
}

/// 当前待审核的加入申请（队长面板展示；未承载该团队时为空）。
pub fn pending_join_requests(team_id: &str) -> Vec<server::PendingJoinInfo> {
    match host_hub() {
        Some(hub) if hub.team_id == team_id => hub.pending_requests(),
        _ => Vec::new(),
    }
}

/// 队长裁决一条加入申请：通过时下发重连令牌，拒绝时通知队员。
pub fn respond_join_request(
    team_id: &str,
    request_id: &str,
    approve: bool,
) -> Result<(), String> {
    let hub = host_hub().ok_or_else(|| "队长服务未运行".to_string())?;
    if hub.team_id != team_id {
        return Err("该团队不由本机承载队长服务".to_string());
    }
    let decision = if approve {
        server::JoinDecision::Approved {
            token: store::generate_member_token(),
        }
    } else {
        server::JoinDecision::Rejected {
            message: "队长拒绝了你的加入申请".to_string(),
        }
    };
    hub.resolve_pending(request_id, decision)
}

// ===== 队员客户端 =====

/// 用邀请码申请加入团队：队长已放行时写入本地数据并返回团队信息，
/// 首次加入则返回待审核状态（队长同意后由长连接写入数据）。
pub async fn join_via_invite(
    database_path: PathBuf,
    endpoint: String,
    invite_code: String,
    client_id: String,
    name: String,
    color: String,
) -> Result<client::JoinOutcome, String> {
    client::join_via_host(database_path, endpoint, invite_code, client_id, name, color).await
}

/// 启动（或替换）队员客户端的长连接。
pub fn connect_client(config: ClientConfig) {
    let team_id = config.team_id.clone();
    let handle = Arc::new(client::connect(config));
    let previous = client_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(team_id, handle);
    if let Some(previous) = previous {
        client::stop(&previous);
    }
}

/// 断开队员客户端。
pub fn disconnect_client(team_id: &str) {
    let handle = client_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(team_id);
    if let Some(handle) = handle {
        client::stop(&handle);
    }
}

/// 断开全部队员客户端（应用退出时调用）。
pub fn disconnect_all_clients() {
    let handles: Vec<Arc<ClientHandle>> = client_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain()
        .map(|(_, handle)| handle)
        .collect();
    for handle in handles {
        client::stop(&handle);
    }
}

fn client_handle(team_id: &str) -> Option<Arc<ClientHandle>> {
    client_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(team_id)
        .cloned()
}

/// 把本地改动推给队长；未连接时返回 false。
pub fn client_send(team_id: &str, message: ClientMessage) -> bool {
    match client_handle(team_id) {
        Some(handle) => handle.send(message),
        None => false,
    }
}

/// 队员是否已连接队长。
pub fn is_client_connected(team_id: &str) -> bool {
    client_handle(team_id).is_some()
}

/// 向队长请求附件内容（本地没有时使用）。
pub async fn client_request_media(
    team_id: &str,
    media_id: &str,
) -> Result<MediaResponse, String> {
    let Some(handle) = client_handle(team_id) else {
        return Err("尚未连接队长".to_string());
    };
    handle.request_media(media_id).await
}

/// 生成邀请链接（仅队长侧使用）。
pub fn build_invite_link(invite_code: &str, endpoint: &str) -> String {
    format!(
        "snow-team://join?host={}&code={}",
        endpoint.trim(),
        invite_code.trim()
    )
}

/// 解析邀请链接，返回 `(endpoint, invite_code)`。
pub fn parse_invite_link(link: &str) -> std::result::Result<(String, String), String> {
    let trimmed = link.trim();
    let rest = trimmed
        .strip_prefix("snow-team://join?")
        .or_else(|| trimmed.strip_prefix("snowteam://join?"))
        .ok_or_else(|| "邀请链接格式不正确".to_string())?;
    let mut endpoint = String::new();
    let mut code = String::new();
    for pair in rest.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let value = percent_decode(value);
        match key.trim() {
            "host" => endpoint = value,
            "code" => code = value.to_uppercase(),
            _ => {}
        }
    }
    if endpoint.is_empty() || code.is_empty() {
        return Err("邀请链接缺少必要参数".to_string());
    }
    Ok((endpoint, code))
}

fn percent_decode(value: &str) -> String {
    percent_encoding::percent_decode_str(value)
        .decode_utf8_lossy()
        .to_string()
}
