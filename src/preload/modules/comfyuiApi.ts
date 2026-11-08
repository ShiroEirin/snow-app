import { ipcRenderer } from "electron";

/**
 * ComfyUI 原生直连的设置面板接口。
 *
 * 这些调用只用于 Settings -> Image generation 里的「本地 ComfyUI」：
 * 列出 ComfyUI 自己保存的画布工作流、把选中的工作流转成 API 格式。
 * 出图本身走 imagegen-generate（provider = comfyui），不经过这里。
 */
export const comfyuiApi = {
  listComfyuiWorkflows: (baseUrl: string): Promise<string> =>
    ipcRenderer.invoke("comfyui:list-workflows", baseUrl),
  convertComfyuiWorkflow: (baseUrl: string, name: string): Promise<string> =>
    ipcRenderer.invoke("comfyui:convert-workflow", baseUrl, name),
  describeComfyuiWorkflow: (path: string): Promise<string> =>
    ipcRenderer.invoke("comfyui:describe-workflow", path),
};
