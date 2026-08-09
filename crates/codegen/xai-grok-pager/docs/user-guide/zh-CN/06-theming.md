<a id="theming-and-appearance-customization"></a>
# 主题与外观自定义

Grok Build 从集中式主题读取 TUI 的所有颜色。你可以在 Grok 运行时切换主题，跟随操作系统的浅色或深色外观，并通过配置文件调整回滚区布局、动画和块样式。

---

<a id="available-themes"></a>
## 可用主题

Grok 内置五种主题，另有跟随系统外观的 `auto` 选项：

| 主题 | 配置名称 | 说明 | 需要 Truecolor |
|-------|-------------|-------------|--------------------|
| **GrokNight** | `groknight`、`grok-night`、`dark` | 中性的深色底色，带洋红强调色。默认主题。在 256 色和 16 色终端上量化后仍能保持良好效果。 | No |
| **GrokDay** | `grokday`、`grok-day`、`light`、`day` | 适合明亮终端背景的浅色主题。 | No |
| **TokyoNight** | `tokyonight`、`tokyo-night`、`tokyo` | 采用 Tokyo Night 调色板的深色蓝调背景。量化后会失去其特色。 | Yes |
| **RosePineMoon** | `rosepine`、`rose-pine`、`rosepine-moon`、`rose-pine-moon` | 来自 Rosé Pine 系列的柔和深色调色板，带灰紫色强调。 | Yes |
| **OscuraMidnight** | `oscura`、`oscura-midnight` | 深色底色，带紫色强调。 | Yes |

主题名称不区分大小写。`auto` 选项（别名 `system`）在[自动主题（系统外观）](#auto-theme-system-appearance)中说明。

<a id="minimal-mode-has-no-theming"></a>
### 精简模式不支持主题

**精简模式**（`--minimal`）始终使用单一固定的终端原生调色板，完全忽略 `theme` 设置（这些设置仍适用于完整 TUI）。精简模式直接绘制到终端自身背景，因此使用终端的默认前景色/背景色和 16 色 ANSI 调色板——与 `git` 或 `ls` 使用的颜色相同——无需检测或配置，在任何浅色或深色终端配置文件中都保持可读。精简模式下无法使用 `/theme` 以及 `/settings` 中的主题行。

精简模式中的语法高亮**不会**在浅色和深色主题文件之间切换（有意避免极性检测）。近灰色 token 会继承终端默认前景色；彩色 token 使用基础 ANSI 强调色（红/绿/黄/蓝/洋红/青），因此读取文件的输出和围栏代码在浅色、深色配置文件中都清晰可读。

---

<a id="switching-themes"></a>
## 切换主题

<a id="in-the-tui"></a>
### 在 TUI 中

运行 `/theme` 斜杠命令（别名 `/t`）打开主题选择器。使用方向键在列表中移动时，Grok 会实时预览每个主题。按 Enter 应用并保存选择，或按 Escape 恢复原主题。

无需打开选择器也可直接传入名称切换：

```
/theme tokyonight
```

单独提交 `/theme`（不从选择器中选择）会循环切换到下一个主题。

<a id="via-config-file"></a>
### 通过配置文件

在 `~/.grok/config.toml` 中设置主题：

```toml
[ui]
theme = "tokyonight"
```

---

<a id="auto-theme-system-appearance"></a>
## 自动主题（系统外观）

将 `theme = "auto"` 设置为让 Grok 跟随操作系统的浅色/深色外观并自动切换主题：

```toml
[ui]
theme = "auto"
```

默认情况下，深色模式映射到 **GrokNight**，浅色模式映射到 **GrokDay**。可通过 `auto_dark_theme` 和 `auto_light_theme` 覆盖任一映射：

```toml
[ui]
theme = "auto"
auto_dark_theme = "tokyonight"
auto_light_theme = "grokday"
```

`theme = "system"` 是 `theme = "auto"` 的别名。

<a id="how-detection-works"></a>
### 检测工作原理

| 平台 | 方法 |
|----------|--------|
| **macOS** | 读取 `AppleInterfaceStyle` 系统偏好 |
| **Linux** | 查询 XDG Desktop Portal（`org.freedesktop.appearance.color-scheme`） |
| **Windows** | 读取系统个性化注册表 |
| **SSH / tmux / 无头模式** | 依次使用 `GROK_APPEARANCE` 或 `LC_GROK_APPEARANCE`（`dark`/`light`）、`COLORFGBG`，然后执行启动时 OSC 11 背景查询。`grok-zh wrap ssh …` 会根据本地操作系统主题写入 `LC_GROK_APPEARANCE`，使其在 SSH 进入登录 Shell 后仍然存在。只有在 tmux 服务器/会话创建时携带该环境变量（或 `update-environment` 包含它）时，新 tmux 会话才会继承。tmux ≥ 3.3 且 tmux 是直接终端（而非编辑器的 `:terminal`）时，OSC 11 会用 DCS 包裹；要到达外层模拟器还需要 `allow-passthrough`，并且回复是尽力获取的。 |

运行后，Grok 每 5 秒轮询桌面 API 和环境提示。在本地桌面将操作系统从浅色切换为深色或反之，几秒内即可生效，无需重启。通过 SSH 时，包装命令写入的环境变量在该跳连接中固定。

也可以设置 `GROK_THEME`（或 `LC_GROK_THEME`），在不编辑 `config.toml` 的情况下强制指定主题或 `auto`。

<a id="via-the-settings-pane"></a>
### 通过设置窗格

运行 `/settings`（别名 `/config`），打开**外观**类别即可交互设置**自动深色主题**和**自动浅色主题**。在 `/theme` 选择器中选择 `auto` 会使用这些映射启用自动模式。

---

<a id="color-support-detection"></a>
## 颜色支持检测

启动时，Grok 会检测终端的颜色能力级别：

| 级别 | 说明 | 检测方式 |
|-------|-------------|-----------|
| **Truecolor**（24 位） | 完整 RGB 颜色。所有主题按设计渲染。 | `COLORTERM=truecolor` 或等效的终端能力 |
| **256-color** | 索引调色板。RGB 值会映射到最接近的调色板条目。 | 标准 xterm-256color |
| **16-color** | 仅 ANSI 名称。颜色会映射到最接近的 ANSI 颜色。 | 基础终端支持 |

设置 `NO_COLOR` 后，Grok 不输出颜色，以单色渲染。

运行 `/doctor` 可查看检测到的颜色级别以及此终端上可用的主题。如果无法使用 Truecolor，Doctor 会显示相关设置步骤或解释终端限制。

<a id="automatic-quantization"></a>
### 自动量化

每个主题都使用完整 RGB 值定义。启动时，Grok 会根据检测到的能力级别量化所有颜色：

- 在 **Truecolor** 终端上，颜色原样通过。
- 在 **256-color** 终端上，每个 RGB 值都会映射到最接近的索引调色板条目。
- 在 **16-color** 终端上，颜色会映射为 ANSI 名称。

GrokNight 和 GrokDay 使用量化效果良好的中性灰色。TokyoNight、RosePineMoon 和 OscuraMidnight 使用具有独特色调的背景，量化后会失去特色，因此主题选择器会在非 Truecolor 终端上隐藏它们。

<a id="runtime-generated-colors"></a>
### 运行时生成的颜色

运行时生成的颜色（语法高亮、背景混合）也会通过同一管线量化，确保在所有终端类型上外观一致。

---

<a id="cursor-color"></a>
## 光标颜色

Grok 使用当前主题的 `accent_user` 颜色，通过 OSC 12 转义序列设置终端光标，以指示活跃的 Grok 会话。光标颜色：

- 在启动和切换主题时应用。
- 退出时通过 OSC 112 重置为终端默认值。

支持 OSC 12 的终端（大多数现代终端）都可使用此功能。

---

<a id="compact-mode"></a>
## 紧凑模式

使用 `/compact-mode` 斜杠命令切换紧凑模式。紧凑模式：

- 移除外部垂直内边距（顶部/底部边距变为 0）。
- 将水平内边距缩减到最小值（1 列）。
- 减少提示区域和信息块的顶部内边距。

该设置会持久化到 `~/.grok/config.toml` 的 `[ui].compact_mode` 下，并在重启后保留。

小屏幕上可使用紧凑模式来最大化内容区域。

---

<a id="syntax-highlighting"></a>
## 语法高亮

Grok 捆绑三个 `.tmTheme` 文件用于代码块语法高亮，并根据活动主题选择其中一个：

- `grok-night.tmTheme` —— GrokNight、RosePineMoon 和 OscuraMidnight
- `grok-day.tmTheme` —— GrokDay
- `tokyo-night.tmTheme` —— TokyoNight

切换主题时，Grok 会自动选择匹配的文件。这些 `.tmTheme` 文件内置于二进制文件中，因此无法用自定义文件替换。

---

<a id="deep-customization-with-pagertoml"></a>
## 使用 pager.toml 深度自定义

如需细粒度控制 TUI 外观，请创建 `~/.grok/pager.toml`。此文件控制回滚区布局、块样式、动画等。所有设置都有默认值，只需指定要覆盖的值。（开发构建会生成包含每个默认值且全部注释掉的模板文件——取消注释一行即可覆盖；保留注释的值可持续跟踪未来默认值。）

<a id="layout"></a>
### 布局

控制视口内边距和块间距：

```toml
[scrollback.layout]
outer_vpad = 1          # 视口的垂直内边距（顶部/底部）
outer_hpad_left = 2     # 左侧边距（最小值：1）
outer_hpad_right = 2    # 右侧边距（最小值：1）
block_pad_left = 2      # 强调线与内容之间的内边距
block_pad_right = 2     # 内容右边缘之后的内边距
```

<a id="scrollbar"></a>
### 滚动条

```toml
[scrollback.scrollbar]
enabled = true          # 显示/隐藏滚动条
gap_left = 0            # 内容与滚动条之间的间隔（0 = 相邻）
gap_right = 0           # 滚动条与屏幕边缘之间的间隔（0 = 位于边缘）
# scrollbar_bg = "none" # 覆盖背景色（或使用 "none" 表示主题默认值）
# scrollbar_fg = "none" # 覆盖滑块颜色（或使用 "none" 表示主题默认值）
```

<a id="scroll-behavior"></a>
### 滚动行为

```toml
[scrollback.scroll]
margin = 0                  # 所选条目上方/下方的上下文行数（0 = 边缘）
min_page_fraction = 0       # 视口最小滚动百分比（0-100）
follow_indicator = "center" # "center" = 显示 ▼/▲ 滚动箭头，"none" = 隐藏
follow_auto_select = true   # 跟随时自动选择最新条目
follow_by_overscroll = true # 滚过底部时进入跟随模式
anchor_on_fold = true       # 折叠时保持块标题在屏幕上的相同位置
```

<a id="display-options"></a>
### 显示选项

```toml
[scrollback.display]
sticky_headers = true              # 将用户提示固定为滚过后仍显示的标题
tab_width = 4                      # 每个制表符的空格数（0 = 原样传递）
expandable_indicator = true        # 在可折叠的折叠条目上显示“›”
expandable_indicator_char = "›"    # 使用的字符（默认：“›”）
collapsed_accent_char = "❙"        # 可折叠块折叠时的强调字符（旧版 Windows 控制台回退为“|”）
dim_accent = 0.5                   # 变暗强调色的混合因子（0.0-1.0）
line_under_last_entry = false      # 在最后一个条目下显示水平线
selection_buttons = false          # 在选框上显示复制/查看按钮
```

<a id="animation"></a>
### 动画

```toml
[animation]
fps = 30           # 帧率（1-60）。越高越平滑，但 CPU 占用也越高
wave_rows = 32     # 强调色动画每个波形周期的行数
```

<a id="block-styling-edit-diffs"></a>
### 块样式：编辑差异

```toml
[scrollback.blocks.edit]
indent = true                   # 缩进差异内容
vpad = false                    # 差异周围的垂直内边距
# expanded_by_default = true    # 未设置：遵循 config.toml 中 [ui] collapsed_edit_blocks
                                #（该标志开启时为折叠单行）；取消注释可固定任一形状
hunk_separator = "…"            # 差异块之间的分隔符（"…"、"───"、"⋯" 或空字符串 "" 表示无分隔符）
dual_line_numbers = false       # 双列行号（旧 + 新，如 GitHub）
# line_summary = false          # 在折叠标题中显示 +N/-M；未设置时遵循相同标志
# bg = "none"                   # 块背景（"none"、"light"、"dark"）
```

<a id="block-styling-thinking-reasoning"></a>
### 块样式：思考/推理

```toml
[scrollback.blocks.thinking]
accent_enabled = true       # 显示思考块的强调线
animate = true              # 思考时为强调线播放动画
truncated_lines = 3         # 截断模式下显示的行数
bg_blend = 70               # Markdown 颜色与背景的混合度（0-100）
header = true               # 显示“Thinking...”标题
header_bright = false       # 明亮的标题样式（而非暗淡/柔和）
```

<a id="block-styling-tool-calls"></a>
### 块样式：工具调用

```toml
[scrollback.blocks.tool]
muted_collapsed = true     # 将折叠的工具调用显示为灰色
dim_details = true          # 将括号内详情（行数、匹配数）变暗
bullet = "diamond"          # 工具标题前的项目符号样式
```

可用的项目符号样式：

| 配置值 | 字符 | 说明 |
|-------------|-----------|-------------|
| `none` | （无） | 无项目符号 |
| `dot` | `·` | 中点（最小） |
| `small-circle` | `•` | 项目符号 |
| `circle` | `●` | 实心圆 |
| `small-triangle` | `▸` | 向右的小三角形 |
| `triangle` | `▶` | 向右三角形 |
| `diamond` | `◆` | 实心菱形（默认） |

<a id="block-styling-execute-shell-commands"></a>
### 块样式：执行（Shell 命令）

```toml
[scrollback.blocks.execute]
first_lines = 2                   # 截断模式开始处显示的输出行数
last_lines = 3                    # 截断模式结尾处显示的输出行数
accent_enabled = true             # 显示强调线（运行时带动画）
header_style = "label"            # "shell"（$ 前缀）或 "label"（Run 前缀）
muted_command_collapsed = true    # 折叠时弱化命令文本
```

<a id="block-styling-user-prompts-scrollback"></a>
### 块样式：用户提示（回滚区）

```toml
[scrollback.blocks.prompt]
vpad = true            # 垂直内边距
bg = "light"           # 背景（"none"、"light"、"dark"）
show_prefix = true     # 显示提示前缀字符
min_lines = 2          # 截断/粘滞模式下的最少内容行数
```

<a id="prompt-input-widget"></a>
### 提示输入组件

```toml
[prompt]
collapse_unfocused = true    # 回滚区获得焦点时折叠
mouse_hover = true           # 鼠标悬停时显示高亮
show_prefix = true           # 显示提示前缀字符
```

<a id="todo-badges"></a>
### Todo 徽章

```toml
[todo]
badge_format = "default"   # "default" = 2/5（done/total），"colon" = [▶:1 □:4 ✓:3 ✗:2]，"comma" = [1 ▶, 4 □, 3 ✓, 2 ✗]
```

<a id="terminal-behavior"></a>
### 终端行为

```toml
[terminal]
alt_screen = "auto"    # "auto"、"always" 或 "never"
```

备用屏幕策略：

- `auto` —— 普通终端和普通 tmux 中使用全屏；tmux 控制模式和 Zellij 中以内联方式运行。
- `always` —— 始终进入全屏。
- `never` —— 从不进入全屏；在主回滚区中以内联方式运行。

<a id="plugins-ui"></a>
### 插件 UI

```toml
disable_plugins = false   # 设为 true 以隐藏 /hooks、/plugins 命令和注释
```

---

<a id="theme-color-slots"></a>
## 主题颜色槽

每个主题定义以下颜色槽，并在整个 TUI 中使用：

**背景：** `bg_base`、`bg_light`、`bg_dark`、`bg_highlight`、`bg_hover`、`bg_terminal`、`bg_visual`

**强调色：** `accent_user`、`accent_assistant`、`accent_thinking`、`accent_tool`、`accent_system`、`accent_error`、`accent_success`、`accent_running`、`accent_skill`、`accent_plan`、`accent_verify`、`accent_remember`、`accent_model`

**文本：** `text_primary`、`text_secondary`

**灰色：** `gray_dim`、`gray`、`gray_bright`

**语义：** `command`、`path`、`running`、`warning`、`fuzzy_accent`

**边框和滚动条：** `selection_border`、`hover_border`、`prompt_border`、`prompt_border_active`、`scrollbar_bg`、`scrollbar_fg`

**粘贴：** `paste_bg`、`paste_fg`、`paste_dim`

**差异：** `diff_delete_bg`、`diff_delete_fg`、`diff_insert_bg`、`diff_insert_fg`、`diff_equal_fg`、`diff_gutter_fg`

**Markdown：** 标题颜色（`md_heading_h1`-`md_heading_h6`）、`md_code`、`md_code_bg`、`md_text`、`md_muted`、`md_task_checked`、`md_task_unchecked`、`link_fg`

主题系统在内部管理这些槽，并针对你的终端自动量化它们。
