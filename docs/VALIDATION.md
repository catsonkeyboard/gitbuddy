# 本地验收记录

环境：macOS / Apple Silicon，Rust 1.95.0，Git 2.54.0，GPUI Kit 0.6.6。

## 2026-10-03 文件历史、Blame 与版本比较

- 新增指定提交版本的文件浏览与筛选、文件历史（跨合并父提交跟随各自路径的重命名）、逐行 Blame、两个提交 / 分支 / 标签之间的直接文件树比较。查看结果固定到已解析的提交 ID，patch 按文件点击加载；全部应用操作经 `git2` / `libgit2`。
- 新增 9 项隔离仓库测试：文件过滤与分页、合并父提交、重命名后旧名复用、删除后重建、Blame 作者 / 日期 / 原路径归属、脏工作区与暂存区保持不变、空文件 / CRLF / 无末尾换行 / 非 UTF-8、二进制 / 大文件 / 符号链接拒绝、行数上限、反向比较、分支移动后版本固定、标签与字面特殊路径，以及跨目录路径拒绝。非 UTF-8 文件名通过 libgit2 索引构造，绕过 APFS 文件名限制。
- `cargo test --locked --no-fail-fast`：52 项集成测试 + 4 项单元测试全部通过；最终 UI 调整后格式检查和 Clippy 通过。依赖 `block 0.1.6` 的未来兼容性提示仍存在。
- 原生窗口在 `target/history-recovery-20261003` 验证：工作区文件行进入 README 历史并展开根提交 diff；Blame 显示 9 行及作者，点击提交 ID 可进入详情并返回；文件选择器筛选 `src/app.rs` 后显示该文件 7 条历史。
- 最终开发包复查：文件筛选无匹配时显示明确提示；不存在路径的 Blame 显示错误及重新选择入口，重新选择源文件后可正常恢复到历史页面。
- 原生窗口比较 `refs/heads/main → refs/heads/feature/workspace`，确认仅列出 `src/app.rs`、统计 +2 / −2，展开后显示正确增删内容；Swap 后增删内容反转。切换到另一个仓库标签并返回、手动刷新后，原比较结果与已展开 diff 保留。
- 查看操作前后核对演示仓库 HEAD，以及 `.git/index`、README.md、src/app.rs 的 SHA-1：全部保持不变。未对真实仓库执行写入或推送；大规模历史上限、Windows/Linux 原生交互未进行压力或桌面验收。
- 已重新生成 `dist/GitBuddy.app` 开发包，入口、操作语义和预览上限补充到 README。

## 2026-10-03 Amend、撤销与 Reflog 恢复

- 新增 Amend（暂存内容 + 独立编辑的原提交说明）、撤销最近提交（保留暂存区与工作区）、HEAD Reflog 查看、从操作前 / 后提交创建恢复分支或恢复当前分支位置。全部受测操作使用 `git2` / `libgit2`。
- 新增 9 项隔离仓库测试：覆盖原作者 / 父提交 / 暂存内容保留、只修改说明、合并提交、首次提交撤销与恢复（包括关闭自动 reflog）、分离 HEAD、过期 HEAD / 同提交切分支 / 暂存快照、跨仓库与缺失对象、重名分支、冲突与进行中的合并。对比暂存区文件字节和工作区内容，确认历史操作不覆盖它们。
- `cargo test --locked --no-fail-fast`：43 项集成测试 + 4 项单元测试通过；格式检查与 Clippy 通过。依赖 `block 0.1.6` 仍有 Cargo 的未来兼容性提示，本次未修改该依赖。
- 原生窗口在 `target/history-recovery-20261003` 实际执行 Amend → Undo → 创建恢复分支 → 恢复当前分支：核对 HEAD、作者、提交说明和 reflog，暂存区与工作区文件的 SHA-1 校验值始终不变；恢复分支创建后未切换当前分支。
- 原生窗口确认 Amend 自动填入原说明、空说明不可提交、普通提交草稿在上述操作后保留；Reflog 可选 Before / After，列表日期及分隔线对齐。`target/history-empty-20261003` 验证空 Reflog 提示与紧凑空状态。
- 已重新打包并打开 `dist/GitBuddy.app` 开发版本。只修改隔离演示仓库，未改写真实仓库历史或推送远程。首次 / 合并 / 分离 HEAD 等边界由集成测试覆盖，未逐项进行原生点击验收。

## 2026-10-03 部分暂存与取消暂存

- 工作区 / 暂存区的文本 diff 增加 hunk 操作和选中行操作；点击选择、Shift 连选、⌘ / Ctrl 多选，选择状态按文件及仓库标签隔离。
- 后端使用 libgit2 的结构化 diff 行及原始字节重建暂存区 blob，不改写工作区；执行前校验仓库、暂存区、diff 及相关 HEAD 树，过期选择拒绝执行。
- 新增 10 项隔离仓库集成测试与 1 项行选择单元测试。覆盖双向 15 种替换行选择组合、多个 hunk、其他文件暂存内容不变、首次提交前的新文件、删除文件、CRLF / 非 UTF-8 / 无末尾换行、可执行权限、过期 / 跨仓库 / 非法选择；二进制、重命名、冲突、符号链接及截断预览不提供部分操作。
- `cargo test --locked --no-fail-fast`：34 项集成测试 + 4 项单元测试通过；Clippy 与格式检查通过。
- 原生窗口在隔离仓库 `target/partial-staging-20261003` 实际执行 Stage selected、Stage hunk、Unstage selected、Unstage hunk。通过仓库 diff 核对每次只更新所选内容，其他文件已暂存内容不变；最后 README 的暂存内容恢复到 HEAD，工作区仍保留全部编辑。
- 原生窗口确认单行高亮、选择数量、操作后选择清空，以及无变化手动刷新保留选择；范围选择和多选修饰键逻辑由单元测试覆盖，未自动化模拟组合键鼠标点击。
- 已重新生成开发包 `dist/GitBuddy.app`。未验证 Windows/Linux 原生交互。

## 2026-10-03 正确性修复

- 修复新仓库标签的历史加载数量默认为 0：统一初始页及分页大小为 300，刷新和操作后重载保留提交列表。
- 刷新指纹增加变更文件元数据与暂存区 blob ID / 模式，覆盖同一状态下的连续编辑、未跟踪文件再次编辑，以及只改暂存区内容的场景。
- Push 读取 `branch.<name>.remote` 与 `branch.<name>.merge`，支持不同名上游和带斜杠的 remote；本地远程跟踪引用缺失时仍使用配置目标，配置不完整时拒绝推送。
- 5 项新增回归测试在修复前均失败，修复后通过。完整测试共 24 项集成测试和 3 项单元测试通过；格式检查、Clippy 通过。
- 推送回归仅使用临时本地 bare remote，验证目标分支更新且远程同名分支保持不变；没有向真实远程服务器推送。
- 原生窗口在隔离仓库 `target/correctness-20261003` 验证：打开新标签后显示 7 条提交，手动刷新仍为 7 条；保持 README.md 差异展开，连续改为 `version one` 和等长的 `version two`，自动刷新均显示最新内容。
- `python3 scripts/bundle_macos.py --debug` 成功，生成本地开发包 `dist/GitBuddy.app`。

## 2026-09-29 重构验收

- `cargo test --locked`：20 项集成测试 + 2 项分支图单元测试通过。新增覆盖：变化指纹（重复读取稳定、工作区/暂存区/提交/建分支均改变指纹、与快照内嵌指纹一致）、`diff_preview` 截断标记（小 diff 无标记、超限追加标记行且行数精确）。
- `cargo clippy --locked --all-targets -- -D warnings`：通过，无警告。
- `cargo fmt --all -- --check`：通过。
- UI 重构（`tabs: Vec<Option<RepoTab>>` + `active: RepoTab` + `Deref`）：`src/ui.rs` 由 2104 行拆为核心 + 6 个子模块（graph / patches / history / sidebar / toolbar / detail / modals），编译与全部测试通过。
- Cargo.lock：为引入 `async-channel`（进度通道，gpui 传递闭包已带此依赖）重新生成；`gpui-kit` 仍锁定 0.6.6。注意——重新生成将全部 861 个依赖重锁到当前兼容版本，非最小变更。

## 自动化

- `cargo test --locked`：集成测试使用临时仓库和本地 bare remote。测试通过系统 Git 创建夹具，受测应用操作均经 `git2` / `libgit2`。
- 覆盖空仓库、首次提交、单文件暂存、暂存/工作区差异、重命名、特殊路径精确丢弃、分支、标签、stash、冲突中止及解决后双父提交、脏工作区拒绝合并、revert/cherry-pick、clone/fetch/pull/push、二进制预览、diff 行号，以及历史提交的按文件拆分（包括根提交与合并提交）。分支图测试覆盖合并线汇入主线与搜索过滤后不连接隐藏提交。
- `cargo build --locked`：开发构建成功。
- `cargo clippy --locked --all-targets -- -D warnings`：通过，无警告。
- `cargo fmt --all -- --check`：通过。

## 原生窗口手动验收

在 `target/demo-repo` 隔离演示仓库中，通过桌面 UI 实际执行：

1. 打开工作区，确认左右分栏、分支、标签、历史、已暂存/未暂存分组。
2. 点击 README.md，确认 diff 红绿高亮和新旧行号。
3. 在 diff 面板点击 Stage file，确认暂存文件数量由 1 变为 2。
4. 填写提交说明并点击 Commit 2 files；确认新提交进入历史、工作区 clean、编辑框清空。
5. 使用 ⌘O 打开仓库对话框并取消。
6. 输入搜索词，确认提交历史正确过滤。

在 `target/demo-ui-v2` 隔离演示仓库中复查新版界面：工具栏、侧栏和提交行距已收紧；历史记录的竖线跨行连续；Repository 菜单可展开；未暂存的 `README.md` 和已暂存的 `src/app.rs` 可分别展开 diff，并能同时保持展开。另在历史提交中确认文件列表默认折叠，点击单个文件才加载对应 diff。原生 macOS 标题栏与深色窗口一致，关闭最后一个窗口后应用进程退出。

多仓库标签复查：在 `baiji` 历史中选中提交后，点击另一个提交只改变选择和右侧详情，侧栏及历史数据保持不变；打开隔离仓库 `target/demo-ui-v2` 后出现第二个标签；切换回 `baiji` 可恢复原提交选择，再切回演示仓库可恢复搜索词 `staging` 和未提交的草稿文本。未在 `baiji` 执行写入操作。

`git2` 迁移后在原生窗口复查：真实仓库中历史列表可显示多条分支及合并线；另用 `target/demo-graph-v1` 隔离仓库检查一个合并提交，两条父线分别连接主线与特性分支，并在共同祖先汇入。点击合并提交后，右侧按文件显示 `feature.txt`，展开后看到该文件的 `+feature` 差异。真实仓库仅做只读查看。

不涉及用户真实仓库的提交或远程推送。

## 未验证范围

- 真实外部 SSH/HTTPS 服务器的认证、网络超时与进度显示（本地 bare remote 不产生 sideband 数据）。
- `libgit2` 提交不会运行 Git hooks 或自动进行 GPG 签名。
- Windows/Linux 构建与原生交互。
- macOS 发行签名、公证、应用商店发布。
- 极大仓库和超大 diff 的压力测试；指纹短路在大仓库上的实际收益未测。
