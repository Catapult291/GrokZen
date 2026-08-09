# 对 xai-grok-markdown 做模糊测试

使用 [cargo-fuzz](https://rust-fuzz.github.io/book/cargo-fuzz.html)（libFuzzer）对 Markdown 渲染器进行覆盖率引导的模糊测试。

## 前置条件

```bash
cargo install cargo-fuzz   # if not already installed
rustup toolchain install nightly
```

## 目标

| 目标 | 模糊测试内容 |
|---|---|
| `render_all` | 对每个输入测试全部 8 种组合：`pretty × syntect × {full, streaming}` |

每次迭代都会运行：
- `render_markdown_ratatui_full()` — 4 种组合（pretty/non-pretty × syntect/no-syntect）
- `StreamingMarkdownRenderer` 逐字符运行 — 同样的 4 种组合

## 运行

从 `crates/codegen/xai-grok-markdown` 运行：

```bash
# Run indefinitely (Ctrl-C to stop):
cargo +nightly fuzz run render_all fuzz/corpus/render_all fuzz/seeds/render_all -- -max_len=16384

# Run for 5 minutes:
cargo +nightly fuzz run render_all fuzz/corpus/render_all fuzz/seeds/render_all -- -max_len=16384 -max_total_time=300
```

- `corpus/` — 自动生成的输入（gitignored）
- `seeds/` — 手写的种子输入（已纳入版本控制）

## 重现崩溃

发现崩溃时，输入会保存到 `artifacts/render_all/crash-<hash>`。使用以下命令重现：

```bash
cargo +nightly fuzz run render_all fuzz/artifacts/render_all/crash-<hash>
```

## 添加种子输入

将 `.txt` 或 `.md` 文件放入 `seeds/render_all/`。好的种子应覆盖不同的 Markdown 特性（表格、代码块、emoji、嵌套列表等），帮助模糊测试器更快到达新的代码路径。
