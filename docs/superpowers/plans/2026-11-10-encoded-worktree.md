# Encoded Worktree Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 编码文件在补丁导入之后，仍能通过普通暂存、Diff 和检出保存正确的规范内容与工作字节。

**Architecture:** 将已验证的补丁转换提取为共享 Snapshot，按 Add / PatchApply 分离策略。独立工作树事务读取目标树属性，预生成输出并固定恢复字节；原业务只在事务成功后发布引用。暂存 / 状态 / Diff 与目标属性 / 检出分组实现，再由根代理接入所有写点并独立复审。

**Tech Stack:** Rust 2024、git2 0.21 / libgit2 1.9.7、gix-attributes / gix-filter、已有严格 Unicode codec / Unix iconv、GPUI Kit 0.6.6。

**Spec:** [编码文件的完整工作流](../specs/2026-11-10-encoded-worktree.md)

## Global Constraints

- 生产实现不调用 system Git / Bash；隔离测试可使用系统 Git oracle。
- `Limits::PATCH` 与 `Limits::ENCODED` 为 16 MiB（16,777,216 字节）；普通无转换文件保持现有大文件快路径，不承受补丁整批预算。
- 使用 `PathBuf` 保留字面路径。未知编码和自定义过滤器必须明确报错；不能有损替换或截断。
- 任何转换失败、属性 / 配置变化或未知外部编辑都保留原始内容；恢复不能重跑过滤器。
- 按已授权的当前并行执行方式持续实现；用户明确不等待新的继续确认。每组验证后独立审查；本轮不提交或推送。
- Windows / Linux 只准备代码和验收入口，用户暂不开放机器；Apple 签名 / 公证仍待身份。
- 原生 GUI 验收由根代理统一执行，最多一个测试实例；验收后立即关闭。用户已明确指出多个测试窗口造成干扰。

## Review Focus

- 分支同时改变 `.gitattributes` 与文件内容：必须使用目标规则，不能沿用旧编码（Task 2 / 3）。
- 暂存区与工作区包含同一路径的不同编辑：部分操作只改变选中部分（Task 1 / 4）。
- 非法码元、不能表示的字符、BOM 变化：失败不得移动引用或覆盖工作文件（Task 1 / 3）。
- symlink / hardlink、未跟踪目标碰撞、外部编辑：不跟随目标，不覆盖无关字节（Task 3）。
- 检出中断以及恢复时规则已变：按 journal 中固定的 raw 内容处理，未知状态拒绝（Task 3 / 5）。

---

### Task 1: 共享转换与 Stage / Status / Diff

**Files:**
- Create: `src/git/conversion.rs`, `tests/encoding_worktree.rs`
- Modify: `src/git/patch_conversion.rs`, `src/git/patch_encoding.rs`, `src/git/patch_batch.rs`
- Modify only Stage / clean hooks: `src/git/lfs.rs`
- Modify only file status / workdir diff functions: `src/git/libgit.rs`
- Modify: `src/git/partial.rs`, `src/git/patch_file.rs`

**Interfaces:**
- Produces `conversion::{Snapshot, CleanPolicy::{Add,PatchApply}, Limits { max_bytes: usize }}`；`Limits::{PATCH,ENCODED}`。
- `Snapshot::capture(repo: &Raw, paths: &BTreeSet<PathBuf>, source: lfs::AttributeLookup) -> Result<Snapshot>`。
- `.clean(repo: &Raw, path: &Path, raw: &[u8], indexed: &[u8], policy: CleanPolicy, limits: Limits) -> Result<Vec<u8>>`。
- `.smudge(repo: &Raw, path: &Path, canonical: &[u8], oid: Oid, limits: Limits) -> Result<Vec<u8>>`。
- `.validate(repo: &Raw) -> Result<()>`；`.validate_path(repo: &Raw, path: &Path) -> Result<()>`；`.has_encoding(path: &Path) -> bool`；`.has_lfs(path: &Path) -> bool`。Snapshot 实现 Clone / Eq。
- Root 在 `src/git.rs` 注册 `mod conversion;`；旧 patch wrapper 保持现有公开接口。

- [ ] 写 `utf16_import_then_stage_stores_utf8`：工作树保留 UTF-16LE，Stage 后 blob 等于期望 UTF-8，状态 clean；再次编辑显示 modified。
- [ ] 运行 `cargo test --locked --test encoding_worktree --quiet`，确认当前 raw Stage 行为失败。
- [ ] 提取共享 Snapshot；Add 应用 safecrlf / roundtrip，PatchApply 保持已验证的纯 apply 规则；对 implicit 配置使用 has_value 三态。
- [ ] 接 Stage、状态、单文件及整份 unstaged diff / 导出；partial 绑定同一转换指纹，未接入处先拒绝 encoded raw 写入。
- [ ] 覆盖 `invalid_encoding_preserves_index_and_worktree`、`encoded_partial_keeps_other_staged_changes`、`encoded_diff_matches_git`，并运行 patch / partial / implicit_config 回归。
- [ ] 交接可审查结果与实际命令输出，不创建提交。

### Task 2: 目标树属性

**Files:**
- Modify: `src/git/lfs/attributes.rs`
- Test: `tests/lfs_attributes.rs` 或新 `tests/encoding_checkout.rs`

**Interfaces:**
- Produces `lfs::AttributeLookup::TargetTree(Oid)`；commit / tree 输入均解析并固定为 tree。
- 复用既有 query 的 owned states + provenance fingerprint，Snapshot 不借用临时 index。

- [ ] 写 `target_attributes_do_not_use_old_worktree_encoding`：当前 UTF-16LE，目标树 UTF-8 / UTF-16BE，属性查询与 Git checkout oracle 一致。
- [ ] 运行该测试，确认旧 lookup 不支持目标来源的失败。
- [ ] 实现目标树规则；明确未被目标树包含的属性 fallback，以真实 Git 对照固定行为；保留 info / global / system 优先级。
- [ ] 覆盖嵌套规则、宏、非 UTF-8 路径及目标引用移动，运行 `cargo test --locked --test lfs_attributes --test encoding_checkout --quiet`。
- [ ] 向 Task 1 / 3 交接接口与规则，不修改共享 conversion 或业务 dispatch。

### Task 3: 可恢复检出事务

**Files:**
- Create: `src/git/worktree_conversion.rs`, `tests/encoding_checkout.rs`
- Root registers module in `src/git.rs`。

**Interfaces:**
- Consumes Task 1 Snapshot / Limits 与 Task 2 TargetTree。
- Produces `CheckoutMode::{Safe,Force}`；`CheckoutOptions { paths: Vec<PathBuf>, mode: CheckoutMode, update_index: bool }`，空 paths 表示全部。
- `checkout_tree(repo: &Raw, target: &git2::Object, options: CheckoutOptions, control: &NetworkControl) -> Result<()>`。调用者持有现有 WriterLock，事务只改变工作文件 / index，不发布 HEAD / 分支。
- `pending(repo: &Raw) -> bool`；`guard_native_write(repo: &Raw, target: Option<Oid>, paths: Option<&[PathBuf]>) -> Result<()>`，未接线原生写点用它拒绝受影响编码文件。
- recovery status / continue / abort 使用 owner + expected journal fingerprint，内部不重复获取 WriterLock；具体返回状态结构落地后由实现者同步给 Root 扩充 Operation / Snapshot。

- [ ] 写 `clean_utf16_checkout_uses_target_encoding`、`dirty_utf16_checkout_preserves_edit` 并观察原生 safe checkout 误判 / 错误工作编码的失败。
- [ ] 预生成全部目标 raw，固定当前 raw / index / HEAD 与属性配置；写持久 journal 后，只正规化 canonical 等价的干净源文件，再执行 native safe checkout。
- [ ] 每次替换验证 preimage，按预计算字节发布；失败保留 journal 与备份，恢复不读取新规则。目标碰撞、真实修改、链接必须保护。
- [ ] 测试 `checkout_interrupt_recovers_pinned_raw_without_filters`、`recovery_keeps_late_external_edit`、`hardlink_other_name_is_preserved`、`untracked_target_blocks_checkout`。
- [ ] 运行 `cargo test --locked --test encoding_checkout --quiet`；交接所有未覆盖写点与稳定 recovery API。

### Task 4: 接入所有业务写点

**Files:**
- Root owns dispatch / mutation hunks in `src/git/libgit.rs`。
- Modify: `src/git/integration.rs`, `src/git/rebase.rs`, `src/git/recovery.rs`, `src/git/stash.rs`, `src/git/conflicts.rs`, `src/git/workspaces.rs`, `src/git/lfs_migration.rs`, `src/git.rs`。
- Modify recovery UI as needed: `src/ui/management.rs`, `src/ui/modals.rs`, `src/ui/toolbar.rs`, `src/ui/tasks.rs`。

**Interfaces:** Consumes Task 3 wrapper / guard / recovery；Stage / stash / conflict canonical writes consume Task 1 Add policy。

- [ ] 用 `rg 'checkout_tree|checkout_index|reset\(|stash_save|cherrypick\(|revert\(' src/git` 列出实际写点，先在尚未包裹的路径接 guard，保证开发期间不写 raw 编码。
- [ ] 分组接普通 checkout / discard / hard reset，再接 merge / pull / rebase、stash / conflict、clone / worktree / submodule；保留引用发布顺序与每个原操作的取消边界。
- [ ] 加 `encoded_branch_switch_changes_rules`、`encoded_stash_round_trip_keeps_index`、`encoded_conflict_result_stores_canonical`、`encoded_rebase_recovery`，同时回归原 LFS / signing / partial / migration 行为。
- [ ] journal 纳入 Snapshot、写阻断和显式恢复 UI；重启只读取状态，不自动重放。
- [ ] 定向测试通过后独立审查，不以仅有 guard 的路径声明完整支持。

### Task 5: 整合与原生验收

**Files:** `docs/VALIDATION.md`, `docs/REMAINING-WORK.md`, `README.md`, 隔离 example 夹具。

- [ ] 运行 `cargo fmt --all -- --check`、`cargo clippy --locked --all-targets -- -D warnings`、`cargo test --locked --no-fail-fast --quiet`、全部 Python tests、`git diff --check`。
- [ ] 使用隔离 bundle 验证导入 UTF-16 → Stage → canonical diff → 切换编码规则分支 → 退出 / 重启恢复；磁盘逐字节核对 raw / index / refs。
- [ ] 重建 macOS 开发 bundle 与 ZIP，并用摘要安装到隔离目标。原生启动不能代替完整 GUI 或其他平台验收。
- [ ] 记录实测范围、尚待环境与审查修复；不提交 / 推送，不对未开放机器或发行签名作成功声明。

## 2026-11-11 实施记录

- 共享转换、部分操作与临时 Diff 对象已落地，回归入口为 `tests/encoding_worktree.rs`、`tests/patch_file.rs` 与 `tests/partial_staging.rs`。部分丢弃保留未选中行的原字节。
- 目标树属性查询可从独立对象库读取候选树，同时保留源仓库的属性上下文；LFS 迁移预览无需提前导入对象。
- 检出事务回归在 `tests/worktree_conversion.rs`，业务接线在 `tests/encoding_operations.rs` 与 `tests/encoded_history.rs`。真实 index.lock 贯穿文件和索引写入；规范对象与实际工作字节 stat 分开保存。未知的中断后编辑不覆盖，保留恢复文件。
- 已接入的业务范围为普通切换分支、分离 HEAD、整文件丢弃、Hard Reset、克隆、Worktree 与子模块、快进和无冲突的编码合并。无冲突合并保留双亲和签名，恢复会补齐 `ORIG_HEAD`；恢复不重复运行钩子。
- 仍拒绝未适配的编码冲突合并、Squash、Rebase / 批量重放与 Stash 组合，不把拒绝称为完整支持。其他平台、发行签名、SSH 和新增恢复页原生点击不属于本次命令行验收；本次不启动测试窗口。
