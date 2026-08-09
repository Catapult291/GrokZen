# 🚀 架构概览 — `xai-grok-pager` 渲染引擎

**xai-grok-pager** 渲染引擎基于分层流水线，将原始 markdown 转换为终端可用的单元格。本文涵盖每个主要子系统——从 markdown 解析，到语法高亮、自动换行、块布局、视口裁剪以及最终缓冲区合成。理解这些层对任何渲染器性能分析或优化都至关重要。

---

## 📐 渲染流水线

每一帧都遵循相同的阶段顺序。内容沿着变换流程**向下**流动，每一步都会增加结构：

1. **Markdown 解析** — `StreamingMarkdownRenderer` 将源文本转换为由样式化 `Line<'static>` span 构成的树。代码围栏会触发 **syntect** 高亮。
2. **自动换行** — `word_wrap_lines_with_joiners()` 将逻辑行拆分为适合视口宽度的物理行，同时跟踪 *joiners*（如 `↳` 这样的续行标记），以确保复制/粘贴时保持一致。
3. **块输出** — `BlockContent::output()` 将换行后的行打包成 `BlockOutput`，并附带逐行元数据：背景色、joiner 字符串和可选装饰。
4. **条目渲染** — `EntryRenderer` 将强调列（`┃`）、左右内边距和块内容合成为水平条带。垂直内边距（vpad）在上方和下方增加留白。
5. **视口裁剪** — `render_scrolled_entries_with_scratch()` 遍历条目列表，跳过屏幕外条目，并使用 `ScratchBuffer` 将部分可见条目渲染到临时缓冲区，再复制可见切片。
6. **缓冲区差分** — ratatui 的 `Terminal::flush()` 对新旧 `Buffer` 做差分，只将发生变化的单元格作为转义序列输出。这是 **O(changed cells)**，而不是 O(total cells)。

> **💡 关键洞见**：第 1–3 步会跨帧**缓存**。只有第 4–5 步每帧运行。性能分析应聚焦于这两步。

### 性能特征

| 阶段 | 复杂度 | 已缓存？ | 热点路径？ |
|---|---|---|---|
| Markdown 解析 | `O(n)`（源文本长度） | ✅ 是，按 generation | ❌ 否 |
| 语法高亮 | `O(n)`（使用 syntect DFA） | ✅ 是，按 generation | ❌ 否 |
| 自动换行 | `O(lines × width)` | ✅ 是，键为 `(width, gen)` | ❌ 否 |
| `BlockContent::output()` | `O(wrapped_lines)` | ✅ 通过 `WrapCache` | ⚠️ 仅首次调用 |
| `EntryRenderer::render()` | `O(height × width)` 单元格写入 | ❌ 否 | ✅ **是** |
| Scratch buffer 复制 | `O(visible_rows × width)` 克隆 | ❌ 否 | ✅ **是** |
| 缓冲区差分 + flush | `O(changed_cells)` | N/A | ✅ **是** |

---

## 🧱 块类型及其渲染开销

每个 `RenderBlock` 变体都有不同的渲染特征。下面列出主要块类型、典型内容模式及相关开销：

### `AgentMessageBlock` — 最重量级 🔥

Agent 消息包含**任意 markdown**：段落、代码块、表格、列表和行内格式。单个 agent 响应很容易超过 200 个换行后的行。`MarkdownContent` 子系统负责主要工作：

- `StreamingMarkdownRenderer::push_and_render()` 增量解析并高亮
- `word_wrap_lines_with_joiners()` 使用 `unicode-width` 处理 Unicode 感知的断行
- 宽字符（CJK、emoji）占用 2 列：`'🦀'.width() == 2`，`'λ'.width() == 1`

```rust
/// The core markdown-to-lines pipeline.
///
/// This function is called on every content mutation (push_chunk, finish)
/// and produces the canonical `Vec<Line<'static>>` that gets cached.
pub fn render_markdown(source: &str, pretty: bool) -> Vec<Line<'static>> {
    let mut renderer = StreamingMarkdownRenderer::new(MD_STYLE, pretty);
    renderer.push(source);
    renderer.render(Some(get_syntect()));
    renderer.view().lines.to_vec()
}

/// Word-wrap with joiner tracking for copy fidelity.
///
/// Each output line knows whether it's a continuation of the previous
/// logical line (joiner = Some("↳")) or a fresh line (joiner = None).
/// This matters for selection/copy: we strip joiners when copying.
pub fn word_wrap_lines_with_joiners(
    lines: Vec<Line<'static>>,
    max_width: usize,
) -> (Vec<Line<'static>>, Vec<Option<String>>) {
    let mut wrapped = Vec::with_capacity(lines.len() * 2);
    let mut joiners = Vec::with_capacity(lines.len() * 2);
    for line in lines {
        let line_width = line.width();
        if line_width <= max_width {
            wrapped.push(line);
            joiners.push(None);
        } else {
            // Split at grapheme cluster boundaries respecting unicode width.
            // This is the expensive path — O(spans × chars) per line.
            let parts = split_line_at_width(&line, max_width);
            for (i, part) in parts.into_iter().enumerate() {
                wrapped.push(part);
                joiners.push(if i > 0 { Some("↳".into()) } else { None });
            }
        }
    }
    (wrapped, joiners)
}
```

### `ThinkingBlock` — 默认截断

Thinking 块的渲染方式与 agent 消息相同，但默认使用 `DisplayMode::Truncated`（3 个可见行 + `⋯ N more lines`）。展开后，其开销与 agent 消息相当。截断逻辑在换行**之后**运行，因此即使折叠也会支付完整的换行开销——这是潜在的优化目标。

### `ToolCallBlock` 变体

| 变体 | 折叠高度 | 展开开销 | 备注 |
|---|---|---|---|
| `Execute` | 1 行（命令摘要） | `O(output_lines)` | Bash 输出可能很大 |
| `Read` | 1 行（路径 + 行数） | `O(file_lines)` | 带语法高亮的文件内容 |
| `Edit` | 1 行（路径 + 编辑次数） | `O(diff_lines)` | 带 `+`/`-` 着色的差异块 |
| `ListDir` | 1 行（路径） | `O(entries)` | 目录树列表 |
| `Search` | 1 行（模式 + 计数） | `O(matches)` | 带上下文的 Grep 结果 |
| `Other` | 1 行（工具名） | `O(output)` | 通用工具输出 |

### `UserPromptBlock` — 轻量 ✨

用户提示通常很短（一般为 1–5 行），使用 `accent_user` 颜色的 `┃` 强调符渲染，并且**永不折叠**。这是渲染成本最低的块。

---

## 🎨 强调列与颜色混合

每个条目的最左列都会显示一条垂直强调条 `┃`，作为视觉类型指示器：

- **用户提示**：`accent_user`（Tokyo Night 蓝色，`#7aa2f7`）
- **工具调用**：`accent_tool` / `accent_success` / `accent_error`
- **Thinking**：`accent_thinking`（紫色，`#bb9af7`）
- **运行中的块**：动画波浪效果 🌊

动画使用 `blend_color(bg, fg, brightness)`，按行、按帧计算：

```rust
/// Compute wave brightness for a single row at a given tick.
///
/// Returns a value in [0.2, 1.0] — never fully invisible.
/// The wave travels downward at WAVE_SPEED radians per tick.
pub fn wave_brightness(tick: u64, row: u16, wave_rows: u16, speed: f32) -> f32 {
    let phase = (tick as f32 * speed) - (row as f32 * std::f32::consts::PI / wave_rows as f32);
    let raw = (phase.sin() + 1.0) / 2.0; // normalize to [0, 1]
    0.2 + raw * 0.8 // scale to [0.2, 1.0]
}

/// Linearly blend two RGB colours.
///
/// `opacity = 0.0` → pure `base`; `opacity = 1.0` → pure `color`.
/// Returns `None` if either colour isn't RGB (indexed colours can't blend).
pub fn blend_color(base: Color, color: Color, opacity: f32) -> Option<Color> {
    match (base, color) {
        (Color::Rgb(br, bg, bb), Color::Rgb(cr, cg, cb)) => {
            let r = br as f32 + (cr as f32 - br as f32) * opacity;
            let g = bg as f32 + (cg as f32 - bg as f32) * opacity;
            let b = bb as f32 + (cb as f32 - bb as f32) * opacity;
            Some(Color::Rgb(r as u8, g as u8, b as u8))
        }
        _ => None,
    }
}
```

---

## 📦 `ScratchBuffer` 与部分渲染

当条目**部分可见**（在视口顶部或底部被裁剪）时，不能直接渲染到输出缓冲区——否则会向可见区域之外写入单元格。改为：

1. 将可复用的 `ScratchBuffer` 调整为条目的完整高度
2. 将完整条目渲染到 scratch
3. 仅将可见行（`skip_rows..skip_rows + visible_height`）复制到输出

这是**逐单元格复制循环**——最热门的路径之一：

```rust
for dy in 0..visible_rows {
    let src_y = skip_rows + dy;
    let dst_y = dest_area.y + dy;
    for dx in 0..dest_area.width {
        if let Some(src_cell) = temp_buf.cell((dx, src_y))
            && let Some(dst_cell) = buf.cell_mut((dest_area.x + dx, dst_y))
        {
            dst_cell.clone_from(src_cell);
        }
    }
}
```

> **🔬 优化机会**：`Cell::clone_from` 会复制 `symbol: String`（栈上 24 字节 + 可能的堆内存）、`fg`、`bg`、`underline_color`、`modifier`、`skip`。基于 `memcpy` 的整行批量复制对于宽终端可能快很多。在 `width=200` 时，每个可见行每帧会执行 200 次 `clone_from` 调用——对于带有顶部和底部裁剪的 30 行视口，每帧可能达到 6000 次调用。

---

## 🔤 Unicode 宽度挑战

终端渲染必须处理**可变宽度字符**。`unicode-width` crate 提供 `UnicodeWidthChar::width()` 和 `UnicodeWidthStr::width()`：

| 字符 | 示例 | `width()` | 备注 |
|---|---|---|---|
| ASCII | `A`, `z`, `!` | 1 | 基本拉丁文 |
| CJK Unified | `漢`, `字`, `中` | 2 | 中日韩表意文字 |
| Fullwidth forms | `Ａ`, `Ｂ`, `１` | 2 | 全角 ASCII 变体 |
| Emoji | `🦀`, `🚀`, `🎨` | 2 | 大多数 emoji 都是宽字符 |
| Combining marks | `é` (e + ◌́) | 1 | 组合字符宽度为 0 |
| Zero-width | ZWJ, ZWNJ | 0 | 用于 👨‍👩‍👧‍👦 等 emoji 序列 |
| Tab | `\t` | — | `unicode-width` 不处理；我们会展开为空格 |

换行器绝不能在列边界处拆分宽字符。如果一个占 2 个单元格的字符将在 `width - 1` 列开始，就必须将其换到下一行，并用空格填充当前行。

下面是一个压力测试：`漢字テスト🦀🚀🎨` 包含 5 个双宽 CJK 字符（10 列）以及 3 个双宽 emoji（6 列），总计 16 列。在 `width = 10` 时会换成 2 行；在 `width = 7` 时会换成 3 行，并带有填充单元格。

---

## 📊 行内代码与语法高亮深入解析

行内代码使用反引号语法：`HashMap<String, Vec<u8>>`、`Option<&'a mut T>`、`impl Fn(usize) -> bool`。每个行内代码 span 都有独立的背景色（`bg_code`），以便与正文在视觉上区分。渲染器必须：

1. 解析反引号分隔符（单个 `` ` `` 或双个 ``` `` ```）
2. 提取代码内容
3. 应用 `Style::default().bg(theme.bg_code).fg(theme.fg_code)`
4. 处理**嵌套格式**——例如 `**bold `code` bold**`，其中代码位于粗体内部

围栏代码块会触发完整的 **syntect** 高亮。高亮流水线如下：

1. 根据语言标识符（`rust`、`python`、`typescript` 等）查找 `SyntaxReference`
2. 使用 Tokyo Night 主题创建 `HighlightLines`
3. 遍历源代码行，调用 `highlight_line()` 获取 `Vec<(syntect::Style, &str)>`
4. 将 syntect 样式转换为 ratatui `Span` 样式（映射 RGB 颜色）
5. 每行使用 `Style::default().bg(theme.bg_dark)` 作为块背景

syntect 状态机是**按行有状态的**——每一行的高亮取决于上一行结束时的解析状态。这意味着不能在单个代码块内并行高亮，但可以缓存结果。

---

## 🧪 测试模式

滚动回放渲染使用 `insta` 进行全面的快照测试。典型模式如下：

```python
# This is a Python code block to exercise a different syntax highlighter.
# The renderer must detect the language and switch syntect grammars.

import asyncio
from dataclasses import dataclass, field
from typing import Optional, Dict, List, Tuple

@dataclass
class TrainingConfig:
    """Configuration for a distributed training run. 🔧"""
    model_name: str
    batch_size: int = 32
    learning_rate: float = 3e-4
    max_epochs: int = 100
    gradient_accumulation_steps: int = 1
    warmup_ratio: float = 0.1
    weight_decay: float = 0.01
    devices: List[str] = field(default_factory=lambda: ["cuda:0"])
    mixed_precision: bool = True
    compile_model: bool = False  # torch.compile — can 2× throughput
    checkpoint_dir: Optional[str] = None

    @property
    def effective_batch_size(self) -> int:
        return self.batch_size * self.gradient_accumulation_steps * len(self.devices)

    def validate(self) -> None:
        assert self.batch_size > 0, f"batch_size must be positive, got {self.batch_size}"
        assert 0 < self.learning_rate < 1, f"learning_rate out of range: {self.learning_rate}"
        assert self.max_epochs > 0, f"max_epochs must be positive, got {self.max_epochs}"
        for device in self.devices:
            assert device.startswith(("cuda", "cpu")), f"unknown device: {device}"


async def train_epoch(
    model,
    dataloader,
    optimizer,
    scheduler,
    config: TrainingConfig,
    epoch: int,
) -> Dict[str, float]:
    """Run a single training epoch. Returns metrics dict. 📈"""
    model.train()
    total_loss = 0.0
    num_batches = 0

    for batch_idx, batch in enumerate(dataloader):
        # Forward pass — compute loss on this micro-batch
        outputs = model(**batch)
        loss = outputs.loss / config.gradient_accumulation_steps
        loss.backward()

        if (batch_idx + 1) % config.gradient_accumulation_steps == 0:
            optimizer.step()
            scheduler.step()
            optimizer.zero_grad()

        total_loss += loss.item() * config.gradient_accumulation_steps
        num_batches += 1

    avg_loss = total_loss / max(num_batches, 1)
    return {"epoch": epoch, "avg_loss": avg_loss, "num_batches": num_batches}
```

---

## ⚡ 基准测试策略

要测量渲染性能，需要将**每帧**开销与一次性设置开销隔离：

- **设置**（不计入测量）：解析 markdown、创建 `ScrollbackEntry`、计算初始换行缓存
- **测量**：对每个滚动偏移量 `0..total_height`，渲染到大小为 `width × viewport_height` 的 `Buffer`

这模拟用户按住 `j`（向下滚动），并测量**最坏情况**——每一帧都会在新的滚动位置重新渲染视口，从而触发：

- `EntryRenderer::render()` — 强调条、内边距、内容布局
- `BlockRenderer::render()` — vpad、内容行、背景填充
- 通过 `ScratchBuffer` 的部分渲染——处理顶部/底部被裁剪的条目
- 逐单元格复制——最内层的热点循环

### 预期结果

在现代机器（M2 Pro）上，我们预期：

- 对于 120×30 视口和包含 200 行 markdown 的文档，**每帧约 50–200 µs**
- **约 80% 的时间**位于 `EntryRenderer::render()` + scratch buffer 复制
- **约 15% 的时间**位于 `BlockContent::output()`（缓存命中路径——只遍历缓存行）
- **约 5% 的时间**用于布局计算（`HorizontalLayout`、`EntryLayout`、间距计算）

如果基准测试显示每帧 >500 µs，说明热点路径中可能存在意外的缓存未命中或分配。使用 `cargo bench -- --profile-time 10` 配合 `flamegraph` 找出原因。

---

## 🌐 杂项宽字符与边界情况

下面这些字符串可用于测试有趣的渲染边界情况：

- **Emoji 序列**：👨‍👩‍👧‍👦（家庭 ZWJ 序列，应为宽度 2，但终端支持情况各异）
- **旗帜**：🇺🇸 🇯🇵 🇩🇪（区域指示符对）
- **全角**：`ＡＢＣＤＥ`（每个字符宽 2 列）
- **组合字符**：`naïve` 与 `naïve`（预组 U+00EF 与组合 U+0308）
- **框线绘图**：`┌─────────┐│ content │└─────────┘`（宽度均为 1）
- **数学符号**：`∀x ∈ ℝ : x² ≥ 0`、`∑_{i=0}^{n} aᵢ = S`、`∫₀^∞ e^{-x} dx = 1`
- **中日韩混排**：`これはテストです — this is a test — 這是測試 — 이것은 시험이다`
- **RTL 标记**：`Hello ‮dlrow‬!`（包含 RLO/PDF 覆盖字符）

渲染器必须处理上述所有内容而不发生 panic 或产生乱码。换行器是关键组件——在决定断行位置时，必须正确计算每个字符的显示宽度。

> **⚠️ 警告**：某些终端会错误渲染 emoji 序列（将其显示为 1 宽或多个字形）。我们的渲染器使用 `unicode-width` 报告**Unicode 标准**宽度，而不是终端的实际渲染宽度。这是已知的错位来源——除非查询终端，否则不存在完美的解决方案。

---

*本文为基准测试而生成。总计：约 230 行丰富的 markdown 内容，包含多个代码块、表格、行内代码、emoji、宽 Unicode 字符以及多样化格式。*
