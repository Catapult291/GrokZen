# PTY 基准线

基准线按平台（macOS arm64 与 Linux arm64 CI runner 的计时差异很大）和场景分别记录。CI 会将当前运行结果与匹配的平台文件比较；如果任一场景的 p99 帧时间增长超过 15%（默认值；`--threshold` 可覆盖），检查就会失败。

文件命名：`<platform>.json`，其中 `<platform>` 与 CI artifact 的架构名称一致：`linux-x86_64`、`linux-aarch64`、`macos-aarch64`。

## 生成基准线

在安静的机器上运行完整基准套件：

```bash
cargo run -p xai-grok-pager --release --bin pty-bench -- \
  --all \
  --write-baseline crates/codegen/xai-grok-pager-pty-harness/benches/pty_baselines/<platform>.json
```

## 有意进行性能变更后的覆盖

有意改变帧计时（任一方向）的 PR 必须更新受影响的基准线。在 PR 正文中附上干净运行的 `pty-bench` 输出，供审阅者核对新数值是否合理。

## 首次运行

平台文件会在第一次 CI 运行时生成（见 `pager-bench` job）。在此之前，`--baseline <missing-file>` 会因缺少文件而明确报错。
