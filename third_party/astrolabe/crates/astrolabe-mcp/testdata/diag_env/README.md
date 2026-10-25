get_diagnostics 环境故障识别（`src/diag_env.rs`）的测试数据，格式与 get_diagnostics 输出行相同：`@path:line [severity code] message`。

- `go_gocache_args.txt`、`go_broken_import_args_test.txt`：第 6 批 E2 跑分里 get_diagnostics 的原样输出（gopls，GOCACHE 被设为只读）。
- `go_ro_gocache_build.txt`、`go_goproxy_network.txt`、`go_missing_gosum.txt`：同类环境下 `go build` 的真实报错文本，套成诊断行。
- `ts_*`、`py_*`：`tsc --noEmit` / `pyright` 在缺 node_modules、`--noLib`、解释器不对时的真实报错文本，套成诊断行。
- `rust_*`、`*_real_errors.txt`：按 rust-analyzer / 各语言常见报错格式手写；`*_real_errors.txt` 是真实代码错误，不能被当成环境故障。
