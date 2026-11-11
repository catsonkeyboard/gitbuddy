# 安装与更新

关闭正在运行的 GitBuddy。macOS 可解压后将 GitBuddy.app 移到应用程序目录；Windows / Linux 可解压到独立目录运行原生可执行文件。当前开发包没有发行签名或公证；正式签名验收仍待开发者身份。

需要保留旧安装的自动备份时，使用包中附带的 `install_local.py`（需要 Python 3.11+）。macOS 位于 `GitBuddy.app/Contents/Resources/`，其他平台位于解压目录根部。安装器不下载任何文件，需要原始 ZIP / tar.gz 和该包的完整 SHA-256；可从 GitBuddy 下载成功页或经过核验的发行信息复制摘要。

```text
python3 /绝对路径/install_local.py --archive /绝对路径/原始发行包.zip --sha256 完整64位SHA256 --destination /绝对路径/安装目录
```

macOS 的安装目录应以 `GitBuddy.app` 结尾。Windows 可将 `python3` 改为 `py -3.11`，所有含空格的路径加引号。只对已发行签名的 macOS 包添加 `--require-signature`，使安装器检查代码签名和 Gatekeeper；开发包会被该选项拒绝。

安装器拒绝摘要不符、路径越界、链接及未知现有目录；已有 GitBuddy 安装会重命名为相邻的 `.backup-时间-随机值`，成功后也保留，供退出应用后手动恢复。用户会话配置在独立配置目录内，安装器不修改它。安装器不负责确认应用已退出，请先自行关闭。
