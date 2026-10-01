# pig 从 Rust 向 Go 演进的落盘线路图

> 起因：Rust 版 `pig` 对个人开发者太重（`target/ 56G`、全量构建慢、磁盘爆炸），
> Go 编译秒级、单二进制分发友好、内存可控；Bun 则内存容易爆。
> 目标：用“绞杀者模式”从外围到核心逐步把 Rust 换成 Go，全程可发布、可回滚、单文件分发。
> 非目标：一次性重写 188 万行。

## 0. 现状盘点（2026-10-01 实测）

- 规模：`3299` 个 `.rs` 文件，约 `188万` 行（含空行），`~100` 个 crate
- 大头：
  - `crates/codegen/xai-grok-shell` 675 文件（agent 主循环 + config + TUI 底座）
  - `crates/codegen/xai-grok-pager` 654 文件（TUI 本体）
  - `crates/codegen/xai-grok-tools` 290 文件（tool 执行面）
  - `crates/codegen/xai-grok-pager-pty-harness` 288 文件（测试脚手架）
  - `crates/codegen/xai-grok-workspace` 151 文件（daemon）
  - `crates/codegen/xai-grok-pager-render` 84 文件
- 构建产物：`target/` 实测 `56G`
- 上游关系：fork 自 `grok-build`，`SOURCE_REV` 定期同步，`AGENTS.md` 要求保护自有改动
- 试点：`crates/codegen/xai-grok-update` 约 6245 行
  - `src/auto_update.rs` 2555 行（check/run/ensure/restart/download）
  - `src/version.rs` 727 行（查最新版 + `version.json` 缓存）
  - 已是子进程模型：`run_update_subcommand()` 就是 `current_exe update --trigger=...` 再 spawn

## 1. 总原则（硬性）

1. 只在进程边界切，不在函数边界切。Rust `pub fn` -> Go `helper <verb>`，中间只走三样：
   `stdout JSON` + `文件（version.json / sqlite / config.toml）` + `exit code`。禁用 cgo/FFI 静态链接。
   原因：cgo 废掉 Go 交叉编译，tokio 和 Go scheduler 打架，改一行要重 link 整个 Rust，磁盘一点没省。
2. 逻辑上双进程，物理上单文件。`build.rs` 里 `go build -o $OUT_DIR/pig-xxx-helper`，
   再 `include_bytes!` 塞进 `pig`，运行时解到 `~/.cache/pig/bin/` 再 `exec`。用户永远只看到一个 `pig`。
3. 每个 crate 可回滚。环境变量开关 `PIG_<NAME>_BACKEND=go|rust`，默认 `rust`。
   切坏了立刻切回去，上游还能合。
4. TUI/PTY 最后动。`shell/pager/pty/sandbox` 是 Rust+ratatui+tokio 优势区，收益最低，放最后。
5. 先治标再治本。日常只 `cargo check -p xai-grok-shell`，配 `sccache + mold/lld + cargo sweep -time 7`，
   把迭代成本先砍 70%，再谈迁移。

## 2. 进程间契约模板（以 update 为例，后面照抄）

### 2.1 CLI 形态

```bash
pig-update-helper check --config-json '<UpdateConfig>'
# stdout -> UpdateStatus JSON，对应 check_update_status()

pig-update-helper update --trigger=user_command|auto_background|leader_converge --config-json '...'
# 阻塞装完，exit 0/非0，对应 run_install_script()

pig-update-helper ensure-latest --config-json '...'
# leader 用，对应 ensure_latest_on_disk()
```

### 2.2 JSON 结构（第一版冻结，后续只加字段）

```json
// 输入 UpdateConfig（6 字段，照搬 version.rs）
{
  "proxy_base_url": "https://cli-chat-proxy.grok.com/v1",
  "auth_scope": "...",
  "deployment_key": null,
  "alpha_test_key": null,
  "channel": "stable|alpha",
  "npm_registry": null
}

// 输出 UpdateStatus（照搬 auto_update.rs）
{
  "currentVersion": "0.1.220",
  "latestVersion": "0.1.221",
  "updateAvailable": true,
  "installer": "github|npm|winget",
  "channel": "stable",
  "autoUpdate": false,
  "error": null
}
```

### 2.3 共享文件

- `~/.config/pig/version.json`（实际经 `grok_home()` 解析）：
  `{"version":"...","stable_version":"...","checked_at":"rfc3339"}`，两边读写同一格式。
- 下载目录：`downloads/` + `bin/pig` 布局不变，原子发布保持 `tmp(pid-seq) + rename`，
  Unix 先 `chmod 0755` 再 rename，避免并发 exec 到一半的文件。

### 2.4 Rust 侧 shim 形态（伪代码）

```rust
// crates/codegen/xai-grok-update/src/lib.rs 新增
if std::env::var("PIG_UPDATE_BACKEND").as_deref() == Ok("go") {
    let out = Command::new(helper_path()).arg("check").arg(json).output()?;
    Ok(serde_json::from_slice::<UpdateStatus>(&out.stdout)?)
} else {
    check_update_status(&cfg).await // 老路
}
```

### 2.5 Go 侧注意点（从 Rust 抄过来，别自创）

- 版本比较用 `golang.org/x/mod/semver`，行为对齐 `needs_update/plan_for`（stable 拒绝预发布、alpha 允许、anti-downgrade 只对 github 生效）。
- 平台映射：`GOOS/GOARCH` -> `linux-x86_64 / macos-arm64`，注意 `aarch64 <-> arm64` 换名。
- Rosetta 探针：macOS x86_64 下 `sysctl hw.optional.arm64`，为 1 则按 arm64 拉包。
- 下载：HEAD 拿长度 -> >=16MiB 则分片（每 16MiB 一片，最多 8 片）Range 并发 -> 单连接兜底；进度条用 `stderr`，`stdout` 只留 JSON。
- 安全：`PIG_GITHUB_API_BASE / PIG_GITHUB_DOWNLOAD_BASE` 只认 loopback，逻辑照抄 `is_loopback_base`。

## 3. 演进阶段

### Phase 0：准备（0.5 天）

- [ ] 建 `go/` 目录，`go.mod module pig-go`，`go/pig-update/main.go` 骨架
- [ ] 定 CI：`go vet + go test ./...`，Rust 侧加 `PIG_*_BACKEND` 开关测试
- [ ] 写 parity 脚本：同一 `UpdateConfig` 下对比 Rust/Go `check` 输出 JSON 一致

### Phase 1：无状态外围（1-2 周，立刻止痛）

顺序：`xai-dirs -> xai-grok-version -> xai-grok-update -> diag-server -> egress-proxy -> login/auth -> file-utils/tty-utils/fuzzy/token-estimation`

- 每个都是纯 IO，只读 config、只写缓存文件，半天一个。
- 切完 `cargo check -p xai-grok-shell` 依赖树明显变小。
- 验收：`PIG_UPDATE_BACKEND=go pig --check` 与老路 JSON 一致；
  `target/` 经 `cargo sweep` 后不再反弹。

### Phase 2：Tool 执行面（收益最大，2-4 周）

对象：`xai-grok-tools(290) + tools-api + mcp + sandbox + session-search + memory + hooks + workflow`

- Rust 从 `in-process call` 改成 `spawn pig-tools-helper run`，stdin 传 `ToolCall`，stdout 回 `ToolResult`。
- `sandbox` 本来就是进程隔离，Go 用 `exec + cgroup/namespace` 对齐行为即可。
- Go 的 goroutine 在这里反而比 tokio 好写，并发 toolcall 不容易爆内存（对比 Bun）。
- 验收：现有 `xai-grok-tools` 单测 + MCP 集成测试全绿，只是后端换成 Go 进程。

### Phase 3：有状态大脑（最难，1-2 月）

对象：`workspace-daemon/client/types + sampler + agent + agent-lifecycle + chat-state + prompt-queue + compaction`

- 现在是 `tokio mpsc + Arc<Mutex>`，改成 `Unix Socket 长连接 JSON-RPC`，Go 做 server，Rust 做 thin client。
- 先迁 `workspace-daemon`（本来就是 daemon 形态），再迁 `sampler/agent`。
- **到这一步冻结上游同步**：`SOURCE_REV` pin 住不再跟。再跟的话 Go/Rust 两边都要改，个人扛不住。

### Phase 4：头身分离 + TUI 追平（3-6 个月，慢慢磨）

对象：`shell(675) + pager(654) + pager-render/minimal/diff + ptyctl + ratatui-inline`

- 先“头身分离”：Go 写头（`cmd/pig/main.go` 命令分发 + config + agent loop），TUI 还是 spawn 老 Rust `pager` 子进程。
- 再用 `bubbletea` 一屏一屏抄 `pager-render`，抄完一屏切一屏开关。
- PTY/signal/Windows 保持 Rust 兜底，直到 Go 追平。

### Phase 5：翻转（1 天 + 清理 1 周）

```text
现在：pig(Rust) -> exec pig-update-helper(Go)
终局：pig(Go) -> exec pig-legacy-pager(Rust，仅剩 TUI)
     -> TUI 追平 -> rm -rf crates/ target/，只留 go/
```

翻转点：`crates/codegen/xai-grok-pager-bin/src/main.rs` 的 `Command::Update/run()` 被 `cmd/pig/main.go` 接管时。
翻转后 Rust 只留一个 `legacy` 二进制做兜底，最终删库。

## 4. 单二进制演进

- 全程：`pig` 主二进制 embed helper，`build.rs` 调 `go build`，发版产物仍是一个文件。
- 中期：`cargo` 只编 `pager-bin + shell`，Go 编其余 helper，CI 时间反而下降。
- 终局：`goreleaser` 接管，`cargo` 退出，发版体积从 Rust 大二进制变成 Go 小二进制（但功能一致）。

## 5. 风险与止损

| 风险 | 对策 |
|---|---|
| Go TUI 追不上 ratatui，PT Y细节丢 | TUI 放最后，长期头身分离，Rust 兜底 |
| config TOML 解析两边不一致 | Go 直接复用同一 config 文件 + JSON 快照对比测试，字段只增不改 |
| semver/渠道语义漂移 | `plan_for/needs_update` 纯函数照抄 + 矩阵单测双语各一份 |
| 上游 grok-build 大改，合不进来 | Phase 3 后冻结同步，只合安全补丁；此前保持开关默认 rust |
| 个人精力不够全量 | 到 Phase 2 结束就已解决磁盘/编译/内存三大痛，可长期停在“Go 头 + Rust TUI”混合态 |

## 6. 退出标准（不用 100% 全 Go 也算赢）

- 及格线：Phase 1+2 完成，日常开发只用 `go build ./...`，`target/` 不再成为负担。
- 良好线：Phase 3 完成，daemon 全 Go，笔记本可久跑不烫。
- 满分线：Phase 5 翻转，`rm -rf target`，单 `go` 仓库。

## 7. 下一步（第一个 PR）

1. 建 `go/pig-update`，只实现 `check` 一个 verb + `httptest`（把 `test_check_status_regression.rs` 的 wiremock case 翻过去）。
2. Rust 加 `PIG_UPDATE_BACKEND` 开关 + parity e2e。
3. 合入后，再切 `update`、`ensure-latest`。半天跑起来，一周闭环。

---
附：关键路径索引
- Rust 主调：`crates/codegen/xai-grok-pager-bin/src/main.rs`（update 分发、`build_update_config`、`finish_update_on_exit`）
- Rust 被调：`crates/codegen/xai-grok-update/src/{lib.rs,auto_update.rs,version.rs,version_policy.rs,winget.rs,cleanup_downloads.rs}`
- TUI 侧：`crates/codegen/xai-grok-pager/src/app/{mod.rs,event_loop.rs,effects/mod.rs}`、`views/welcome/mod.rs`
- Go 新址：`go/pig-update/`（拟建）、`go/internal/update/`（semver/channel/download 三纯包）
