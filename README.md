# GitBuddy

使用 **Rust + GPUI + [GPUI Kit](https://github.com/longbridge/gpui-kit)** 构建的原生 Git 桌面客户端。参考 Sublime Merge 的深色、高密度工作区，采用仓库导航、提交列表、差异 / 提交编辑三栏布局。

## 运行

构建需要 Rust 1.95+。macOS 需要 Xcode Command Line Tools 和支持 Metal 的设备。应用运行时通过 `git2` / `libgit2` 访问仓库，不需要系统 `git` 可执行文件。本项目使用 `gpui-kit = 0.6.6`，并提交 Cargo.lock 固定依赖。集成测试和演示仓库生成器使用系统 Git 准备测试数据。

```sh
cargo run --locked
# 直接打开仓库
cargo run --locked -- /absolute/path/to/repository
```

首次启动可选择 Open repository、Clone repository 或 Initialize new。打开对话框支持原生文件夹选择器，也支持输入路径和 `~/`。同一窗口可以打开多个仓库标签，点击标签切换、点击 × 关闭；每个标签保留搜索词、提交草稿、当前选择和已展开的差异。再次启动会恢复最近打开的仓库。

```sh
# 创建独立演示仓库，目标存在时拒绝覆盖
cargo run --locked --example demo_repo
cargo run --locked -- ./target/demo-repo

# macOS 打包；添加 --debug 可快速打包开发版本
python3 scripts/bundle_macos.py --debug
open dist/GitBuddy.app
```

## 已实现

| 区域 | 功能 |
| --- | --- |
| 仓库 | 打开、初始化、克隆、多仓库标签、最近仓库、每 10 秒刷新、手动刷新 |
| 工作区 | 已暂存 / 未暂存状态、按文件 / hunk / 选中行暂存与取消暂存、全部暂存 / 取消暂存、重命名、删除、冲突识别 |
| 差异 | 工作区、暂存区、历史提交按文件折叠；点击文件才加载该文件的统一 diff；红绿增删行、双侧行号、虚拟滚动；二进制和大文件提示 |
| 提交 | 多行提交说明、提交已暂存文件、复制提交 ID、revert、cherry-pick 及继续 / 中止 |
| 历史 | 所有分支的提交、作者、相对时间、哈希、引用标记；按父提交绘制分支与合并线；在已加载历史中搜索；分页加载 |
| 分支 | 创建并切换、本地切换、远程跟踪分支、合并、中止合并、安全删除已合并分支 |
| 远程 | 添加 remote、fetch/prune、pull --ff-only、按配置的上游分支 push（支持不同名分支）、首次推送到 origin 并设置 upstream、领先 / 落后计数 |
| Stash | 保存（包含未跟踪文件）、应用并保留、确认后删除 |
| 标签 | 创建 HEAD 标签、删除本地标签 |
| 配置 | 设置仓库级作者姓名 / 邮箱，读取 Git 配置，并通过 SSH agent 或凭据助手认证 |

快捷键：`⌘O` 打开仓库，`⌘R` 刷新，`⌘Enter` 提交，`⌘Q` 退出。其他平台使用对应的 Ctrl 修饰键。

### 部分暂存与取消暂存

展开工作区或暂存区中的文本文件，每个 `@@` 标题行提供 **Stage hunk / Unstage hunk**。点击红色删除行或绿色新增行可选中该行，`Shift` 连选一个范围内的变更行，`⌘ / Ctrl` 点击可添加或移除不连续的选择；点击 **Stage selected / Unstage selected** 应用选择，**Clear** 清空选择。上下文行和文件头不会被选中。

新增行与删除行独立选择：若要暂存一次完整替换，请同时选中对应的红、绿行。部分操作只更新暂存区，不改写工作区，保留其他文件与未选中行；操作完成后刷新 diff。外部编辑、HEAD 或暂存区变化导致 diff 过期时会拒绝执行，需重新选择。普通的新建、删除文件也支持部分操作；文件权限变化、重命名、二进制、符号链接、子模块、冲突及截断预览应使用整文件操作。

## 操作约定

- 仓库操作由 Rust `git2` 在后台调用 `libgit2` 完成，应用运行时不会启动 Bash 或系统 `git`。单文件操作按字面路径匹配，支持空格、换行、通配字符及 Unix 非 UTF-8 文件名。
- 丢弃操作只恢复工作区到暂存区版本，不清除已暂存内容；不提供批量删除未跟踪文件的快捷操作。
- 删除 stash、丢弃修改、合并、revert 等操作需要在应用内确认。删除分支前检查分支是否已合入当前 HEAD，推送不使用 force。
- 远程操作通过 `libgit2` 使用 SSH agent / Git credential helper。GUI 不提供终端密码输入，首次 SSH 信任及凭据准备应先完成；错误可在状态栏复制。
- 合并、cherry-pick 和 revert 要求操作前工作区及暂存区无改动，避免中止操作时覆盖已有修改。
- 合并冲突显示在工作区；在编辑器解决后暂存并提交。Cherry-pick/revert 的继续和中止入口位于提交的 Actions 菜单。
- 最近仓库记录保存在 `~/.config/gitbuddy/settings.json`。

## 验证

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

集成测试使用系统 Git 创建隔离临时仓库与本地 bare remote，再验证 `git2` 后端的首次提交、暂存区独立性、特殊文件名、重命名、分支/标签、stash、冲突中止、revert/cherry-pick 和远程同步，不连接真实远程服务。

## 当前边界

这是可运行的首版客户端，尚未实现交互式 rebase、三方冲突编辑器、Git LFS / 子模块专用管理。提交图依据已加载提交的父子关系绘制；搜索过滤或分页边界外的提交不会显示连线。`libgit2` 创建提交时不会执行用户的 Git hooks 或自动进行 GPG 签名。

差异预览最多 20,000 行，未跟踪文件超过 2 MB 不加载正文。远程操作（fetch / pull / push / clone）在状态栏显示传输进度（每 100ms 节流），但尚不支持取消；真实外部服务器认证尚未验收。当前在 macOS 验证，Windows/Linux 尚未验收。生成的 .app 用于本地运行，未做发行签名或公证。

## 代码结构

- `src/git.rs`：仓库数据类型、diff 行号处理与共享预览上限（`diff_preview`）、进度通道类型。
- `src/git/libgit.rs`：基于 `git2` / `libgit2` 的仓库快照（含变化指纹）、差异、Git 操作与网络进度回调。
- `src/git/partial.rs`：结构化 hunk / 行选择、过期校验与暂存区内容重建，保留原始字节及换行。
- `src/ui.rs`：状态机核心——多仓库标签（`tabs` + `active` 两段式）、后台任务分发（`dispatch`）、异步代际控制。
- `src/ui/graph.rs`：提交分支及合并线布局。
- `src/ui/patches.rs`：按文件懒加载的 diff 面板。
- `src/ui/history.rs`、`src/ui/sidebar.rs`、`src/ui/toolbar.rs`、`src/ui/detail.rs`、`src/ui/modals.rs`：对应界面区域。
- `src/settings.rs`：最近仓库记录（损坏时备份为 `.json.broken` 并重建）。
- `tests/git_workflows.rs`：真实 Git 仓库的集成测试。
- `tests/partial_staging.rs`：部分暂存 / 取消暂存的隔离仓库回归测试，夹具和受测操作均使用 libgit2。
- `examples/demo_repo.rs`：可重复生成的隔离演示仓库。

## 性能设计

- 每 10 秒的自动刷新先计算指纹（HEAD、引用、状态列表、暂存区 blob ID 与模式、变更文件的大小及修改时间；Unix 另含 ctime / inode），与上次快照一致则跳过全量重建。已修改文件再次编辑或暂存区内容更新也会触发刷新，无需每次遍历提交历史或读取所有工作区文件正文。
- 切换仓库标签是对 `RepoTab` 结构体的整体交换（`mem::take`），不再逐字段拷贝 15 个状态。
