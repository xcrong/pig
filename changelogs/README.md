# changelogs

pig 每次发版的 release 日志，一版两个文件（中英双语，结构一致，为国际化打基础）：

- 命名与 git tag 同名，后缀区分语言：
  - `pig-v<version>.zh-CN.md`（中文，如 `pig-v1.0.41.zh-CN.md`）
  - `pig-v<version>.en.md`（英文，如 `pig-v1.0.41.en.md`）
- 版本号与官方版本对齐：tag 后缀即上游版本号。
- 内容：上次发版以来的变更归纳。
- 写日志的完整流程见 skill：`.agents/skills/release-notes/SKILL.md`。
- 打 tag 推送后，二进制构建与 GitHub Release 由 `.github/workflows/release-pig.yml` 自动完成。
