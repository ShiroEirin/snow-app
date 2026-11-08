//! ComfyUI 原生协议层（与 imagegen provider、内置 comfyui 服务共用）。
//!
//! 设计边界：
//! - `client`：只负责 HTTP（upload / prompt / history / view / queue / object_info / userdata）。
//! - `workflow`：只负责 API 格式图的参数注入与输出收集（纯函数）。
//! - `convert`：只负责 UI 格式画布工作流 → API 格式 + paramMap 自动推导（纯函数）。
//!
//! 这里不出现任何 MCP 工具定义，也不读配置——配置由调用方（imagegen / comfyui 服务）解析后传入。

pub mod client;
pub mod convert;
pub mod validate;
pub mod workflow;
