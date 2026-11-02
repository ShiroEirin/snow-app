import { app } from "electron";

/** 系统调度拉起队长服务时附带的命令行参数。 */
export const TEAM_HOST_ARG = "--team-host";

/** 本次进程是否由系统调度以队长服务模式启动（后台运行，不显示主窗口）。 */
export const isTeamHostLaunch = (
  argv: readonly string[] = process.argv,
): boolean => argv.includes(TEAM_HOST_ARG);

export type TeamServiceSpec = {
  /** 任务标识：launchd label / 计划任务名后缀 / systemd 单元名。 */
  label: string;
  program: string;
  args: string[];
  env?: Record<string, string>;
};

/** Linux systemd 服务启动图形应用所需的会话变量。 */
const LINUX_SESSION_ENV_KEYS = [
  "DISPLAY",
  "WAYLAND_DISPLAY",
  "XAUTHORITY",
  "XDG_RUNTIME_DIR",
  "XDG_SESSION_TYPE",
  "XDG_CURRENT_DESKTOP",
  "DBUS_SESSION_BUS_ADDRESS",
];

/** 从可执行文件路径推出 macOS .app 包路径。 */
const resolveMacBundlePath = (execPath: string): string | null => {
  const marker = ".app/Contents/MacOS/";
  const index = execPath.indexOf(marker);
  return index >= 0 ? execPath.slice(0, index + ".app".length) : null;
};

/**
 * 组装「队长常驻服务」的系统任务规格：登录时（或崩溃后）以 `--team-host`
 * 参数后台拉起应用，使团队 WebSocket 服务自动恢复。
 */
export const buildTeamServiceSpec = (): TeamServiceSpec => {
  const label = app.isPackaged
    ? "com.snow.app.team-host"
    : "com.snow.app.team-host.dev";
  const appArgs = app.isPackaged
    ? [TEAM_HOST_ARG]
    : [app.getAppPath(), TEAM_HOST_ARG];

  if (process.platform === "darwin") {
    const bundle = resolveMacBundlePath(process.execPath);
    if (bundle) {
      // 经 open 交给 LaunchServices 启动，应用不挂在 launchd 任务下；
      // -n 避免向已运行实例发送 reopen 事件而弹出窗口。
      return {
        label,
        program: "/usr/bin/open",
        args: ["-n", "-g", "-a", bundle, "--args", ...appArgs],
      };
    }
    return { label, program: process.execPath, args: appArgs };
  }

  if (process.platform === "linux") {
    const env: Record<string, string> = {};
    for (const key of LINUX_SESSION_ENV_KEYS) {
      const value = process.env[key];
      if (value) env[key] = value;
    }
    return {
      label,
      program: process.env.APPIMAGE || process.execPath,
      args: appArgs,
      env,
    };
  }

  return { label, program: process.execPath, args: appArgs };
};
