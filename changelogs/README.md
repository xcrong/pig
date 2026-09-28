# changelogs

pig 每次发版的 release 日志，一版两个文件（中英双语，结构一致，为国际化打基础）：

- 命名与 git tag 同名，后缀区分语言：
  - `v<version>.zh-CN.md`（中文，如 `v1.0.1.zh-CN.md`；`pig-v1.0.41.*.md` 是旧命名，保留为历史）
  - `v<version>.en.md`（英文，如 `v1.0.1.en.md`）
- pig 使用独立版本号，不跟随上游版本（旧命名 `pig-v*` 已废弃）。将来需要对齐上游时再整体 bump，并在日志里说明。
- 内容：上次发版以来的变更归纳。
- 写日志的完整流程见 skill：`.agents/skills/release-notes/SKILL.md`。
- 打 tag 推送后，二进制构建与 GitHub Release 由 `.github/workflows/release-pig.yml` 自动完成。
