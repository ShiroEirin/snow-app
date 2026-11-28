import { useMemo } from "react";

import {
  CheckpointDiffViewer,
  type CheckpointDiffReader,
} from "../common/CheckpointDiffViewer";
import type { CheckpointDiffTabData } from "./types";

/**
 * 右侧面板「检查点 diff」tab：读取检查点 manifest 中单个文件两端对象
 * （original → expected）的分页 diff，语义为「这一轮把这个文件从什么改成
 * 了什么」。明细由 Rust 按页计算，滚动到底自动翻页。
 */
export const CheckpointDiffTab = ({
  data,
}: {
  data: CheckpointDiffTabData;
}): React.JSX.Element => {
  const read = useMemo<CheckpointDiffReader>(
    () => (startRow, maxRows, maxBytes) =>
      window.snow.readCheckpointManifestDiffPage(
        data.checkpointId,
        data.filePath,
        startRow,
        maxRows,
        maxBytes,
      ),
    [data.checkpointId, data.filePath],
  );

  return (
    <CheckpointDiffViewer
      read={read}
      displayPath={data.filePath}
      changeType={data.changeType}
    />
  );
};
