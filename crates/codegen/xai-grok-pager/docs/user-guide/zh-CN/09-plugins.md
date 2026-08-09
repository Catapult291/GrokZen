<a id="plugins"></a>
# 插件

插件将技能、斜杠命令、智能体、钩子和 MCP 服务器打包成一个可安装单元。你可以从市场获取插件，安装需要的插件，Grok 会加载它们添加的内容。若要构建并共享自己的插件，请参阅[创建自己的市场](#create-your-own-marketplace)。

---

<a id="how-marketplaces-work"></a>
## 市场如何工作

市场是某人发布并共享的插件目录。使用市场分两步，类似添加应用商店：添加市场后可以浏览其中的插件，然后选择要安装的插件。

1. **添加市场**，让 Grok 显示它提供的内容。此时尚未安装任何东西。
2. **安装所需插件**，一次安装一个。

插件在安装并启用前保持关闭状态，插件的钩子和 MCP 服务器在你[信任](#trust-and-security)它之前也保持不活动。

---

<a id="add-a-marketplace"></a>
## 添加市场

市场源可以是 GitHub 仓库、任意主机上的 git URL 或本地文件夹。从命令行添加：

```bash
grok-zh plugin marketplace add my-org/team-plugins                  # GitHub 简写（owner/repo）
grok-zh plugin marketplace add https://gitlab.com/acme/plugins.git  # 任意 git 主机，包含 https:// 和 .git
grok-zh plugin marketplace add ./my-marketplace                     # 本地文件夹
```

使用 `grok-zh plugin marketplace list`、`grok-zh plugin marketplace update [<name>]` 和 `grok-zh plugin marketplace remove <url>` 列出、刷新和移除源。

也可以在配置中声明源，让它们始终存在。

<a id="in-configtoml"></a>
### 在 config.toml 中

每个源都需要 `name`，并且需要提供一个 git URL（可选 `branch`）或本地 `path`：

```toml
[[marketplace.sources]]
name = "My Team Plugins"
git = "https://github.com/my-org/plugins.git"

[[marketplace.sources]]
name = "Local Dev"
path = "~/dev/my-plugins"
```

<a id="in-settingsjson"></a>
### 在 settings.json 中

在 `extraKnownMarketplaces` 下以名称为键添加源。每个条目的 `source` 是 `git`（带 `url`）、`github`（带 `repo`）或 `local`（带 `path`）之一：

```json
{
  "extraKnownMarketplaces": {
    "my-marketplace": {
      "source": { "source": "git", "url": "git@github.com:my-org/plugins.git" }
    }
  }
}
```

将该文件放在 `~/.grok/settings.json` 或 `~/.claude/settings.json`。

---

<a id="install-and-use-a-plugin"></a>
## 安装并使用插件

添加市场后，按名称安装插件。也可以直接从仓库或本地路径安装：

```bash
grok-zh plugin install deploy-tools --trust
```

安装源接受多种形式：

- `owner/repo`（GitHub 简写）、`owner/repo@v1.0`（引用）、`owner/repo@<commit-sha>`（获取后验证的精确提交）或 `owner/repo#subdir`；
- 完整 git URL（`https://github.com/user/repo.git`）或 SSH（`git@github.com:user/repo.git`）；
- 本地路径（`./local-dir` 或 `/absolute/path`）。

运行不带 `--trust` 的 `grok-zh plugin install <source>` 时，Grok 会显示来源，警告安装将激活插件的钩子、MCP 服务器和技能，然后停止。添加 `--trust` 才会继续。只从你信任的来源安装插件（参阅[信任与安全](#trust-and-security)）。

插件技能会出现在斜杠菜单中。技能名称有歧义时，Grok 会显示带插件名称前缀的限定形式，例如 `/deploy-tools:release`。若要加载新安装的插件，在 Plugins 选项卡中按 `r`，或启动新会话。

---

<a id="manage-plugins"></a>
## 管理插件

<a id="from-the-command-line"></a>
### 从命令行

```bash
grok-zh plugin list [--json] [--available]   # 已安装插件（--available 需要 --json）
grok-zh plugin uninstall <name> [--confirm] [--keep-data]   # 别名：rm、remove
grok-zh plugin update [<name>]               # 省略名称则更新每个插件
grok-zh plugin enable <name>
grok-zh plugin disable <name>
grok-zh plugin details <name>                # 显示插件组件清单
```

<a id="in-the-terminal-ui"></a>
### 在终端 UI 中

在 VS Code 系列之外按 `Ctrl+L`，或在任何终端中使用 `/plugins`（VS Code 系列必须使用），打开插件模态窗口。窗口有五个选项卡：**Hooks**、**Plugins**、**Marketplace**、**Skills** 和 **MCP Servers**；使用 `Tab` / `Shift+Tab` 切换。`/hooks`、`/marketplace`、`/skills` 和 `/mcps` 命令会在对应选项卡打开模态窗口。

在 **Plugins** 选项卡中，按 `Enter` 展开插件，查看其名称、版本、作用域（`cli`、`project`、`user`、`custom path` 或市场源名称）、技能、智能体、钩子、MCP 服务器（插件不受信任时显示为 `blocked`）、描述和路径。然后：

| 按键 | 操作 |
|-----|--------|
| `r` | 重新加载所有插件 |
| `a` | 从 `owner/repo`、URL 或本地路径添加插件 |
| `Space` | 启用或禁用选中的插件 |
| `x` | 卸载选中的插件 |
| `f` | 按状态筛选（全部、已启用或已禁用） |
| `/` | 按名称搜索 |

在 **Marketplace** 选项卡中，从你的源浏览并安装：

| 按键 | 操作 |
|-----|--------|
| `i` | 安装选中的插件 |
| `d` | 卸载选中的插件 |
| `a` | 添加市场源 |
| `x` | 移除选中的源及其插件 |
| `r` | 刷新源 |
| `u` | 更新选中的插件 |

Marketplace 选项卡中的组件摘要仅对发布了 [`plugin-index.json`](#add-a-catalog-optional) 目录的市场显示。破坏性操作会请求确认：按小写 `y` 确认，按任何其他键（包括 `Esc`）取消。

<a id="turn-plugins-on-or-off-in-config"></a>
### 在配置中开关插件

在 `~/.grok/config.toml` 中设置：

```toml
[plugins]
paths = ["~/my-plugins/custom-tools"]        # 其他插件目录
disabled = ["user/a1b2c3d4/noisy-plugin"]    # 要跳过的名称或 ID
enabled = ["project/9f8e7d6c/team-tools"]    # 要强制启用的名称或 ID
```

插件默认关闭；将插件列在 `enabled` 中可启用，列在 `disabled` 中则会发现它但跳过加载。每个条目可以是普通插件名称（来自 `grok-zh plugin list`）或完整 ID（`<scope>/<hash>/<name>`）。

若要完全隐藏插件和钩子界面，请在 `~/.grok/pager.toml` 中设置 `disable_plugins = true`。

---

<a id="trust-and-security"></a>
## 信任与安全

插件以你的权限运行，因此应像对待安装的任何软件一样：只添加你信任的市场，只从你信任的来源安装插件。

启用插件会加载其技能、命令和智能体。信任是独立的控制项，用于决定插件代码是否运行：即使插件已启用，其钩子、MCP 服务器和 LSP 服务器在你信任它之前仍保持不活动。Grok 会自动信任 `~/.grok/plugins/` 中的插件；`.grok/plugins/` 中的项目插件需要信任。安装时使用 `--trust` 授予信任：

```bash
grok-zh plugin install <source> --trust
```

受信任插件的 `.mcp.json` 服务器会像其他 MCP 配置一样附加到会话，子智能体会继承它们。插件智能体（`plugin-name:agent-name`）默认使用父会话的 MCP 服务器，与 `~/.grok/agents/` 下的用户智能体相同；可用 `mcpInheritance` frontmatter 限制该集合（参阅[子智能体](16-subagents.md#mcp-inheritance)）。出于安全原因，插件智能体 frontmatter 不能声明 `mcpServers` 或钩子，也不能设置 `permissionMode: bypassPermissions`。

---

<a id="create-your-own-marketplace"></a>
## 创建自己的市场

市场是列出一组插件的 git 仓库（或本地文件夹）。添加市场类似添加应用商店：它让人们浏览你的插件，然后选择要安装的插件。发布自己的市场是团队或组织从一个地方共享技能、命令、智能体、钩子和 MCP 服务器的方式。

你需要三样东西：一个 git 仓库、每个插件一个文件夹，以及一个列出插件的单一索引文件。

<a id="set-up-the-repository"></a>
### 设置仓库

1. **创建 git 仓库。** 私有仓库也可以；访问权限使用每个人自己的 git 凭据。
2. **将每个插件添加为文件夹。** 插件文件夹可以包含 `skills/`、`commands/`、`agents/`、`hooks/hooks.json`、`.mcp.json` 和可选的 `plugin.json` 清单（参阅[插件包含什么](#what-a-plugin-contains)）。
3. **在 `.grok-plugin/marketplace.json` 中列出插件。** 这是 Grok 读取的索引。
4. **推送仓库。**

典型布局：

```
my-org-plugins/
  .grok-plugin/
    marketplace.json      # Grok 读取的索引（必需）
    plugin-index.json     # 更丰富浏览体验的可选目录
  plugins/
    gdrive/
      plugin.json         # 可选清单
      skills/gdrive/SKILL.md
      .mcp.json           # 此插件添加的 MCP 服务器
```

Grok 从 `.grok-plugin/marketplace.json` 读取索引，也接受 `.grok-plugin/plugin.json` 和 `.claude-plugin/` 对应形式。

<a id="write-the-index"></a>
### 编写索引

`marketplace.json` 命名市场并列出每个插件：

```json
{
  "name": "My Org Plugins",
  "description": "Internal skills and tools",
  "owner": { "name": "Platform Team", "email": "platform@example.com" },
  "plugins": [
    {
      "name": "gdrive",
      "description": "Search and edit Google Drive, Docs, Sheets, and Slides",
      "category": "productivity",
      "source": { "type": "local", "path": "./plugins/gdrive" }
    }
  ]
}
```

每个插件的 `source` 都指向其文件，方式有两种：

- **在此仓库中：** `{ "type": "local", "path": "./plugins/gdrive" }`。普通字符串 `"./plugins/gdrive"` 也可用。
- **在独立仓库中：** `{ "source": "url", "url": "https://github.com/my-org/gdrive.git", "sha": "<full commit sha>" }`。固定 `sha` 可使安装可复现（当你[要求固定版本](#require-pinned-versions)时必需）。

每个插件可选字段：`version`、`author`、`homepage`、`tags` 和 `keywords`。

<a id="add-a-catalog-optional"></a>
### 添加目录（可选）

`plugin-index.json` 目录让市场浏览器在任何人安装前显示每个插件的技能、命令、钩子和智能体。它仅用于显示；没有它也能安装，团队通常在 CI 中生成它：

```json
{
  "version": 1,
  "plugins": {
    "gdrive": {
      "components": {
        "skills": [{ "name": "gdrive", "description": "Google Drive access" }]
      }
    }
  }
}
```

<a id="check-and-share-it"></a>
### 检查并共享

发布前用 `grok-zh plugin validate [<path>]` 验证插件，用清单版本运行 `grok-zh plugin tag [<path>] [--push]` 标记发布。然后将仓库地址提供给其他人。他们添加一次市场，再安装所需插件：

```bash
grok-zh plugin marketplace add my-org/my-org-plugins   # GitHub 简写、git URL 或本地路径
grok-zh plugin install gdrive --trust
```

若要自动为所有人安装而不要求每个人逐一操作，请参阅[在组织中分发](#distribute-across-an-organization)。

---

<a id="distribute-across-an-organization"></a>
## 在组织中分发

管理员通过部署发送给每个用户的两个受管层控制插件、市场和 MCP 服务器：

- **`managed_config.toml`** 包含与用户 `config.toml` 相同的设置，并与其合并。用它向所有人提供市场并启用插件。
- **`managed-settings.json`** 是用于允许列表和默认值的受保护策略文件。其值优先于用户、项目和本地配置，且不能被覆盖。

<a id="roll-a-marketplace-out-to-everyone"></a>
### 向所有人推出市场

在 `managed_config.toml` 中添加源并启用所需插件：

```toml
[[marketplace.sources]]
name = "My Org Plugins"
git = "https://github.com/my-org/my-org-plugins.git"

# 插件在启用前保持关闭。列出插件名称（来自 `grok-zh plugin list`）
# 或完整 ID（`<scope>/<hash>/<name>`）。
[plugins]
enabled = ["gdrive"]
```

若要无人值守安装、无需每个人单独操作，还应将插件文件放在 Grok 会自动发现并信任的位置：`~/.grok/plugins/`，或放在设备管理工具管理的目录中，并通过 `[plugins].paths` 指向该目录。然后使用 `[plugins].enabled` 启用它们。

受管工作区还可以直接将技能同步给用户，无需插件。同步技能显示为 `server` 作用域，由工作区管理；用户自己的同名技能会遮蔽同步技能。参阅[技能](08-skills.md)。

<a id="restrict-which-marketplaces-can-be-added"></a>
### 限制可添加的市场

在 `managed-settings.json` 中列出用户唯一可以添加的源。任何其他市场都会被拒绝：

```json
{
  "strictKnownMarketplaces": [
    { "source": "git", "url": "git@github.enterprise.example:ACME/my-org-plugins.git" }
  ]
}
```

<a id="restrict-which-mcp-servers-can-run"></a>
### 限制可运行的 MCP 服务器

同样设置在 `managed-settings.json` 中。每个条目允许一个 HTTP 地址（可使用 `*` 通配符）或一个本地命令；未列出的内容一律拒绝：

```json
{
  "allowedMcpServers": [
    { "serverUrl": "https://*.example.com/*" },
    { "command": "npx" }
  ]
}
```

部署也可以直接向用户发送 MCP 服务器。允许列表限制任何配置（受管配置或个人配置）允许运行的内容。

<a id="require-pinned-versions"></a>
### 要求固定版本

拒绝任何未固定到完整提交 sha 的远程插件安装或更新：

```toml
[marketplace]
require_sha = true
```

也可以设置 `GROK_MARKETPLACE_REQUIRE_SHA=1`。两者都只会收紧策略，不会重新关闭它。请在市场的 `plugin-index.json` 中发布 `sha` 值，使从该市场安装的插件满足规则。直接在市场仓库中随附的插件会从该仓库的 checkout 复制，因此也要用 `plugin-index.json` 中的 `sha` 值为它们固定版本。

<a id="turn-off-the-plugins-ui"></a>
### 关闭插件 UI

要隐藏插件和钩子界面，请在 `pager.toml` 中设置：

```toml
disable_plugins = true
```

<a id="what-this-does-not-cover"></a>
### 这不涵盖的内容

市场分发 Grok 内容：技能、命令、智能体、钩子和 MCP 服务器配置。它们不会在机器上安装程序。运行辅助二进制文件（例如自定义登录工具）的技能或 MCP 服务器仍需要单独提供该二进制文件，可将其随部署一起打包，或通过设备管理工具推送。

---

<a id="troubleshooting"></a>
## 故障排查

**安装的插件没有显示。** 插件在启用前处于关闭状态。检查 `grok-zh plugin list`，然后将插件名称或 ID 添加到 `[plugins].enabled`，或在 Plugins 选项卡中按 `Space`。在 Plugins 选项卡中按 `r`，或启动新会话重新加载。

**插件的钩子或 MCP 服务器不运行。** 在插件受信任前它们保持不活动。使用 `--trust` 重新安装，或将插件放在 `~/.grok/plugins/` 下（自动受信任）。参阅[信任与安全](#trust-and-security)。

**市场中的技能或 MCP 服务器缺失。** 使用 `grok-zh plugin marketplace update` 刷新源，确认插件已安装并启用；如果组织限制了源，检查市场仍在允许范围内（参阅[在组织中分发](#distribute-across-an-organization)）。有些 MCP 服务器需要登录，在完成身份验证前不会出现。

**安装因未固定版本而被拒绝。** 你的部署要求固定提交。安装精确提交（`owner/repo@<sha>`），或使用 `plugin-index.json` 发布 `sha` 值的市场。参阅[要求固定版本](#require-pinned-versions)。

**查看具体加载了什么。** 运行 `grok-zh inspect`（添加 `--json` 获取机器可读输出），列出发现的每个插件，以及它提供的技能、智能体、钩子和 MCP 服务器；每项都会标有 `plugin: <name>` 来源。

---

<a id="reference"></a>
## 参考

<a id="what-a-plugin-contains"></a>
### 插件包含什么

插件是一个目录，可以包含以下任意组合：

- **技能：** 包含 SKILL.md 文件的 `skills/` 目录
- **斜杠命令：** `commands/` 目录
- **智能体：** `agents/` 目录
- **钩子：** `hooks/hooks.json` 文件
- **MCP 服务器：** `.mcp.json` 文件
- **LSP 服务器：** `.lsp.json` 文件

可选的 `plugin.json` 清单可以覆盖路径或添加元数据；没有清单时，Grok 会从这些标准目录发现组件。例如，`team-tools` 插件可以将部署技能、代码审查智能体、提交前钩子和 Linear MCP 服务器打包在一起，一步安装。

技能或命令可以在其 SKILL.md 旁附带**辅助脚本**（例如它调用的 Python 文件）。将脚本放进插件，并让技能通过相对路径运行它；脚本会随插件复制到机器上。脚本的运行时及其导入的任何包必须已存在；插件提供文件，不提供运行时或本机二进制文件（参阅[这不涵盖的内容](#what-this-does-not-cover)）。

<a id="where-grok-looks-for-plugins"></a>
### Grok 在哪里查找插件

Grok 按以下优先级从这些位置发现插件。`.claude/plugins/` 对应位置也可用；两个插件同名时，高优先级位置胜出：

| 位置 | 作用域 | 信任 |
|----------|-------|-------|
| `_meta.pluginDirs`（`session/new` / `session/load`） | 会话，仅此会话 | 自动受信任 |
| `--plugin-dir`（`grok agent … stdio` 标志） | 进程，仅此智能体进程 | 自动受信任 |
| `.grok/plugins/` | 项目，通过版本控制共享 | 需要信任 |
| `~/.grok/plugins/` | 用户，所有项目 | 自动受信任 |
| `[plugins].paths`（配置） | 你添加的自定义目录 | 取决于位置 |

`session/new` 和 `session/load` 请求中的 `_meta.pluginDirs` 字段会为单个会话加载插件；由于调用方提供目录，这些插件会自动受信任，且会话结束后不会持久化。`--plugin-dir` 是专用 `grok agent … stdio` 进程级等效项，可重复使用（`grok agent --no-leader --plugin-dir A --plugin-dir B stdio`），并在 leader 模式下忽略；leader 模式中共享 leader 会发现自己的插件。

<a id="environment-variables-in-plugin-hooks"></a>
### 插件钩子中的环境变量

除标准钩子环境外，插件钩子还会收到两个变量：

| 变量 | 说明 |
|----------|-------------|
| `GROK_PLUGIN_ROOT` | 插件安装目录的绝对路径。 |
| `GROK_PLUGIN_DATA` | 插件可写数据目录的绝对路径，用于状态、缓存和日志。 |

Grok 会设置这两个变量，并覆盖钩子 `env` 映射中同名的值（同时也会设置 `CLAUDE_PLUGIN_ROOT` 和 `CLAUDE_PLUGIN_DATA` 别名）。所有传递给钩子的变量请参阅[钩子指南](10-hooks.md)。

<a id="keyboard-shortcuts"></a>
### 键盘快捷键

以下按键在插件模态窗口的每个选项卡中都有效：

| 按键 | 操作 |
|-----|--------|
| `Tab` / `Shift+Tab` | 下一个 / 上一个选项卡 |
| `j` / `k` 或方向键 | 移动选择 |
| `Enter` | 展开或折叠选中项 |
| `/` | 按名称搜索当前选项卡 |
| `Esc` | 清除搜索，或关闭模态窗口 |
