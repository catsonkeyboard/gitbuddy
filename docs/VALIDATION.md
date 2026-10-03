# 本地验收记录

环境：macOS / Apple Silicon，Rust 1.95.0，Git 2.54.0，GPUI Kit 0.6.6。

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
