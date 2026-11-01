import { useSyncExternalStore } from "react";

/**
 * 团队协作面板发布给 TopBar 的状态快照：TeamPanel 是唯一生产者（挂载时写入、
 * 卸载时清空），TopBar 只作为消费者渲染团队名、连接状态与身份操作。
 */
export type TeamTopBarSnapshot = {
  /** 团队名。 */
  teamName: string;
  role: "host" | "member" | "";
  /** 队长：服务运行中；队员：已连上队长。 */
  connected: boolean;
  /** 队长：本机可连地址；队员：队长地址。 */
  endpoint: string;
  /** 队长专属：可直接分享的邀请链接。 */
  inviteLink: string;
  error: string | null;
  meName: string;
  meSeed: string;
  /** 队长：是否已注册系统级常驻服务。 */
  hostService: boolean;
  hostServiceBusy: boolean;
  toggleHostService: (enabled: boolean) => void;
  editIdentity: () => void;
};

let snapshot: TeamTopBarSnapshot | null = null;
const listeners = new Set<() => void>();

const subscribe = (listener: () => void): (() => void) => {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
};

const getSnapshot = (): TeamTopBarSnapshot | null => snapshot;

export const teamTopBarStore = {
  set(next: TeamTopBarSnapshot | null): void {
    snapshot = next;
    for (const listener of listeners) {
      listener();
    }
  },
};

export const useTeamTopBarSnapshot = (): TeamTopBarSnapshot | null =>
  useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
