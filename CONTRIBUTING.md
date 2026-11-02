# Snow App 贡献规范：Issue 与 PR / Snow App Contribution Guidelines: Issues & PRs

[中文](#中文) &nbsp;·&nbsp; [English](#english)

本手册规定向 Snow App 提交 Issue 与 Pull Request 时必须遵守的规则。所有提交者（包括借助 AI 辅助开发的提交者）都应先阅读并遵守。

This document defines the mandatory rules for submitting Issues and Pull Requests to Snow App. Every contributor (including those who develop with AI assistance) must read and follow it first.

---

## 中文

### 1. 禁止用 AI 生成提交信息

- 禁止使用 AI 生成 Issue 或 PR 的标题、正文等提交信息。
- 本规则不禁止使用 AI 参与开发，只禁止让 AI 代写提交信息。
- 允许使用 AI 对你本人写好的内容进行润色（语法、措辞、排版），但核心内容必须由你本人撰写。

原因：提交信息代表作者对问题与改动的理解与判断。AI 代写会让信息失真、无法追责，也会降低维护者的沟通效率。

### 2. Issue 只允许两种形式

提交 Issue 时，只接受以下两种：

1. **直接提出**：清晰描述问题或需求本身，不附带方案。
2. **提出并给出唯一可行方案**：描述问题的同时，给出一个你认为唯一可行的方案。

禁止：一次性抛出大量方案，却没有明确的核心结论。维护者无法从"方案清单"中判断你的真实意图，此类 Issue 会被关闭。

### 3. PR 禁止修改核心 AI Loop 工作流

- 核心 AI Loop 工作流是项目的根基，PR 不得修改。
- 如果你认为该工作流存在问题，请先提交 Issue 讨论，不要直接提 PR 改动。

### 4. 引用第三方代码必须给出链接

- 如果 PR 使用了其他开源仓库的代码，必须在 PR 描述中引用来源链接（仓库地址、文件路径、许可证）。
- 未标注来源的 PR 不予合并。

### 5. PR 前必须验证 UI 是否符合应用主题

所有涉及 UI 的 PR，在提交前必须完成主题适配检查：

1. 打开应用的"主题设置"，逐一切换各类主题。
2. 观察你新增或修改的 UI 内容。
3. 判定标准：如果切换主题时，新增的 UI 内容没有任何颜色或布局变化，说明它没有接入应用主题体系。
4. 此类 PR 不予合并。

原因：应用支持多主题，硬编码颜色或样式会破坏整体一致性。

### 提交前检查清单

- [ ] Issue / PR 提交信息由本人撰写（AI 仅用于润色）
- [ ] Issue 属于允许的两种形式之一
- [ ] PR 未改动核心 AI Loop 工作流
- [ ] 引用第三方代码已附来源链接
- [ ] 涉及 UI 的改动已通过多主题切换验证

---

## English

### 1. Do not use AI to generate submission text

- Do not use AI to generate the title or body of an Issue or PR.
- This rule does not prohibit using AI for development; it only prohibits letting AI write the submission text.
- You may use AI to polish text you wrote yourself (grammar, wording, formatting), but the core content must be written by you.

Rationale: the submission text represents the author's own understanding and judgment. AI-written text becomes unreliable, cannot be attributed, and slows down maintainer communication.

### 2. Issues may take only two forms

When filing an Issue, only the following two forms are accepted:

1. **Raise it directly**: clearly describe the problem or requirement itself, without proposing a solution.
2. **Raise it with a single viable solution**: describe the problem and provide the one solution you consider viable.

Prohibited: dumping a large list of options without a clear core conclusion. Maintainers cannot tell your real intent from a "list of options", and such Issues will be closed.

### 3. PRs must not modify the core AI Loop workflow

- The core AI Loop workflow is the foundation of the project and must not be changed by a PR.
- If you believe the workflow has a problem, open an Issue to discuss it first instead of submitting a PR that changes it.

### 4. Citing third-party code requires a link

- If a PR uses code from another open-source repository, the PR description must cite the source link (repository URL, file path, license).
- PRs without source attribution will not be merged.

### 5. Verify UI theme compliance before submitting a PR

Every PR that touches the UI must pass a theme-compliance check before submission:

1. Open the app's "Theme Settings" and switch through each theme one by one.
2. Observe the UI you added or modified.
3. Criterion: if the new UI content shows no color or layout change when switching themes, it is not wired into the app's theme system.
4. Such PRs will not be merged.

Rationale: the app supports multiple themes, and hard-coded colors or styles break overall consistency.

### Pre-submission checklist

- [ ] Issue / PR submission text is written by you (AI used only for polishing)
- [ ] The Issue is in one of the two allowed forms
- [ ] The PR does not modify the core AI Loop workflow
- [ ] Third-party code is attributed with a source link
- [ ] UI changes have been verified across multiple themes
