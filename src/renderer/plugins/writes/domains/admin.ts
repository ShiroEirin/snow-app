import { pluginStore } from "../../pluginStore";
import {
  callSnow,
  l10n,
  optionalNumber,
  optionalRecord,
  optionalString,
  optionalStringArray,
  requireBoolean,
  requireNumber,
  requireRecord,
  requireString,
  requireStringArray,
} from "../helpers";
import type { PluginWriteActionDefinition } from "../types";

const SSH_AUTH_METHODS = ["password", "privateKey", "agent"];

const SSH_DRAFT_STATUSES = ["pending", "conflict"];

const STORAGE_LOCATION_KINDS = ["checkpoint", "upload"];

const DATABASE_KINDS = ["runtime", "archive"];

const CLEANUP_CATEGORY_IDS = [
  "checkpoints",
  "upload",
  "imageLibrary",
  "backgrounds",
  "pets",
  "browserState",
  "appLogs",
];

const TEAM_RECORD_KINDS = ["message", "task", "review", "note"];

const REMOTE_FIXED_TOKEN_KINDS = ["lan", "wan"];

const requireOneOf = (
  params: Record<string, unknown>,
  key: string,
  allowed: readonly string[],
): string => {
  const value = requireString(params, key);
  if (!allowed.includes(value)) {
    throw new Error(`Parameter '${key}' must be one of ${allowed.join(", ")}`);
  }
  return value;
};

const requireEnumArray = (
  params: Record<string, unknown>,
  key: string,
  allowed: readonly string[],
): string[] => {
  const values = requireStringArray(params, key);
  for (const value of values) {
    if (!allowed.includes(value)) {
      throw new Error(
        `Parameter '${key}' must be one of ${allowed.join(", ")}`,
      );
    }
  }
  return values;
};

const requireNumberArray = (
  params: Record<string, unknown>,
  key: string,
): number[] => {
  const value = params[key];
  if (!Array.isArray(value) || value.length === 0) {
    throw new Error(`Parameter '${key}' must be a non-empty number array`);
  }
  return value.map((item) => {
    if (typeof item !== "number" || !Number.isFinite(item)) {
      throw new Error(`Parameter '${key}' must contain finite numbers`);
    }
    return item;
  });
};

const requireRecordArray = (
  params: Record<string, unknown>,
  key: string,
): Record<string, unknown>[] => {
  const value = params[key];
  if (!Array.isArray(value)) {
    throw new Error(`Parameter '${key}' must be an array of objects`);
  }
  return value.map((item) => {
    if (!item || typeof item !== "object" || Array.isArray(item)) {
      throw new Error(`Parameter '${key}' must contain objects`);
    }
    return item as Record<string, unknown>;
  });
};

export const ADMIN_WRITE_ACTIONS: PluginWriteActionDefinition[] = [
  {
    domain: "browserData",
    action: "passwordSave",
    scope: "browserData",
    summary: l10n(
      "Save a browser password",
      "保存浏览器密码",
      "儲存瀏覽器密碼",
    ),
    invoke: async ({ params }) =>
      await callSnow("browserPasswordSave", {
        origin: requireString(params, "origin"),
        username: requireString(params, "username"),
        password: requireString(params, "password"),
      }),
  },
  {
    domain: "browserData",
    action: "passwordDelete",
    scope: "browserData",
    summary: l10n(
      "Delete a browser password",
      "删除浏览器密码",
      "刪除瀏覽器密碼",
    ),
    invoke: async ({ params }) => {
      const id = requireString(params, "id");
      const deleted = await callSnow<boolean>("browserPasswordDelete", id);
      return { id, deleted };
    },
  },
  {
    domain: "browserData",
    action: "passwordDeleteBatch",
    scope: "browserData",
    summary: l10n(
      "Delete browser passwords in bulk",
      "批量删除浏览器密码",
      "批次刪除瀏覽器密碼",
    ),
    invoke: async ({ params }) => {
      const ids = requireStringArray(params, "ids");
      const deleted = await callSnow<number>("browserPasswordDeleteBatch", ids);
      return { ids, deleted };
    },
  },
  {
    domain: "browserData",
    action: "bookmarkAdd",
    scope: "browserData",
    summary: l10n("Add a bookmark", "新增书签", "新增書籤"),
    invoke: async ({ params }) =>
      await callSnow(
        "browserBookmarkAdd",
        requireString(params, "url"),
        requireString(params, "title"),
        optionalString(params, "folder"),
      ),
  },
  {
    domain: "browserData",
    action: "bookmarkUpdate",
    scope: "browserData",
    summary: l10n("Update a bookmark", "修改书签", "修改書籤"),
    invoke: async ({ params }) =>
      await callSnow(
        "browserBookmarkUpdate",
        requireString(params, "id"),
        requireString(params, "url"),
        requireString(params, "title"),
        optionalString(params, "folder") ?? "",
      ),
  },
  {
    domain: "browserData",
    action: "bookmarkDelete",
    scope: "browserData",
    summary: l10n("Delete a bookmark", "删除书签", "刪除書籤"),
    invoke: async ({ params }) => {
      const id = requireString(params, "id");
      const deleted = await callSnow<boolean>("browserBookmarkDelete", id);
      return { id, deleted };
    },
  },
  {
    domain: "browserData",
    action: "bookmarkDeleteBatch",
    scope: "browserData",
    summary: l10n("Delete bookmarks in bulk", "批量删除书签", "批次刪除書籤"),
    invoke: async ({ params }) => {
      const ids = requireStringArray(params, "ids");
      const deleted = await callSnow<number>("browserBookmarkDeleteBatch", ids);
      return { ids, deleted };
    },
  },
  {
    domain: "browserData",
    action: "importPasswords",
    scope: "browserData",
    summary: l10n(
      "Import passwords from a browser",
      "从本地浏览器导入密码",
      "從本機瀏覽器匯入密碼",
    ),
    invoke: async ({ params }) =>
      await callSnow(
        "browserImportPasswords",
        requireString(params, "sourceId"),
        requireString(params, "profile"),
      ),
  },
  {
    domain: "browserData",
    action: "importCookies",
    scope: "browserData",
    summary: l10n(
      "Import cookies from a browser",
      "从本地浏览器导入 Cookie",
      "從本機瀏覽器匯入 Cookie",
    ),
    invoke: async ({ params }) =>
      await callSnow(
        "browserImportCookies",
        requireString(params, "sourceId"),
        requireString(params, "profile"),
      ),
  },
  {
    domain: "browserData",
    action: "importBookmarks",
    scope: "browserData",
    summary: l10n(
      "Import bookmarks from a browser",
      "从本地浏览器导入书签",
      "從本機瀏覽器匯入書籤",
    ),
    invoke: async ({ params }) =>
      await callSnow(
        "browserImportBookmarks",
        requireString(params, "sourceId"),
        requireString(params, "profile"),
      ),
  },
  {
    domain: "browserData",
    action: "cookieDelete",
    scope: "browserData",
    summary: l10n(
      "Delete a browser cookie",
      "删除浏览器 Cookie",
      "刪除瀏覽器 Cookie",
    ),
    invoke: async ({ params }) => {
      const webContentsId = requireNumber(params, "webContentsId");
      const name = requireString(params, "name");
      const domain = requireString(params, "domain");
      return await callSnow("browserCookieDelete", webContentsId, name, domain);
    },
  },
  {
    domain: "browserData",
    action: "clearCache",
    scope: "browserData",
    summary: l10n(
      "Clear the browser cache",
      "清空浏览器缓存",
      "清空瀏覽器快取",
    ),
    invoke: async () => {
      await callSnow("clearBrowserCache");
      return { cleared: true };
    },
  },
  {
    domain: "browserData",
    action: "clearCookies",
    scope: "browserData",
    summary: l10n(
      "Clear all browser cookies",
      "清空全部浏览器 Cookie",
      "清空全部瀏覽器 Cookie",
    ),
    invoke: async () => {
      await callSnow("clearBrowserCookies");
      return { cleared: true };
    },
  },
  {
    domain: "browserData",
    action: "routeSet",
    scope: "browserData",
    summary: l10n(
      "Set browser route mock rules",
      "设置浏览器路由拦截规则",
      "設定瀏覽器路由攔截規則",
    ),
    invoke: async ({ params }) => {
      const webContentsId = requireNumber(params, "webContentsId");
      const rules = requireRecordArray(params, "rules").map((rule) => ({
        pattern: requireString(rule, "pattern"),
        status: optionalNumber(rule, "status"),
        body: optionalString(rule, "body"),
        contentType: optionalString(rule, "contentType"),
        headers: optionalRecord(rule, "headers"),
      }));
      return await callSnow("browserRouteSet", webContentsId, rules);
    },
  },
  {
    domain: "browserData",
    action: "routeClear",
    scope: "browserData",
    summary: l10n(
      "Clear browser route rules",
      "清除浏览器路由规则",
      "清除瀏覽器路由規則",
    ),
    invoke: async ({ params }) =>
      await callSnow(
        "browserRouteClear",
        requireNumber(params, "webContentsId"),
      ),
  },
  {
    domain: "browserData",
    action: "storageSave",
    scope: "browserData",
    summary: l10n(
      "Save the browser login state",
      "保存浏览器登录态",
      "儲存瀏覽器登入狀態",
    ),
    invoke: async ({ params }) =>
      await callSnow(
        "browserStorageSave",
        requireNumber(params, "webContentsId"),
        optionalString(params, "fileName"),
      ),
  },
  {
    domain: "browserData",
    action: "storageRestore",
    scope: "browserData",
    summary: l10n(
      "Restore the browser login state",
      "恢复浏览器登录态",
      "還原瀏覽器登入狀態",
    ),
    invoke: async ({ params }) =>
      await callSnow(
        "browserStorageRestore",
        requireNumber(params, "webContentsId"),
        requireString(params, "fileName"),
      ),
  },
  {
    domain: "browserData",
    action: "deviceEmulate",
    scope: "browserData",
    summary: l10n(
      "Apply or clear device emulation",
      "应用或关闭设备模拟",
      "套用或關閉裝置模擬",
    ),
    invoke: async ({ params }) => {
      const webContentsId = requireNumber(params, "webContentsId");
      const emulation = optionalRecord(params, "emulation");
      if (!emulation) {
        await callSnow("browserDeviceEmulation", webContentsId, null);
        return { webContentsId, emulation: null };
      }
      const applied = {
        width: requireNumber(emulation, "width"),
        height: requireNumber(emulation, "height"),
        dpr: requireNumber(emulation, "dpr"),
        mobile: requireBoolean(emulation, "mobile"),
        userAgent: requireString(emulation, "userAgent"),
      };
      await callSnow("browserDeviceEmulation", webContentsId, applied);
      return { webContentsId, emulation: applied };
    },
  },
  {
    domain: "browserData",
    action: "dialogRespond",
    scope: "browserData",
    summary: l10n(
      "Answer a browser dialog",
      "回应浏览器弹窗",
      "回應瀏覽器彈窗",
    ),
    invoke: async ({ params }) =>
      await callSnow(
        "browserDialogRespond",
        requireNumber(params, "webContentsId"),
        requireBoolean(params, "accept"),
        optionalString(params, "promptText"),
      ),
  },
  {
    domain: "browserData",
    action: "cdpCommand",
    scope: "browserData",
    summary: l10n(
      "Run a browser CDP command",
      "执行浏览器 CDP 命令",
      "執行瀏覽器 CDP 命令",
    ),
    invoke: async ({ params }) =>
      await callSnow(
        "browserCdpCommand",
        requireNumber(params, "webContentsId"),
        requireString(params, "method"),
        optionalRecord(params, "cdpParams"),
      ),
  },
  {
    domain: "browserData",
    action: "cancelDownload",
    scope: "browserData",
    summary: l10n(
      "Cancel a browser download",
      "取消浏览器下载",
      "取消瀏覽器下載",
    ),
    invoke: async ({ params }) => {
      const id = requireNumber(params, "id");
      const canceled = await callSnow<boolean>("cancelBrowserDownload", id);
      return { id, canceled };
    },
  },

  {
    domain: "ssh",
    action: "saveCredential",
    scope: "ssh",
    summary: l10n("Save an SSH credential", "保存 SSH 凭据", "儲存 SSH 認證"),
    invoke: async ({ params }) =>
      await callSnow("sshSaveCredential", {
        host: requireString(params, "host"),
        port: requireNumber(params, "port"),
        username: requireString(params, "username"),
        authMethod: requireOneOf(params, "authMethod", SSH_AUTH_METHODS),
        privateKeyPath: optionalString(params, "privateKeyPath"),
        secret: optionalString(params, "secret"),
      }),
  },
  {
    domain: "ssh",
    action: "deleteCredential",
    scope: "ssh",
    summary: l10n("Delete an SSH credential", "删除 SSH 凭据", "刪除 SSH 認證"),
    invoke: async ({ params }) => {
      const host = requireString(params, "host");
      const port = requireNumber(params, "port");
      const username = requireString(params, "username");
      await callSnow("sshDeleteCredential", host, port, username);
      return { host, port, username };
    },
  },
  {
    domain: "ssh",
    action: "writeFile",
    scope: "ssh",
    summary: l10n(
      "Write a remote file over SSH",
      "写入 SSH 远端文件",
      "寫入 SSH 遠端檔案",
    ),
    invoke: async ({ params }) => {
      const sessionId = requireString(params, "sessionId");
      const remotePath = requireString(params, "remotePath");
      const content = requireString(params, "content");
      const options = requireRecord(params, "options");
      const expectedVersion = requireRecord(options, "expectedVersion");
      return await callSnow("sshWriteFile", sessionId, remotePath, content, {
        workspaceId: requireString(options, "workspaceId"),
        expectedVersion: {
          exists: requireBoolean(expectedVersion, "exists"),
          sha256: optionalString(expectedVersion, "sha256"),
          size: optionalNumber(expectedVersion, "size"),
          mtime: optionalNumber(expectedVersion, "mtime"),
        },
      });
    },
  },
  {
    domain: "ssh",
    action: "deleteEntry",
    scope: "ssh",
    summary: l10n(
      "Delete a remote SSH entry",
      "删除 SSH 远端条目",
      "刪除 SSH 遠端項目",
    ),
    invoke: async ({ params }) => {
      const sessionId = requireString(params, "sessionId");
      const remotePath = requireString(params, "remotePath");
      await callSnow("sshDeleteEntry", sessionId, remotePath);
      return { sessionId, remotePath };
    },
  },
  {
    domain: "ssh",
    action: "deleteEntries",
    scope: "ssh",
    summary: l10n(
      "Delete remote SSH entries in bulk",
      "批量删除 SSH 远端条目",
      "批次刪除 SSH 遠端項目",
    ),
    invoke: async ({ params }) => {
      const sessionId = requireString(params, "sessionId");
      const remotePaths = requireStringArray(params, "remotePaths");
      return await callSnow("sshDeleteEntries", sessionId, remotePaths);
    },
  },
  {
    domain: "ssh",
    action: "renameEntry",
    scope: "ssh",
    summary: l10n(
      "Rename a remote SSH entry",
      "重命名 SSH 远端条目",
      "重新命名 SSH 遠端項目",
    ),
    invoke: async ({ params }) => {
      const sessionId = requireString(params, "sessionId");
      const remotePath = requireString(params, "remotePath");
      const newName = requireString(params, "newName");
      await callSnow("sshRenameEntry", sessionId, remotePath, newName);
      return { sessionId, remotePath, newName };
    },
  },
  {
    domain: "ssh",
    action: "executeCommand",
    scope: "ssh",
    summary: l10n(
      "Run a command over SSH",
      "执行 SSH 远端命令",
      "執行 SSH 遠端命令",
    ),
    invoke: async ({ params }) => {
      const sessionId = requireString(params, "sessionId");
      const command = requireString(params, "command");
      const output = await callSnow<string>(
        "sshExecuteCommand",
        sessionId,
        command,
      );
      return { command, output };
    },
  },
  {
    domain: "ssh",
    action: "upsertDraft",
    scope: "ssh",
    summary: l10n(
      "Save a remote SSH draft",
      "保存 SSH 远端草稿",
      "儲存 SSH 遠端草稿",
    ),
    invoke: async ({ params }) => {
      const draft = requireRecord(params, "draft");
      return await callSnow("sshUpsertRemoteDraft", {
        profileId: requireString(draft, "profileId"),
        workspaceId: requireString(draft, "workspaceId"),
        remotePath: requireString(draft, "remotePath"),
        baseVersionJson: optionalString(draft, "baseVersionJson") ?? "",
        content: optionalString(draft, "content") ?? "",
        status: requireOneOf(draft, "status", SSH_DRAFT_STATUSES),
      });
    },
  },
  {
    domain: "ssh",
    action: "deleteDraft",
    scope: "ssh",
    summary: l10n(
      "Delete a remote SSH draft",
      "删除 SSH 远端草稿",
      "刪除 SSH 遠端草稿",
    ),
    invoke: async ({ params }) => {
      const profileId = requireString(params, "profileId");
      const workspaceId = requireString(params, "workspaceId");
      const remotePath = requireString(params, "remotePath");
      await callSnow(
        "sshDeleteRemoteDraft",
        profileId,
        workspaceId,
        remotePath,
      );
      return { profileId, workspaceId, remotePath };
    },
  },
  {
    domain: "ssh",
    action: "disconnect",
    scope: "ssh",
    summary: l10n("Close an SSH session", "断开 SSH 会话", "中斷 SSH 工作階段"),
    invoke: async ({ params }) => {
      const sessionId = requireString(params, "sessionId");
      await callSnow("sshDisconnect", sessionId);
      return { sessionId };
    },
  },

  {
    domain: "remoteControl",
    action: "setEnabled",
    scope: "remoteControl",
    summary: l10n(
      "Enable or disable remote control",
      "启用或关闭远程控制",
      "啟用或關閉遠端控制",
    ),
    invoke: async ({ params }) =>
      await callSnow(
        "setRemoteControlEnabled",
        requireBoolean(params, "enabled"),
      ),
  },
  {
    domain: "remoteControl",
    action: "setPort",
    scope: "remoteControl",
    summary: l10n(
      "Set the remote control port",
      "设置远程控制端口",
      "設定遠端控制埠",
    ),
    invoke: async ({ params }) =>
      await callSnow("setRemoteControlPort", requireNumber(params, "port")),
  },
  {
    domain: "remoteControl",
    action: "setFixedToken",
    scope: "remoteControl",
    summary: l10n(
      "Pin or clear a remote token",
      "固定或清除远程访问令牌",
      "固定或清除遠端存取權杖",
    ),
    invoke: async ({ params }) => {
      const kind = requireOneOf(params, "kind", REMOTE_FIXED_TOKEN_KINDS);
      const token = optionalString(params, "token") ?? null;
      return await callSnow("setRemoteControlFixedToken", kind, token);
    },
  },
  {
    domain: "remoteControl",
    action: "saveTunnelConfig",
    scope: "remoteControl",
    summary: l10n(
      "Save the tunnel config",
      "保存隧道穿透配置",
      "儲存通道穿透設定",
    ),
    invoke: async ({ params }) =>
      await callSnow("saveRemoteTunnelConfig", requireRecord(params, "config")),
  },
  {
    domain: "remoteControl",
    action: "connectTunnel",
    scope: "remoteControl",
    summary: l10n("Connect the tunnel", "连接隧道穿透", "連線通道穿透"),
    invoke: async () => await callSnow("connectRemoteTunnel"),
  },
  {
    domain: "remoteControl",
    action: "disconnectTunnel",
    scope: "remoteControl",
    summary: l10n("Disconnect the tunnel", "断开隧道穿透", "中斷通道穿透"),
    invoke: async () => await callSnow("disconnectRemoteTunnel"),
  },
  {
    domain: "remoteControl",
    action: "removeTunnelConfig",
    scope: "remoteControl",
    summary: l10n(
      "Remove the tunnel config",
      "移除隧道穿透配置",
      "移除通道穿透設定",
    ),
    invoke: async () => await callSnow("removeRemoteTunnelConfig"),
  },

  {
    domain: "storage",
    action: "setDir",
    scope: "storage",
    summary: l10n("Set a storage directory", "设置存储目录", "設定儲存目錄"),
    invoke: async ({ params }) => {
      const kind = requireOneOf(params, "kind", STORAGE_LOCATION_KINDS);
      const dir = optionalString(params, "dir") ?? "";
      await callSnow("setStorageDir", kind, dir);
      return { kind, dir };
    },
  },
  {
    domain: "storage",
    action: "repair",
    scope: "storage",
    summary: l10n("Repair a database", "修复数据库", "修復資料庫"),
    invoke: async ({ params }) =>
      await callSnow(
        "repairDatabase",
        requireOneOf(params, "kind", DATABASE_KINDS),
      ),
  },
  {
    domain: "storage",
    action: "optimize",
    scope: "storage",
    summary: l10n(
      "Optimize database space",
      "优化数据库空间",
      "最佳化資料庫空間",
    ),
    invoke: async ({ params }) =>
      await callSnow(
        "optimizeDatabase",
        requireOneOf(params, "kind", DATABASE_KINDS),
      ),
  },
  {
    domain: "storage",
    action: "scanCleanup",
    scope: "storage",
    summary: l10n(
      "Scan local data usage",
      "扫描本地数据占用",
      "掃描本機資料佔用",
    ),
    invoke: async ({ params }) =>
      await callSnow("scanCleanup", requireNumberArray(params, "daysList")),
  },
  {
    domain: "storage",
    action: "deleteCleanupData",
    scope: "storage",
    summary: l10n(
      "Delete selected cleanup data",
      "删除选中的数据分类",
      "刪除選取的資料分類",
    ),
    invoke: async ({ params }) =>
      await callSnow(
        "deleteCleanupData",
        requireEnumArray(params, "categories", CLEANUP_CATEGORY_IDS),
        requireNumber(params, "maxAgeDays"),
      ),
  },
  {
    domain: "storage",
    action: "prepareMigration",
    scope: "storage",
    summary: l10n(
      "Prepare a storage migration",
      "准备存储目录迁移",
      "準備儲存目錄遷移",
    ),
    invoke: async ({ params }) => {
      const kind = requireOneOf(params, "kind", STORAGE_LOCATION_KINDS);
      const targetDir = requireString(params, "targetDir");
      const fileCount = await callSnow<number>(
        "prepareStorageMigration",
        kind,
        targetDir,
      );
      return { kind, targetDir, fileCount };
    },
  },
  {
    domain: "storage",
    action: "commitMigration",
    scope: "storage",
    summary: l10n(
      "Commit a storage migration",
      "提交存储目录迁移",
      "提交儲存目錄遷移",
    ),
    invoke: async ({ params }) => {
      const kind = requireOneOf(params, "kind", STORAGE_LOCATION_KINDS);
      await callSnow("commitStorageMigration", kind);
      return { kind };
    },
  },
  {
    domain: "storage",
    action: "rollbackMigration",
    scope: "storage",
    summary: l10n(
      "Roll back a storage migration",
      "回滚存储目录迁移",
      "回復儲存目錄遷移",
    ),
    invoke: async ({ params }) => {
      const kind = requireOneOf(params, "kind", STORAGE_LOCATION_KINDS);
      await callSnow("rollbackStorageMigration", kind);
      return { kind };
    },
  },
  {
    domain: "storage",
    action: "optimizeMemory",
    scope: "storage",
    summary: l10n(
      "Trim the process memory",
      "整理进程内存占用",
      "整理行程記憶體佔用",
    ),
    invoke: async () => await callSnow("optimizeMemory"),
  },

  {
    domain: "checkpoints",
    action: "create",
    scope: "checkpoints",
    summary: l10n("Create a checkpoint", "新建检查点", "新增檢查點"),
    invoke: async ({ params }) => {
      const workDir = requireString(params, "workDir");
      const checkpointId = await callSnow<string>("createCheckpoint", workDir);
      return { checkpointId, workDir };
    },
  },
  {
    domain: "checkpoints",
    action: "restore",
    scope: "checkpoints",
    summary: l10n("Restore a checkpoint", "恢复检查点", "還原檢查點"),
    invoke: async ({ params }) => {
      const checkpointId = requireString(params, "checkpointId");
      const workDir = requireString(params, "workDir");
      await callSnow("restoreCheckpoint", checkpointId, workDir);
      return { checkpointId, workDir };
    },
  },
  {
    domain: "checkpoints",
    action: "restoreMany",
    scope: "checkpoints",
    summary: l10n(
      "Restore checkpoints in order",
      "按顺序恢复多个检查点",
      "依序還原多個檢查點",
    ),
    invoke: async ({ params }) => {
      const checkpointIds = requireStringArray(params, "checkpointIds");
      const workDir = requireString(params, "workDir");
      await callSnow("restoreCheckpoints", checkpointIds, workDir);
      return { checkpointIds, workDir };
    },
  },
  {
    domain: "checkpoints",
    action: "remove",
    scope: "checkpoints",
    summary: l10n("Delete a checkpoint", "删除检查点", "刪除檢查點"),
    invoke: async ({ params }) => {
      const checkpointId = requireString(params, "checkpointId");
      await callSnow("deleteCheckpoint", checkpointId);
      return { checkpointId };
    },
  },

  {
    domain: "filesystem",
    action: "writeFile",
    scope: "filesystem",
    summary: l10n("Write a local file", "写入本地文件", "寫入本機檔案"),
    invoke: async ({ params }) => {
      const filePath = requireString(params, "filePath");
      const content = requireString(params, "content");
      await callSnow("writeFileContent", filePath, content);
      return { filePath };
    },
  },
  {
    domain: "filesystem",
    action: "rename",
    scope: "filesystem",
    summary: l10n(
      "Rename a file or folder",
      "重命名文件或目录",
      "重新命名檔案或目錄",
    ),
    invoke: async ({ params }) => {
      const rootPath = requireString(params, "rootPath");
      const entryPath = requireString(params, "entryPath");
      const newName = requireString(params, "newName");
      await callSnow("renameWorkspaceEntry", rootPath, entryPath, newName);
      return { rootPath, entryPath, newName };
    },
  },
  {
    domain: "filesystem",
    action: "delete",
    scope: "filesystem",
    summary: l10n(
      "Delete a file or folder",
      "删除文件或目录",
      "刪除檔案或目錄",
    ),
    invoke: async ({ params }) => {
      const rootPath = requireString(params, "rootPath");
      const entryPath = requireString(params, "entryPath");
      await callSnow("deleteWorkspaceEntry", rootPath, entryPath);
      return { rootPath, entryPath };
    },
  },
  {
    domain: "filesystem",
    action: "deleteBatch",
    scope: "filesystem",
    summary: l10n(
      "Delete files or folders in bulk",
      "批量删除文件或目录",
      "批次刪除檔案或目錄",
    ),
    invoke: async ({ params }) => {
      const rootPath = requireString(params, "rootPath");
      const entryPaths = requireStringArray(params, "entryPaths");
      return await callSnow("deleteWorkspaceEntries", rootPath, entryPaths);
    },
  },

  {
    domain: "terminal",
    action: "create",
    scope: "terminal",
    summary: l10n("Create a terminal", "新建终端会话", "新增終端工作階段"),
    invoke: async ({ params }) => {
      const terminalId = await callSnow<string>("ptyCreate", {
        cwd: requireString(params, "cwd"),
        cols: requireNumber(params, "cols"),
        rows: requireNumber(params, "rows"),
        shellPath: optionalString(params, "shellPath"),
        sessionId: optionalString(params, "sessionId"),
      });
      return { terminalId };
    },
  },
  {
    domain: "terminal",
    action: "write",
    scope: "terminal",
    summary: l10n("Write terminal input", "向终端写入内容", "向終端寫入內容"),
    invoke: async ({ params }) => {
      const terminalId = requireString(params, "terminalId");
      await callSnow("ptyWrite", terminalId, requireString(params, "data"));
      return { terminalId };
    },
  },
  {
    domain: "terminal",
    action: "resize",
    scope: "terminal",
    summary: l10n("Resize a terminal", "调整终端尺寸", "調整終端尺寸"),
    invoke: async ({ params }) => {
      const terminalId = requireString(params, "terminalId");
      const cols = requireNumber(params, "cols");
      const rows = requireNumber(params, "rows");
      await callSnow("ptyResize", terminalId, cols, rows);
      return { terminalId, cols, rows };
    },
  },
  {
    domain: "terminal",
    action: "kill",
    scope: "terminal",
    summary: l10n("Close a terminal", "关闭终端会话", "關閉終端工作階段"),
    invoke: async ({ params }) => {
      const terminalId = requireString(params, "terminalId");
      await callSnow("ptyKill", terminalId);
      return { terminalId };
    },
  },

  {
    domain: "window",
    action: "minimize",
    scope: "window",
    summary: l10n("Minimize the window", "最小化窗口", "最小化視窗"),
    invoke: async () => {
      await callSnow("minimizeWindow");
      return { minimized: true };
    },
  },
  {
    domain: "window",
    action: "toggleMaximize",
    scope: "window",
    summary: l10n("Toggle window maximize", "切换窗口最大化", "切換視窗最大化"),
    invoke: async () => {
      await callSnow("toggleMaximizeWindow");
      return { toggled: true };
    },
  },
  {
    domain: "window",
    action: "close",
    scope: "window",
    summary: l10n("Close the window", "关闭窗口", "關閉視窗"),
    invoke: async () => {
      await callSnow("closeWindow");
      return { closed: true };
    },
  },
  {
    domain: "window",
    action: "hideToTray",
    scope: "window",
    summary: l10n(
      "Hide the window to tray",
      "隐藏窗口到托盘",
      "隱藏視窗到系統匣",
    ),
    invoke: async () => {
      await callSnow("hideWindowToTray");
      return { hidden: true };
    },
  },
  {
    domain: "window",
    action: "setAlwaysOnTop",
    scope: "window",
    summary: l10n("Pin the window on top", "设置窗口置顶", "設定視窗置頂"),
    invoke: async ({ params }) => {
      const alwaysOnTop = requireBoolean(params, "alwaysOnTop");
      const applied = await callSnow<boolean>(
        "setWindowAlwaysOnTop",
        alwaysOnTop,
      );
      return { alwaysOnTop: applied };
    },
  },
  {
    domain: "window",
    action: "reload",
    scope: "window",
    summary: l10n("Reload the window", "重新加载窗口", "重新載入視窗"),
    invoke: async () => {
      await callSnow("reloadWindow");
      return { reloaded: true };
    },
  },
  {
    domain: "window",
    action: "clearState",
    scope: "window",
    summary: l10n(
      "Reset the saved window state",
      "重置窗口状态记录",
      "重設視窗狀態記錄",
    ),
    invoke: async () => {
      await callSnow("clearWindowState");
      return { cleared: true };
    },
  },

  {
    domain: "updater",
    action: "check",
    scope: "updater",
    summary: l10n("Check for app updates", "检查应用更新", "檢查應用程式更新"),
    invoke: async () => await callSnow("checkForUpdates"),
  },
  {
    domain: "updater",
    action: "download",
    scope: "updater",
    summary: l10n(
      "Download the available update",
      "下载可用更新",
      "下載可用更新",
    ),
    invoke: async () => await callSnow("downloadUpdate"),
  },
  {
    domain: "updater",
    action: "install",
    scope: "updater",
    summary: l10n(
      "Install the downloaded update",
      "安装已下载的更新",
      "安裝已下載的更新",
    ),
    invoke: async () => {
      await callSnow("installUpdate");
      return { installing: true };
    },
  },

  {
    domain: "mcpTools",
    action: "call",
    scope: "mcpSecrets",
    summary: l10n("Call an MCP tool", "调用 MCP 工具", "呼叫 MCP 工具"),
    invoke: async ({ params }) => {
      const args = optionalRecord(params, "args");
      return await callSnow(
        "callMcpTool",
        requireString(params, "toolFullName"),
        args ? JSON.stringify(args) : "{}",
        optionalString(params, "projectId"),
        optionalStringArray(params, "checkpointIds"),
        optionalString(params, "checkpointWorkDir"),
        optionalString(params, "sensitiveAuthorizationToken"),
        undefined,
        optionalString(params, "interactionId"),
        optionalStringArray(params, "subAgentAllowedTools"),
        optionalString(params, "conversationId"),
      );
    },
  },
  {
    domain: "mcpTools",
    action: "abort",
    scope: "mcpSecrets",
    summary: l10n(
      "Abort a running tool call",
      "中止正在执行的工具调用",
      "中止正在執行的工具呼叫",
    ),
    invoke: async ({ params }) =>
      await callSnow(
        "abortToolExecution",
        requireString(params, "toolExecutionId"),
        optionalString(params, "reason"),
      ),
  },
  {
    domain: "mcpTools",
    action: "writeStdin",
    scope: "mcpSecrets",
    summary: l10n(
      "Send input to a tool session",
      "向工具会话发送输入",
      "向工具工作階段傳送輸入",
    ),
    invoke: async ({ params }) => {
      const sessionId = requireString(params, "sessionId");
      await callSnow(
        "writeInteractiveStdin",
        sessionId,
        requireString(params, "input"),
      );
      return { sessionId };
    },
  },

  {
    domain: "toolApproval",
    action: "setGlobal",
    scope: "toolApproval",
    summary: l10n(
      "Replace the global approved tools",
      "覆盖全局免审批工具列表",
      "覆寫全域免審批工具清單",
    ),
    invoke: async ({ params }) => {
      const tools = requireStringArray(params, "tools");
      await callSnow("setAlwaysApprovedTools", tools);
      return { tools };
    },
  },
  {
    domain: "toolApproval",
    action: "setProject",
    scope: "toolApproval",
    summary: l10n(
      "Approve or revoke one project tool",
      "授权或取消单个项目工具",
      "授權或取消單一專案工具",
    ),
    invoke: async ({ params }) => {
      const projectId = requireString(params, "projectId");
      const toolName = requireString(params, "toolName");
      const approved = requireBoolean(params, "approved");
      await callSnow(
        "setToolApprovalProjectToolApproved",
        projectId,
        toolName,
        approved,
      );
      return { projectId, toolName, approved };
    },
  },
  {
    domain: "toolApproval",
    action: "setProjectMany",
    scope: "toolApproval",
    summary: l10n(
      "Approve or revoke project tools in bulk",
      "批量授权或取消项目工具",
      "批次授權或取消專案工具",
    ),
    invoke: async ({ params }) => {
      const projectId = requireString(params, "projectId");
      const toolNames = requireStringArray(params, "toolNames");
      const approved = requireBoolean(params, "approved");
      await callSnow(
        "setToolApprovalProjectToolsApproved",
        projectId,
        toolNames,
        approved,
      );
      return { projectId, toolNames, approved };
    },
  },
  {
    domain: "toolApproval",
    action: "sensitiveCommandUpsert",
    scope: "toolApproval",
    summary: l10n(
      "Add or update a sensitive command rule",
      "新增或修改敏感命令规则",
      "新增或修改敏感命令規則",
    ),
    invoke: async ({ params }) =>
      await callSnow(
        "upsertSensitiveCommandConfig",
        requireRecord(params, "item"),
      ),
  },
  {
    domain: "toolApproval",
    action: "sensitiveCommandDelete",
    scope: "toolApproval",
    summary: l10n(
      "Delete a sensitive command rule",
      "删除敏感命令规则",
      "刪除敏感命令規則",
    ),
    invoke: async ({ params }) =>
      await callSnow(
        "deleteSensitiveCommandConfig",
        requireString(params, "commandId"),
      ),
  },
  {
    domain: "toolApproval",
    action: "sensitiveCommandReset",
    scope: "toolApproval",
    summary: l10n(
      "Reset sensitive command rules",
      "重置敏感命令规则",
      "重設敏感命令規則",
    ),
    invoke: async () => await callSnow("resetSensitiveCommandConfigs"),
  },
  {
    domain: "toolApproval",
    action: "sensitiveCommandUpsertProject",
    scope: "toolApproval",
    summary: l10n(
      "Add or update a project command rule",
      "新增或修改项目敏感命令规则",
      "新增或修改專案敏感命令規則",
    ),
    invoke: async ({ params }) =>
      await callSnow(
        "upsertProjectSensitiveCommandConfig",
        requireString(params, "projectId"),
        requireRecord(params, "item"),
      ),
  },
  {
    domain: "toolApproval",
    action: "sensitiveCommandDeleteProject",
    scope: "toolApproval",
    summary: l10n(
      "Delete a project command rule",
      "删除项目敏感命令规则",
      "刪除專案敏感命令規則",
    ),
    invoke: async ({ params }) =>
      await callSnow(
        "deleteProjectSensitiveCommandConfig",
        requireString(params, "projectId"),
        requireString(params, "commandId"),
      ),
  },
  {
    domain: "toolApproval",
    action: "sensitiveCommandSetProjectEnabled",
    scope: "toolApproval",
    summary: l10n(
      "Enable or disable a project command rule",
      "启用或禁用项目敏感命令规则",
      "啟用或停用專案敏感命令規則",
    ),
    invoke: async ({ params }) => {
      const projectId = requireString(params, "projectId");
      const commandId = requireString(params, "commandId");
      const enabled = requireBoolean(params, "enabled");
      return await callSnow(
        "setProjectSensitiveCommandEnabled",
        projectId,
        commandId,
        enabled,
      );
    },
  },

  {
    domain: "pluginsAdmin",
    action: "install",
    scope: "plugins",
    summary: l10n("Install a plugin", "安装插件", "安裝外掛"),
    invoke: async ({ params }) => {
      const sourceDir = requireString(params, "sourceDir");
      const record = await callSnow("installPlugin", sourceDir);
      await pluginStore.refresh();
      return record;
    },
  },
  {
    domain: "pluginsAdmin",
    action: "rescan",
    scope: "plugins",
    summary: l10n("Rescan a plugin", "重新扫描插件", "重新掃描外掛"),
    invoke: async ({ params }) => {
      const pluginId = requireString(params, "pluginId");
      const record = await callSnow("rescanPlugin", pluginId);
      await pluginStore.refresh();
      return record;
    },
  },
  {
    domain: "pluginsAdmin",
    action: "setEnabled",
    scope: "plugins",
    summary: l10n(
      "Enable or disable a plugin",
      "启用或禁用插件",
      "啟用或停用外掛",
    ),
    invoke: async ({ params }) => {
      const pluginId = requireString(params, "pluginId");
      const enabled = requireBoolean(params, "enabled");
      await callSnow("setPluginEnabled", pluginId, enabled);
      await pluginStore.refresh();
      return { pluginId, enabled };
    },
  },
  {
    domain: "pluginsAdmin",
    action: "remove",
    scope: "plugins",
    summary: l10n("Uninstall a plugin", "卸载插件", "解除安裝外掛"),
    invoke: async ({ params }) => {
      const pluginId = requireString(params, "pluginId");
      const deleteFiles = requireBoolean(params, "deleteFiles");
      await callSnow("deletePlugin", pluginId, deleteFiles);
      await pluginStore.refresh();
      return { pluginId, deleteFiles };
    },
  },

  {
    domain: "team",
    action: "getState",
    scope: "storage",
    summary: l10n(
      "Read the team state of a project",
      "读取项目的团队状态",
      "讀取專案的團隊狀態",
    ),
    invoke: async ({ params }) =>
      await callSnow(
        "teamGetState",
        optionalString(params, "directoryId") ?? "",
        optionalString(params, "directoryPath") ?? "",
      ),
  },
  {
    domain: "team",
    action: "create",
    scope: "storage",
    summary: l10n("Create a team", "创建团队", "建立團隊"),
    invoke: async ({ params }) =>
      await callSnow(
        "teamCreateTeam",
        optionalString(params, "directoryId") ?? "",
        optionalString(params, "directoryPath") ?? "",
        requireString(params, "name"),
        requireString(params, "selfName"),
        optionalString(params, "selfColor") ?? "",
      ),
  },
  {
    domain: "team",
    action: "join",
    scope: "storage",
    summary: l10n(
      "Join a team by invite link",
      "通过邀请链接加入团队",
      "透過邀請連結加入團隊",
    ),
    invoke: async ({ params }) =>
      await callSnow(
        "teamJoinTeam",
        requireString(params, "inviteLink"),
        requireString(params, "selfName"),
        optionalString(params, "selfColor") ?? "",
        optionalString(params, "directoryId") ?? "",
        optionalString(params, "directoryPath") ?? "",
      ),
  },
  {
    domain: "team",
    action: "leave",
    scope: "storage",
    summary: l10n("Leave a team", "退出团队", "退出團隊"),
    invoke: async ({ params }) => {
      const teamId = requireString(params, "teamId");
      const removed = await callSnow<boolean>("teamLeaveTeam", teamId);
      return { teamId, removed };
    },
  },
  {
    domain: "team",
    action: "disband",
    scope: "storage",
    summary: l10n(
      "Disband a hosted team",
      "解散自己创建的团队",
      "解散自己建立的團隊",
    ),
    invoke: async ({ params }) => {
      const teamId = requireString(params, "teamId");
      const removed = await callSnow<boolean>("teamDisbandTeam", teamId);
      return { teamId, removed };
    },
  },
  {
    domain: "team",
    action: "setProfile",
    scope: "storage",
    summary: l10n(
      "Update the local team profile",
      "更新本机团队资料",
      "更新本機團隊資料",
    ),
    invoke: async ({ params }) =>
      await callSnow(
        "teamSetProfile",
        requireString(params, "teamId"),
        requireString(params, "name"),
        optionalString(params, "color") ?? "",
      ),
  },
  {
    domain: "team",
    action: "listRecords",
    scope: "storage",
    summary: l10n("List team records", "列出团队记录", "列出團隊記錄"),
    invoke: async ({ params }) =>
      await callSnow(
        "teamListRecords",
        requireString(params, "teamId"),
        requireOneOf(params, "kind", TEAM_RECORD_KINDS),
      ),
  },
  {
    domain: "team",
    action: "upsert",
    scope: "storage",
    summary: l10n(
      "Add or update a team record",
      "新增或修改团队记录",
      "新增或修改團隊記錄",
    ),
    invoke: async ({ params }) =>
      await callSnow(
        "teamUpsertRecord",
        requireString(params, "teamId"),
        requireOneOf(params, "kind", TEAM_RECORD_KINDS),
        requireString(params, "recordId"),
        JSON.stringify(requireRecord(params, "record")),
      ),
  },
  {
    domain: "team",
    action: "remove",
    scope: "storage",
    summary: l10n("Delete a team record", "删除团队记录", "刪除團隊記錄"),
    invoke: async ({ params }) => {
      const teamId = requireString(params, "teamId");
      const kind = requireOneOf(params, "kind", TEAM_RECORD_KINDS);
      const recordId = requireString(params, "recordId");
      const deleted = await callSnow<boolean>(
        "teamDeleteRecord",
        teamId,
        kind,
        recordId,
      );
      return { teamId, kind, recordId, deleted };
    },
  },
  {
    domain: "team",
    action: "mediaSave",
    scope: "storage",
    summary: l10n("Save a team attachment", "保存团队附件", "儲存團隊附件"),
    invoke: async ({ params }) =>
      await callSnow(
        "teamSaveMedia",
        requireString(params, "teamId"),
        requireString(params, "ownerId"),
        requireString(params, "fileName"),
        requireString(params, "base64Data"),
        params.imageOnly === true,
      ),
  },
  {
    domain: "team",
    action: "mediaRead",
    scope: "storage",
    summary: l10n(
      "Read a team attachment as a data URL",
      "读取团队附件（data URL）",
      "讀取團隊附件（data URL）",
    ),
    invoke: async ({ params }) =>
      await callSnow(
        "teamReadMedia",
        requireString(params, "teamId"),
        requireString(params, "mediaId"),
      ),
  },
  {
    domain: "team",
    action: "mediaDelete",
    scope: "storage",
    summary: l10n(
      "Delete team record media",
      "删除团队记录媒体文件",
      "刪除團隊記錄媒體檔案",
    ),
    invoke: async ({ params }) => {
      const teamId = requireString(params, "teamId");
      const ownerId = requireString(params, "ownerId");
      const deleted = await callSnow<boolean>(
        "teamDeleteMedia",
        teamId,
        ownerId,
      );
      return { teamId, ownerId, deleted };
    },
  },
  {
    domain: "team",
    action: "respondJoinRequest",
    scope: "storage",
    summary: l10n(
      "Approve or reject a pending join request (host only)",
      "审核加入申请（仅队长）",
      "審核加入申請（僅隊長）",
    ),
    invoke: async ({ params }) => {
      const teamId = requireString(params, "teamId");
      const requestId = requireString(params, "requestId");
      const approve = requireBoolean(params, "approve");
      await callSnow("teamRespondJoinRequest", teamId, requestId, approve);
      return { teamId, requestId, approve };
    },
  },
  {
    domain: "team",
    action: "resetInvite",
    scope: "storage",
    summary: l10n(
      "Replace the invite code (host only)",
      "更换邀请码（仅队长）",
      "更換邀請碼（僅隊長）",
    ),
    invoke: async ({ params }) =>
      await callSnow("teamResetInvite", requireString(params, "teamId")),
  },
  {
    domain: "team",
    action: "blockMember",
    scope: "storage",
    summary: l10n(
      "Block a member and remove them (host only)",
      "拉黑成员并移出团队（仅队长）",
      "封鎖成員並移出團隊（僅隊長）",
    ),
    invoke: async ({ params }) => {
      const teamId = requireString(params, "teamId");
      const peerId = requireString(params, "peerId");
      await callSnow("teamBlockMember", teamId, peerId);
      return { teamId, peerId };
    },
  },
  {
    domain: "team",
    action: "unblockMember",
    scope: "storage",
    summary: l10n(
      "Unblock a device (host only)",
      "解除拉黑（仅队长）",
      "解除封鎖（僅隊長）",
    ),
    invoke: async ({ params }) => {
      const teamId = requireString(params, "teamId");
      const clientId = requireString(params, "clientId");
      const removed = await callSnow<boolean>(
        "teamUnblockMember",
        teamId,
        clientId,
      );
      return { teamId, clientId, removed };
    },
  },
  {
    domain: "team",
    action: "listBlocklist",
    scope: "storage",
    summary: l10n("List blocked devices", "列出团队黑名单", "列出團隊封鎖名單"),
    invoke: async ({ params }) =>
      await callSnow("teamListBlocklist", requireString(params, "teamId")),
  },
];
