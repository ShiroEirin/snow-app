import { Ban, Check, Link2, Loader2, RefreshCw, Server } from "lucide-react";
import { useState } from "react";
import { useI18n } from "../../../i18n";
import { TeamAvatar } from "./TeamShared";
import { formatTime } from "./teamUtils";
import type { TeamData } from "./useTeamData";

/**
 * 团队设置（队长专属）：队长常驻服务、邀请链接与黑名单。
 * 复用设置页的通用行样式（settings-about-row / settings-item-* / nav-item），
 * 不再把这些管理项堆在成员列表里。
 */
export const TeamSettings = ({
  team,
  hostServiceBusy,
  onToggleHostService,
}: {
  team: TeamData;
  hostServiceBusy: boolean;
  onToggleHostService: (enabled: boolean) => void;
}): React.JSX.Element => {
  const { t } = useI18n();
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  const run = (key: string, action: () => Promise<void>): void => {
    setBusy(key);
    setError(null);
    void action()
      .catch((e: unknown) =>
        setError(e instanceof Error ? e.message : String(e)),
      )
      .finally(() => setBusy(null));
  };

  const copyInvite = (): void => {
    if (!team.inviteLink) {
      return;
    }
    void navigator.clipboard
      .writeText(team.inviteLink)
      .then(() => {
        setCopied(true);
        window.setTimeout(() => setCopied(false), 1600);
      })
      .catch(() => undefined);
  };

  return (
    <div className="team-settings">
      <div className="api-settings-form-section">
        <div className="api-settings-form-section-header">
          <span className="api-settings-form-section-title">
            {t("team.settings.hostSection", { defaultValue: "团队管理" })}
          </span>
        </div>

        <div className="settings-about-row">
          <span className="settings-item-icon">
            <Server size={15} strokeWidth={1.8} />
          </span>
          <div className="settings-item-content">
            <span className="settings-item-title">
              {t("team.members.hostService", { defaultValue: "队长常驻服务" })}
            </span>
            <span className="settings-item-description">
              {t("team.members.hostServiceHint", {
                defaultValue:
                  "开启后登录系统时自动后台启动 Snow，队长关窗或设备重启后团队仍保持在线",
              })}
            </span>
          </div>
          <label className="toggle-switch">
            <input
              type="checkbox"
              checked={team.team?.hostService === true}
              disabled={hostServiceBusy}
              onChange={(e) => onToggleHostService(e.target.checked)}
              hidden
            />
            <span className="toggle-slider" aria-hidden="true" />
          </label>
        </div>

        <div className="settings-about-row">
          <span className="settings-item-icon">
            <Link2 size={15} strokeWidth={1.8} />
          </span>
          <div className="settings-item-content">
            <span className="settings-item-title">
              {t("team.members.inviteLink", { defaultValue: "邀请链接" })}
            </span>
            <span className="settings-item-description team-settings-link">
              {team.inviteLink ||
                t("team.members.inviteUnavailable", {
                  defaultValue: "队长服务未运行，暂不可用。",
                })}
            </span>
          </div>
          <div className="team-settings-row-actions">
            <button
              type="button"
              className="nav-item"
              disabled={!team.inviteLink}
              onClick={copyInvite}
            >
              {copied ? <Check size={14} /> : <Link2 size={14} />}
              <span>
                {copied
                  ? t("team.header.inviteCopied", { defaultValue: "已复制" })
                  : t("team.members.copyInvite", { defaultValue: "复制" })}
              </span>
            </button>
            <button
              type="button"
              className="nav-item"
              disabled={busy !== null}
              title={t("team.members.resetInviteHint", {
                defaultValue:
                  "生成新的邀请码：旧邀请链接立即失效，已加入成员不受影响。",
              })}
              onClick={() => run("invite", team.resetInvite)}
            >
              {busy === "invite" ? (
                <Loader2 size={14} className="spin" />
              ) : (
                <RefreshCw size={14} />
              )}
              <span>
                {t("team.members.resetInvite", {
                  defaultValue: "更换邀请链接",
                })}
              </span>
            </button>
          </div>
        </div>
      </div>

      <div className="api-settings-form-section">
        <div className="api-settings-form-section-header">
          <span className="api-settings-form-section-title">
            {t("team.members.blocklist", { defaultValue: "黑名单" })}
          </span>
          <span className="team-task-group-count">{team.blocklist.length}</span>
        </div>
        <span className="settings-item-description">
          {t("team.members.blocklistHint", {
            defaultValue: "被拉黑的设备无法再次加入团队，可随时解除。",
          })}
        </span>
        {team.blocklist.length === 0 ? (
          <span className="team-settings-empty">
            {t("team.members.blocklistEmpty", {
              defaultValue: "暂无被拉黑的成员。",
            })}
          </span>
        ) : (
          <div className="team-settings-list">
            {team.blocklist.map((blocked) => (
              <div key={blocked.clientId} className="team-settings-item">
                <TeamAvatar
                  name={blocked.name || blocked.clientId}
                  seed={blocked.clientId}
                  size={28}
                />
                <div className="settings-item-content">
                  <span className="settings-item-title">
                    {blocked.name ||
                      t("team.members.blockedUnknown", {
                        defaultValue: "未知成员",
                      })}
                  </span>
                  <span className="settings-item-description">
                    {formatTime(blocked.blockedAt)}
                  </span>
                </div>
                <button
                  type="button"
                  className="nav-item"
                  disabled={busy !== null}
                  onClick={() =>
                    run(`unblock-${blocked.clientId}`, () =>
                      team.unblockMember(blocked.clientId),
                    )
                  }
                >
                  {busy === `unblock-${blocked.clientId}` ? (
                    <Loader2 size={14} className="spin" />
                  ) : (
                    <Ban size={14} />
                  )}
                  <span>
                    {t("team.members.unblock", { defaultValue: "解除拉黑" })}
                  </span>
                </button>
              </div>
            ))}
          </div>
        )}
      </div>

      {error ? <div className="team-form-error">{error}</div> : null}
    </div>
  );
};
