# GitBuddy

使用 **Rust + GPUI + [GPUI Kit](https://github.com/longbridge/gpui-kit)** 构建的原生 Git 桌面客户端。参考 Sublime Merge 的深色、高密度工作区，采用仓库导航、提交列表、差异 / 提交编辑三栏布局。

## 运行

构建需要 Rust 1.95+。macOS 需要 Xcode Command Line Tools 和支持 Metal 的设备。普通 Git 操作通过 `git2` / `libgit2` 完成，不依赖系统 `git` 可执行文件；LFS 远程传输与锁管理单独依赖官方 `git-lfs` 及其 Git 运行环境，通过进程参数直接调用，不经过 Bash。本项目使用 `gpui-kit = 0.6.6`，并提交 Cargo.lock 固定依赖。部分集成测试和演示仓库生成器使用系统 Git 准备测试数据。

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

# 创建包含可 Squash 提交、Worktree、子模块和 LFS 对象的演示仓库
cargo run --locked --example demo_advanced
cargo run --locked -- ./target/advanced-ui-20261004/project

# 创建远程管理 / 推送选项演示仓库（两个独立本地 bare 远程）
cargo run --locked --example demo_remotes
cargo run --locked -- ./target/remote-ui-20261005/project

# 创建多选 / Reset / 旧提交编辑与拆分的独立演示仓库
cargo run --locked --example demo_history
cargo run --locked -- ./target/history-ui-20261005/project

# 创建并排 Diff / 多冲突块夹具及独立会话配置（全部使用 libgit2）
cargo run --locked --example demo_diff
GITBUDDY_CONFIG_DIR="$PWD/target/demo-diff/config" cargo run --locked

# 创建含暂存 / 未暂存 / 未跟踪内容的独立 Stash 演示仓库
cargo run --locked --example demo_stash
GITBUDDY_CONFIG_DIR="$PWD/target/demo-stash/config" cargo run --locked

# 创建 Rebase 恢复、递归子模块、Worktree 与 LFS 维护的独立演示仓库
cargo run --locked --example demo_advanced_recovery
GITBUDDY_CONFIG_DIR="$PWD/target/demo-advanced-recovery/config" cargo run --locked

# 创建 5,001 个提交、2,000 个变更文件、1,000 个标签的隔离演示仓库
cargo run --locked --example demo_scalability
GITBUDDY_CONFIG_DIR="$PWD/target/demo-scalability/config" cargo run --locked

# 创建分支重命名、远程分支删除及 detached checkout 的隔离演示仓库
cargo run --locked --example demo_branches
GITBUDDY_CONFIG_DIR="$PWD/target/demo-branches/config" cargo run --locked

# 创建快进、分叉合并 / squash、Pull rebase 与冲突的独立演示仓库
cargo run --locked --example demo_merge_pull
GITBUDDY_CONFIG_DIR="$PWD/target/demo-merge-pull/config" cargo run --locked

# 创建两个远程、独立来源分支和同名本地 / 远端标签的演示仓库
cargo run --locked --example demo_remote_operations
GITBUDDY_CONFIG_DIR="$PWD/target/demo-remote-operations/config" cargo run --locked

# 创建含历史提交的标签演示仓库；可附加 --ssh-test-key 生成仅供演示的临时密钥
cargo run --locked --example demo_tags
GITBUDDY_CONFIG_DIR="$PWD/target/demo-tags/config" cargo run --locked

# macOS 打包；添加 --debug 可快速打包开发版本
python3 scripts/bundle_macos.py --debug
open dist/GitBuddy.app
```

## 已实现

| 区域 | 功能 |
| --- | --- |
| 仓库 | 打开、初始化、克隆、多仓库标签、完整会话恢复、按仓库独立后台任务、最近仓库、每 10 秒刷新、手动刷新 |
| 工作区 | 已暂存 / 未暂存状态、按文件 / hunk / 选中行暂存与取消暂存、全部暂存 / 取消暂存、重命名、删除、冲突识别 |
| 冲突处理 | 按文件查看 Ours / Base / Theirs、逐块选择 Ours / Theirs / Both、编辑结果、外部合并工具及取消、选择完整版本或删除、保存并标记解决、继续 / 中止 merge、cherry-pick、revert 和 GitBuddy Rebase |
| 差异 | 工作区、暂存区、历史和比较按文件折叠 / 延迟加载；统一 / 并排 Diff、词级高亮、3 行 / 完整上下文、行号、同步滚动与虚拟列表；二进制和大文件提示 |
| 提交 | 多行提交说明、提交已暂存文件、Amend、撤销最近一次提交、复制提交 ID、revert、cherry-pick 及继续 / 中止 |
| 历史 | 所有分支的提交与分支图、搜索 / 分页；多选提交、批量改写计划、任意提交创建分支、Soft / Mixed / Hard Reset |
| 文件追踪 | 指定版本的文件列表、跟随重命名的文件历史、按提交展开文件 diff、逐行 Blame 与提交详情跳转 |
| 比较 | 两个提交 / 分支 / 标签的文件树比较、增删统计、按文件折叠、反向比较、设置比较基准 |
| 恢复 | HEAD Reflog 分页查看、选择操作前 / 后的提交、创建恢复分支、恢复当前分支位置 |
| Rebase / Squash | 编辑线性提交计划、调整顺序、Pick / Reword / Squash / Fixup / Drop / Edit / Split / Break、--onto、Autosquash、旧提交编辑与拆分、冲突继续 / 中止、带现场备份的重试 / 跳过、持久化进度和原历史备份 |
| Worktree | 列出关联工作树、新建分支及工作树、独立标签打开、锁定 / 解锁、移除干净工作树、清理失效登记、创建后展开共享 LFS 缓存、准备递归子模块 |
| 子模块 | 添加、递归列出 / 初始化 / 更新 / 同步 URL、嵌套 gitlink 暂存、独立标签打开、Worktree 独立检出、脏工作区保护及网络取消 |
| Git LFS | Track / Untrack、原生 SHA-256 缓存及指针暂存、本地对象展开、remote Fetch / Push、普通 Push 前上传、锁列表 / 创建 / 解锁 / 强制解锁、缓存校验 / 隔离清理 / 恢复 / 确认后永久删除 |
| 分支 | 创建并切换、本地切换、重命名并保留自身配置 / Reflog、带远端 ID 校验的远程分支删除、检出提交 / 标签进入 detached HEAD、远程跟踪分支、合并、中止合并、安全删除已合并分支 |
| Merge / Pull | 默认快进或合并、no-ff、squash 暂存后提交；Pull 的仅快进 / merge / rebase 策略、冲突继续 / 中止及持久化恢复 |
| 远程 | 添加 / 编辑 URL / 重命名 / 删除 remote、设置 / 清除 upstream、独立选择 Fetch / Pull 的 remote 和分支、远端标签查询 / 带对象 ID 校验的删除、选择推送远程 / 本地源分支 / 目标分支、force-with-lease、fetch/prune、Pull 策略选择、快捷 push、传输进度及取消、领先 / 落后计数 |
| Stash | 按选定文件保存、工作区 / 索引 / 未跟踪文件分组预览、Apply / Pop、恢复索引、从 Stash 建分支、确认后删除 |
| 标签 | 在任意提交创建轻量、附注或 OpenPGP / SSH 签名标签，删除本地标签、推送选定标签 / 全部标签 |
| 大仓库 | 提交、侧栏与文件列表虚拟化，按文件展开 Diff；缓存图与过滤索引，历史加载上限、按条数及估算字节的 LRU 缓存 |
| 任务日志 | 按仓库 / 全部查看操作、进度、成功 / 失败 / 取消结果，重启保留、复制 JSON、清理已完成记录 |
| 配置 | 性能上限、20 项快捷键及组合键、热更新、冲突校验；仓库级作者姓名 / 邮箱，SSH agent 或凭据助手认证 |

默认快捷键：`⌘O` 打开仓库，`⌘R` 刷新，`⌘Enter` 提交，`⌘Q` 退出，`⌘,` 设置，`⌘Shift L` 任务日志，`Ctrl Tab / Ctrl Shift Tab` 切换标签，`⌘W` 关闭标签。其他平台的 `secondary` 对应 Ctrl。

### 性能、缓存与快捷键

**Repository → Preferences / Shortcuts…** 可逐项配置，Save & apply 原子保存后立即生效。历史默认最多加载 10,000 条，每次加载 300 条；搜索覆盖当前已加载历史。每仓库默认提交详情缓存最多 64 条 / 16 MiB，Diff 缓存最多 16 条 / 64 MiB，同时最多加载 4 份 Diff；按最近读取淘汰，超过预算的补丁明确报错，可调大预算后重新展开。预算估算包含文本、行映射和部分暂存数据；当前文档、仓库快照和后台临时分配另计，**不是整个进程的内存硬上限**。淘汰补丁会折叠对应文件并释放其滚动句柄。

快捷键支持 `secondary-shift-r`、`ctrl-k ctrl-o` 等最多两次按键，留空禁用，Load defaults 后需保存。重复按键、组合键前缀冲突及会抢占普通输入的单键会拒绝保存；组件输入框的编辑绑定保留。设置保存在配置目录的 `settings.json`。

### 任务日志

**Repository → Task log…** 或状态栏 **Tasks…** 查看。每条记录包含仓库、时间、操作、结果及进度；后台自动刷新不生成日志。最多保留 100 项任务、每项 128 条事件、合计保守估算 1 MiB，旧事件省略数量可见；超限优先淘汰已完成任务。Copy JSON 复制当前筛选，Clear finished 清除筛选内已结束任务，保留正在执行的任务。

开始与结束时原子保存 `tasks.json`，重启将遗留 Running 标为 Interrupted，不自动重放操作。运行中采样进度保留在内存，异常退出可能丢失最后一次保存后的事件。日志记录操作标签、进度与结果，移除 URL 的认证信息、查询参数和控制字符；不记录完整命令参数或文件正文，也不保证过滤任意错误消息中的所有敏感文本。默认配置目录 `~/.config/gitbuddy`，可通过 `GITBUDDY_CONFIG_DIR` 隔离。

### 部分暂存与取消暂存

展开工作区或暂存区中的文本文件，每个 `@@` 标题行提供 **Stage hunk / Unstage hunk**。点击红色删除行或绿色新增行可选中该行，`Shift` 连选一个范围内的变更行，`⌘ / Ctrl` 点击可添加或移除不连续的选择；点击 **Stage selected / Unstage selected** 应用选择，**Clear** 清空选择。上下文行和文件头不会被选中。

新增行与删除行独立选择：若要暂存一次完整替换，请同时选中对应的红、绿行。部分操作只更新暂存区，不改写工作区，保留其他文件与未选中行；操作完成后刷新 diff。外部编辑、HEAD 或暂存区变化导致 diff 过期时会拒绝执行，需重新选择。普通的新建、删除文件也支持部分操作；文件权限变化、重命名、二进制、符号链接、子模块、冲突及截断预览应使用整文件操作。

## 操作约定

### 创建标签

侧栏 **TAGS → +** 默认以 HEAD 为目标，也可输入提交 ID、分支、标签或 `HEAD~2` 等 revision；提交的 **Actions… → Create tag at this commit…** 直接填入所选提交。选择 **Lightweight** 创建指向提交的引用；**Annotated** 保存多行说明和标签作者；**Signed** 创建带签名的附注标签。附注和签名标签需要配置仓库作者姓名、邮箱。预览会固定目标提交并显示作者、说明及签名配置；执行前复核目标、HEAD 和配置，同名标签不会被覆盖。工作区和暂存区的已有修改保留。

签名默认读取有效 Git 配置中的 `tag.gpgsign`、`gpg.format`、`user.signingkey` 和对应的 `gpg.openpgp.program` / `gpg.program` 或 `gpg.ssh.program`，也可在表单中选择 OpenPGP / SSH、指定密钥和签名程序。程序填写可执行文件路径，不附加命令参数；SSH 密钥可使用文件路径或 `key::` 公钥，`~` 和相对路径可用。需要先安装并配置 `gpg` 或 `ssh-keygen`，由相应 agent / pinentry 解锁；应用不收集私钥或口令。当前不支持 X.509 格式，配置为不支持的格式时需明确选择可用格式。

标签对象及引用由 libgit2 写入，签名工具直接启动而不经过 Bash。签名后会验证签名，再发布引用；SSH 验证检查密码学有效性，不建立身份信任，Git 验证 SSH 标签还需要配置 allowed signers。失败、取消或目标过期不会创建标签引用，也不会静默降级为未签名标签；可能留下无引用对象，后续可由 Git 垃圾回收清理。签名任务可取消，最多等待五分钟；说明和工具输出各限 1 MiB。标签表单和确认计划是临时状态，重启不会自动重放。

### Merge / Pull 策略

工具栏 **Fetch / Pull** 和对应快捷键先打开 **Remote operations**。Fetch 可选 **Fetch all remotes**、指定 remote 的 **Fetch remote**，或 **Query branches → Fetch branch**。指定 remote 沿用其配置的 Fetch refspec 和 prune；指定分支使用唯一匹配的 `refs/remotes/…` 目标，只更新该分支，不 prune 其他分支、不自动取标签。没有 Fetch 映射的分支仍可取对象到 FETCH_HEAD，不创建 tracking ref；多重映射或会写入本地分支 / 标签的显式分支映射会拒绝。普通 Fetch 不修改 HEAD、索引或工作文件。

Pull 可通过 **Use configured upstream…** 保留原流程，也可选择任意配置的 remote，**Query branches → Pull branch…** 后选择仅快进 / merge / rebase。这是本次操作的临时来源，不保存或改写当前分支 upstream；没有 upstream 的本地分支也可以显式 Pull。执行时取本次 Fetch 实际广告并下载的提交，避免使用旧 tracking ref；分支已删除、配置 / HEAD / 工作区变化时拒绝整合。两个 remote 的同名分支、含 `/` 的 remote 名称和自定义 Fetch 映射分别处理。

**Repository → Remote tags…**，或 Remote operations 顶部 **Remote tags…**，选择 remote 后 **Query tags** 查询服务器实际引用，不下载对象或创建本地标签；列表标出 annotated 标签并显示标签对象 ID，保留 peeled 提交信息。**Delete remote tag…** 确认 endpoint 和完整标签对象 ID；执行时按这个对象 ID 校验，标签已移动 / 已删除则拒绝，需重新查询。支持轻量、附注及指向 blob 的标签，删除保留所有本地标签。Fetch / Push URL 不同的 remote 暂不允许从该查询结果删除，避免对另一台服务器误操作；服务器权限与保护规则仍然生效。查询和删除分别作为可取消仓库任务记录；发出最终远端删除后等待结果，不声称回滚服务端写入。列表使用虚拟滚动，远端广告超过 100,000 refs 时明确拒绝；查询结果仅保留于当前运行会话。

当前远端引用列表只支持 UTF-8 名称；遇到其他编码时明确报错。git2 0.21 的 RemoteHead 名称接口内部会对 UTF-8 解码 unwrap，读取时捕获其异常并转为任务错误，避免查询任务丢失；没有伪造替换名称或对部分列表执行删除。

侧栏本地 / 远程分支的 **Merge…** 打开选项页，展示固定的源提交和当前目标分支。默认 **Fast-forward or merge** 可以快进时直接移动分支，否则创建双父提交；**No fast-forward** 即使可以快进也创建双父提交，已完全合并的来源仍返回 Already up to date。**Squash** 合并到索引和工作区但不移动 HEAD，不自动提交；检查暂存内容后填写说明提交，或点击 **Use squash message** 使用建议说明。最终提交只有当前 HEAD 一个父提交，不记录来源分支为父提交。没有净改动时直接结束。

Squash 冲突沿用逐文件 / 逐块解决界面；可通过普通 Commit 提交自定义说明，也可 **Continue Squash merge…** 使用建议说明。待完成状态保存在当前工作树 Git 管理目录的 `gitbuddy-squash.json` / `SQUASH_MSG`，重启后可继续。**Abort squash…** 恢复开始前的索引和 tracked 文件，丢弃期间的 tracked 编辑，保留未跟踪文件；执行前有修改时拒绝开始。若应用过程被中断、尚未确认索引完整，禁用提交，先保存需要的编辑再 Abort / 重试。已准备提交的日志在引用发布前 / 后可恢复，避免重复提交；状态损坏时保留现场并报错。

Pull 选择来源后打开策略页，每次默认 **Fast-forward only**；分叉时拒绝移动本地分支。**Merge** 可快进则快进，否则创建双父合并提交；**Rebase** 把本地线性提交重放到本次 Fetch 的来源上，保留作者并改变本地提交 ID。仅有远程更新时直接快进；来源已包含在本地历史中时不改写。使用配置的 upstream 时只 Fetch 对应 remote；本地 upstream（remote 为 `.`）不联网。仓库、HEAD、工作区及所选远程配置变化时要求重新审查。

Merge 冲突通过现有 Continue / Abort 操作处理。Pull rebase 复用持久化 GitBuddy Rebase，冲突或重启后到 **Rebase progress…** 继续 / 中止；保留原历史备份。它要求共同祖先、线性本地范围且最多 2,000 条，不支持保留合并提交，含合并提交时改用 Pull merge。Pull Fetch 阶段可取消；开始本地合并 / 引用更新 / 重放后等待结果，此时取消不再生效。Fetch 已取得的对象及 tracking 更新不会回滚。Merge 和 Pull 都要求无未提交修改，复用 LFS 原生指针处理与本地展开，不自动下载 LFS 正文。后端旧 `Operation::Pull` 仍默认仅快进，`Operation::Merge` 仍使用默认合并策略；新界面通过带预览的策略操作执行。

### 分支管理

点击侧栏本地分支的 **Rename branch…**，输入新名字后执行。支持当前分支及非当前分支，保留提交位置、自身 upstream / 自定义配置与 Reflog，不修改索引和工作文件；当前分支的 HEAD 跟随新名称。其他 Worktree 正在使用的分支需在其所在标签中重命名。不会覆盖已有分支、遗留分支配置或 Reflog；`main → main/topic` 这类嵌套引用路径需先改为独立的中间名称。分支专属设置位于全局 / include / Worktree 配置时要求先手动迁移，避免遗漏或改写其他仓库配置。

重命名前取得源 / 目标引用、HEAD 与标准 `config.lock`，在配置副本中仅替换 section 名并通过 libgit2 解析核对所有值，保留重复值、空值、隐式布尔值、多行内容和注释。新引用 / HEAD / 配置就绪后才移除旧引用；遇到后期 I/O 失败会保留旧引用并提示检查可能存在的两个分支。配置与 refs 是不同文件，不宣称跨文件断电原子性。其他本地分支若以旧名称为本地 upstream，可通过 Set / clear upstream 更新。

点击远程跟踪分支的 **Delete branch on remote…**，确认实际服务器地址、完整远程引用和期望提交 ID 后删除。目标由 Fetch refspec 反向映射，支持含 `/` 的 remote 名称与自定义 tracking 路径；有歧义的映射、符号引用 `origin/HEAD`、默认分支及 Fetch / Push URL 不同的配置会拒绝操作。不会隐式 Fetch：远端已变化时先 Fetch 并重新复核，避免删除他人刚更新的分支。服务器保护规则仍然生效。

远程删除保留本地分支和 upstream 配置，仅清理对应的本地 tracking ref；原有 upstream 可能显示为缺失。沿用网络任务日志和取消：发出最终远端写入前可停止，之后等待结果，不承诺撤销已完成的删除。远程成功但本地 tracking 清理被锁阻止时明确提示，需 Fetch 核对；不会删除他人的锁文件。

提交的 **Actions… → Checkout this commit (detached HEAD)…**、标签的 **Checkout tag (detached HEAD)…**，以及 **Repository → Checkout commit / tag…** 支持检出任意提交 / 标签 / revision。先确认固定目标，再检查 HEAD、目标引用及仓库指纹；不移动任何本地分支，保留不冲突的暂存 / 未跟踪内容，会覆盖修改时拒绝执行，继续使用本地 LFS 展开逻辑。返回普通状态可选择本地分支 **Switch to branch**。在 detached HEAD 上创建提交后，应先新建分支保留它们再切走。

### Stash

工作区 **Stash…** 或侧栏 **STASHES → +** 打开文件选择器，默认选择全部变更，可按文件勾选；每个选定文件同时保存它的已暂存和未暂存状态，选定的未跟踪文件也会保存。成功后仅清理所选文件，未选文件的工作内容和索引条目保留。重命名包含旧、新两个路径；旧路径已被另一个文件复用时需同时选中，避免混淆。子模块内部工作区不能由父仓库 Stash 保存，需在子模块仓库标签中操作。

点击侧栏某条 Stash，预览其 **Working tree**（基础提交到保存的完整工作版本）、**Index**（基础提交到保存的暂存版本）、**Untracked files**。各部分按文件折叠，点击文件行加载 Diff，沿用并排、词级高亮与完整上下文选项；当前 Stash、展开文件和滚动位置随会话恢复。

预览的 **Actions…** 中，**Apply (keep stash)** 恢复并保留原记录，**Pop** 在恢复成功后删除选定记录。勾选 **Restore saved index** 同时恢复保存时的暂存 / 未暂存划分，不勾选时将已保存的已跟踪文件变更作为未暂存修改恢复。原来的无关暂存修改会保留；同一文件已有暂存修改则拒绝操作，需先提交或另存 Stash。全部操作由 `git2` / `libgit2` 实现，按固定对象 ID 定位；仓库在对话框打开后变化时需重新打开复核。

**Create branch from Stash…** 要求当前工作区与索引干净，使用 Stash 的基础提交创建并切换到新分支，强制恢复保存的索引，成功后删除该 Stash。原来的分支不移动。恢复冲突、文件占用或 LFS 对象缺失时保留 Stash；建分支后的恢复失败可能留下已创建的分支，应先检查现场。libgit2 可能在后续失败前恢复部分未跟踪文件，错误会提醒复核；不会自动删除这些文件或在失败后自动 Pop。

### Diff 显示

展开文件后，点击 **Unified / Side by side** 切换统一 / 并排视图，**Word highlight** 切换词级高亮，**Context: 3 lines / Full context** 切换上下文。并排视图左侧为旧版本 / index 基准，右侧为新版本；两侧同步滚动，长行可通过底部滚动条浏览。工作区、暂存区、提交历史和两版本比较使用相同显示选项，各仓库独立保存并在重启后恢复。

并排显示仍支持 hunk / 行暂存和取消暂存，左右变更行分别对应原始补丁中的删除 / 新增行。切换布局或词级高亮保留选择；切换上下文会重新加载补丁并清空行选择。完整上下文显示整个文件，超过 100,000 补丁行或 16 MB 时明确提示，需切回紧凑上下文。词级比较在后台预计算；长行和计算量过大的补丁退回整行高亮。

### 远程管理、upstream 与推送选项

- **远程管理**：点击侧栏 REMOTES 的 **…** / 远程名称，或 **Repository → Manage remotes…**。可添加远程、编辑 Fetch URL 和单独的 Push URL、重命名、确认后删除。Push URL 留空表示使用 Fetch URL；删除移除本地配置和远程跟踪引用，保留本地分支及远程服务器内容。重命名同步标准跟踪引用和分支配置，特殊 refspec 无法自动重写时显示提示。
- **Upstream**：点击本地分支，选择 **Set / clear upstream…**。支持已经 Fetch 的远程分支及本地分支，也可清除上游；本地与远程同名时按完整引用区分。修改上游不会切换分支或更新工作文件。
- **选择推送目标**：工具栏 **Push** 保留当前分支的快捷推送；旁边 **▾** 或 **Repository → Push options / Tags…** 打开选项。选择 remote、任一本地源分支，填写远程目标分支名；可以推送到不同名或尚不存在的分支，并选择成功后设置 upstream。点击 **Review push…** 核对引用、提交 ID 和目标，确认后执行。
- **Force-with-lease**：只对选定分支生效。以本地已获取的目标 tracking ref 为期望值，按该 remote 的 Fetch refspec 映射，不隐式 Fetch；Review 后固定期望 ID。目标在远端变化则拒绝覆盖；本地尚无 tracking ref 时仅允许创建远端不存在的目标。SSH / HTTP(S) 在 libgit2 推送协商阶段校验，再由服务端校验更新；本地 bare 远程锁定引用直到最终更新。为了避免拿错服务器的 tracking ID，Push URL 与 Fetch URL 不同时拒绝 lease。操作不修改本地源分支。
- **标签推送**：标签菜单的 **Push this tag…** 直接进入只推标签模式；也可在推送选项选择多个标签 / Select all，单独推送或与分支一起推送。保留附注标签对象，已有不同目标的远端标签拒绝替换，即使分支勾选了 force-with-lease。多引用推送可能部分成功，错误会报告被拒绝引用，不能假定整批回滚。

推送选项沿用各仓库独立后台任务和取消机制。确认前固定源对象、远程地址及 Fetch refspec；执行前若源引用或远程配置已改变，需重新打开操作。设置 upstream 的目标必须匹配唯一 Fetch refspec；特殊映射需先整理 Git 配置。选项对话框本身不作为会话草稿保存。

### 网络取消与仓库任务

每个仓库一次执行一个后台 Git 任务，不同仓库可同时执行。操作期间可切换标签、打开／克隆其他仓库和关闭没有运行任务的标签；标签显示运行中及后台完成／错误标记，悬停可查看该仓库的状态。进度、结果、提交草稿和错误都归属发起操作的仓库，后台完成不会切换当前标签。运行任务的标签需等任务结束后才能关闭。

正常退出或关闭窗口时，先停止接受新任务并请求取消所有仓库的网络任务；LFS 会停止传输进程及其 Unix 子进程组。已进入最终写入 / 上传阶段的任务，以及本地 Git 写操作，等待完成后才退出并保存会话。原生退出回调同样等待后台工作结束；DNS、连接或凭据助手阻塞时仍需等待调用返回，强制终止不属于这一保证。

Fetch、Pull、Push、Clone 在状态栏提供 **Cancel**。点击后显示 **Cancelling…**，等 libgit2 确认停止后才解除该仓库的操作锁；网络连接、DNS 或凭据助手阻塞时，可能需要等当前调用返回或超时。取消是协作式停止，不会回滚已接收的对象或已经更新的远程跟踪引用。

- **Fetch / Pull**：传输阶段可取消；Pull 按分支配置解析完整 remote 名（支持 `team/origin`）和上游引用，支持本地上游 `.`；远程跟踪引用缺失时先 Fetch。开始更新 HEAD、暂存区和工作区前进入 **Finishing…**，此后禁用取消并报告最终结果。
- **Push**：可在连接／协商阶段取消；SSH / HTTP(S) 开始打包上传前进入 **Finishing…**，此后等待远端确认。本地带 lease 推送可取消对象传输，在更新锁定的目标引用前进入 **Finishing…**。不承诺撤销已经完成的推送。
- **Clone**：先克隆到目标父目录内的临时目录，传输和临时工作区 checkout 可取消；最终安装前禁用取消。失败／取消清理本次创建的临时内容，不递归删除目标目录中的文件；目标必须不存在或为空，原有空目录在取消后保留。失败／取消标签提供 **Retry**，也可关闭后重新选择地址。

### 冲突处理

从工作区的 **Resolve conflicts…**、冲突文件行的 **Resolve…** 或 **Repository → Resolve conflicts…** 打开。左侧列出未解决文件，右侧并排显示索引中的 **Ours / Base / Theirs**，下方编辑最终结果。点击 **Use ours / Use theirs** 采用完整版本，**Use working file** 保留当前加载的工作文件；某一侧不存在时可选择删除。Ours / Theirs 指 Git 索引的当前侧 / 传入侧，在 rebase 等操作中不能简单等同于本地 / 远程分支。

**Save & mark resolved** 同时写入工作文件和暂存区，并移除该文件的索引冲突；删除需要确认，其他文件的已暂存内容保留。残留的冲突标记会阻止手工结果和工作文件保存，冲突未全部解决时禁用批量暂存 / 取消暂存。文本草稿随文件和仓库标签保存到会话，重启后继续编辑。若冲突内容或操作状态已变化，保留草稿文本供复核，完整版本选择需重新确认。

结果编辑区上方列出剩余冲突块。每个块可独立选择 **Ours / Theirs / Both**；Both 按 ours、theirs 顺序保留两侧内容，仅替换该块，保留其他块和无冲突编辑。支持标准 / diff3 标记、不同标记长度和 CRLF。选择后仍为未保存草稿；处理完所有块后复核结果，再保存并标记解决。手工编辑产生不完整或嵌套标记时，先修正文本或重新加载文件。

**External tool…** 配置可执行文件和 JSON 参数数组，提供 VS Code / KDiff3 预设。参数中独立的 `$BASE`、`$LOCAL`、`$REMOTE`、`$MERGED` 分别替换为临时祖先、ours、theirs 和当前结果草稿文件，必须包含 `$MERGED`；可指定已安装工具的绝对路径。工具应等待编辑完成后才退出（VS Code 预设包含 `--wait`）。参数规则参见 [VS Code CLI](https://code.visualstudio.com/docs/configure/command-line) 和 [KDiff3 文档](https://docs.kde.org/trunk_kf6/en/kdiff3/kdiff3/documentation.html)。

外部工具通过进程参数直接启动，不执行 shell 或仓库中的 `mergetool.cmd`。它编辑临时副本，退出后仅导入 UTF-8 文本草稿；不会自动写工作文件、暂存或提交。运行时可切换仓库，状态栏 **Cancel** 停止工具及 Unix 子进程组。失败或工作文件 / index / HEAD / 操作状态发生变化时拒绝导入，并显示保留结果的恢复路径；草稿在工具运行期间变化时也拒绝覆盖。结果仍需单独审查和保存。

外部修改工作文件、冲突索引、HEAD 或操作状态后，过期的保存请求会拒绝执行。**Refresh list** 刷新文件列表，**Reload file** 重新读取三方内容和工作文件；重新加载保留手工文本草稿供复核，完整版本选择则需要重新选择。也可以使用外部编辑器修改文件，再重新加载并选 **Use working file**。

全部解决后可先 **Review working tree** 查看暂存内容，再 **Continue Merge / Cherry-pick / Revert…**。继续操作使用全部已暂存内容和操作原有提交说明，merge 保留两个父提交，cherry-pick 保留原作者；确认后再次校验暂存区和 HEAD。**Abort…** 会恢复 tracked 文件到 HEAD，丢弃操作期间保存的解决结果及其他 tracked 编辑。

内置结果编辑和集成外部工具仅支持不超过 2 MB 的 UTF-8 常规文本；外部工具要求每个存在的三方版本都可读取为文本，三方预览最多显示 20,000 行。二进制、非 UTF-8 和大文件可选择完整版本或使用独立外部编辑器修改后的工作文件，按原始字节保存。重命名、符号链接和子模块冲突需在客户端外处理。GitBuddy 发起的 Rebase 可在内置界面继续 / 中止；外部工具发起的 Rebase 仍需由原工具继续 / 中止。暂不提供三方同步滚动。

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

### 历史选择、建分支与 Reset

提交列表支持普通点击单选、`Shift` 按当前可见顺序连选、`⌘ / Ctrl` 点击增减选择，也可用行尾勾选按钮。列表上方 **N selected…** 可复制 IDs、比较两个提交（列表中较后者作为 base）、建立 Squash / Drop / Edit / Split 批量计划。多选集合与范围锚点按仓库标签保存并随重启恢复；搜索隐藏的选择仍保留，已加载历史中缺少所选提交时禁用批量操作，可 Load more 或清空重选。

选中提交后，详情的 **Actions… → Create branch from this commit…** 在该确切提交创建新分支，不切换当前分支或改动文件。**Reset current branch to this commit…** 展示固定目标和三种模式：Soft 仅移动 HEAD；Mixed 同时重置暂存区但保留工作文件；Hard 同时重置暂存区及 tracked 文件。Hard 可能覆盖阻挡 checkout 的未跟踪文件，其余未跟踪文件保留。确认前后的 HEAD、引用、暂存区或工作文件变化会拒绝过期操作。原提交保留于备份引用、ORIG_HEAD 和 Reflog；备份不包含被丢弃的未提交编辑。Reset 支持本地分支或 detached HEAD，进行中的仓库操作会阻止 Reset。

### 编辑／拆分旧提交

从提交的 **Actions… → Edit this commit… / Split this commit…** 建立包含该提交及后续提交的计划；多选菜单可设置多个暂停点。批量 Squash 要求当前分支里相邻的至少两条提交，其余操作允许间隔选择；未选中的后续提交仍以 Pick 重放。计划展示后再确认执行。只改消息可使用计划的 Message… / Reword。

**Edit** 应用旧提交后保留已暂存内容并暂停。可用外部编辑器修改工作文件，重新暂存并在正常提交框填写替代说明；**Split** 则把旧提交的修改全部退回未暂存状态，保留工作文件。逐次按文件、hunk 或选中行暂存并 Commit，生成一个或多个替代提交；每条保留原作者并更新提交者。进度页提供 **Use original message**，也可填写新说明。普通提交按钮在这个暂停点调用专用 sequencer 操作，外部 Rebase 仍不能接管。

至少创建一条含暂存变更的替代提交，并提交完所有 staged / unstaged / untracked 修改后，点击工作区 **Rebase progress… → Continue rebase…** 重放后续提交或进入下一个暂停点。重启后暂停状态、替代提交数及普通草稿保留；Abort 恢复原分支与 tracked 文件，保留未跟踪文件。根提交、合并范围、其他分支的非祖先提交暂不支持编辑／拆分；空提交使用 Reword / Drop。这类计划明确报错，任意提交创建分支不受此限制。若在切换到编辑暂停点的 index / 状态保存期间意外中断，不能验证现场时会拒绝 Continue，应保存编辑后 Abort 并重建计划。

### 交互式 Rebase 与 Squash

打开 **Repository → Interactive rebase / Squash…**，输入基准版本（例如 `HEAD~3` 编辑最近三次提交，或输入上游分支将当前分支的独有提交重放到该版本）。计划从旧到新列出提交，可调整顺序、编辑说明，并选择 **Pick / Reword / Squash / Fixup / Drop**。**Squash all into one** 将第一条保留，其余合并；Squash 拼接说明，Fixup 保留前一组的说明。第一条保留的提交不能选 Squash / Fixup。开始前要求工作区及暂存区干净，再次校验分支、HEAD 和计划。

发生冲突时转到工作区，进入已有冲突处理页；解决并暂存所有文件后，选择 **Continue rebase…**。也可再次打开管理页查看进度并继续或中止。进度保存在该工作树的 Git 管理目录，正常冲突暂停可在重启后继续；原分支直到计划完成才更新，原历史保留在 `refs/gitbuddy/rewrites/` 和 Reflog 中。中止恢复原分支及 tracked 文件，保留未跟踪文件。未提交的计划编辑属于临时对话框内容，不随会话保存。

第二个输入框可指定 **--onto** 目标；此时第一个版本是重放范围的排他边界，必须是当前分支祖先。例如边界 `old-base`、目标 `new-base` 只重放 `old-base..HEAD` 的线性提交。留空保留原共同祖先模式。**Autosquash** 将 `fixup! <subject / ID>` / `squash! <subject / ID>` 放在唯一匹配的较早提交之后，目标缺失或有歧义时拒绝修改计划。**Break** 重放该提交后暂停，可重启后在干净工作区继续。

补丁应用期间中断且尚未持久化“应用完成”确认时，Continue 保留现场并拒绝创建提交。进度页的 **Retry current step… / Skip current step…** 在确认后，先将工作文件（含未跟踪内容及符号链接本身）、索引和恢复日志同步保存到该工作树 Git 管理目录的 `gitbuddy-rebase-recovery/<步骤-时间>/`，再重置到前一个重放提交并重试或跳过；不复制 `.git` 行政目录或跟随符号链接。备份失败不执行重置，恢复中再次中断仍需显式重试，不自动信任索引。跳过后紧邻的 Squash / Fixup 转为 Pick，避免误改写基准或其他提交组。现场副本不会自动清理，可手动取回文件后重新暂存。升级前缺少应用确认的日志使用相同保护。存在 `index.lock` 时不会自动删除锁，需先确认没有其他 Git 进程。外部工具发起的 Rebase 由原工具继续 / 中止。

这是 GitBuddy 通过 libgit2 实现的提交序列，必须使用 GitBuddy 继续 / 中止，不能用外部 `git rebase --continue` 接管。当前最多 2,000 条，只支持本地分支的线性区间，不支持根提交重写或保留合并；显式 --onto 目标可独立于范围边界。不自动 force push，也不自动签名。

### Worktree 与子模块

**Repository → Worktrees…** 列出关联工作树（主工作区不在列表中），可创建、打开为独立标签、锁定 / 解锁或移除。创建时从当前 HEAD 新建简单名称的分支，目标使用尚不存在的绝对路径；暂不支持复用已有分支。移除前再次确认名称、路径、HEAD、锁定状态和干净状态，并递归检查子模块；子模块的 `ignore=all` 等显示策略不会掩盖未提交编辑、未跟踪文件或 gitlink 变化。拒绝移除当前工作树，移除后保留分支。目录已缺失时可清理登记。切换、删除和改写分支时检查其他工作树是否占用该分支。

**Repository → Submodules…** 支持添加 URL 与相对路径、初始化 / 更新、递归更新、同步 `.gitmodules` URL、暂存当前子仓库提交，以及打开为独立标签。更新到父仓库暂存区记录的 gitlink，不自动追踪远程最新分支；拒绝覆盖子仓库的未提交编辑。新增子模块要求父工作区干净，成功后 `.gitmodules` 与 gitlink 进入暂存区；克隆失败可能保留登记，可通过更新重试。递归更新最多 32 层，网络阶段支持取消，已完成的子模块不会回滚。

子模块列表递归展示已初始化模块及其直接子模块；未初始化的更深层级在更新后出现。嵌套行的 Stage 写入实际直接父仓库的索引，需逐层提交 / 暂存。Sync URLs 递归同步已初始化模块。更新前先检查所选范围内已存在的后代，即使 `ignore=all` 也拒绝覆盖编辑；拒绝路径穿越、符号链接路径和指向其他检出位置的 Git 元数据。

创建 Worktree 后展开共享缓存中已有的 LFS 对象，管理页 **Prepare submodules / LFS** 递归初始化其子模块。新 Worktree 的子模块使用独立嵌入式 `.git` 检出，避免共享子模块 HEAD / index；已有指向其他工作区的元数据会拒绝操作。子模块添加 / 更新后也展开自身缓存，缺失对象明确提示，需进入该子模块标签 Fetch LFS，不自动下载所有正文。父子仓库的后台任务不能同时写入，关联 Worktree 的 GitBuddy 写操作使用共同锁；无关联仓库仍独立运行。锁只约束 GitBuddy，不能代替与外部 Git 工具的协调。

### Git LFS

**Repository → Git LFS…** 查看根 `.gitattributes` 中的 LFS 规则、暂存区指针及缓存状态。**Track pattern… / Untrack pattern…** 修改本客户端生成的规则，例如 `*.bin`，修改仍需暂存及提交；不会迁移既有历史。由于当前 libgit2 对属性模式的支持限制，规则不接受空白、引号或反斜杠；可以用 `*.bin` 匹配含空格的文件名。已有带引号的属性规则会明确拒绝暂存，包括通过属性宏间接设置 LFS 的规则；工作区、索引回退、info 和配置的全局属性文件使用相同保护，需先改为无引号的 glob。

整文件暂存原生计算 SHA-256、缓存对象并写入标准 LFS 指针；LFS 冲突编辑、采用工作文件及选择完整版本也通过相同 clean 逻辑，提交保存指针，正文保留在工作区或本地对象缓存。LFS 不支持 hunk / 行暂存；新的选定文件 Stash 使用相同 clean 逻辑，可分别保存 LFS 文件的暂存版本和未暂存正文，不要求提前暂存。切换版本、丢弃、Rebase 和 Stash 恢复会展开本地对象，写入前校验哈希及大小；缺失对象时保留指针，不覆盖用户编辑，可随后下载对象。使用默认共享 LFS 对象目录，不支持自定义 `lfs.storage` 或扩展指针。

本地 Track、暂存和 **Checkout local objects** 不需要 `git-lfs`。远程 Fetch / Push 需要安装官方 `git-lfs` 及其 Git 运行环境；选择 remote 后下载 / 上传 HEAD 所需的 LFS 对象，Fetch 成功后展开本地文件。普通 Push 先上传待推送历史涉及的 LFS 对象，上传失败则不更新远程 Git 引用。普通 Clone / Fetch 不自动下载 LFS 对象。LFS 传输提供取消，停止进程及其 Unix 子进程组，但已上传 / 下载的对象不会回滚。

**Locks <remote>…** 查询服务器核验过的 yours / others 列表，可为现有 LFS 文件创建锁、按稳定 ID 解锁，强制解锁他人的锁需要单独确认。执行前重新核验路径、创建时间和归属；服务器不支持锁、认证失败、返回不完整或重复 ID 时拒绝操作。已配置的 `lfs.url` 会覆盖 remote 的端点选择，详见[官方锁命令说明](https://github.com/git-lfs/git-lfs/blob/main/docs/man/git-lfs-locks.adoc)。锁查询 / 写入沿用可取消任务；取消可能发生在服务器写入之后，需刷新核对，客户端不声称回滚远端锁。

**Verify / Prune preview…** 不依赖 `git-lfs`，后台计算默认缓存中标准对象的 SHA-256；损坏对象保留并报告。保护所有引用、Reflog（含遗留日志）、GitBuddy 历史备份、索引及冲突阶段和所有关联 Worktree 的对象；历史不可读、失效 Worktree 或自定义存储时拒绝清理。预览固定保护集合和缓存内容指纹，执行时重新计算，拒绝过期结果。**Quarantine unreferenced objects…** 将未引用对象移到共享 LFS 目录的 `gitbuddy-quarantine/<时间>/`，不释放空间；**Restore quarantine** 校验后移回缓存。刷新预览后，**Delete quarantine…** 再次确认并永久删除仍未被引用的隔离对象，释放正文文件占用的空间；损坏或重新被引用的对象不会删除。清理 / 永久删除要求所有 Worktree 无修改且没有未完成操作，失败可能只完成部分对象，并提示剩余目录。对象校验可取消，最终移动 / 删除开始后等待完成。

### 通用约定

- 普通仓库操作由 Rust `git2` 在后台调用 `libgit2` 完成；标签签名直接启动 `gpg` / `ssh-keygen`，LFS 远程传输与锁管理直接启动官方 `git-lfs`（它自身依赖 Git），不经过 Bash。单文件操作按字面路径匹配，支持空格、换行、通配字符及 Unix 非 UTF-8 文件名；LFS 路径与规则的限制见上文。
- 丢弃操作只恢复工作区到暂存区版本，不清除已暂存内容；不提供批量删除未跟踪文件的快捷操作。
- 删除 stash、丢弃修改、合并、revert 等操作需要在应用内确认。Stash 应用及删除按对象 ID 定位，在引用锁内读取当前位置；外部新增 Stash 不会改变已选目标，目标已消失或不唯一时拒绝操作。分支合并菜单使用完整本地 / 远程引用，同名简写有歧义时拒绝合并。删除分支前检查分支是否已合入当前 HEAD，推送不使用 force。
- 远程操作通过 `libgit2` 使用 SSH agent / Git credential helper。现有仓库的 Fetch、Pull、Push 和子模块操作使用有效仓库配置（包含全局、仓库及 include 配置）；URL 级凭据助手、用户名及 `useHttpPath` 由 git2 解析。首次 Clone 使用全局默认配置。GUI 不提供终端密码输入，首次 SSH 信任及凭据准备应先完成；错误可在状态栏复制。
- 合并、cherry-pick 和 revert 要求操作前工作区及暂存区无改动，避免中止操作时覆盖已有修改。Cherry-pick 无论直接完成还是冲突后继续，均保留原作者、邮箱和作者时间，更新当前提交者；Revert 使用当前作者。普通 Commit 的按钮、快捷键及后端均拒绝没有暂存内容的空提交，合并完成允许不改变文件树的双父提交。
- 合并冲突显示在工作区，可进入内置冲突处理界面；Cherry-pick/revert 的继续和中止也保留在提交的 Actions 菜单。
- 最近仓库记录保存在 `~/.config/gitbuddy/settings.json`，复用会话路径编码，支持 Unix 非 UTF-8 路径及旧版字符串列表。先原子保存成功再更新内存列表，保存失败在状态栏显示。

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

Rebase 当前只支持本地分支线性历史，不支持根提交重写或保留合并提交。LFS 当前支持默认对象目录和标准 SHA-256 指针，不支持扩展指针、自定义 `lfs.storage` 或历史迁移。提交图依据已加载提交的父子关系绘制；图宽按所有 lane 及连线端点计算，多分支时保留文字区域并支持横向滚动。搜索过滤或分页边界外的提交不会显示连线，内部历史备份引用不作为普通分支显示。`libgit2` 创建提交时不会执行用户的 Git hooks 或自动进行 GPG 签名。

紧凑差异预览最多 20,000 行，完整上下文最多 100,000 补丁行 / 16 MB；未跟踪文件超过 2 MB 不加载正文。网络传输进度每 100ms 节流，阶段切换即时显示；取消边界见上文。真实外部服务器认证尚未验收。当前在 macOS 验证，Windows/Linux 尚未验收。生成的 .app 用于本地运行，未做发行签名或公证。

## 代码结构

- `src/git.rs`：仓库数据类型、diff 行号处理与共享预览上限（`diff_preview`）、进度通道类型。
- `src/git/libgit.rs`：基于 `git2` / `libgit2` 的仓库快照（含变化指纹）、差异、Git 操作与网络进度回调。
- `src/git/network.rs`：线程安全取消令牌、取消／最终写入阶段互斥、进度节流与取消结果。
- `src/git/partial.rs`：结构化 hunk / 行选择、过期校验与暂存区内容重建，保留原始字节及换行。
- `src/git/diff_view.rs`：并排行映射、词级高亮预计算与完整上下文限制。
- `src/git/recovery.rs`：Amend、保留暂存区的提交撤销、HEAD Reflog 与恢复；使用引用锁和快照校验防止操作落到已切换的分支。
- `src/git/history.rs`、`src/ui/history_actions.rs`：明确 Reset 模式、任意提交建分支、多选历史和旧提交改写入口。
- `src/git/inspect.rs`：固定版本的文件浏览、跨父提交跟随重命名的历史、Blame、两棵树比较与精确文件 patch。
- `src/git/conflicts.rs`：索引三方内容、结果保存、引用 / 索引锁和过期校验、继续冲突操作。
- `src/git/conflict_blocks.rs`、`src/git/merge_tool.rs`：逐块选择、直接参数启动外部工具、临时结果与取消 / 过期保护。
- `src/git/rebase.rs`：基于 libgit2 的可编辑提交序列、引用事务、持久化进度、冲突继续与中止。
- `src/git/integration.rs`、`src/ui/integration.rs`：Merge / Pull 策略、upstream 预检、squash 单父提交与持久化恢复、异步选项页。
- `src/git/remote_refs.rs`、`src/ui/remote_ops.rs`：远端引用广告、临时 Fetch / Pull 来源、标签对象 lease 删除及查询结果按标签归属，虚拟化引用列表。
- `src/git/lfs_maintenance.rs`：LFS 锁核验、全历史 / Worktree 对象保护、缓存校验与隔离清理。
- `src/git/workspaces.rs`：Worktree 与子模块管理、跨工作树分支占用检查。
- `src/git/lfs.rs`：原生指针暂存 / 本地展开、对象校验，以及可取消的官方 git-lfs 传输进程。
- `src/ui.rs`：状态机核心——多仓库标签（`tabs` + `active` 两段式）与异步代际控制。
- `src/ui/tasks.rs`：按稳定仓库标签 ID 分发后台任务、结果与进度归属、取消和失败后重试。
- `src/ui/graph.rs`：提交分支及合并线布局。
- `src/ui/patches.rs`：按文件懒加载的 diff 面板。
- `src/ui/history.rs`、`src/ui/sidebar.rs`、`src/ui/toolbar.rs`、`src/ui/detail.rs`、`src/ui/modals.rs`：对应界面区域。
- `src/ui/recovery.rs`：异步加载历史操作对话框、独立 Amend 编辑器与 Reflog 恢复列表。
- `src/ui/inspect.rs`：文件选择器、文件历史、Blame 虚拟列表和版本比较页面，后台加载及过期结果隔离。
- `src/ui/conflicts.rs`：三方预览、独立文件草稿、结果编辑、删除确认与继续 / 中止入口。
- `src/ui/management.rs`：Rebase 计划编辑与进度、Worktree / 子模块 / LFS 管理对话框。
- `src/git/stash.rs`、`src/ui/stash.rs`：分组预览、选定文件保存、恢复索引、Pop 与建分支；保护无关暂存内容和固定 Stash 对象身份。
- `src/settings.rs`：最近仓库和外部合并工具配置（损坏时备份为 `.json.broken` 并重建）。
- `src/session.rs`：带版本的会话格式、路径编码、原子保存与旧快照保护。
- `src/ui/session.rs`：按仓库捕获 / 恢复界面状态、独立滚动句柄及退出保存。
- `tests/git_workflows.rs`：真实 Git 仓库的集成测试。
- `src/git/remotes.rs`、`src/ui/remotes.rs`：远程配置、完整引用 upstream、固定推送计划及管理面板。
- `src/git/branches.rs`、`src/ui/branches.rs`：固定目标的分支重命名 / detached checkout，以及带标签身份校验的后台操作预览；远程分支删除复用带 lease 的推送计划。
- `tests/remote_management.rs`：远程配置、推送目标、lease 并发保护与标签推送的 libgit2 隔离回归；`examples/demo_remotes.rs` 提供本地演示仓库。
- `tests/merge_pull_strategies.rs`、`examples/demo_merge_pull.rs`：no-ff / squash、Pull 三种策略、冲突和重启恢复、取消 / 过期保护及 LFS 回归，独立多仓库演示会话。
- `tests/remote_operations.rs`、`examples/demo_remote_operations.rs`：选择来源、自定义 / 无 Fetch 映射、远端标签对象 / 删除竞争和取消的隔离回归与双远程演示仓库；网络测试只使用本地 Git daemon 服务端。
- `tests/p1_regressions.rs`、`tests/p2_regressions.rs`：审查问题的隔离仓库回归，P2 含本地 Basic-auth HTTP 服务验证凭据助手、Push 和子模块克隆；HTTP 夹具通过系统 Git 提供协议，客户端仍使用 libgit2。
- `tests/partial_staging.rs`：部分暂存 / 取消暂存的隔离仓库回归测试，夹具和受测操作均使用 libgit2。
- `tests/history_recovery.rs`：历史改写、首次 / 合并 / 分离 HEAD 提交、过期选择、冲突与恢复的 libgit2 隔离仓库测试。
- `tests/history_operations.rs`、`examples/demo_history.rs`：Reset 模式与过期保护、编辑 / hunk 拆分 / 重启继续、LFS 和冲突恢复回归及演示仓库。
- `tests/repository_inspection.rs`：文件历史 / Blame / 比较的隔离仓库测试，包含合并、重命名后旧名复用、删除后重建、特殊路径和版本固定。
- `tests/conflict_resolution.rs`：三方版本、删除 / 二进制 / 可执行文件、过期请求、索引锁、多文件独立解决和继续操作的 libgit2 隔离仓库测试。
- `tests/diff_conflict_tools.rs`、`examples/demo_diff.rs`：完整上下文 / 部分暂存、词级高亮、块选择、外部工具保护的隔离回归与多仓库演示。
- `tests/advanced_git.rs`：提交重写、跨工作树保护、子模块及 LFS 指针 / 缓存 / Stash 的隔离仓库回归测试。
- `tests/stash_management.rs`、`examples/demo_stash.rs`：Stash 操作、冲突与数据保留、LFS、关联工作树及会话回归；提供独立演示仓库与配置。
- `examples/demo_repo.rs`：可重复生成的隔离演示仓库。
- `examples/demo_conflicts.rs`：包含文本、二进制、修改 / 删除冲突的隔离演示仓库。
- `examples/demo_advanced.rs`：包含 Rebase / Squash、Worktree、子模块与 LFS 对象的隔离演示仓库。

## 性能设计

- 每 10 秒的自动刷新先计算指纹（HEAD、引用、有效 Git 配置、状态列表、暂存区 blob ID 与模式、变更文件的大小及修改时间；Unix 另含 ctime / inode），与上次快照一致则跳过全量重建。已修改文件再次编辑或暂存区内容更新也会触发刷新，无需每次遍历提交历史；普通操作错误不会停用后续自动刷新。已展开的 LFS 文件可能需要流式计算 SHA-256 以判断是否修改，不把大文件正文一次性加载到内存。
- 切换仓库标签是对 `RepoTab` 结构体的整体交换（`mem::take`），不再逐字段拷贝 15 个状态。
