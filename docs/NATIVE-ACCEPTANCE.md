# 原生构建与验收

用户暂未开放 Windows / Linux 测试机器。本文件是实际可执行入口，不代表这些平台已经通过验收。

使用目标系统的 Rust stable、原生 C/C++ 编译环境和 Python 3.11 或更新版本。完整测试还需要 Git 和官方 Git LFS；脚本从 PATH 查找 `git-lfs`，也可传入 `--lfs-program` 指定路径，缺失时明确停止，不把未执行的迁移 / 扩展测试算作通过。Windows 使用带 MSVC 工具链的开发终端；Linux 需安装 GPUI / 图形栈及 OpenSSL 的开发依赖，缺项会由 Cargo 的原生依赖构建报告。图形会话需要可用 GPU 和显示服务；通过无图形 SSH 会话启动进程不能作为原生 GUI 验收。

在项目根目录运行：

```text
python3 scripts/verify_native.py --package --gui
```

Windows 可使用 `py -3.11 scripts/verify_native.py --package --gui`。报告保存在 `target/native-validation-<OS>.json`；失败时保留具体命令退出码，不把启动成功写成验收通过。GUI 使用独立的 `target/native-qa-config-<OS>`，不读取或覆盖日常会话配置。

需要人工确认的主要回路：

1. 打开演示仓库，输入提交草稿；切换、拖动排序、关闭和重新打开标签，退出重启后检查草稿、选择和滚动。
2. 检查深色、浅色、跟随系统和宽度配置；命令面板可搜索并回车执行。
3. 文本 / 并排 Diff 搜索、hunk 与行操作、图片预览、文件定位和外部编辑器。
4. 重命名冲突组的 Base / Ours / Theirs 路径预览及索引结果；工作文件保留可见，解决后继续 / 中止。
5. 根提交改写 / 拆分；保留合并拓扑 Rebase；冲突重启、继续、跳过和中止。
6. 批量 Cherry-pick / Revert 对每个合并提交选择不同主父；核对最终树和作者。
7. Worktree 创建、移动、修复；子模块递归、deinit / 移除和恢复；LFS 工具缺失时提示明确。
8. 使用隔离测试目标验证真实认证、取消和失败后的保留状态。自动测试中的真实网络用例默认忽略，需按测试注解设置专用端点再单独运行；不得对重要分支试验破坏性操作。

可用的隔离夹具：

```text
cargo run --locked --example demo_advanced -- target/native-advanced
cargo run --locked --example demo_complex_history -- target/native-complex
cargo run --locked --example demo_scalability -- target/native-large
```

示例拒绝覆盖已有目录。压力测试可单独运行 `cargo run --locked --example bench_history -- target/native-history-bench 50000`；追加 `--existing` 可重复读取同一压力夹具。机器配置、是否打包对象、缓存冷热会影响测量，应保留原始输出后比较。

macOS 的可选发行流程：

```text
python3 scripts/bundle_macos.py --archive --sign-identity "Developer ID Application: …" --notary-profile "已有钥匙串配置"
```

签名身份和公证配置由用户保存在本机钥匙串，脚本不接收或记录密码。当前用户没有 Apple Developer 身份，因此实际签名和公证仍待执行。
