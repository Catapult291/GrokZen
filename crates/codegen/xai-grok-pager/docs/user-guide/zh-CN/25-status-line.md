# 状态栏

状态栏是全屏分页器底部、快捷键栏上方的一行可选内容，默认关闭。它可以显示模型、上下文窗口用量、费用、目录和 Git 工作树等实时会话信息，也可以显示自定义脚本的输出。在 `~/.grok/config.toml` 中添加 `[ui.status_line]` 即可启用。

## 设置

### 内置状态栏

```toml
[ui.status_line]
type = "builtin"
items = ["cwd", "model", "context"]   # 省略时的默认值
```

例如，它会显示 `grok-shell-status-line │ Grok 4.5 │ 12% 上下文`。各项按配置顺序排列，过长内容会以 `…` 省略：目录和会话名称最多 40 列，模型名称最多 30 列。

| 项目 | 显示内容 |
| --- | --- |
| `cwd` | 当前目录（最后一级目录名） |
| `model` | 模型显示名称 |
| `context` | 上下文窗口使用百分比；达到自动压缩阈值时显示琥珀色，智能体未报告阈值时则以 80% 为界 |
| `cost` | 会话费用；低于 0.005 美元时隐藏，避免显示具有误导性的 `$0.00` |
| `turn-timer` | 当前回合的已用时间，从运行满一秒后开始显示 |
| `session-name` | 已设置的会话名称 |

### 命令状态栏

将 `command` 指向脚本。Grok 会把 [JSON](#available-data) 通过标准输入传给脚本，并显示其标准输出。路径开头的 `~/` 会展开为用户主目录。

```toml
[ui.status_line]
type = "command"
command = "~/.grok/statusline.sh"
```

字段名称和嵌套结构遵循通用状态栏约定，因此移植脚本通常只需少量修改。下方表格未列出的内容不会发送。

### 关闭

默认的 `type = "disabled"` 不显示任何内容；`off`、`none` 和 `hidden` 也会按 `disabled` 处理。

### 选项

| 键 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `type` | string | `disabled` | `builtin`、`command` 或 `disabled`。 |
| `items` | array | `["cwd", "model", "context"]` | 按顺序显示的内置分段。 |
| `command` | string | 无 | `type = "command"` 时运行的脚本。 |
| `padding` | integer | `0` | 每侧的水平留白字符数，上限为 16。若留白大到不剩可用列，系统仍保留该行，但不会绘制内容。 |
| `refresh_interval` | integer | 未设置 | 仅用于 `command` 状态栏，单位为秒，范围 1–86,400。即使状态没有变化，也按该间隔重新运行脚本，让空闲会话能够显示告警页面或 CI 状态等变化。未设置时只由事件触发。定时运行的载荷含 `"trigger": "poll"`，失败时会保留上次输出，而不直接绘制错误（见[轮询](#polling)）。访问网络的脚本应使用较长间隔，并在 `state` 运行中读取缓存。 |

## 工作方式

- **刷新。**会话状态变化（会话启动、回合结束、切换模型或推理强度、HEAD 移动、压缩、客户端连接）时状态栏会刷新，回合运行期间也会持续刷新，并非固定定时。空闲会话不会自动重跑脚本，因此脚本中的时钟不会自行走动；设置 `refresh_interval` 后，会在这些事件之外增加定时刷新。更新固定防抖 300 毫秒，繁忙回合不会每帧运行脚本；必须尽快呈现的变化（调整窗口大小、新快照、切换智能体）只等待 100 毫秒。正在运行的脚本绝不会被取消，下一次变化会等它结束。Grok 在启动时读取 `[ui.status_line]`，修改后需下次启动才会生效。
- **输出。**脚本输出的每一行都会成为状态栏的一行，最多五行；每行最多 1024 个字符，ANSI 转义序列也计入长度，因此颜色越多，文本空间越少。终端高度不足时会从底部丢弃多余行。支持 ANSI 颜色；其他转义（光标移动、清行、回车覆盖）会被过滤。支持指向 `http`、`https` 和 `mailto` 的 OSC 8 超链接，其他目标只显示纯文本。标准输出超过 64 KiB 时会被截断并终止脚本。脚本成功但不输出内容时，状态栏会消失，而不会回退到内置分段；因此只在某些情况下输出的脚本会让记录区随状态栏出现或消失而上下移动一行。
- **尺寸。**Grok 将 `COLUMNS` 和 `LINES` 设为脚本输出可占用的状态栏尺寸，而不是整个窗口尺寸；窗格留白和你设置的 `padding` 已经扣除。由于标准输出不是终端，`tput` 也会读取这些值。`LINES` 表示状态栏当前占用的行数，而不是可增长到的行数，因此在输出更多内容前为 `1`，最大始终为五。状态栏首次绘制前，或当前帧没有空间时，尺寸沿用上次绘制值；从未绘制过则使用 80x1。
- **Shell。**`command` 是一条 shell 命令行，因此 `jq -r '…'` 和管道可以照常使用。如果内容是可执行文件路径，则直接运行；否则通过 `sh -c` 运行，这也能处理缺失或写错 `#!` 行的脚本。路径含空格时应像在命令提示符中一样加引号。每次运行都是新进程，因此脚本文件的修改会在下一次运行时生效。
- **后台任务不会保留。**脚本遗留的进程会在本次运行结束时被终止，无论脚本正常退出、超时还是输出过多。脚本退出即表示本次运行结束，后台任务随后输出的内容会丢失。
- **环境。**脚本会依次选择会话工作目录、仓库根目录、分页器自身目录中第一个本地路径作为工作目录，超时时间为 10 秒；超时后状态栏显示 `[状态栏：已超时]`。`COLUMNS` 和 `LINES` 描述状态栏，而不是窗口。不加载 shell rc 文件（会清除 `BASH_ENV` 和 `ENV`），并设置 `GIT_OPTIONAL_LOCKS=0`。分页器和编辑器环境也会像 Grok 运行其他命令时一样被禁用，因此脚本中的 `git` 或 `gh` 不会阻塞等待交互。
- **输入。**JSON 载荷写入标准输入，并以换行结尾，因此 `read -r line` 和 `input=$(cat)` 都可使用。

<a id="polling"></a>
## 轮询

为 `command` 状态栏设置 `refresh_interval` 后，脚本还会按计时器重跑，让告警页面或 CI 状态在会话空闲时也能更新：

```toml
[ui.status_line]
type = "command"
command = "~/.grok/statusline.sh"
refresh_interval = 300   # 秒
```

- **载荷会说明运行原因。**计时器触发的运行带有 `"trigger": "poll"`；若轮询到期时恰好发生状态变化，该变化会随本次轮询运行一起处理。没有待处理轮询时，运行带有 `"trigger": "state"`。请只在 `poll` 时访问网络，在 `state` 时读取缓存，否则持续重跑脚本的繁忙回合会对目标服务造成请求风暴。
- **载荷是 Grok 上次发送的状态。**轮询会用最近一次状态变化时的载荷重跑脚本，因此其中的费用、上下文和 token 等会话数值截至该次变化，而非当前轮询时刻。只有脚本自行获取的内容是最新的。
- **轮询失败会保留上次输出。**轮询运行失败或超时时，状态栏继续显示上次内容（上次输出，或状态运行已经绘制的失败），并把失败写入 `~/.grok/logs/unified.jsonl`，避免不稳定端点在空闲时覆盖正常内容。连续三次轮询失败表示脚本本身可能损坏，此时会显示错误；状态栏尚无任何内容时的首次轮询失败也会立即显示，因为没有旧内容可保留。会话状态触发的运行仍会立即报告失败。
- **错过的轮询会合并。**状态栏隐藏（例如全屏子智能体视图或欢迎屏幕）或已有脚本占用运行槽时，轮询会等待，并在可运行时只补一次，绝不会为暂停或长回合期间错过的每次轮询集中爆发。计时器节奏不受脚本耗时影响；脚本尚未结束时到期的轮询会并入下一次运行，而不是排队堆积。
- **计时器只属于运行脚本的模式。**`builtin` 下的 `refresh_interval` 不会创建定时任务，但会由 `grok-zh inspect` 报告；`disabled` 下则随全部状态栏功能一起关闭。

<a id="available-data"></a>
## 可用数据

移植脚本时请特别注意以下三点：`workspace.repo_root` 是仓库根目录，不存在其他系统用于表示启动目录的 `project_dir`；`context_window.session_usage` 和 `session_*` token 计数是整个会话的累计值，而实时窗口大小由 `context_window.context_tokens` 表示；Grok 没有额外会话目录列表。移植脚本读取这些不存在的值时会得到空值，而不是错误答案，因此请对使用的字段做好缺失处理。

下表之外的内容一律不会发送。若移植脚本读取智能体修改行数、限额摘要、编辑器模式、思考或快速模式标记、输出样式、拉取请求、额外会话目录或工作树来源目录，这些字段都会缺失：它们要么不是 Grok 的功能，要么没有可靠的数据来源。

| 字段 | 说明 |
| --- | --- |
| `cwd`, `session_id` | 工作目录和唯一会话 ID |
| `session_name` | 客户端填写的会话标签页名称。命令脚本的标准输入中存在，`SessionStatus` 通知中不存在 |
| `prompt_id` | 当前正在处理的提示 UUID，仅在回合期间存在 |
| `transcript_path` | 会话 `updates.jsonl` 的路径，供脚本直接读取记录 |
| `model.id`, `model.display_name` | 模型标识和显示名称；智能体无法读取会话模型时省略 |
| `workspace.current_dir` | 当前目录 |
| `workspace.repo_root` | 仓库根目录；仓库外省略。它不是其他系统中表示启动目录的 `project_dir` |
| `workspace.branch` | 任意仓库中当前检出的分支；分离 HEAD 时省略 |
| `workspace.git_worktree` | 位于链接工作树中时的工作树名称 |
| `workspace.repo.{host,owner,name}` | 在 Git 仓库中从 `origin` 远端解析；远端没有 owner 路径段时省略 `owner` |
| `schema_version` | 载荷结构版本。新增字段不会提升版本，删除字段或改变字段类型才会。请用 `>=` 测试，并按它而不是 `version` 分支 |
| `version` | 用于显示的 Grok 版本 |
| `cost.total_duration_ms` | 当前进程连接会话后的毫秒数；恢复会话从恢复时重新计时，费用也同样处理 |
| `cost.total_cost_usd`, `cost.total_api_duration_ms` | 会话费用和等待 API 的毫秒数。会话尚无可定价内容或用量账本不可读时会省略费用；缺失表示未知，而不是零 |
| `context_window.context_window_size` | 最大上下文 token 数；尚未知晓模型窗口时省略 |
| `context_window.context_tokens` | 对话当前占用的输入 token 数，因此压缩后会下降。智能体无法读取时省略，所以 `0` 始终表示空上下文 |
| `context_window.session_input_tokens`, `.session_output_tokens` | 整个会话累计计费值，只增不减。名称含 `session` 是因为它们统计整个会话；其他位置的 `total_*` 表示当前窗口，而此处由 `context_tokens` 表示。用它们除以窗口大小可能超过 100%。用量账本不可读时省略 |
| `context_window.used_percentage`, `.remaining_percentage` | 当前窗口的已用/剩余整数百分比，范围 0–100。窗口大小或 token 数未知时省略 |
| `context_window.session_usage.{input_tokens,output_tokens,cache_creation_input_tokens,cache_read_input_tokens}` | `input_tokens`、`cache_creation_input_tokens`、`cache_read_input_tokens` 相加等于 `session_input_tokens`，另含 `output_tokens`。按整个会话累计，而非单个回合；首次调用前省略 |
| `context_window.auto_compact_threshold_percent` | 会话自动压缩阈值；智能体未报告时省略 |
| `effort.level` | 模型支持时的推理强度 |
| `turn.started_at_ms` | 当前回合开始时的 Unix 毫秒时间戳；回合之间省略。可用自己的时钟相减得到已用时间 |
| `worktree.{name,path,branch,main_worktree_root}` | 链接工作树中的活动工作树；工作树位于文件系统根目录时省略 `name`，`main_worktree_root` 是该工作树的分支来源 |
| `trigger` | 本次运行原因：`refresh_interval` 计时器请求的运行为 `poll`，其他为 `state`。命令状态栏的标准输入中存在，描述会话而非单次运行的 `SessionStatus` 通知中不存在 |

Grok 无法可靠获取的字段会直接省略，而不会发送占位值，状态栏因此不会显示伪造数据。务必处理缺失字段：`jq -r` 会把缺失键打印成字面量 `null`，因此可使用 `// 0` 或 `// "?"`；JavaScript 中可使用 `?.`。

## 示例

保存脚本（例如 `~/.grok/statusline.sh`），用 `chmod +x` 设为可执行，再将其设为 `command`。以下示例使用 [`jq`](https://jqlang.org/)；Python 和 Node.js 可以原生解析 JSON。载荷不含脏文件数量，因此脚本自行调用 `git` 获取。

```bash
#!/bin/bash
input=$(cat)
DIR=$(echo "$input" | jq -r '.workspace.current_dir')
MODEL=$(echo "$input" | jq -r '.model.display_name // "?"')
PCT=$(echo "$input" | jq -r '.context_window.used_percentage // 0')
BRANCH=$(echo "$input" | jq -r '.workspace.branch // "detached"')
DIRTY=$(git diff --numstat 2>/dev/null | wc -l | tr -d ' ')
printf '%b\n' "${DIR##*/} │ $MODEL │ ${PCT}% 上下文 │ \033[32m$BRANCH\033[0m ~$DIRTY"
```

## 提示

- 可用模拟输入测试：`echo '{"session_id":"t","workspace":{"current_dir":"/tmp/demo"},"model":{"display_name":"Grok 4.5"},"context_window":{"used_percentage":25}}' | ./statusline.sh`
- `git status` 等较慢命令可缓存到以 `session_id` 为键的临时文件中，并每隔几秒刷新。`session_id` 在单个会话内稳定，且各会话之间唯一。
- 使用 `printf '%b'` 而不是 `echo -e`，以可靠处理转义序列。

## 故障排除

- **没有显示任何内容。**Grok 在启动时读取 `[ui.status_line]`，编辑 `config.toml` 后请重启。只需重启客户端：新客户端连接后，智能体会为仍在运行的会话开启状态栏。状态栏只在智能体视图激活后的全屏分页器中绘制，不会出现在欢迎屏幕、最小模式或全屏子智能体视图中。请确认 `type` 不是 `disabled`，并确认命令脚本可执行且会写入标准输出。
- **状态栏中出现提示。**以 `[ui.status_line]` 开头的内容表示 Grok 无法按当前写法使用该配置段：提示会指出无法读取的键，或当前模式还缺少的内容。`grok-zh inspect` 会列出相同问题，包括当前版本不认识的键；当状态栏被关闭时可在此检查。能够读取的配置仍然生效，Grok 不会重写无法读取的配置段。设置 `type = "disabled"` 可移除状态栏及提示。
- **空白行始终没有内容。**智能体没有发送状态更新，通常是因为 `grok-zh` 或 leader 进程比当前客户端旧。请重启 leader 或更新 Grok。
- **只有你自己的配置可以设置此功能。**命令状态栏会运行程序，因此只能从你的 `~/.grok/config.toml` 或管理员托管的配置读取。仓库无法设置命令状态栏：仓库级 `.grok/config.toml` 只读取 MCP 服务器，`[ui.status_line]` 不属于项目作用域可提供的键，所以克隆仓库不会使 Grok 运行其中的脚本。
- **推送的配置没有生效。**campaign 和版本覆盖补丁会移除 `[ui.status_line]`，因为状态栏可以指定本机要执行的命令。请在自己的 `config.toml` 中设置。
- **错误。**脚本打印的所有内容都会显示，即使退出码非零，因此 `printf …; [[ -n $dirty ]]` 会按预期工作。脚本不输出内容且失败时显示 `[状态栏：退出 N]`，并保持到下次成功运行；会话状态触发的运行会立即报告失败，而轮询失败会保留上次输出（见[轮询](#polling)）。脚本的标准错误绝不会绘制到状态栏，因此调试用 `echo` 不会干扰显示；使用 `--debug` 运行 Grok 可查看。完全无法启动脚本时显示 `[状态栏：无法启动脚本：…]`，例如文件没有可执行位；被系统终止时显示 `[状态栏：被信号终止]`。若 `#!` 指向不存在的解释器，系统会改用 `sh` 重试，因此最终显示退出码。
