import { ipcRenderer } from "electron";
import type {
  TeamBlockedMember,
  TeamEventPayload,
  TeamInfo,
  TeamJoinResult,
  TeamProjectLink,
  TeamRecordKind,
  TeamState,
} from "../types";

/** 团队事件通道（与主进程 teamHandlers 保持一致）。 */
const TEAM_EVENT_CHANNEL = "team:event";

/** 队长服务状态（与 Rust `HostState` 一致）。 */
export type TeamHostState = {
  running: boolean;
  teamId: string;
  port: number;
  endpoint: string;
};

export const teamApi = {
  /** 读取当前项目所属团队的状态快照。 */
  teamGetState: async (
    directoryId: string,
    directoryPath: string,
  ): Promise<TeamState> =>
    JSON.parse(
      await ipcRenderer.invoke("team:get-state", directoryId, directoryPath),
    ),
  /** 列出本机参与的全部团队。 */
  teamListTeams: async (): Promise<TeamInfo[]> =>
    JSON.parse(await ipcRenderer.invoke("team:list")),
  /** 创建团队并把当前项目关联进去。 */
  teamCreateTeam: async (
    directoryId: string,
    directoryPath: string,
    name: string,
    selfName: string,
    selfColor: string,
  ): Promise<TeamInfo> =>
    JSON.parse(
      await ipcRenderer.invoke(
        "team:create",
        directoryId,
        directoryPath,
        name,
        selfName,
        selfColor,
      ),
    ),
  /** 通过邀请链接申请加入团队并建立长连接（首次加入需队长审核）。 */
  teamJoinTeam: async (
    inviteLink: string,
    selfName: string,
    selfColor: string,
  ): Promise<TeamJoinResult> =>
    JSON.parse(
      await ipcRenderer.invoke("team:join", inviteLink, selfName, selfColor),
    ),
  /** 退出并删除团队。 */
  teamLeaveTeam: (teamId: string): Promise<boolean> =>
    ipcRenderer.invoke("team:leave", teamId),
  /** 更新本机在团队中的昵称与头像色。 */
  teamSetProfile: async (
    teamId: string,
    name: string,
    color: string,
  ): Promise<TeamInfo> =>
    JSON.parse(
      await ipcRenderer.invoke("team:set-profile", teamId, name, color),
    ),
  /** 关联一个本地项目到团队。 */
  teamLinkProject: (
    teamId: string,
    directoryId: string,
    name: string,
  ): Promise<void> =>
    ipcRenderer.invoke("team:link-project", teamId, directoryId, name),
  /** 解除项目与团队的关联。 */
  teamUnlinkProject: (teamId: string, directoryId: string): Promise<boolean> =>
    ipcRenderer.invoke("team:unlink-project", teamId, directoryId),
  /** 列出团队关联的项目。 */
  teamListProjects: async (teamId: string): Promise<TeamProjectLink[]> =>
    JSON.parse(await ipcRenderer.invoke("team:list-projects", teamId)),
  /** 列出某类记录（原始 JSON 字符串数组）。 */
  teamListRecords: (teamId: string, kind: TeamRecordKind): Promise<string[]> =>
    ipcRenderer.invoke("team:list-records", teamId, kind),
  /** 写入或更新一条记录。 */
  teamUpsertRecord: (
    teamId: string,
    kind: TeamRecordKind,
    recordId: string,
    json: string,
  ): Promise<string> =>
    ipcRenderer.invoke("team:upsert-record", teamId, kind, recordId, json),
  /** 删除一条记录。 */
  teamDeleteRecord: (
    teamId: string,
    kind: TeamRecordKind,
    recordId: string,
  ): Promise<boolean> =>
    ipcRenderer.invoke("team:delete-record", teamId, kind, recordId),
  /** 保存附件，返回附件 id。 */
  teamSaveMedia: (
    teamId: string,
    ownerId: string,
    fileName: string,
    base64Data: string,
    imageOnly: boolean,
  ): Promise<string> =>
    ipcRenderer.invoke(
      "team:media-save",
      teamId,
      ownerId,
      fileName,
      base64Data,
      imageOnly,
    ),
  /** 读取附件内容，返回 data URL。 */
  teamReadMedia: (teamId: string, mediaId: string): Promise<string> =>
    ipcRenderer.invoke("team:media-read", teamId, mediaId),
  /** 删除某条记录名下的全部附件。 */
  teamDeleteMedia: (teamId: string, ownerId: string): Promise<boolean> =>
    ipcRenderer.invoke("team:media-delete", teamId, ownerId),
  /** 启动队长服务（`port` 为 0 时由系统分配端口）。 */
  teamStartHost: async (teamId: string, port = 0): Promise<TeamHostState> =>
    JSON.parse(await ipcRenderer.invoke("team:start-host", teamId, port)),
  /** 停止队长服务。 */
  teamStopHost: (): Promise<void> => ipcRenderer.invoke("team:stop-host"),
  /** 查询队长服务状态。 */
  teamHostState: async (): Promise<TeamHostState> =>
    JSON.parse(await ipcRenderer.invoke("team:host-state")),
  /** 设置队长常驻服务开关（系统级登录自启）。 */
  teamSetHostService: (teamId: string, enabled: boolean): Promise<void> =>
    ipcRenderer.invoke("team:set-host-service", teamId, enabled),
  /** 队长移除成员：删除成员条目并断开其连接。 */
  teamRemoveMember: (teamId: string, peerId: string): Promise<void> =>
    ipcRenderer.invoke("team:remove-member", teamId, peerId),
  /** 队长裁决一条加入申请（通过 / 拒绝）。 */
  teamRespondJoinRequest: (
    teamId: string,
    requestId: string,
    approve: boolean,
  ): Promise<void> =>
    ipcRenderer.invoke("team:respond-join-request", teamId, requestId, approve),
  /** 重置邀请码：旧邀请链接立即失效。 */
  teamResetInvite: async (teamId: string): Promise<TeamInfo> =>
    JSON.parse(await ipcRenderer.invoke("team:reset-invite", teamId)),
  /** 拉黑成员：加入黑名单、移出团队并断开其连接。 */
  teamBlockMember: (teamId: string, peerId: string): Promise<void> =>
    ipcRenderer.invoke("team:block-member", teamId, peerId),
  /** 解除拉黑。 */
  teamUnblockMember: (teamId: string, clientId: string): Promise<boolean> =>
    ipcRenderer.invoke("team:unblock-member", teamId, clientId),
  /** 列出团队黑名单。 */
  teamListBlocklist: async (teamId: string): Promise<TeamBlockedMember[]> =>
    JSON.parse(await ipcRenderer.invoke("team:list-blocklist", teamId)),
  /** 依据团队笔记重建关联项目的「团队项目知识」Skill。 */
  teamSyncKnowledge: (teamId: string): Promise<void> =>
    ipcRenderer.invoke("team:sync-knowledge", teamId),
  /** 订阅团队事件，返回取消订阅函数。 */
  onTeamEvent: (
    listener: (payload: TeamEventPayload) => void,
  ): (() => void) => {
    const handler = (_event: unknown, payload: TeamEventPayload): void =>
      listener(payload);
    ipcRenderer.on(TEAM_EVENT_CHANNEL, handler);
    return () => {
      ipcRenderer.off(TEAM_EVENT_CHANNEL, handler);
    };
  },
};
