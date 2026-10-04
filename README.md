# GitBuddy

使用 **Rust + GPUI + [GPUI Kit](https://github.com/longbridge/gpui-kit)** 构建的原生 Git 桌面客户端。参考 Sublime Merge 的深色、高密度工作区，采用仓库导航、提交列表、差异 / 提交编辑三栏布局。

## 运行

构建需要 Rust 1.95+。macOS 需要 Xcode Command Line Tools 和支持 Metal 的设备。应用运行时通过 `git2` / `libgit2` 访问仓库，不需要系统 `git` 可执行文件。本项目使用 `gpui-kit = 0.6.6`，并提交 Cargo.lock 固定依赖。集成测试和演示仓库生成器使用系统 Git 准备测试数据。

```sh
cargo run --locked
# 直接打开仓库
cargo run --locked -- /absolute/path/to/repository
```

首次启动可选择 Open repository、Clone repository 或 Initialize new。打开对话框支持原生文件夹选择器，也支持输入路径和 `~/`。同一窗口可以打开多个仓库标签，点击标签切换、点击 × 关闭。重启后恢复所有标签及顺序、活动仓库、各仓库的提交草稿、搜索词、当前选择、展开文件与滚动位置。没有会话记录时才打开最近仓库；关闭全部标签后，重启保持欢迎页。命令行指定仓库时，在恢复的标签中打开或聚焦它。

```sh
# 创建独立演示仓库，目标存在时拒绝覆盖
cargo run --locked --example demo_repo
cargo run --locked -- ./target/demo-repo

# 创建四种冲突的独立演示仓库（全部使用 libgit2）
cargo run --locked --example demo_conflicts
cargo run --locked -- ./target/demo-conflicts

# macOS 打包；添加 --debug 可快速打包开发版本
python3 scripts/bundle_macos.py --debug
open dist/GitBuddy.app
```

## 已实现

| 区域 | 功能 |
| --- | --- |
| 仓库 | 打开、初始化、克隆、多仓库标签、完整会话恢复、按仓库独立后台任务、最近仓库、每 10 秒刷新、手动刷新 |
| 工作区 | 已暂存 / 未暂存状态、按文件 / hunk / 选中行暂存与取消暂存、全部暂存 / 取消暂存、重命名、删除、冲突识别 |
| 冲突处理 | 按文件查看 Ours / Base / Theirs、编辑合并结果、选择完整版本或删除、保存并标记解决、继续 / 中止 merge、cherry-pick、revert |
| 差异 | 工作区、暂存区、历史提交按文件折叠；点击文件才加载该文件的统一 diff；红绿增删行、双侧行号、虚拟滚动；二进制和大文件提示 |
| 提交 | 多行提交说明、提交已暂存文件、Amend、撤销最近一次提交、复制提交 ID、revert、cherry-pick 及继续 / 中止 |
| 历史 | 所有分支的提交、作者、相对时间、哈希、引用标记；按父提交绘制分支与合并线；在已加载历史中搜索；分页加载 |
| 文件追踪 | 指定版本的文件列表、跟随重命名的文件历史、按提交展开文件 diff、逐行 Blame 与提交详情跳转 |
| 比较 | 两个提交 / 分支 / 标签的文件树比较、增删统计、按文件折叠、反向比较、设置比较基准 |
| 恢复 | HEAD Reflog 分页查看、选择操作前 / 后的提交、创建恢复分支、恢复当前分支位置 |
| 分支 | 创建并切换、本地切换、远程跟踪分支、合并、中止合并、安全删除已合并分支 |
| 远程 | 添加 remote、fetch/prune、pull --ff-only、按配置的上游分支 push（支持不同名分支）、首次推送到 origin 并设置 upstream、传输进度及取消、领先 / 落后计数 |
| Stash | 保存（包含未跟踪文件）、应用并保留、确认后删除 |
| 标签 | 创建 HEAD 标签、删除本地标签 |
| 配置 | 设置仓库级作者姓名 / 邮箱，读取 Git 配置，并通过 SSH agent 或凭据助手认证 |

快捷键：`⌘O` 打开仓库，`⌘R` 刷新，`⌘Enter` 提交，`⌘Q` 退出。其他平台使用对应的 Ctrl 修饰键。

### 部分暂存与取消暂存

展开工作区或暂存区中的文本文件，每个 `@@` 标题行提供 **Stage hunk / Unstage hunk**。点击红色删除行或绿色新增行可选中该行，`Shift` 连选一个范围内的变更行，`⌘ / Ctrl` 点击可添加或移除不连续的选择；点击 **Stage selected / Unstage selected** 应用选择，**Clear** 清空选择。上下文行和文件头不会被选中。

新增行与删除行独立选择：若要暂存一次完整替换，请同时选中对应的红、绿行。部分操作只更新暂存区，不改写工作区，保留其他文件与未选中行；操作完成后刷新 diff。外部编辑、HEAD 或暂存区变化导致 diff 过期时会拒绝执行，需重新选择。普通的新建、删除文件也支持部分操作；文件权限变化、重命名、二进制、符号链接、子模块、冲突及截断预览应使用整文件操作。

## 操作约定

### 网络取消与仓库任务

每个仓库一次执行一个后台 Git 任务，不同仓库可同时执行。操作期间可切换标签、打开／克隆其他仓库和关闭没有运行任务的标签；标签显示运行中及后台完成／错误标记，悬停可查看该仓库的状态。进度、结果、提交草稿和错误都归属发起操作的仓库，后台完成不会切换当前标签。运行任务的标签需等任务结束后才能关闭。

Fetch、Pull、Push、Clone 在状态栏提供 **Cancel**。点击后显示 **Cancelling…**，等 libgit2 确认停止后才解除该仓库的操作锁；网络连接、DNS 或凭据助手阻塞时，可能需要等当前调用返回或超时。取消是协作式停止，不会回滚已接收的对象或已经更新的远程跟踪引用。

- **Fetch / Pull**：传输阶段可取消；Pull 开始更新 HEAD、暂存区和工作区前进入 **Finishing…**，此后禁用取消并报告最终结果。
- **Push**：可在连接／协商阶段取消；开始打包上传前进入 **Finishing…**，此后等待远端确认，不承诺撤销已发送的推送。
- **Clone**：先克隆到目标父目录内的临时目录，传输和临时工作区 checkout 可取消；最终安装前禁用取消。失败／取消清理本次创建的临时内容，不递归删除目标目录中的文件；目标必须不存在或为空，原有空目录在取消后保留。失败／取消标签提供 **Retry**，也可关闭后重新选择地址。

### 冲突处理

从工作区的 **Resolve conflicts…**、冲突文件行的 **Resolve…** 或 **Repository → Resolve conflicts…** 打开。左侧列出未解决文件，右侧并排显示索引中的 **Ours / Base / Theirs**，下方编辑最终结果。点击 **Use ours / Use theirs** 采用完整版本，**Use working file** 保留当前加载的工作文件；某一侧不存在时可选择删除。Ours / Theirs 指 Git 索引的当前侧 / 传入侧，在 rebase 等操作中不能简单等同于本地 / 远程分支。

**Save & mark resolved** 同时写入工作文件和暂存区，并移除该文件的索引冲突；删除需要确认，其他文件的已暂存内容保留。残留的冲突标记会阻止手工结果和工作文件保存，冲突未全部解决时禁用批量暂存 / 取消暂存。文本草稿随文件和仓库标签保存到会话，重启后继续编辑。若冲突内容或操作状态已变化，保留草稿文本供复核，完整版本选择需重新确认。

外部修改工作文件、冲突索引、HEAD 或操作状态后，过期的保存请求会拒绝执行。**Refresh list** 刷新文件列表，**Reload file** 重新读取三方内容和工作文件；重新加载保留手工文本草稿供复核，完整版本选择则需要重新选择。也可以使用外部编辑器修改文件，再重新加载并选 **Use working file**。

全部解决后可先 **Review working tree** 查看暂存内容，再 **Continue Merge / Cherry-pick / Revert…**。继续操作使用全部已暂存内容和操作原有提交说明，merge 保留两个父提交，cherry-pick 保留原作者；确认后再次校验暂存区和 HEAD。**Abort…** 会恢复 tracked 文件到 HEAD，丢弃操作期间保存的解决结果及其他 tracked 编辑。

内置结果编辑仅支持不超过 2 MB 的 UTF-8 常规文本，三方预览最多显示 20,000 行。二进制、非 UTF-8 和大文件可选择完整版本或使用外部编辑后的工作文件，按原始字节保存。重命名、符号链接和子模块冲突需使用外部工具；rebase 等其他操作可查看 / 解决支持的索引冲突，继续 / 中止需使用外部工具。暂不提供按冲突块自动合并或同步三方滚动。

### 文件历史、Blame 与版本比较

- **文件历史**：点击工作区或历史提交文件行的 **History**；也可从 **Repository → File history / Blame…** 按路径筛选指定版本的文件，或直接输入已删除文件的路径。历史跟随重命名，遍历可达的合并父提交；点击记录展开该文件 diff，**Commit** 打开完整提交详情。合并提交的 diff 以第一个父提交为基准。
- **Blame**：文件行的 **Blame** 或历史页的 **Blame at revision** 显示已提交版本的逐行作者、日期、提交 ID 和正文；点击提交 ID 查看详情，**Back to inspection** 返回。未提交编辑不参与归属计算。
- **比较**：选择 **Repository → Compare commits / branches…**，填写 Base 和 Target（分支、标签、提交 ID 或 `HEAD~1` 等版本表达式）。比较从 Base 到 Target 的两棵文件树，不以共同祖先为基准；**Swap** 反转方向。分支操作菜单提供与 HEAD 比较，提交的 **Actions…** 可设置比较基准，再与另一个提交比较。同名分支 / 标签可使用 `refs/heads/name` / `refs/tags/name` 区分。

结果固定到读取时的提交 ID，分支后续移动不会改变已打开的 diff。各仓库标签保留查看结果和已展开文件；这些查看操作不改动 HEAD、暂存区或工作区。文件历史每页增加 100 条，最多显示 2,000 条、扫描 50,000 个提交；文件选择器最多列出 5,000 个文件（仍可手动输入其他路径）。Blame 只支持常规文本文件，拒绝二进制和超过 2 MB 的内容，最多预览 20,000 行。

### Amend、撤销提交与 Reflog 恢复

- **Amend**：从工作区的 **Amend…** 或 **Repository → Amend latest commit…** 打开。预填原提交说明，使用已暂存内容替换 HEAD，保留原作者和父提交，更新提交者；没有新暂存改动时可只修改说明。编辑框独立于普通提交草稿。
- **撤销最近一次提交**：选择 **Repository → Undo latest commit…**。HEAD 回到第一个父提交，暂存区与工作区保持原样；合并提交也按第一个父提交回退。撤销首次提交后，当前分支回到尚未提交的状态，文件仍在暂存区；分离 HEAD 下的首次提交需先创建分支。
- **Reflog 恢复**：选择 **Repository → Reflog / Recover…**，点击记录中的 **Before / After** 选择操作前后的确切提交。**Create recovery branch…** 新建分支但不切换；**Restore current branch…** 只恢复当前分支（或分离 HEAD）的位置，保留暂存区与工作区，因此相对目标提交的差异可能变成已暂存修改。

改写前会再次校验 HEAD；Amend 还校验暂存快照，打开对话框后发生变化则拒绝执行。未完成的合并、rebase、cherry-pick、revert 或索引冲突会阻止改写历史，但仍可从 Reflog 创建独立恢复分支。历史改写会保留 HEAD Reflog 记录，不自动推送；已被 Git 清理的提交对象无法恢复。

### 通用约定

- 仓库操作由 Rust `git2` 在后台调用 `libgit2` 完成，应用运行时不会启动 Bash 或系统 `git`。单文件操作按字面路径匹配，支持空格、换行、通配字符及 Unix 非 UTF-8 文件名。
- 丢弃操作只恢复工作区到暂存区版本，不清除已暂存内容；不提供批量删除未跟踪文件的快捷操作。
- 删除 stash、丢弃修改、合并、revert 等操作需要在应用内确认。删除分支前检查分支是否已合入当前 HEAD，推送不使用 force。
- 远程操作通过 `libgit2` 使用 SSH agent / Git credential helper。GUI 不提供终端密码输入，首次 SSH 信任及凭据准备应先完成；错误可在状态栏复制。
- 合并、cherry-pick 和 revert 要求操作前工作区及暂存区无改动，避免中止操作时覆盖已有修改。
- 合并冲突显示在工作区，可进入内置冲突处理界面；Cherry-pick/revert 的继续和中止也保留在提交的 Actions 菜单。
- 最近仓库记录保存在 `~/.config/gitbuddy/settings.json`。

### 会话恢复

会话保存在 `~/.config/gitbuddy/session.json`，每秒在后台检查并保存变化，正常退出和关闭窗口时执行最终保存。临时文件写入后原子替换，旧后台快照不会覆盖退出时的新草稿。可设置 `GITBUDDY_CONFIG_DIR` 使用独立配置目录；最近仓库仍保存在单独的 `settings.json`。

恢复内容包括 Commits / Files 页面、历史加载数量、提交详情及说明展开、工作区 / 暂存区的展开文件与选中行、提交草稿的光标 / 选区 / 编辑区滚动位置，以及侧栏、历史列表和各文件 diff 的滚动位置。文件历史、Blame 和比较恢复到原来的提交 ID；分支移动不会替换已打开的版本。Amend 草稿独立保存，重新打开 Amend 时仅在 HEAD 和分支仍匹配时填入；冲突结果草稿及编辑位置也按文件保存。

仓库暂时不可用时保留标签和状态，恢复目录后可 **Retry**。被清理的提交无法重新读取时显示错误并返回工作区；diff 内容发生变化时不恢复旧行号的选择。滚动位置等待内容加载及布局后恢复，内容缩短时由界面限制到有效范围。网络任务、Git 写操作和确认对话框不自动重跑；普通操作对话框的临时输入不作为会话内容。强制终止可能丢失最后一次自动保存后的编辑。损坏会话保留为 `session.json.broken-*`，未知版本和无法读取的文件不会被覆盖，保存错误在状态栏提示。

## 验证

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

集成测试使用系统 Git 创建隔离临时仓库与本地 bare remote，再验证 `git2` 后端的首次提交、暂存区独立性、特殊文件名、重命名、分支/标签、stash、冲突中止、revert/cherry-pick 和远程同步，不连接真实远程服务。

## 当前边界

这是可运行的首版客户端，尚未实现交互式 rebase、Git LFS / 子模块专用管理。提交图依据已加载提交的父子关系绘制；搜索过滤或分页边界外的提交不会显示连线。`libgit2` 创建提交时不会执行用户的 Git hooks 或自动进行 GPG 签名。

差异预览最多 20,000 行，未跟踪文件超过 2 MB 不加载正文。网络传输进度每 100ms 节流，阶段切换即时显示；取消边界见上文。真实外部服务器认证尚未验收。当前在 macOS 验证，Windows/Linux 尚未验收。生成的 .app 用于本地运行，未做发行签名或公证。

## 代码结构

- `src/git.rs`：仓库数据类型、diff 行号处理与共享预览上限（`diff_preview`）、进度通道类型。
- `src/git/libgit.rs`：基于 `git2` / `libgit2` 的仓库快照（含变化指纹）、差异、Git 操作与网络进度回调。
- `src/git/network.rs`：线程安全取消令牌、取消／最终写入阶段互斥、进度节流与取消结果。
- `src/git/partial.rs`：结构化 hunk / 行选择、过期校验与暂存区内容重建，保留原始字节及换行。
- `src/git/recovery.rs`：Amend、保留暂存区的提交撤销、HEAD Reflog 与恢复；使用引用锁和快照校验防止操作落到已切换的分支。
- `src/git/inspect.rs`：固定版本的文件浏览、跨父提交跟随重命名的历史、Blame、两棵树比较与精确文件 patch。
- `src/git/conflicts.rs`：索引三方内容、结果保存、引用 / 索引锁和过期校验、继续冲突操作。
- `src/ui.rs`：状态机核心——多仓库标签（`tabs` + `active` 两段式）与异步代际控制。
- `src/ui/tasks.rs`：按稳定仓库标签 ID 分发后台任务、结果与进度归属、取消和失败后重试。
- `src/ui/graph.rs`：提交分支及合并线布局。
- `src/ui/patches.rs`：按文件懒加载的 diff 面板。
- `src/ui/history.rs`、`src/ui/sidebar.rs`、`src/ui/toolbar.rs`、`src/ui/detail.rs`、`src/ui/modals.rs`：对应界面区域。
- `src/ui/recovery.rs`：异步加载历史操作对话框、独立 Amend 编辑器与 Reflog 恢复列表。
- `src/ui/inspect.rs`：文件选择器、文件历史、Blame 虚拟列表和版本比较页面，后台加载及过期结果隔离。
- `src/ui/conflicts.rs`：三方预览、独立文件草稿、结果编辑、删除确认与继续 / 中止入口。
- `src/settings.rs`：最近仓库记录（损坏时备份为 `.json.broken` 并重建）。
- `src/session.rs`：带版本的会话格式、路径编码、原子保存与旧快照保护。
- `src/ui/session.rs`：按仓库捕获 / 恢复界面状态、独立滚动句柄及退出保存。
- `tests/git_workflows.rs`：真实 Git 仓库的集成测试。
- `tests/partial_staging.rs`：部分暂存 / 取消暂存的隔离仓库回归测试，夹具和受测操作均使用 libgit2。
- `tests/history_recovery.rs`：历史改写、首次 / 合并 / 分离 HEAD 提交、过期选择、冲突与恢复的 libgit2 隔离仓库测试。
- `tests/repository_inspection.rs`：文件历史 / Blame / 比较的隔离仓库测试，包含合并、重命名后旧名复用、删除后重建、特殊路径和版本固定。
- `tests/conflict_resolution.rs`：三方版本、删除 / 二进制 / 可执行文件、过期请求、索引锁、多文件独立解决和继续操作的 libgit2 隔离仓库测试。
- `examples/demo_repo.rs`：可重复生成的隔离演示仓库。
- `examples/demo_conflicts.rs`：包含文本、二进制、修改 / 删除冲突的隔离演示仓库。

## 性能设计

- 每 10 秒的自动刷新先计算指纹（HEAD、引用、状态列表、暂存区 blob ID 与模式、变更文件的大小及修改时间；Unix 另含 ctime / inode），与上次快照一致则跳过全量重建。已修改文件再次编辑或暂存区内容更新也会触发刷新，无需每次遍历提交历史或读取所有工作区文件正文。
- 切换仓库标签是对 `RepoTab` 结构体的整体交换（`mem::take`），不再逐字段拷贝 15 个状态。
