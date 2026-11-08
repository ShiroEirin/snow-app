import { contextBridge } from "electron";
import { initClientScriptHost } from "./clientScriptHost";
import { apiConfigApi } from "./modules/apiConfigApi";
import { appLockApi } from "./modules/appLockApi";
import { configApi } from "./modules/configApi";
import { conversationApi } from "./modules/conversationApi";
import { workspaceApi } from "./modules/workspaceApi";
import { sshApi } from "./modules/sshApi";
import { gitApi } from "./modules/gitApi";
import { teamApi } from "./modules/teamApi";
import { systemApi, ptyApi, windowApi } from "./modules/systemApi";
import { memoApi } from "./modules/memoApi";
import { fileReviewAnnotationApi } from "./modules/fileReviewAnnotationApi";
import { diffCommentApi } from "./modules/diffCommentApi";
import { memoryApi } from "./modules/memoryApi";
import { scheduledTaskApi } from "./modules/scheduledTaskApi";
import { personalizationApi } from "./modules/personalizationApi";
import { userscriptsApi } from "./modules/userscriptsApi";
import { pluginsApi } from "./modules/pluginsApi";
import { imageGenerationApi } from "./modules/imageGenerationApi";
import { comfyuiApi } from "./modules/comfyuiApi";
import { imageLibraryApi } from "./modules/imageLibraryApi";
import { htmlPreviewApi } from "./modules/htmlPreviewApi";
import { storageApi } from "./modules/storageApi";
import { resourceApi } from "./modules/resourceApi";
import { ideApi } from "./modules/ideApi";
import { petApi } from "./modules/petApi";
import { remoteControlApi } from "./modules/remoteControlApi";
import { promptOptimizationApi } from "./modules/promptOptimizationApi";
import { repoWikiApi } from "./modules/repoWikiApi";

export type * from "./types";

const api = {
  ...apiConfigApi,
  ...appLockApi,
  ...configApi,
  ...conversationApi,
  ...workspaceApi,
  ...sshApi,
  ...gitApi,
  ...teamApi,
  ...systemApi,
  ...ptyApi,
  ...windowApi,
  ...memoApi,
  ...diffCommentApi,
  ...fileReviewAnnotationApi,
  ...memoryApi,
  ...scheduledTaskApi,
  ...personalizationApi,
  ...userscriptsApi,
  ...pluginsApi,
  ...imageGenerationApi,
  ...comfyuiApi,
  ...imageLibraryApi,
  ...htmlPreviewApi,
  ...storageApi,
  ...resourceApi,
  ...ideApi,
  ...petApi,
  ...remoteControlApi,
  ...promptOptimizationApi,
  ...repoWikiApi,
};

contextBridge.exposeInMainWorld("snow", api);

// 客户端 UI 脚本宿主：接收主进程推送的匹配结果并注入脚本（两档执行模型）。
initClientScriptHost();

export type SnowApi = typeof api;
