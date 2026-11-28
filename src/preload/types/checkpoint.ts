export type CheckpointChangeType = "added" | "modified" | "deleted";

export type CheckpointFileChange = {
  path: string;
  changeType: CheckpointChangeType;
};

/** 回滚链上单个文件的变更摘要（不含 diff 正文）。 */
export type CheckpointFileSummary = CheckpointFileChange & {
  isBinary: boolean;
  /** SSH 工作区中超过传输上限、未读取内容的文件：无行数统计、不提供明细。 */
  tooLarge: boolean;
  additions: number;
  deletions: number;
};

/** 单个文件 diff 明细的一页；按顺序拼接各页 content 即完整 unified diff。 */
export type CheckpointDiffPage = {
  isBinary: boolean;
  tooLarge: boolean;
  content: string;
  totalRows: number;
  nextRow: number;
  /** 内容版本：基线或当前文件变化后改变，调用方据此丢弃已加载的页。 */
  revision: string;
};

/** 检查点时间轴用的单个 manifest 条目（纯读 manifest JSON，不扫描工作树）。 */
export type CheckpointManifestEntryInfo = {
  path: string;
  /** "added" | "modified" | "deleted" | "pending"（捕获中断/进行中的待定条目） */
  changeType: string;
};

/** 单个检查点的 manifest 条目清单与类型统计。 */
export type CheckpointManifestEntriesResult = {
  checkpointId: string;
  entries: CheckpointManifestEntryInfo[];
  addedCount: number;
  modifiedCount: number;
  deletedCount: number;
  pendingCount: number;
};
