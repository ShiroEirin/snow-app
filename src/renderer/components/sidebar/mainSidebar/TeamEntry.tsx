import { Users } from "lucide-react";
import type { TeamInfo } from "../../../../preload";
import { useI18n } from "../../../i18n";
import { TeamAvatar } from "../../mainContent/team/TeamShared";

/**
 * 侧边栏的团队协作入口：展示当前项目所属团队与本机待办徽标。
 * 团队数据存于本机数据库，由队长设备的 WebSocket 服务实时同步。
 */
export const TeamEntry = ({
  team,
  pendingCount = 0,
  onClick,
}: {
  team: TeamInfo | null;
  pendingCount?: number;
  onClick: () => void;
}): React.JSX.Element => {
  const { t } = useI18n();
  const displayName = team?.selfName || team?.name || t("team.nameFallback");

  return (
    <button
      type="button"
      className="nav-item team-entry"
      onClick={onClick}
      title={t("team.sidebarEntry", { defaultValue: "团队协作" })}
    >
      {team ? (
        <TeamAvatar
          name={displayName}
          seed={team.selfColor || team.selfPeerId}
          size={22}
        />
      ) : (
        <Users size={16} strokeWidth={1.8} />
      )}
      <span>{t("team.sidebarEntry", { defaultValue: "团队协作" })}</span>
      {pendingCount > 0 ? (
        <span className="sidebar-memo-badge">{pendingCount}</span>
      ) : null}
    </button>
  );
};
