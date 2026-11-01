import { BrowserWindow, ipcMain } from "electron";
import type { NativeBridge } from "../../native/types";
import { buildTeamServiceSpec } from "../../team/teamServiceLaunch";

/** Rust → 渲染层的团队事件通道（记录变更、成员在线状态、连接状态）。 */
export const TEAM_EVENT_CHANNEL = "team:event";

/**
 * 团队协作 IPC 桥：数据读写、WebSocket 服务与队员连接全部由 Rust 完成，
 * 这里只做参数校验、事件转发与渲染进程的调用入口。
 */
export const registerTeamHandlers = (native: NativeBridge): void => {
  // Rust 侧事件 → 所有渲染窗口（多窗口下保持一致）
  native.setTeamEventHandler(async (payload) => {
    for (const window of BrowserWindow.getAllWindows()) {
      if (!window.isDestroyed() && !window.webContents.isDestroyed()) {
        window.webContents.send(TEAM_EVENT_CHANNEL, payload);
      }
    }
  });

  const requireText = (value: unknown, label: string): string => {
    const trimmed = typeof value === "string" ? value.trim() : "";
    if (!trimmed) {
      throw new Error(`${label} is required`);
    }
    return trimmed;
  };

  ipcMain.handle(
    "team:get-state",
    async (_event, directoryId: unknown, directoryPath: unknown) =>
      native.teamGetState(
        typeof directoryId === "string" ? directoryId.trim() : "",
        typeof directoryPath === "string" ? directoryPath.trim() : "",
      ),
  );

  ipcMain.handle("team:list", async () => native.teamListTeams());

  ipcMain.handle(
    "team:create",
    async (
      _event,
      directoryId: unknown,
      directoryPath: unknown,
      name: unknown,
      selfName: unknown,
      selfColor: unknown,
    ) =>
      native.teamCreateTeam(
        typeof directoryId === "string" ? directoryId.trim() : "",
        typeof directoryPath === "string" ? directoryPath.trim() : "",
        requireText(name, "Team name"),
        requireText(selfName, "Display name"),
        typeof selfColor === "string" ? selfColor.trim() : "",
      ),
  );

  ipcMain.handle(
    "team:join",
    async (
      _event,
      inviteLink: unknown,
      selfName: unknown,
      selfColor: unknown,
      directoryId: unknown,
      directoryPath: unknown,
    ) =>
      native.teamJoinTeam(
        requireText(inviteLink, "Invite link"),
        requireText(selfName, "Display name"),
        typeof selfColor === "string" ? selfColor.trim() : "",
        typeof directoryId === "string" ? directoryId.trim() : "",
        typeof directoryPath === "string" ? directoryPath.trim() : "",
      ),
  );

  ipcMain.handle("team:leave", async (_event, teamId: unknown) =>
    native.teamLeaveTeam(requireText(teamId, "Team id")),
  );

  ipcMain.handle("team:disband", async (_event, teamId: unknown) =>
    native.teamDisbandTeam(requireText(teamId, "Team id")),
  );

  ipcMain.handle("team:cancel-join", async () => native.teamCancelJoin());

  ipcMain.handle(
    "team:set-profile",
    async (_event, teamId: unknown, name: unknown, color: unknown) =>
      native.teamSetProfile(
        requireText(teamId, "Team id"),
        requireText(name, "Display name"),
        typeof color === "string" ? color.trim() : "",
      ),
  );

  ipcMain.handle(
    "team:link-project",
    async (_event, teamId: unknown, directoryId: unknown, name: unknown) =>
      native.teamLinkProject(
        requireText(teamId, "Team id"),
        requireText(directoryId, "Directory id"),
        typeof name === "string" ? name.trim() : "",
      ),
  );

  ipcMain.handle(
    "team:unlink-project",
    async (_event, teamId: unknown, directoryId: unknown) =>
      native.teamUnlinkProject(
        requireText(teamId, "Team id"),
        requireText(directoryId, "Directory id"),
      ),
  );

  ipcMain.handle("team:list-projects", async (_event, teamId: unknown) =>
    native.teamListProjects(requireText(teamId, "Team id")),
  );

  ipcMain.handle(
    "team:list-records",
    async (_event, teamId: unknown, kind: unknown) =>
      native.teamListRecords(
        requireText(teamId, "Team id"),
        requireText(kind, "Record kind"),
      ),
  );

  ipcMain.handle(
    "team:upsert-record",
    async (
      _event,
      teamId: unknown,
      kind: unknown,
      recordId: unknown,
      json: unknown,
    ) =>
      native.teamUpsertRecord(
        requireText(teamId, "Team id"),
        requireText(kind, "Record kind"),
        requireText(recordId, "Record id"),
        requireText(json, "Record payload"),
      ),
  );

  ipcMain.handle(
    "team:delete-record",
    async (_event, teamId: unknown, kind: unknown, recordId: unknown) =>
      native.teamDeleteRecord(
        requireText(teamId, "Team id"),
        requireText(kind, "Record kind"),
        requireText(recordId, "Record id"),
      ),
  );

  ipcMain.handle(
    "team:media-save",
    async (
      _event,
      teamId: unknown,
      ownerId: unknown,
      fileName: unknown,
      base64Data: unknown,
      imageOnly: unknown,
    ) =>
      native.teamSaveMedia(
        requireText(teamId, "Team id"),
        requireText(ownerId, "Owner id"),
        requireText(fileName, "File name"),
        requireText(base64Data, "File data"),
        imageOnly === true,
      ),
  );

  ipcMain.handle(
    "team:media-read",
    async (_event, teamId: unknown, mediaId: unknown) =>
      native.teamReadMedia(
        requireText(teamId, "Team id"),
        requireText(mediaId, "Media id"),
      ),
  );

  ipcMain.handle(
    "team:media-delete",
    async (_event, teamId: unknown, ownerId: unknown) =>
      native.teamDeleteMedia(
        requireText(teamId, "Team id"),
        requireText(ownerId, "Owner id"),
      ),
  );

  ipcMain.handle(
    "team:start-host",
    async (_event, teamId: unknown, port: unknown) =>
      native.teamStartHost(
        requireText(teamId, "Team id"),
        typeof port === "number" && Number.isInteger(port) && port >= 0
          ? port
          : 0,
      ),
  );

  ipcMain.handle("team:stop-host", async () => native.teamStopHost());

  ipcMain.handle("team:host-state", async () => native.teamHostState());

  ipcMain.handle(
    "team:set-host-service",
    async (_event, teamId: unknown, enabled: unknown) => {
      const id = requireText(teamId, "Team id");
      const on = enabled === true;
      // 先注册系统级登录自启（队长关窗 / 设备重启后仍能自动恢复在线），
      // 成功后再落库，避免注册失败时界面开关与实际状态不一致。
      const spec = buildTeamServiceSpec();
      await native.syncTeamService({ ...spec, enabled: on });
      await native.teamSetHostService(id, on);
    },
  );

  ipcMain.handle(
    "team:set-host-port",
    async (_event, teamId: unknown, port: unknown) => {
      if (
        typeof port !== "number" ||
        !Number.isInteger(port) ||
        port < 1 ||
        port > 65535
      ) {
        throw new Error("Port must be an integer between 1 and 65535");
      }
      await native.teamSetHostPort(requireText(teamId, "Team id"), port);
    },
  );

  ipcMain.handle(
    "team:remove-member",
    async (_event, teamId: unknown, peerId: unknown) =>
      native.teamRemoveMember(
        requireText(teamId, "Team id"),
        requireText(peerId, "Member id"),
      ),
  );

  ipcMain.handle(
    "team:respond-join-request",
    async (_event, teamId: unknown, requestId: unknown, approve: unknown) =>
      native.teamRespondJoinRequest(
        requireText(teamId, "Team id"),
        requireText(requestId, "Join request id"),
        approve === true,
      ),
  );

  ipcMain.handle("team:reset-invite", async (_event, teamId: unknown) =>
    native.teamResetInvite(requireText(teamId, "Team id")),
  );

  ipcMain.handle(
    "team:block-member",
    async (_event, teamId: unknown, peerId: unknown) =>
      native.teamBlockMember(
        requireText(teamId, "Team id"),
        requireText(peerId, "Member id"),
      ),
  );

  ipcMain.handle(
    "team:unblock-member",
    async (_event, teamId: unknown, clientId: unknown) =>
      native.teamUnblockMember(
        requireText(teamId, "Team id"),
        requireText(clientId, "Client id"),
      ),
  );

  ipcMain.handle("team:list-blocklist", async (_event, teamId: unknown) =>
    native.teamListBlocklist(requireText(teamId, "Team id")),
  );

  ipcMain.handle("team:sync-knowledge", async (_event, teamId: unknown) =>
    native.teamSyncKnowledge(requireText(teamId, "Team id")),
  );
};
