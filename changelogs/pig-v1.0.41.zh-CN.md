# pig v1.0.41（2026-09-28）

首个 pig 版本：在官方 1.0.41 的基础上完成 Pig Agent 品牌切换，配置目录与二进制改名，并开箱支持第三方供应商模型。

## 新功能

- 第三方供应商模型目录（pi 兼容）：内置 opencode / opencode-go 模型快照，模型键为 `<vendor>/<id>` 命名空间，两家供应商预注册开箱即用；附 `scripts/sync-pi-vendors.sh` 刷新快照（含漂移检查）。
- 会话路由头 `session_header`：每轮对话以会话 id 为值发送（如 `x-opencode-session`），subagent 与辅助模型随主会话继承。

## 品牌与界面

- 欢迎页品牌切换为 Pig Agent，新增盲文猪 logo（按终端高度自动切换大小两档）。
- 二进制改名为 `pig`，退出 resume 提示、重启失败提示等跟随实际二进制名。
- 配置目录迁至 `~/.config/pig`（解析顺序 `$PIG_HOME` → `$GROK_HOME` 兼容旧用户 → 默认）；首次运行时一次性迁移旧 `~/.grok`（只拷贝不删除，失败则降级为空目录）。

## 构建与发布

- `pig-v*` tag 自动构建：Linux x86_64 / ARM64、macOS ARM64 / Intel、Windows x86_64 五个平台，自动创建 GitHub Release 并上传二进制。
- 每版附中英双语 release 日志（`changelogs/` 目录）。

## 其他

- 上游基线：monorepo `036a5d8`（版本 1.0.41）。
- `google-generative-ai` 明确不支持并过滤；企业锁定模式（custom endpoint）跳过 vendor 快照。
