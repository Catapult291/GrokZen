# 动态公告中文翻译

官方仍负责下发公告。此目录只提供可独立更新的中文映射，不发布公告，也不更改公告的 ID、严重性、有效期、链接目标、点击行为或遥测。只按 `field` 和完整 `source` 精确匹配；大小写、标点或空格不同都保留官方原文。

用户需要先安装一次包含此功能的客户端；此前的客户端（含 1.0.16）不会读取此目录。此后新增或修正公告翻译只需更新本目录并推送到 `zh-dev`，无需新建应用版本、打 Tag 或发布 Release。

## 文件与版本

- `manifest.json`：`schema_version`、独立递增的整数 `version`、当前译文文件原始 UTF-8 字节的 `sha256`。
- `catalogs/<version>.json`：该版本的完整映射，字段为 `schema_version`、`version`、`locale: "zh-CN"` 和 `entries`。
- 每条映射只有 `field`、`source`、`translation`；`field` 只允许 `title`、`message`、`cta_label`、`cta_caption`。

**已发布的版本文件不可修改或删除。** `catalogs/1.json` 也是客户端内置的离线基线。新版本替换整张映射表，不与旧表合并；新表未包含的内容直接显示官方原文。需要撤回错误翻译时，发布更高版本并修改或移除相应条目，不回退版本号。

## 更新步骤

1. 从 `manifest.json` 读取当前版本，把它对应的完整目录文件复制为下一个版本，例如 `catalogs/2.json`，同时修改文件内部的 `version`。
2. 保留仍需翻译的条目，新增或修正官方公告的准确英文原文与中文翻译。保留命令、模型名称、快捷键和正文中的 URL；不要把翻译写成额外说明。
3. 在仓库根目录生成摘要并校验（示例为版本 2）：

   ```text
   python .github/scripts/validate-announcement-translations.py --write-manifest 2 --base HEAD
   ```

4. 审核后将新目录文件与 `manifest.json` 放在同一提交，推送至 `zh-dev`。`Announcement translations` 工作流会校验格式、摘要、递增版本和历史文件不可变。只有本目录变化时跳过三平台二进制构建；涉及程序代码的变化仍执行原有 CI 和发布门禁。

译文文件使用 UTF-8、LF 换行，仓库属性已固定换行格式。单个目录最大 256 KiB，最多 512 条；单条原文或译文最大 16 KiB。禁止空白文本、重复的 `(field, source)` 和控制字符；只有 `message` 可以包含换行。可发布空的 `entries`，用于让全部公告恢复官方原文。

推送后 GitHub Raw/CDN 可能短暂返回旧清单或缺少新文件。客户端保留当前可用版本，稍后重试。直接推送的内容会随 GitHub Raw 可用而被读取，Actions 校验不是远端分发开关；需要发布前门禁时，应通过合并前已通过该检查的 PR 发布。

## 客户端行为

中文界面在官方远端设置开始预取的同一阶段启动独立后台任务。界面不等待 GitHub 或磁盘读取；先使用内置译文，缓存载入后、远端新版验证成功后分别发送快照并触发重绘。英文界面不启动此任务。

固定读取本仓库 `zh-dev` 的以下路径：

```text
https://raw.githubusercontent.com/JoyElliot/grok-build-Chinese/refs/heads/zh-dev/community/announcements/manifest.json
https://raw.githubusercontent.com/JoyElliot/grok-build-Chinese/refs/heads/zh-dev/community/announcements/catalogs/<version>.json
```

每次启动检查一次，运行期间每 5 分钟检查。版本相同只获取小清单，不下载译文；较旧版本忽略。版本增加时才下载整表，验证版本、结构与 SHA-256 后替换快照。网络连接超时为 2 秒，单请求 5 秒，一轮清单和译文下载共最多 8 秒。403、429、超时、重定向、超限或损坏内容都保留当前可用译文，等待下个周期；无弹窗、无即时循环重试，也不阻断官方公告和用户操作。

请求只访问上述固定 HTTPS 源，不携带 Grok 账号令牌，不使用 GitHub API，不跟随重定向。不会改变二进制更新器的源或下载规则。系统代理和企业 CA 沿用已有 HTTP 支持。

缓存为 `~/.grok/cache/grok-zh/announcement-translations.json`（设置 `GROK_HOME` 时位于对应目录）。内容和清单一起验证，以临时文件写入后替换；缓存损坏时使用内置译文，磁盘不可写时本次运行仍可使用已下载的译文。退出界面时取消后台任务。

设置 `GROK_ZH_ANNOUNCEMENTS_OFFLINE=1` 可关闭网络检查，继续使用缓存和内置译文。现有的 `GROK_CHANGELOG_OFFLINE=1` 也会关闭这项网络检查，便于离线和终端测试。两者均按现有约定处理：非空且不等于 `0` 时启用离线模式。
