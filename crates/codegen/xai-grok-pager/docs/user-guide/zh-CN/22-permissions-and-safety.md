# 权限与安全

控制 Grok 可以访问和执行的内容：权限模式、allow/ask/deny 规则、钩子，以及可选的操作系统级沙箱。

- **模式**决定 Grok 多久请求一次批准（always-approve、auto、ask 等）。
- **规则**在该基线之上决定哪些工具被允许、需要询问或被阻止。

---
<a id="permission-modes"></a>
## 权限模式

当 Grok 编辑文件、运行命令或调用外部工具时，可能会暂停等待批准。权限模式控制这种情况发生的频率。

模式设定一个基线。无论使用哪种模式，allow、ask 和 deny [规则](#configuring-permissions)仍会叠加生效。

### 入门选择

| 情况 | 模式 |
| --------- | ---- |
| 交互式 TUI | Default（ask），或使用 auto 以配合后台检查、减少提示 |
| 脚本、SDK、CI、代理服务器 | Always-approve；添加 [deny 规则](#configuring-permissions)或钩子来设置硬限制 |

```bash
grok-zh -p "Run the tests" --always-approve
grok-zh agent --always-approve stdio
grok-zh agent --always-approve serve --bind 127.0.0.1:2419 --secret <token>
```

ACP 客户端可以在 session/new 上设置 "_meta": { "yoloMode": true }。参见[代理模式](15-agent-mode.md#automation-and-sdks)。

### 可用模式

| 模式 | 无需询问即可运行的内容 | 适用场景 |
| ---- | ------------------------ | -------- |
| default（**ask**） | 只读工具和内置只读 shell 命令 | 日常交互使用 |
| acceptEdits | 无提示执行文件编辑 | 本地编码，之后再审查 diff |
| plan | 为兼容性保留；需要受门控的规划时使用[规划模式](19-plan-mode.md) | Claude 兼容设置 |
| auto | 安全检查允许的工作；其他调用被阻止或升级处理 | 希望减少提示的交互会话 |
| dontAsk | 仅预先批准的工具和内置只读处理 | 严格的 CI 允许列表 |
| bypassPermissions（**always-approve**） | 通常允许工具调用（deny 规则、钩子及部分 shell ask 规则仍适用） | 受信任的自动化和代理服务器 |

**Always-approve** 是产品名称；配置和 Claude 兼容设置可能使用 bypassPermissions 表示同一模式。Always-approve 与 auto 互斥（同时请求时 always-approve 优先）。

<a id="how-to-set-the-mode"></a>
### 如何设置模式

**交互式 TUI：** Shift+Tab / Ctrl+O、/always-approve 或 /auto，或者 /settings（[快捷键](03-keyboard-shortcuts.md)、[命令](04-slash-commands.md)）。

**CLI：**

```bash
grok-zh --always-approve -p "Run the test suite"
grok-zh --permission-mode auto
grok-zh agent --always-approve serve --bind 127.0.0.1:2419 --secret <token>
```

**配置：**

```toml
[ui]
permission_mode = "always-approve"   # or "auto", "ask", …
```

也支持 .claude/settings.json 中 Claude 兼容的 defaultMode（见[Claude 兼容设置](#3-claude-code-compatibility-claudesettingsjson)）。CLI 会为该进程覆盖配置。

<a id="always-approve"></a>
### Always-approve

跳过普通权限提示，让工具无需等待点击即可运行。deny 规则、钩子和部分 shell ask 规则仍然适用。管理员可以按下文所述锁定此模式。

| 机制 | 示例 |
| --------- | ------- |
| CLI | --always-approve（别名 --yolo），或 --permission-mode bypassPermissions |
| 配置 | [ui] permission_mode = "always-approve" |
| 交互式 | /always-approve、Ctrl+O |
| ACP | _meta.yoloMode: true on session/new |

#### 带硬限制的 Always-approve

自动化场景保留 always-approve，并为绝不应运行的路径或命令添加 deny 规则：

```toml
# project .grok/config.toml
[ui]
permission_mode = "always-approve"

[permission]
deny = [
  "Bash(rm -rf *)",
  "MCPTool(sales__delete_*)",
]
```

```bash
grok-zh -p "Deploy the service" --always-approve --deny 'Bash(rm -rf *)'
```

Deny 始终优先于 allow，也优先于 always-approve 的常规直通行为。参见[配置权限](#configuring-permissions)。

### Auto 模式

在许多工具调用运行前先进行检查，以减少交互提示。日常本地工作通常会继续；其他调用可能被阻止或升级。在非交互会话中，被阻止的调用会失败并报告给模型（例如 Auto mode blocked this action …）。grok-zh -p、agent stdio 和 agent serve 的行为相同。

若自动化必须在无交互批准的情况下运行工具，请使用 always-approve（如需硬阻止则添加 deny 规则），不要只使用 auto。

### 禁用 Always-approve（管理员）

组织可以阻止通过 CLI、TUI 或 /always-approve 启用 always-approve。在 requirements.toml 中设置（用户级位于 ~/.grok/，或系统级位于 /etc/grok/；强制执行时用户无法移除）：

```toml
[ui]
disable_bypass_permissions_mode = true
```

不要使用 permission_mode 来实现此锁定；该键是可切换的默认值。为兼容性，requirements.toml 中旧版的 [ui] yolo = false 键也会禁用 always-approve。

Grok 仍可从受管设置加载 Claude 风格的权限**规则**；always-approve 会按上面所示通过 requirements.toml 锁定。

---
<a id="how-a-tool-call-is-authorized"></a>
## 工具调用如何获得授权

当模型请求工具时，按以下顺序执行检查：

1. **PreToolUse 钩子**。钩子可以在任何其他检查前拒绝工具调用。允许调用的钩子不会跳过下面的检查，只是选择不拒绝。参见 [10-hooks.md](10-hooks.md)。

2. **权限规则**（来自配置文件或 --allow/--deny 标志）
   - 匹配的 deny 规则拒绝调用。deny 优先于所有其他规则。
   - 匹配的 ask 规则会提示你，包括原本会自动批准的文件读取、搜索和 shell 命令。
   - 匹配的 allow 规则批准调用。

3. **已记住的授权**。你在早先提示中保存的、按命令区分的批准在此生效，并限定于当前项目。已有授权可以满足 ask 规则而无需再次提示。[危险列表](#dangerous-commands)中的命令会再次提示，而不会使用已记住的前缀。参见[交互式批准及其保存位置](#interactive-approvals-and-where-they-persist)。

4. **内置自动批准**。只读工具和固定的一组只读 shell 命令无需提示即可运行（见下文）。

5. **提示策略**（由[权限模式](#permission-modes)设置）：提示你、自动批准或自动拒绝调用。

[Always-approve](#always-approve) 会在第 2 步之后短路此流程：匹配 shell 命令各分段的 deny 规则、钩子和 ask 规则仍适用，但不会查询已记住的授权（包括已记住的“永不允许”条目），而且非 shell 工具上的 ask 规则不会提示。

---

## 默认永不提示的操作

除非匹配的 deny 规则或钩子阻止，以下操作被视为只读，在包括 dontAsk 在内的每种模式中都无需提示即可运行。ask 规则会对文件读取、搜索和 shell 命令强制提示（见[工具调用如何获得授权](#how-a-tool-call-is-authorized)）。

### 只读工具

- read_file
- list_dir
- grep（内容搜索）
- web_search
- todo_write
- get_command_or_subagent_output / wait_commands_or_subagents / kill_command_or_subagent（子代理控制）
- 调用技能

### 只读 Shell 命令

将链式命令按 &&、||、; 和管道拆分后，以下命令在作为主命令出现时会被识别为只读。该列表按单词边界匹配，因此 ls 不会匹配 lsof 或 less。（你自己的 Bash(...) 规则匹配方式不同；见[规则匹配参考](#rule-matching-reference)。）

**文件系统（只读查看）：**
- ls、cat、pwd、date、whoami、hostname、uptime、ps
- head、tail、wc、sort、uniq、tr、cut

**Git（只读）：**
- git status、git branch、git log、git diff、git ls-files、git show、git rev-parse
- git blame、git describe、git merge-base、git shortlog
- git check-ignore、git check-attr、git cat-file、git ls-tree、git show-ref、git for-each-ref、git rev-list、git name-rev、git count-objects

**搜索和检查：**
- grep、rg（不包括 rg --pre / rg --pre=…，后者会为每个文件启动预处理器）

**Kubernetes（只读）：**
- kubectl get、kubectl logs、kubectl describe

> **注意：**此列表不包含 tee，因为它可以把输入写入任意文件。此列表也不包含 cargo check，因为它会编译并运行仓库中的 build.rs、proc-macros 以及任何 build.rustc-wrapper（在 Ask 模式下因此会提示；Auto 模式可能仍会将 cargo 作为项目代码运行器按启发式放行）。sort --compress-program=…（包括唯一的长选项缩写）、git -c / --config-env 覆盖，以及本地/工作树配置安装了可执行钩子的 git 命令（core.fsmonitor、diff.*.command/textconv/external 驱动，或 shell alias.<safe-subcommand> = !…）会提高请求级下限并提示，而不是自动批准；除非用户授予了那个完整且精确的脚本，或已启用 always-approve。

这些检查按分段应用。例如 ls && rm -rf / 中，ls 分段被识别为只读，但 rm 分段不在列表中。在 default 模式下 rm 分段会提示；在 dontAsk 下会被拒绝。

---

---

<a id="configuring-permissions"></a>
## 配置权限

Grok 从三个兼容来源读取权限规则。所有来源的规则会合并为一组；规则的效果取决于其操作（deny > ask > allow），而不是来源文件。

### 权限规则存放位置（作用域）

权限规则可以是全局的（所有项目）、项目范围的（一个仓库），或在项目内仅属于你的个人规则：

| 作用域 | 文件 | 与队友共享 |
|-------|------|-----------------------|
| 全局（所有项目） | ~/.grok/config.toml | 否 |
| 项目（已提交） | <project>/.grok/config.toml | 是（提交它） |
| 项目（个人） | <project>/.claude/settings.local.json | 否（加入 gitignore） |
| 交互式授权 | Grok 在内部按项目存储 | 否 |

作用域说明：

- Grok 会从仓库根目录到工作目录的每一级目录发现 .grok/config.toml，因此子目录可以在仓库根目录规则之上添加规则。
- 所有作用域的规则会合并为一组；deny > ask > allow 跨作用域适用，因此全局 deny 不能被项目 allow 覆盖。
- Grok 没有原生的 config.local.toml。项目中个人且不提交的规则请使用 .claude/settings.local.json；Grok 会直接读取它（见[Claude Code 兼容性](#3-claude-code-compatibility-claudesettingsjson)）。
- 交互式“始终允许”决策保存在仓库外，并限定于项目（见[交互式批准及其保存位置](#interactive-approvals-and-where-they-persist)）。

若要停止某个项目中特定命令的提示，请在该项目的 .grok/config.toml（或 .claude/settings.json）中添加范围窄的 allow 规则：

```toml
[permission]
allow = ["Bash(cargo test *)", "Bash(npm run build)"]
```

这只会批准列出的命令。相比之下，Always-approve 模式会批准所有工具调用。

### 1. CLI 标志

```bash
grok-zh -p "Review the API changes" \
  --allow 'Bash(git *)' \
  --allow 'Bash(gh *)' \
  --allow 'Read' \
  --allow 'Grep' \
  --deny 'Bash(rm -rf *)'
```

--allow RULE 和 --deny RULE 可以重复，并且始终会强制执行。

规则语法示例：
- Bash(git *) — 以 git 开头的任意命令
- Bash(npm run build) — 精确命令（或前缀）
- Bash(git commit:*) — cmd:* 后缀形式，等同于对 git commit 做前缀匹配
- Read(src/**) — 读取 src/ 下的内容
- Edit(**/*.rs) — 编辑任意 Rust 文件
- Grep — 所有 grep 操作
- MCPTool(my-server__*) — 来自指定服务器的 MCP 工具

确切匹配语义（包括链式命令和通配符的计算方式）见[规则匹配参考](#rule-matching-reference)。

### 2. 原生配置（~/.grok/config.toml 和 .grok/config.toml）

```toml
[permission]
rules = [
  { action = "allow", tool = "bash", pattern = "git *" },
  { action = "allow", tool = "bash", pattern = "gh *" },
  { action = "allow", tool = "read" },
  { action = "allow", tool = "grep" },
  { action = "deny",  tool = "bash", pattern = "rm -rf *" },  # block a dangerous pattern
  { action = "ask",   tool = "edit" },
]
```

结构化的 tool 字段接受小写名称 bash、read、edit、grep、mcp、webfetch 和 websearch，对应[工具名称](#tool-names)中的工具类别。

由于 deny 始终优先，不能把这些 allow 规则与针对 bash 的全匹配 deny 组合来表示“只允许 git/gh”；deny tool = "bash" 规则也会阻止 git 和 gh。要默认拒绝，请在 .claude/settings.json 使用 defaultMode: "dontAsk"，或使用 PreToolUse 钩子（见下文）。

全局 ~/.grok/config.toml 和每个项目 .grok/config.toml（从仓库根目录到工作目录）的规则会合并为一组，同时还会并入 .claude/settings.json 的规则。

组织部署的受管配置也会贡献 [permission] 规则：系统级 /etc/grok/managed_config.toml，以及 Grok 自动维护的用户级副本 ~/.grok/managed_config.toml。受管规则与任何其他来源的规则一样合并，但受管 allow 规则有两个特性：你自己的 deny 和 ask 规则（按严重性排序）会优先于受管 allow；当 always-approve 被锁定关闭时，全匹配受管 allow 会被忽略。对于用户无法编辑移除的规则，请使用 root 所有的系统文件 /etc/grok/requirements.toml。

所有来源的权限规则只在会话启动时读取一次。修改会在下一次会话生效。

原生 [permission] 部分还接受紧凑的 allow / deny / ask 字符串数组形式，使用与 --allow / --deny 标志和 .claude/settings.json 相同的规则字符串：

```toml
[permission]
deny = [
  "Read(/Users/you/private/**)",
  "Edit(/Users/you/private/**)",
  "Bash(rm -rf *)",
]
allow = [
  "Bash(git *)",
  "Bash(gh *)",
]
```

无论顺序或来源如何，deny 始终优先于 allow（计算顺序为 deny > ask > allow）。若还要在操作系统层面阻止读取项目之外的路径，请把 deny 规则与 strict 沙箱配置组合使用（见[18-sandbox.md](18-sandbox.md)）。
<a id="3-claude-code-compatibility-claudesettingsjson"></a>
### 3. Claude Code 兼容性（.claude/settings.json）

Grok 会读取 ~/.claude/settings.json 和 ~/.claude/settings.local.json，以及项目级 <project>/.claude/settings.json 和 settings.local.json（向上遍历至仓库根目录）。权限规则的原生 .grok 来源是上节描述的 config.toml。

示例：

```json
{
  "permissions": {
    "defaultMode": "dontAsk",
    "allow": [
      "Read",
      "Grep",
      "Bash(git *)",
      "Bash(gh *)"
    ],
    "deny": [
      "Bash(rm -rf *)"
    ]
  }
}
```

支持的 defaultMode 值包括 default、auto、acceptEdits、bypassPermissions、dontAsk 和 plan。Grok 从 permissions 下的规范位置读取 defaultMode；当嵌套键不存在时，也接受顶层 defaultMode。

permissions.allow、permissions.deny 和 permissions.ask 条目会转换为原生规则，然后按[规则匹配参考](#rule-matching-reference)中的语义匹配。转换说明：

- MCP 工具规则必须使用 MCPTool(server__tool) 形式；mcp__server__tool 形式永远不会匹配（见[MCP 规则](#mcp-rules)）。
- 命名未知工具的规则，以及 Agent(model:opus) 等参数规则，会带警告跳过，而不会导致加载失败。
- permissions.additionalDirectories 会被解析，但不受支持。

可以使用 **Ctrl+I**（“Import Claude settings”）以交互方式导入现有 Claude 设置。

---

<a id="rule-matching-reference"></a>
## 规则匹配参考

本节精确定义规则的匹配方式。

### Bash 规则

Bash(...) 模式以两种方式之一匹配命令：

- **前缀：**命令逐字符比较，必须以模式文本开头。不要求单词边界，因此 Bash(git) 会匹配 gitleaks 和 git status。添加尾随空格和通配符（Bash(git *)）可要求前缀为完整单词。
- **Glob：**模式作为 glob 匹配整个命令。* 可以出现在任意位置并匹配任意字符（包括空格和斜杠），因此 Bash(git * main) 会匹配 git checkout main。同时支持 ? 和 [...]。

匹配区分大小写。匹配前会去掉命令开头的空白；其他内容不做规范化。

Bash 规则末尾的 :* 后缀会被去掉，转为普通前缀：Bash(git commit:*) 变成前缀 git commit。由于前缀没有单词边界，写成 Bash(sed:*) 的 deny 也会阻止 sed-custom 等命令。

**链式命令。**Grok 像 shell 一样解析每条命令，并按 &&、||、;、| 和换行拆分。规则操作对分段的处理不同：

- deny 和 ask 规则会针对每个分段以及整个字符串检查。任何被拒绝的分段都会拒绝整个命令。
- allow 规则只针对完整命令字符串检查。因此 Bash(git *) 会自动批准 git status && rm -rf /，因为完整字符串以 git 开头。应将窄范围的 allow 规则与要阻止的模式的 deny 规则配对。

无法拆分成简单分段的命令（子 shell、命令替换 $(...)、反引号、后台 &、控制流）在配置了 Bash 限制时会作为单个单元提示。

分段级检查（deny 和 ask 规则、已记住的授权以及只读命令列表）会去除 RUST_LOG=debug 等环境变量前缀，并剥离一组固定的进程包装器（timeout、nice、ionice、chrt、stdbuf、env），使 deny 和 ask 规则能够匹配包装后的命令或内部命令。传给 bash -c 的内联脚本内部也会检查 deny 和 ask 规则。包括 sudo、xargs 和 nohup 在内的其他包装器不会被剥离；请把它们显式写入规则。allow 规则不做此处理：它们匹配原样的命令字符串，因此前导环境赋值或包装器会使 allow 规则无法匹配，命令转而提示。

<a id="dangerous-commands"></a>
### 危险命令

内置列表（rm、chmod、chown、chgrp、chattr、pkill、kill、killall、git push）即使某分段被已记住的命令前缀或只读命令列表覆盖，也会提示。配置中的显式 allow 规则可以批准它们；always-approve 模式也会像其他命令一样自动批准它们；要无条件阻止请使用 deny 规则。在将类似 Bash(rm *) 的规则添加为 allow 规则前请仔细审查。

### Read、Edit 和 Grep 规则

路径模式是针对工具路径（经过词法规范化后：折叠 ./..；相对路径与会话工作目录拼接）的 glob。以 ~ 开头的工具路径按字面匹配——绝不会与工作目录拼接——因为工具仅在权限检查后才展开 ~ 到主目录：

- * 和 ? 不跨越 /；** 可以。Read(src/*) 匹配 src/main.rs，但不匹配 src/nested/mod.rs；整个树请使用 Read(src/**)。
- 裸文件名只匹配该精确字符串。要匹配任意深度的 .env，请使用 **/.env。
- 没有锚点前缀：模式开头的 // 或 ~/ 会被视为字面 glob 文本。请改写为绝对路径模式或 **/ 模式。
- 由于匹配前会折叠 ./..，根定模式无法通过遍历逃逸：Read(./**) 限定于工作目录（裸相对路径如 src/main.rs 会匹配；./../../etc/passwd 不会），Read(src/**) 始终停留在 src/ 下。无根模式（*，或以 ** 开头的 **/*.rs）按设计在任意深度、任意位置匹配。
- Read 规则也控制 grep 搜索；Grep(...) 规则只匹配 grep。

Read 和 Edit 的 deny 规则还会应用于 shell 命令操作的文件路径（例如在被拒绝路径上执行 cat 或 sed），包括通过 -c 传给 bash、sh、dash、zsh 或 ksh 的字面内联脚本；该 shell 层检查使用同样考虑工作目录的规范化（工作目录下的绝对操作数也会匹配 Read(src/**) 等根定规则），并且还会解析符号链接。直接的 read_file/search_replace 工具检查不会解析符号链接。若要在操作系统层面覆盖每个进程，请将 deny 规则与沙箱组合（见[18-sandbox.md](18-sandbox.md)）。

<a id="mcp-rules"></a>
### MCP 规则

MCPTool(...) 模式以 server__tool 形式匹配完整的 Grok 工具名称，并支持 glob：MCPTool(linear__*) 匹配 linear 服务器的每个工具。Grok 工具名称没有 mcp__ 前缀，因此写成 mcp__server__tool 的规则永远不会匹配 MCP 调用；应写为 MCPTool(server__tool)。

### WebFetch 规则

- WebFetch(domain:example.com) 匹配该主机及其所有子域名（api.example.com），不区分大小写并忽略前导 www.。domain: 模式内部不支持通配符。
- 不带 domain: 前缀的模式会对整个 URL 做 glob：WebFetch(https://api.example.com/*)。

<a id="tool-names"></a>
### 工具名称

可识别的工具名称：Bash、Read、Edit（以及 Write）、Grep（以及 Glob）、MCPTool、WebFetch、WebSearch。裸 * 规则匹配每个工具。工具名称位置不支持 glob。

命名未知工具的规则（例如 Agent(model:opus)）会带警告跳过，而不会导致加载失败。

### 计算顺序

所有来源的规则会合并为一组，并按严重性而非顺序计算：任意匹配的 deny 都拒绝；否则任意匹配的 ask 会提示；否则任意匹配的 allow 会批准。当没有规则匹配时，请求按[工具调用如何获得授权](#how-a-tool-call-is-authorized)所述继续经过内置自动批准和提示策略。

---

<a id="interactive-approvals-and-where-they-persist"></a>
## 交互式批准及其保存位置

当工具调用需要批准时，权限提示提供以下选择：

- **Allow once：**批准这一次调用。
- **Reject once：**拒绝这一次调用，可选地向模型返回消息。
- **Enable always-approve mode：**批准以后所有工具调用，而不仅是当前提示的调用。
- **Allow all edits this session：**文件编辑时显示。此授权只保存在内存中，重启后不会保留。

### 按命令的“始终允许”

更窄的一组选项只记住当前提示的特定命令、MCP 工具或 web-fetch 域名，例如“Always allow cargo test”。这些行默认关闭。使用以下配置启用：

```toml
# ~/.grok/config.toml
[ui]
remember_tool_approvals = true
```

启用该开关后，提示会增加：

- **Always allow: <command>**，为该命令前缀持久化 allow。
- 对应的“never allow”行，以相同方式持久化 deny。
- MCP 工具和 web-fetch 域名也有等效的“always allow”行。

记住的前缀被限制为命令的短形式：只读命令只保存其列出的前缀（例如 git status，而不是完整参数列表），其他命令保存简短的开头前缀。确认前提示会准确显示将记住的内容。[危险列表](#dangerous-commands)中的命令会再次提示，而不使用已记住的前缀。

### 按项目持久化

交互式授权保存在主目录下 Grok 自己的状态目录中，作用域是启动 Grok 的目录。在一个项目中创建的授权不会应用于另一个项目，授权不会写入仓库，也不建议手动编辑。

交互式授权是个人、按机器保存的状态。若要使用可在代码审查中查看并与队友共享的允许列表，请改用项目 .grok/config.toml 中的声明式规则。

---

## 使用钩子将 Bash 限制为特定命令

PreToolUse 钩子可以对 Bash 工具强制执行允许列表，并在每种权限模式下生效。钩子在权限系统之前计算；钩子拒绝会停止调用，钩子允许则继续正常权限检查（因此你的 deny 规则仍会生效）。

> **注意：**钩子采用失败开放策略。若钩子脚本崩溃、超时或缺失，工具调用会像钩子允许一样继续，并在 UI 中报告失败。作为安全边界使用的钩子必须自行处理错误，并且必须考虑链式命令，如下例所示。参见 [10-hooks.md](10-hooks.md)。

### 示例：仅允许 git 和 gh

**~/.grok/hooks/git-gh-only.json**

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "git-gh-only.sh",
            "timeout": 5
          }
        ]
      }
    ]
  }
}
```

**~/.grok/hooks/git-gh-only.sh**

```bash
#!/bin/sh
# Allow only git and gh commands, including within chained commands.

set -eu

deny() {
  echo '{"decision": "deny", "reason": "'"$1"'"}'
  exit 2
}

INPUT=$(cat)
CMD=$(echo "$INPUT" | jq -r '.toolInput.command // empty')

[ -n "$CMD" ] || deny "Empty command is not allowed"

# Normalize '&&' and '||' to ';' so chains can be checked segment by
# segment, then reject constructs this script cannot inspect.
CMD=$(echo "$CMD" | sed 's/&&/;/g; s/||/;/g')
case "$CMD" in
  *'$('*|*'`'*|*'&'*|*'>'*|*'<'*) deny "Substitution, background, and redirection are not permitted" ;;
esac

# Split on the separators and require every segment to start with git or gh.
echo "$CMD" | tr ';|' '\n\n' | while IFS= read -r SEGMENT; do
  SEGMENT=$(echo "$SEGMENT" | sed 's/^[[:space:]]*//')
  [ -n "$SEGMENT" ] || continue
  case "$SEGMENT" in
    git\ *|git|gh\ *|gh) ;;
    *) deny "Only git and gh commands are permitted. Blocked segment: $SEGMENT" ;;
  esac
done
```

```bash
chmod +x ~/.grok/hooks/git-gh-only.sh
```

此钩子会拒绝每条 Bash 命令，除非每个链式分段都以 git 或 gh 开头；还会直接拒绝命令替换、后台运行和重定向，因为它无法验证这些构造实际执行的内容。它在每种权限模式下都有效。

有关钩子安装、JSON 格式、项目钩子的信任模型以及其他事件，见 [10-hooks.md](10-hooks.md)，其中也有互补的“阻止危险模式”示例。

---

## 配置示例

### 仅限无头 git 和 gh（CI 与自动化）

```bash
grok-zh -p "Implement the feature using only git and GitHub CLI" \
  --allow 'Read' \
  --allow 'Grep' \
  --allow 'Bash(git *)' \
  --allow 'Bash(gh *)'
```

安装上面的 git-gh-only 钩子即可拒绝所有其他 Bash 命令。要对所有工具默认拒绝，还应在 .claude/settings.json 设置 {"permissions": {"defaultMode": "dontAsk"}}。

### 只读代码审查者

```toml
# .grok/config.toml
[permission]
rules = [
  { action = "allow", tool = "read" },
  { action = "allow", tool = "grep" },
  { action = "deny",  tool = "edit" },
  { action = "deny",  tool = "bash" },
]
```

### 交互式开发

使用 default 模式，并为最常运行的命令（git、cargo test、rg 等）添加窄范围的 Bash(...) allow 规则。

---

## 与沙箱组合

权限控制模型被允许请求的内容。操作系统级沙箱（见 [18-sandbox.md](18-sandbox.md)）控制命令获批后进程实际能够执行的内容。

不受信任代码的推荐组合：

1. dontAsk 加窄范围 allow 规则，或使用限制性钩子
2. --sandbox strict 或自定义配置
3. 项目信任，以及审查任何 SessionStart 钩子

---

## 在 TUI 中管理权限

- 权限决策会显示在记录中。
- /always-approve 命令切换 always-approve 模式；其他模式通过 defaultMode 设置（见[如何设置模式](#how-to-set-the-mode)）。
- 使用 [ui] remember_tool_approvals = true 时，权限提示会包含仅对当前项目持久化的按命令“Always allow”选项。见[交互式批准及其保存位置](#interactive-approvals-and-where-they-persist)。
- 要管理钩子和插件，请运行 /hooks 或 /plugins（在大多数终端中，**Ctrl+L** 也会打开扩展模态框；在 VS Code、Cursor、Windsurf 和 Zed 中，Ctrl+L 是回合中插话操作）。参见 [10-hooks.md](10-hooks.md)。

---

## 最佳实践

1. **优先使用窄范围模式。**Bash(git *) 比裸 Bash allow 规则授予的访问权限更少。
2. **组合多层防护。**dontAsk、窄范围 allow 规则、限制性钩子和沙箱会分别施加限制。
3. **审查来自不熟悉来源的项目配置。**.grok/config.toml 和 .claude/settings.json 中的项目权限规则（包括 allow 规则）无需单独的信任提示即可生效。在不熟悉的 checkout 中工作前，审查这些规则以及项目钩子（见 [10-hooks.md](10-hooks.md) 的安全说明）。
4. **测试策略。**设置 defaultMode: "dontAsk"（或安装 PreToolUse 钩子）后，运行代表性命令并确认哪些会被阻止。
5. **把只读命令列表当作便利功能，而不是安全边界。**

---

## 另请参阅

- [钩子](10-hooks.md) — PreToolUse 及其他生命周期脚本
- [无头模式](14-headless-mode.md) — 一次性 CLI 和自动化标志
- [代理模式](15-agent-mode.md) — ACP、stdio 和代理服务器
- [沙箱](18-sandbox.md) — 操作系统级隔离配置
- [配置](05-configuration.md) — 原生 config.toml 结构
