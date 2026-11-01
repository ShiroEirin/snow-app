//! 团队协作的 NAPI 导出：数据读写、队长服务与队员客户端的生命周期编排。
//!
//! 数据库与网络工作全部在 Rust 完成，Node 主进程只做 IPC 外壳与事件转发。

use napi::bindgen_prelude::*;
use napi_derive::napi;
use serde::Serialize;

use crate::storage::ensure_database_file;
use crate::storage::services::team as store;
use crate::team as team_net;
use crate::team::client::ClientConfig;
use crate::team::protocol::{ClientMessage, ServerMessage};
use crate::team::TeamEventCallback;

/// 团队协作的对外状态快照（当前项目所属团队 + 运行时连接状态）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TeamState {
    team: Option<store::TeamInfo>,
    /// 团队协作总开关（设置页「团队协作」）。
    enabled: bool,
    role: String,
    connected: bool,
    endpoint: String,
    invite_link: String,
    members: Vec<store::TeamMemberRecord>,
    projects: Vec<store::TeamProjectLink>,
    /// 队长专属：待审核的加入申请（队员需要队长放行后才能加入）。
    join_requests: Vec<team_net::server::PendingJoinInfo>,
    /// 当前项目下是否残留旧版基于 Git 的团队数据。
    legacy_available: bool,
}

/// 加入团队的结果：直接加入成功，或已提交申请等待队长审核。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TeamJoinResult {
    /// joined | pending
    status: String,
    team: Option<store::TeamInfo>,
}

/// 附件名黑名单：禁止可执行 / 脚本类文件随团队分发。
const BLOCKED_FILE_EXTS: &[&str] = &[
    "exe", "dll", "so", "dylib", "bat", "cmd", "com", "msi", "scr", "vbs", "vbe", "ps1", "psm1",
    "app", "deb", "rpm", "pkg", "dmg", "reg", "inf",
];

fn image_ext_ok(ext: &str) -> bool {
    matches!(ext, "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "svg")
}

/// 文件名安全化：保留原名（含中文），剥掉路径成分与危险字符。
fn sanitize_file_name(file_name: &str) -> napi::Result<String> {
    let base = std::path::Path::new(file_name.trim())
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let cleaned: String = base
        .chars()
        .filter(|ch| {
            !ch.is_control() && !matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
        })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').to_string();
    if cleaned.is_empty() || cleaned.len() > 160 {
        return Err(Error::from_reason("invalid file name"));
    }
    Ok(cleaned)
}

fn extension_of(file_name: &str) -> String {
    std::path::Path::new(file_name)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_lowercase()
}

/// 按扩展名推断 MIME 类型（用于把附件还原为 data URL）。
fn mime_of(file_name: &str) -> &'static str {
    match extension_of(file_name).as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "pdf" => "application/pdf",
        "json" => "application/json",
        "zip" => "application/zip",
        "gz" | "gzip" => "application/gzip",
        "tar" => "application/x-tar",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "txt" | "md" | "log" => "text/plain; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "xml" | "html" | "htm" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn data_url(file_name: &str, bytes: &[u8]) -> String {
    format!(
        "data:{};base64,{}",
        mime_of(file_name),
        team_net::server::encode_media(bytes)
    )
}

fn project_name_of(path: &str) -> String {
    std::path::Path::new(path.trim())
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_string()
}

async fn role_of(database_path: &std::path::Path, team_id: &str) -> String {
    let database_path = database_path.to_path_buf();
    let team_id = team_id.to_string();
    team_net::db(move || store::get_team(&database_path, &team_id))
        .await
        .ok()
        .flatten()
        .map(|team| team.role)
        .unwrap_or_default()
}

/// 注册团队事件回调（Rust → Node，用于把变更实时推给渲染层）。
#[napi]
pub fn set_team_event_handler(callback: TeamEventCallback) {
    team_net::register_event_sink(callback);
}

/// 读取当前项目所属团队的状态快照（JSON 字符串）。
#[napi]
pub async fn team_get_state(
    directory_id: String,
    directory_path: String,
) -> napi::Result<String> {
    let database_path = ensure_database_file()?;
    let directory_id = directory_id.trim().to_string();
    let directory_path = directory_path.trim().to_string();
    let legacy_available =
        !directory_path.is_empty() && store::legacy_team_data_exists(&directory_path);

    let team = if directory_id.is_empty() {
        None
    } else {
        team_net::db({
            let database_path = database_path.clone();
            let directory_id = directory_id.clone();
            move || store::find_team_for_directory(&database_path, &directory_id)
        })
        .await
        .map_err(Error::from_reason)?
    };

    let Some(team) = team else {
        return Ok(encode_state(TeamState {
            team: None,
            enabled: store::is_team_enabled(),
            role: String::new(),
            connected: false,
            endpoint: String::new(),
            invite_link: String::new(),
            members: Vec::new(),
            projects: Vec::new(),
            join_requests: Vec::new(),
            legacy_available,
        }));
    };

    let team_id = team.team_id.clone();
    let role = team.role.clone();
    let members = team_net::db({
        let database_path = database_path.clone();
        let team_id = team_id.clone();
        move || store::list_members(&database_path, &team_id)
    })
    .await
    .map_err(Error::from_reason)?;
    let projects = team_net::db({
        let database_path = database_path.clone();
        let team_id = team_id.clone();
        move || store::list_team_projects(&database_path, &team_id)
    })
    .await
    .map_err(Error::from_reason)?;

    let host_state = team_net::host_state();
    let is_host = role == store::ROLE_HOST;
    let host_running = is_host && host_state.running && host_state.team_id == team_id;
    let connected = if is_host {
        host_running
    } else {
        team_net::is_client_connected(&team_id)
    };
    let endpoint = if host_running {
        host_state.endpoint.clone()
    } else {
        team.host_endpoint.clone()
    };
    let invite_link = if is_host && !team.invite_code.is_empty() && !endpoint.is_empty() {
        team_net::build_invite_link(&team.invite_code, &endpoint)
    } else {
        String::new()
    };
    let join_requests = if host_running {
        team_net::pending_join_requests(&team_id)
    } else {
        Vec::new()
    };

    Ok(encode_state(TeamState {
        team: Some(team),
        enabled: store::is_team_enabled(),
        role,
        connected,
        endpoint,
        invite_link,
        members,
        projects,
        join_requests,
        legacy_available,
    }))
}

fn encode_state(state: TeamState) -> String {
    serde_json::to_string(&state).unwrap_or_else(|_| "{}".to_string())
}

/// 列出本机参与的全部团队（JSON 数组）。
#[napi]
pub async fn team_list_teams() -> napi::Result<String> {
    let database_path = ensure_database_file()?;
    let teams = team_net::db(move || store::list_teams(&database_path))
        .await
        .map_err(Error::from_reason)?;
    Ok(serde_json::to_string(&teams).unwrap_or_else(|_| "[]".to_string()))
}

/// 创建团队并把当前项目关联进去（旧版 Git 团队数据会一并导入）。
#[napi]
pub async fn team_create_team(
    directory_id: String,
    directory_path: String,
    name: String,
    self_name: String,
    self_color: String,
) -> napi::Result<String> {
    let database_path = ensure_database_file()?;
    let directory_id = directory_id.trim().to_string();
    let directory_path = directory_path.trim().to_string();
    let project_name = project_name_of(&directory_path);
    let team = team_net::db({
        let database_path = database_path.clone();
        move || {
            let team = store::create_team(&database_path, &name, &self_name, &self_color)?;
            if !directory_id.is_empty() {
                store::link_project(
                    &database_path,
                    &team.team_id,
                    &directory_id,
                    &project_name,
                )?;
            }
            if !directory_path.is_empty() && store::legacy_team_data_exists(&directory_path) {
                let _ = store::import_legacy_team_data(
                    &database_path,
                    &team.team_id,
                    &directory_path,
                );
            }
            Ok(team)
        }
    })
    .await
    .map_err(Error::from_reason)?;
    Ok(serde_json::to_string(&team).unwrap_or_else(|_| "{}".to_string()))
}

/// 通过邀请链接申请加入团队，并启动与队长的长连接。
/// 首次加入需要队长审核（返回 `pending`），审核通过后由长连接写入团队数据。
#[napi]
pub async fn team_join_team(
    invite_link: String,
    self_name: String,
    self_color: String,
) -> napi::Result<String> {
    let database_path = ensure_database_file()?;
    let (endpoint, invite_code) =
        team_net::parse_invite_link(&invite_link).map_err(Error::from_reason)?;
    let client_id = {
        let database_path = database_path.clone();
        team_net::db(move || Ok(team_net::local_client_id(&database_path)))
            .await
            .map_err(Error::from_reason)?
    };
    let outcome = team_net::join_via_invite(
        database_path.clone(),
        endpoint.clone(),
        invite_code.clone(),
        client_id.clone(),
        self_name.clone(),
        self_color.clone(),
    )
    .await
    .map_err(Error::from_reason)?;

    match outcome {
        crate::team::client::JoinOutcome::Joined(team) => {
            let token = member_token_of(&database_path, &team.team_id).await;
            team_net::connect_client(ClientConfig {
                database_path,
                team_id: team.team_id.clone(),
                endpoint,
                invite_code,
                client_id,
                token,
                name: self_name,
                color: self_color,
            });
            Ok(encode_join_result("joined", Some(team)))
        }
        crate::team::client::JoinOutcome::Pending => {
            // 待审核：长连接继续等待队长放行（团队 id 在收到快照后补齐）。
            team_net::connect_client(ClientConfig {
                database_path,
                team_id: String::new(),
                endpoint,
                invite_code,
                client_id,
                token: String::new(),
                name: self_name,
                color: self_color,
            });
            Ok(encode_join_result("pending", None))
        }
    }
}

fn encode_join_result(status: &str, team: Option<store::TeamInfo>) -> String {
    serde_json::to_string(&TeamJoinResult {
        status: status.to_string(),
        team,
    })
    .unwrap_or_else(|_| "{}".to_string())
}

/// 读取本机（队员）在该团队中的重连令牌；未下发时返回空字符串。
async fn member_token_of(database_path: &std::path::Path, team_id: &str) -> String {
    let database_path = database_path.to_path_buf();
    let team_id = team_id.to_string();
    team_net::db(move || store::get_member_token(&database_path, &team_id))
        .await
        .unwrap_or_default()
}

/// 退出并删除团队（队长会同时停止服务）。
#[napi]
pub async fn team_leave_team(team_id: String) -> napi::Result<bool> {
    let database_path = ensure_database_file()?;
    let team_id = team_id.trim().to_string();
    let role = role_of(&database_path, &team_id).await;
    if role == store::ROLE_HOST {
        team_net::stop_host().await;
    } else {
        team_net::disconnect_client(&team_id);
    }
    let removed = team_net::db({
        let database_path = database_path.clone();
        let team_id = team_id.clone();
        move || store::delete_team(&database_path, &team_id)
    })
    .await
    .map_err(Error::from_reason)?;
    Ok(removed)
}

/// 更新本机在团队中的昵称与头像色。
#[napi]
pub async fn team_set_profile(
    team_id: String,
    name: String,
    color: String,
) -> napi::Result<String> {
    let database_path = ensure_database_file()?;
    let team_id = team_id.trim().to_string();
    let team = team_net::db({
        let database_path = database_path.clone();
        let team_id = team_id.clone();
        move || store::update_team_profile(&database_path, &team_id, &name, &color)
    })
    .await
    .map_err(Error::from_reason)?;
    Ok(serde_json::to_string(&team).unwrap_or_else(|_| "{}".to_string()))
}

/// 关联一个本地项目到团队。
#[napi]
pub async fn team_link_project(
    team_id: String,
    directory_id: String,
    name: String,
) -> napi::Result<()> {
    let database_path = ensure_database_file()?;
    let team_id = team_id.trim().to_string();
    team_net::db({
        let database_path = database_path.clone();
        move || store::link_project(&database_path, &team_id, &directory_id, &name)
    })
    .await
    .map_err(Error::from_reason)
}

/// 解除项目与团队的关联。
#[napi]
pub async fn team_unlink_project(
    team_id: String,
    directory_id: String,
) -> napi::Result<bool> {
    let database_path = ensure_database_file()?;
    let team_id = team_id.trim().to_string();
    team_net::db({
        let database_path = database_path.clone();
        move || store::unlink_project(&database_path, &team_id, &directory_id)
    })
    .await
    .map_err(Error::from_reason)
}

/// 列出团队关联的项目（JSON 数组）。
#[napi]
pub async fn team_list_projects(team_id: String) -> napi::Result<String> {
    let database_path = ensure_database_file()?;
    let team_id = team_id.trim().to_string();
    let projects = team_net::db({
        let database_path = database_path.clone();
        move || store::list_team_projects(&database_path, &team_id)
    })
    .await
    .map_err(Error::from_reason)?;
    Ok(serde_json::to_string(&projects).unwrap_or_else(|_| "[]".to_string()))
}

/// 列出某类记录（原始 JSON 字符串数组）。
#[napi]
pub async fn team_list_records(team_id: String, kind: String) -> napi::Result<Vec<String>> {
    let database_path = ensure_database_file()?;
    let team_id = team_id.trim().to_string();
    team_net::db({
        let database_path = database_path.clone();
        move || store::list_records(&database_path, &team_id, &kind)
    })
    .await
    .map_err(Error::from_reason)
}

/// 写入或更新一条记录，并把变更同步给其他成员。
#[napi]
pub async fn team_upsert_record(
    team_id: String,
    kind: String,
    record_id: String,
    json: String,
) -> napi::Result<String> {
    let database_path = ensure_database_file()?;
    let team_id = team_id.trim().to_string();
    let payload = team_net::db({
        let database_path = database_path.clone();
        let team_id = team_id.clone();
        let kind = kind.clone();
        let record_id = record_id.clone();
        let json = json.clone();
        move || store::upsert_record(&database_path, &team_id, &kind, &record_id, &json)
    })
    .await
    .map_err(Error::from_reason)?;

    let message = ServerMessage::RecordUpserted {
        kind: kind.clone(),
        record_id: record_id.clone(),
        payload: payload.clone(),
    };
    broadcast_change(&database_path, &team_id, message).await;
    Ok(payload)
}

/// 删除一条记录，并把变更同步给其他成员。
#[napi]
pub async fn team_delete_record(
    team_id: String,
    kind: String,
    record_id: String,
) -> napi::Result<bool> {
    let database_path = ensure_database_file()?;
    let team_id = team_id.trim().to_string();
    let removed = team_net::db({
        let database_path = database_path.clone();
        let team_id = team_id.clone();
        let kind = kind.clone();
        let record_id = record_id.clone();
        move || store::delete_record(&database_path, &team_id, &kind, &record_id)
    })
    .await
    .map_err(Error::from_reason)?;
    if removed {
        broadcast_change(
            &database_path,
            &team_id,
            ServerMessage::RecordDeleted {
                kind,
                record_id,
            },
        )
        .await;
    }
    Ok(removed)
}

/// 队长本地写入后广播；队员本地写入后推给队长。
async fn broadcast_change(
    database_path: &std::path::Path,
    team_id: &str,
    message: ServerMessage,
) {
    let role = role_of(database_path, team_id).await;
    if role == store::ROLE_HOST {
        team_net::host_broadcast(message);
        return;
    }
    if role == store::ROLE_MEMBER {
        let client_message = match message {
            ServerMessage::RecordUpserted {
                kind,
                record_id,
                payload,
            } => ClientMessage::RecordUpsert {
                kind,
                record_id,
                payload,
            },
            ServerMessage::RecordDeleted { kind, record_id } => {
                ClientMessage::RecordDelete { kind, record_id }
            }
            _ => return,
        };
        team_net::client_send(team_id, client_message);
    }
}

/// 保存附件，返回附件 id。
#[napi]
pub async fn team_save_media(
    team_id: String,
    owner_id: String,
    file_name: String,
    base64_data: String,
    image_only: bool,
) -> napi::Result<String> {
    let database_path = ensure_database_file()?;
    let team_id = team_id.trim().to_string();
    let owner_id = owner_id.trim().to_string();
    let file_name = sanitize_file_name(&file_name)?;
    let ext = extension_of(&file_name);
    if image_only {
        if !image_ext_ok(&ext) {
            return Err(Error::from_reason("unsupported image extension"));
        }
    } else if BLOCKED_FILE_EXTS.contains(&ext.as_str()) {
        return Err(Error::from_reason("file type not allowed"));
    }
    let is_image = image_ext_ok(&ext);
    let bytes = team_net::server::decode_media(&base64_data).map_err(Error::from_reason)?;
    if bytes.is_empty() {
        return Err(Error::from_reason("empty media data"));
    }
    if bytes.len() > store::MEDIA_MAX_BYTES {
        return Err(Error::from_reason("media exceeds size limit"));
    }

    let encoded = team_net::server::encode_media(&bytes);
    let media_id = team_net::db({
        let database_path = database_path.clone();
        let team_id = team_id.clone();
        let owner_id = owner_id.clone();
        let file_name = file_name.clone();
        move || {
            store::save_media(
                &database_path,
                &team_id,
                &owner_id,
                &file_name,
                is_image,
                &bytes,
            )
        }
    })
    .await
    .map_err(Error::from_reason)?;

    let role = role_of(&database_path, &team_id).await;
    if role == store::ROLE_MEMBER {
        team_net::client_send(
            &team_id,
            ClientMessage::MediaUpload {
                owner_id,
                file_name,
                is_image,
                data: encoded,
            },
        );
    }
    Ok(media_id)
}

/// 读取附件内容，返回 data URL；队员本地缺失时向队长请求。
#[napi]
pub async fn team_read_media(team_id: String, media_id: String) -> napi::Result<String> {
    let database_path = ensure_database_file()?;
    let team_id = team_id.trim().to_string();
    let media_id = media_id.trim().to_string();
    let local = team_net::db({
        let database_path = database_path.clone();
        let team_id = team_id.clone();
        let media_id = media_id.clone();
        move || store::read_media(&database_path, &team_id, &media_id)
    })
    .await
    .map_err(Error::from_reason)?;
    if let Some((meta, bytes)) = local {
        return Ok(data_url(&meta.file_name, &bytes));
    }
    let (file_name, _is_image, bytes) = team_net::client_request_media(&team_id, &media_id)
        .await
        .map_err(Error::from_reason)?;
    Ok(data_url(&file_name, &bytes))
}

/// 删除某条记录名下的全部附件。
#[napi]
pub async fn team_delete_media(team_id: String, owner_id: String) -> napi::Result<bool> {
    let database_path = ensure_database_file()?;
    let team_id = team_id.trim().to_string();
    team_net::db({
        let database_path = database_path.clone();
        move || store::delete_media(&database_path, &team_id, &owner_id)
    })
    .await
    .map_err(Error::from_reason)
}

/// 启动队长服务（`port` 为 0 时由系统分配），并把可连地址写入团队记录。
#[napi]
pub async fn team_start_host(team_id: String, port: u32) -> napi::Result<String> {
    let database_path = ensure_database_file()?;
    let team_id = team_id.trim().to_string();
    let state = team_net::start_host(database_path.clone(), team_id.clone(), port as u16)
        .await
        .map_err(Error::from_reason)?;
    team_net::db({
        let database_path = database_path.clone();
        let team_id = team_id.clone();
        let endpoint = state.endpoint.clone();
        move || store::update_team_host_endpoint(&database_path, &team_id, &endpoint)
    })
    .await
    .map_err(Error::from_reason)?;
    Ok(serde_json::to_string(&state).unwrap_or_else(|_| "{}".to_string()))
}

/// 停止队长服务。
#[napi]
pub async fn team_stop_host() -> napi::Result<()> {
    team_net::stop_host().await;
    Ok(())
}

/// 查询队长服务状态（JSON 字符串）。
#[napi]
pub fn team_host_state() -> String {
    serde_json::to_string(&team_net::host_state()).unwrap_or_else(|_| "{}".to_string())
}

/// 设置队长常驻服务开关（是否注册系统级登录自启）。
#[napi]
pub async fn team_set_host_service(team_id: String, enabled: bool) -> napi::Result<()> {
    let database_path = ensure_database_file()?;
    let team_id = team_id.trim().to_string();
    team_net::db({
        let database_path = database_path.clone();
        move || store::set_host_service(&database_path, &team_id, enabled)
    })
    .await
    .map_err(Error::from_reason)
}

/// 队长移除团队成员：删除成员记录、断开其连接并广播最新成员列表。
#[napi]
pub async fn team_remove_member(team_id: String, peer_id: String) -> napi::Result<()> {
    let database_path = ensure_database_file()?;
    let team_id = team_id.trim().to_string();
    let peer_id = peer_id.trim().to_string();
    if peer_id.is_empty() {
        return Err(Error::from_reason("成员标识不能为空"));
    }
    let team = team_net::db({
        let database_path = database_path.clone();
        let team_id = team_id.clone();
        move || store::get_team(&database_path, &team_id)
    })
    .await
    .map_err(Error::from_reason)?
    .ok_or_else(|| Error::from_reason("团队不存在"))?;
    if team.role != store::ROLE_HOST {
        return Err(Error::from_reason("只有队长可以移除成员"));
    }
    if team.self_peer_id == peer_id {
        return Err(Error::from_reason("不能移除自己"));
    }
    team_net::host_remove_member(database_path, team_id, peer_id)
        .await
        .map_err(Error::from_reason)
}

/// 校验本机是该团队的队长，并返回团队记录。
async fn require_host_team(
    database_path: &std::path::Path,
    team_id: &str,
) -> napi::Result<store::TeamInfo> {
    let database_path = database_path.to_path_buf();
    let team_id = team_id.to_string();
    let team = team_net::db(move || store::get_team(&database_path, &team_id))
        .await
        .map_err(Error::from_reason)?
        .ok_or_else(|| Error::from_reason("团队不存在"))?;
    if team.role != store::ROLE_HOST {
        return Err(Error::from_reason("只有队长可以执行该操作"));
    }
    Ok(team)
}

/// 队长裁决一条加入申请：通过则对方立即加入，拒绝则断开其连接。
#[napi]
pub async fn team_respond_join_request(
    team_id: String,
    request_id: String,
    approve: bool,
) -> napi::Result<()> {
    let database_path = ensure_database_file()?;
    let team_id = team_id.trim().to_string();
    let request_id = request_id.trim().to_string();
    if request_id.is_empty() {
        return Err(Error::from_reason("加入申请标识不能为空"));
    }
    require_host_team(&database_path, &team_id).await?;
    team_net::respond_join_request(&team_id, &request_id, approve).map_err(Error::from_reason)
}

/// 重置邀请码：旧邀请链接立即失效，已加入成员凭重连令牌不受影响。
#[napi]
pub async fn team_reset_invite(team_id: String) -> napi::Result<String> {
    let database_path = ensure_database_file()?;
    let team_id = team_id.trim().to_string();
    require_host_team(&database_path, &team_id).await?;
    let team = team_net::db({
        let database_path = database_path.clone();
        let team_id = team_id.clone();
        move || store::reset_invite_code(&database_path, &team_id)
    })
    .await
    .map_err(Error::from_reason)?;
    Ok(serde_json::to_string(&team).unwrap_or_else(|_| "{}".to_string()))
}

/// 拉黑成员：加入黑名单、移出团队并断开其连接（对方无法再次加入）。
#[napi]
pub async fn team_block_member(team_id: String, peer_id: String) -> napi::Result<()> {
    let database_path = ensure_database_file()?;
    let team_id = team_id.trim().to_string();
    let peer_id = peer_id.trim().to_string();
    if peer_id.is_empty() {
        return Err(Error::from_reason("成员标识不能为空"));
    }
    let team = require_host_team(&database_path, &team_id).await?;
    if team.self_peer_id == peer_id {
        return Err(Error::from_reason("不能拉黑自己"));
    }
    let members = team_net::db({
        let database_path = database_path.clone();
        let team_id = team_id.clone();
        move || store::list_members(&database_path, &team_id)
    })
    .await
    .map_err(Error::from_reason)?;
    let member_name = members
        .into_iter()
        .find(|item| item.peer_id == peer_id)
        .map(|item| item.name)
        .ok_or_else(|| Error::from_reason("成员不存在"))?;
    let client_id = team_net::db({
        let database_path = database_path.clone();
        let team_id = team_id.clone();
        let peer_id = peer_id.clone();
        move || store::member_client_id(&database_path, &team_id, &peer_id)
    })
    .await
    .map_err(Error::from_reason)?
    .unwrap_or_default();
    if client_id.trim().is_empty() {
        return Err(Error::from_reason("该成员缺少设备标识，无法拉黑"));
    }
    team_net::db({
        let database_path = database_path.clone();
        let team_id = team_id.clone();
        move || store::add_blocklist_entry(&database_path, &team_id, &client_id, &member_name)
    })
    .await
    .map_err(Error::from_reason)?;
    team_net::host_remove_member(database_path, team_id, peer_id)
        .await
        .map_err(Error::from_reason)
}

/// 解除拉黑：对方可再次凭邀请链接申请加入。
#[napi]
pub async fn team_unblock_member(team_id: String, client_id: String) -> napi::Result<bool> {
    let database_path = ensure_database_file()?;
    let team_id = team_id.trim().to_string();
    let client_id = client_id.trim().to_string();
    if client_id.is_empty() {
        return Err(Error::from_reason("设备标识不能为空"));
    }
    require_host_team(&database_path, &team_id).await?;
    team_net::db({
        let database_path = database_path.clone();
        move || store::remove_blocklist_entry(&database_path, &team_id, &client_id)
    })
    .await
    .map_err(Error::from_reason)
}

/// 列出团队黑名单（JSON 数组）。
#[napi]
pub async fn team_list_blocklist(team_id: String) -> napi::Result<String> {
    let database_path = ensure_database_file()?;
    let team_id = team_id.trim().to_string();
    let blocked = team_net::db({
        let database_path = database_path.clone();
        move || store::list_blocklist(&database_path, &team_id)
    })
    .await
    .map_err(Error::from_reason)?;
    Ok(serde_json::to_string(&blocked).unwrap_or_else(|_| "[]".to_string()))
}

/// 依据团队笔记重建关联项目的「团队项目知识」Skill（笔记增删改后调用）。
#[napi]
pub async fn team_sync_knowledge(team_id: String) -> napi::Result<()> {
    let database_path = ensure_database_file()?;
    let team_id = team_id.trim().to_string();
    let projects = team_net::db({
        let database_path = database_path.clone();
        let team_id = team_id.clone();
        move || store::list_team_projects(&database_path, &team_id)
    })
    .await
    .map_err(Error::from_reason)?;
    if projects.is_empty() {
        return Ok(());
    }
    let directories = tokio::task::spawn_blocking(crate::storage::list_workspace_directories)
        .await
        .map_err(|error| Error::from_reason(format!("team task failed: {error}")))?
        .map_err(|error| Error::from_reason(error.reason.clone()))?;
    for project in projects {
        let Some(directory) = directories
            .iter()
            .find(|item| item.directory_id == project.directory_id)
        else {
            continue;
        };
        let path = directory.path.clone();
        let directory_id = project.directory_id.clone();
        let _ = team_net::db({
            let database_path = database_path.clone();
            move || {
                store::sync_knowledge_skill(&database_path, &directory_id, &path);
                Ok(())
            }
        })
        .await;
    }
    Ok(())
}

/// 应用启动时恢复团队连接：队长启动服务，队员恢复长连接。
#[napi]
pub async fn team_resume_services() -> napi::Result<()> {
    let database_path = ensure_database_file()?;
    let teams = team_net::db({
        let database_path = database_path.clone();
        move || store::list_teams(&database_path)
    })
    .await
    .map_err(Error::from_reason)?;

    let client_id = team_net::db({
        let database_path = database_path.clone();
        move || Ok(team_net::local_client_id(&database_path))
    })
    .await
    .map_err(Error::from_reason)?;

    for team in teams {
        if team.role == store::ROLE_HOST {
            let team_id = team.team_id.clone();
            if let Ok(state) = team_net::start_host(database_path.clone(), team_id.clone(), 0).await
            {
                let endpoint = state.endpoint.clone();
                let _ = team_net::db({
                    let database_path = database_path.clone();
                    let team_id = team_id.clone();
                    move || store::update_team_host_endpoint(&database_path, &team_id, &endpoint)
                })
                .await;
            }
        } else if !team.host_endpoint.is_empty() {
            let token = member_token_of(&database_path, &team.team_id).await;
            team_net::connect_client(ClientConfig {
                database_path: database_path.clone(),
                team_id: team.team_id.clone(),
                endpoint: team.host_endpoint.clone(),
                invite_code: team.invite_code.clone(),
                client_id: client_id.clone(),
                token,
                name: team.self_name.clone(),
                color: team.self_color.clone(),
            });
        }
    }
    Ok(())
}

/// 应用退出时停止全部团队网络活动。
#[napi]
pub async fn team_shutdown_services() -> napi::Result<()> {
    team_net::disconnect_all_clients();
    team_net::stop_host().await;
    Ok(())
}
