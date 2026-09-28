# pig · Agent 工作指南

- `pig` fork 自上游 `grok-build`（SpaceXAI monorepo 定期同步，上游版本见根目录 `SOURCE_REV`），并叠加自有改动。
- 合并上游时重点保护自有改动，冲突优先保留本 fork 行为：
  - `crates/codegen/xai-grok-shell/src/agent/vendors/`（第三方供应商目录及 pi 快照）
  - `scripts/sync-pi-vendors.sh` 及其 `manifest.json`
  - `session_header` / `session_id` 全链路（sampler、模型配置、会话重建）
  - `.gitignore` 中 `vendors/data/opencode.json` 的白名单（全局 ignore 会吞掉它）
- 上游 `CONTRIBUTING.md` 声明不接受外部 PR——那是上游政策，本 fork 的改动直接提交到本仓库分支即可。

## 品牌映射约定（pig fork）

- 为保持与上游的合并干净，crate 名一律保留 `xai-grok-*`，函数名保留 `grok_home` 等历史命名，仅改解析值与用户可见产物名。
- 配置目录：`$PIG_HOME` → `$GROK_HOME`（兼容旧用户）→ `~/.config/pig`，收口于 `xai-dirs`；纯默认路径下首次创建时一次性迁移旧 `~/.grok`（只拷不删）；UI 展示为 `~/.config/pig` / `$PIG_HOME`。
- 二进制产物：`xai-grok-pager-bin` 的 `[[bin]]` 名为 `pig`；clap `bin_name` 白名单含 `grok/agent/pig`，缺省 `pig`；测试脚手架经 `CARGO_BIN_EXE_pig` / `PAGER_BINARY` 找二进制。
- 界面产品名统一为 `Pig Agent`：欢迎页版本徽标、副标题、信任提示等用户可见文案不再保留上游 `Grok Build` 字样；合并上游时冲突优先保留本 fork 文案。
- 客户端身份统一为 `pig-agent` / `pig-pager`：User-Agent 形状为 `pig-agent/<version> (os; arch)`，默认 `x-grok-client-identifier`、`ClientType::user_agent_label`、遥测 `KNOWN_CLIENT_IDENTIFIERS`（新值追加，旧值保留兼容）、sentry/otel client 名同步；后端定义的键（auth scope、`x-grok-client-version` 门禁头、serde wire 名、第三方标识）一律不动。
- `[vendors.<id>]` 支持自定义 provider：`base_url` + `snapshot_file`（pi 形目录 JSON，相对路径按 pig home 解析）+ 可选 `session_header`；未知 id 无自定义字段仍按拼写错误警告；`PATCH_STRIP_KEYS` 剥离整个 `vendors` 表，天然覆盖。
- `[cli].auto_update` 默认关闭（未设置视为关闭，三处门禁只在显式 `true` 时放行，首次运行回写 `false`）；更新源仍指向上游，手动 `pig update` 与显式 opt-in 不受影响；分发主要靠包管理器。
- 刻意不动（v2 品牌 pass 再议）：`GROK_*` 环境变量全改名、文档中 `~/.grok` 路径、自更新源切换（仍指向上游产物）。

## 构建与测试

- 工具链以 `rust-toolchain.toml` 为准；构建需要 `protoc`（官方经 DotSlash 取 hermetic 二进制；环境缺 DotSlash 时可用系统 `protobuf` 应急）。
- 常用命令（仓库根目录 `pig/` 下执行）：
  - `cargo check -p xai-grok-shell`
  - `cargo test -p xai-grok-sampler --lib` / `cargo test -p xai-grok-shell --lib -- <filter>`
  - `cargo clippy -p xai-grok-shell`、`cargo fmt`（提交前必跑，保持干净）
- 全量 shell 单测在小栈环境易栈溢出，用 `RUST_MIN_STACK=33554432 cargo test -p xai-grok-shell --lib` 跑。
- 已知与本 fork 无关的失败（干净树同样存在，勿“修复”）：`xai-grok-sampler` 的 retry 抖动单测、`mvp_agent` installer 家族栈溢出。
- 新增全局 env 凭据（如 `OPENCODE_API_KEY`）时，必须同步更新各测试的 `EnvGuard` 隔离，否则 `has_own_credentials()` 会实时读到 ambient env 导致误判。已知隔离点：`cli_models.rs::isolate_auth_sources`、`auth_method.rs` 相关单测、`mvp_agent/tests.rs`。

## 模型目录相关约定

- 显式优先（硬性）：模型目录、凭证来源、请求行为一律不做隐式加载与隐式读取。新增模型来源必须提供显式 opt-in 开关且默认关闭；环境变量 KEY 必须写在受信配置文件里按名读取，不得扫描环境、不得内建默认值；`--config` 补丁、campaign/远端补丁一律不得开启这类来源（见 `PATCH_STRIP_KEYS`）。先例：`[vendors.<id>] enabled` + `env_key`（默认关闭，未启用时快照不进目录、不注册 preset、不读变量）。
- `resolve_model_list` 分层：`默认/预取 → vendor 快照 → [model.*]`（用户永远最高）；企业锁定（custom endpoint）跳过 vendor。
- vendor 快照只用 `scripts/sync-pi-vendors.sh` 更新（含 `--check` 漂移门禁），提交前务必人工审 diff；`manifest.json` 记录来源与哈希。
- `google-generative-ai` 明确不支持并过滤；若 pi 出现新 api 形状，`vendors` 的收敛测试会失败，此时必须有意识地决定映射或加名单，不得静默丢弃。
- 提交信息用中文，不带 `Co-authored-by`。
