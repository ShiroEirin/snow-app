//! ComfyUI HTTP 协议客户端。
//!
//! 只封装引擎公开 API，不含任何业务语义（参数映射在 `workflow`，格式转换在 `convert`）。
//! 所有方法都返回 `napi::Result`，错误信息带可操作提示，便于直接冒泡给 Agent/UI。

use std::time::Duration;

use napi::bindgen_prelude::*;
use serde_json::{json, Value};

/// 参考图负载（base64 + mime），与 imagegen 的 `ReferenceImage` 解耦，
/// 避免共享层反向依赖某个具体 provider 的私有类型。
pub struct ImagePayload {
    pub data: String,
    pub mime_type: String,
}

/// ComfyUI 客户端。`base_url` 已归一（去尾斜杠），形如 `http://127.0.0.1:8188`。
///
/// `Clone` 只复制 `String` 与 `reqwest::Client` 句柄，用于让清理守卫在
/// 生成 future 被丢弃后仍能发起一次队列清理。
#[derive(Clone)]
pub struct ComfyUiClient {
    base_url: String,
    http: reqwest::Client,
}

impl ComfyUiClient {
    pub async fn new(base_url: &str, timeout_secs: Option<u64>) -> Result<Self> {
        let base_url = base_url.trim().trim_end_matches('/').to_string();
        if base_url.is_empty() {
            return Err(Error::from_reason(
                "ComfyUI base URL is empty; set it to http://127.0.0.1:8188 (ComfyUI's own root, no /v1 path).",
            ));
        }
        if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
            return Err(Error::from_reason(format!(
                "Invalid ComfyUI base URL \"{base_url}\": only http(s) is supported"
            )));
        }
        // 与 imagegen 全局超时口径一致：1 分钟 ~ 1 小时。
        let timeout = Duration::from_secs(timeout_secs.unwrap_or(300).clamp(60, 3600));
        let http = crate::api::http_client::build_proxied_client_with_timeout(timeout)
            .await
            .map_err(|error| Error::from_reason(format!("Failed to create HTTP client: {error}")))?;
        Ok(Self { base_url, http })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    async fn get_json(&self, path: &str, label: &str) -> Result<Value> {
        let response = self
            .http
            .get(self.url(path))
            .send()
            .await
            .map_err(|error| Error::from_reason(format!("ComfyUI {label} request failed: {error}")))?;
        let status = response.status();
        let body: Value = response.json().await.unwrap_or_else(|_| json!({}));
        if !status.is_success() {
            return Err(api_error(&format!("ComfyUI {label} failed"), status.as_u16(), &body));
        }
        Ok(body)
    }

    /// `GET /system_stats`：设备 / 显存 / ComfyUI 版本。
    pub async fn system_stats(&self) -> Result<Value> {
        self.get_json("/system_stats", "system stats").await
    }

    /// `GET /queue`：运行中与排队中的任务。
    pub async fn queue(&self) -> Result<Value> {
        self.get_json("/queue", "queue").await
    }

    /// `GET /object_info`：全部节点定义（原始 JSON 文本）。
    ///
    /// 返回文本而不是 `Value`：UI→API 转换必须按节点输入的**声明顺序**映射 widget 值，
    /// 而 `serde_json::Value` 的对象键会排序，顺序信息一旦丢失就无法正确映射。
    pub async fn object_info_text(&self) -> Result<String> {
        let response = self
            .http
            .get(self.url("/object_info"))
            .send()
            .await
            .map_err(|error| {
                Error::from_reason(format!("ComfyUI object info request failed: {error}"))
            })?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let body: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({}));
            return Err(api_error("ComfyUI object info failed", status.as_u16(), &body));
        }
        Ok(text)
    }

    /// `GET /userdata?dir=workflows`：用户在 ComfyUI 界面里保存的画布工作流文件名。
    pub async fn list_workflows(&self) -> Result<Vec<String>> {
        let mut url = reqwest::Url::parse(&self.url("/userdata"))
            .map_err(|error| Error::from_reason(format!("Invalid ComfyUI base URL: {error}")))?;
        url.query_pairs_mut().append_pair("dir", "workflows");
        let response = self.http.get(url).send().await.map_err(|error| {
            Error::from_reason(format!("ComfyUI workflow list request failed: {error}"))
        })?;
        let status = response.status();
        let body: Value = response.json().await.unwrap_or_else(|_| json!([]));
        if !status.is_success() {
            return Err(api_error("ComfyUI workflow list failed", status.as_u16(), &body));
        }
        Ok(body
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default())
    }

    /// `GET /userdata/{path}`：读取一个画布工作流（UI 格式）。
    ///
    /// 注意：ComfyUI 把 userdata 路径当成**单个** URL 段解析，必须整段百分号编码
    /// （`/userdata/workflows%2Fsd15.json`）；用真斜杠 `/userdata/workflows/name`
    /// 会直接 404（实机 v0.43 已验证）。
    pub async fn get_workflow(&self, name: &str) -> Result<Value> {
        let encoded = encode_userdata_path(&format!("workflows/{name}"));
        let url = self.url(&format!("/userdata/{encoded}"));
        let response = self.http.get(url).send().await.map_err(|error| {
            Error::from_reason(format!("ComfyUI workflow read failed: {error}"))
        })?;
        let status = response.status();
        let body: Value = response.json().await.unwrap_or_else(|_| json!({}));
        if !status.is_success() {
            return Err(api_error("ComfyUI workflow read failed", status.as_u16(), &body));
        }
        Ok(body)
    }

    /// `POST /upload/image`：把参考图放进 ComfyUI 的 input 目录，返回可写进工作流的
    /// `subfolder/name`（subfolder 为空时只返回 name）。
    pub async fn upload_image(&self, image: &ImagePayload) -> Result<String> {
        let bytes = crate::comfyui::client::decode_base64(&image.data)?;
        let file_name = format!("snow-ref-{}.{}", now_nanos(), ext_for_mime(&image.mime_type));
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(file_name)
            .mime_str(&image.mime_type)
            .map_err(|error| {
                Error::from_reason(format!("Failed to build ComfyUI upload part: {error}"))
            })?;
        let form = reqwest::multipart::Form::new()
            .part("image", part)
            .text("overwrite", "true")
            .text("type", "input");

        let response = self
            .http
            .post(self.url("/upload/image"))
            .multipart(form)
            .send()
            .await
            .map_err(|error| Error::from_reason(format!("ComfyUI image upload failed: {error}")))?;
        let status = response.status();
        let body: Value = response.json().await.unwrap_or_else(|_| json!({}));
        if !status.is_success() {
            return Err(api_error("ComfyUI image upload failed", status.as_u16(), &body));
        }
        let name = body
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if name.is_empty() {
            return Err(Error::from_reason(format!(
                "ComfyUI upload response is missing `name`: {body}"
            )));
        }
        let subfolder = body
            .get("subfolder")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        Ok(if subfolder.is_empty() {
            name.to_string()
        } else {
            format!("{subfolder}/{name}")
        })
    }

    /// `POST /prompt`：提交 API 格式工作流，返回 `prompt_id`。
    pub async fn submit_prompt(&self, graph: &Value, client_id: &str) -> Result<String> {
        let body = json!({ "prompt": graph, "client_id": client_id });
        let response = self
            .http
            .post(self.url("/prompt"))
            .json(&body)
            .send()
            .await
            .map_err(|error| Error::from_reason(format!("ComfyUI /prompt request failed: {error}")))?;
        let status = response.status();
        let response_body: Value = response.json().await.unwrap_or_else(|_| json!({}));
        if !status.is_success() {
            return Err(api_error("ComfyUI prompt failed", status.as_u16(), &response_body));
        }
        let prompt_id = response_body
            .get("prompt_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                Error::from_reason(format!(
                    "ComfyUI /prompt response is missing `prompt_id`: {response_body}"
                ))
            })?;
        Ok(prompt_id)
    }

    /// `GET /history/{prompt_id}`：已完成任务的记录（未完成时该 id 不在响应里）。
    pub async fn history(&self, prompt_id: &str) -> Result<Value> {
        self.get_json(&format!("/history/{prompt_id}"), "history").await
    }

    /// `GET /history`：全部已结束任务（引擎只保留最近记录，重启即清空）。
    pub async fn history_all(&self) -> Result<Value> {
        self.get_json("/history", "history").await
    }

    /// `GET /view`：下载输出图片字节。
    pub async fn view_image(&self, filename: &str, subfolder: &str, kind: &str) -> Result<Vec<u8>> {
        let mut url = reqwest::Url::parse(&self.url("/view"))
            .map_err(|error| Error::from_reason(format!("Invalid ComfyUI base URL: {error}")))?;
        url.query_pairs_mut()
            .append_pair("filename", filename)
            .append_pair("subfolder", subfolder)
            .append_pair("type", kind);
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|error| Error::from_reason(format!("ComfyUI /view request failed: {error}")))?;
        let status = response.status();
        if !status.is_success() {
            let body: Value = response.json().await.unwrap_or_else(|_| json!({}));
            return Err(api_error("ComfyUI view failed", status.as_u16(), &body));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|error| Error::from_reason(format!("Failed to read ComfyUI image bytes: {error}")))?;
        if bytes.is_empty() {
            return Err(Error::from_reason(format!(
                "ComfyUI returned an empty image for \"{filename}\""
            )));
        }
        Ok(bytes.to_vec())
    }

    /// `POST /interrupt`：中断当前执行（全局语义，ComfyUI 没有单任务中断）。
    pub async fn interrupt(&self) -> Result<()> {
        let response = self
            .http
            .post(self.url("/interrupt"))
            .json(&json!({}))
            .send()
            .await
            .map_err(|error| Error::from_reason(format!("ComfyUI /interrupt failed: {error}")))?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body: Value = response.json().await.unwrap_or_else(|_| json!({}));
            return Err(api_error("ComfyUI interrupt failed", status, &body));
        }
        Ok(())
    }

    /// `POST /queue {"delete": [...]}`：从队列移除指定任务（排队中可删，运行中需先 interrupt）。
    pub async fn queue_delete(&self, prompt_ids: &[String]) -> Result<()> {
        let body = json!({ "delete": prompt_ids });
        let response = self
            .http
            .post(self.url("/queue"))
            .json(&body)
            .send()
            .await
            .map_err(|error| Error::from_reason(format!("ComfyUI queue delete failed: {error}")))?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body: Value = response.json().await.unwrap_or_else(|_| json!({}));
            return Err(api_error("ComfyUI queue delete failed", status, &body));
        }
        Ok(())
    }
}

/// 整段百分号编码（RFC 3986 unreserved 之外全部编码）。
///
/// ComfyUI 的 `/userdata/{path}` 把路径当单一 URL 段解析，斜杠必须编码成 `%2F`，
/// 中文文件名按 UTF-8 字节逐字节编码。
pub fn encode_userdata_path(path: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(path.len() * 3);
    for byte in path.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char)
            }
            other => {
                out.push('%');
                out.push(HEX[(other >> 4) as usize] as char);
                out.push(HEX[(other & 0x0F) as usize] as char);
            }
        }
    }
    out
}

/// 把上游错误统一成可读消息（400 时附带最常见修复提示）。
pub fn api_error(prefix: &str, status: u16, response_body: &Value) -> Error {
    let message = response_body
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("Unknown error");
    let error_type = response_body
        .get("error")
        .and_then(|error| error.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut detail = format!("{prefix}: {status} {message}");
    if !error_type.is_empty() {
        detail.push_str(&format!(" (type: {error_type})"));
    }
    // ComfyUI 校验失败时把 node_errors 一并透出，否则用户只能看到 "Prompt outputs failed validation"。
    if let Some(node_errors) = response_body.get("node_errors") {
        if node_errors.is_object() {
            detail.push_str(&format!(" node_errors: {node_errors}"));
        }
    }
    Error::from_reason(detail)
}

/// base64 解码（容忍 `data:...;base64,` 前缀与空白字符）。
pub fn decode_base64(data: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    let raw = data.trim();
    let payload = raw
        .rsplit_once(";base64,")
        .map(|(_, tail)| tail)
        .unwrap_or(raw);
    base64::engine::general_purpose::STANDARD
        .decode(payload.trim())
        .map_err(|error| Error::from_reason(format!("Invalid base64 image data: {error}")))
}

pub fn ext_for_mime(mime_type: &str) -> &'static str {
    match mime_type.trim().to_ascii_lowercase().as_str() {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/bmp" => "bmp",
        _ => "png",
    }
}

pub fn mime_for_file_name(filename: &str) -> &'static str {
    match filename
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        _ => "image/png",
    }
}

/// 纳秒时间戳（无额外随机数依赖）。
pub fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0)
}

/// 随机种子：纳秒时间戳与进程 id 混合，避免 ComfyUI 命中相同图缓存。
pub fn random_seed() -> u64 {
    (now_nanos() as u64) ^ (u64::from(std::process::id()) << 32)
}
