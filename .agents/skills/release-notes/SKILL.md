---
name: release-notes
description: Cut a pig release: collect changes since the previous pig-v* tag, write the bilingual (zh-CN/en) release log to changelogs/, commit it, and push the tag to trigger the release build. Use when the user wants to 发布版本, 打 tag, 写 release 日志 or 更新 changelog.
---

# Pig Release Notes

为 pig 发版写双语 release 日志。每次打 `pig-v*` tag 发版本时，根据上次发版以来的变更写中英两篇日志，存入 `changelogs/`，为未来国际化打基础。

## 前置约定

- 版本号与官方版本对齐：tag 后缀即上游版本号（如官方发到 1.0.41，pig 就打 `pig-v1.0.41`），也就是 `crates/codegen/xai-grok-pager-bin/Cargo.toml` 的 `version`。
- 日志文件（每版两个，结构一致）：
  - `changelogs/pig-v<version>.zh-CN.md`（中文）
  - `changelogs/pig-v<version>.en.md`（英文）
- 提交信息用中文，不带 `Co-authored-by`。
- 打完 tag 并推送后，`.github/workflows/release-pig.yml` 会自动构建多平台二进制并创建 GitHub Release（附件），而本 skill 产出的 markdown 是仓库内的 release 日志正文。

## 流程

### 1. 确定新版本号

向用户确认新版本号（默认建议：pager-bin Cargo 版本）。检查：

```bash
git tag --list 'pig-v*' --sort=-v:refname | head -5
git status --short  # 工作区必须干净，有未提交改动先处理
```

tag 已存在则报错停下，不要覆盖已发布的 tag。

### 2. 收集上次发版以来的变更

```bash
PREV=$(git tag --list 'pig-v*' --sort=-v:refname | head -1)
git log ${PREV}..HEAD --oneline
git diff --stat ${PREV}..HEAD
```

注意点：

- `Synced from monorepo` 这类上游同步提交只用一句话概括，不要展开。
- 重点是 pig fork 自有改动：品牌映射、欢迎页、`vendors/` 快照、`scripts/sync-pi-vendors.sh`、`session_header` 全链路等（见 `AGENTS.md`）。
- 必要时用 `git show --stat <commit>` 看单个提交的改动面。

### 3. 分类整理

按以下分组归纳（没有的组省略）：

- **新功能**：用户可见的新能力
- **修复**：bug 修复
- **品牌与界面**：Pig Agent 品牌、欢迎页、logo 等
- **构建与发布**：CI、打包、安装相关
- **其他**：上游同步、依赖升级、文档等杂项

### 4. 写日志文件

同时写中英两个文件，章节结构完全一致，先写中文版，再逐节翻译成英文版：

- `changelogs/pig-v<version>.zh-CN.md`
- `changelogs/pig-v<version>.en.md`

中文模板：

```markdown
# pig v<version>（<YYYY-MM-DD>）

一句话亮点：这次版本最值得说的一件事。

## 新功能

- ...

## 修复

- ...

## 品牌与界面

- ...

## 构建与发布

- ...

## 其他

- 上游同步至 monorepo `<SOURCE_REV 前 12 位>`（如有）
```

写作要求：面向用户说人话，写“变化是什么、有什么用”，不要贴 commit 哈希堆砌。破坏性变更（如配置目录迁移、环境变量改名）必须单独列一条并写清迁移动作。英文版是中文版的忠实翻译，同一条目在两个文件里顺序一致。

### 5. 提交并打 tag

```bash
git add changelogs/pig-v<version>.zh-CN.md changelogs/pig-v<version>.en.md
git commit -m "发布 pig v<version>"
git tag pig-v<version>
git push origin pig <branch commits if any>
git push origin pig-v<version>   # 触发自动构建与 GitHub Release
```

tag 推送后提醒用户去 Actions 页确认 `pig release` 工作流的构建结果。
