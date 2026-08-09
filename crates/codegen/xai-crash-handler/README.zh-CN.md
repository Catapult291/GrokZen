# xai-crash-handler

针对 SIGBUS/SIGSEGV 的崩溃处理器，尽力捕获回溯信息。

## 工作原理

`install()` 注册一个 `sigaction` 处理器。发生崩溃时，它会把二进制数据块（`GCRX` 格式）写入 `crash_dir/last-crash.bin`，并通过预先计算好的转义序列恢复终端。处理器对文件 I/O、终端恢复和重新抛出信号只使用异步信号安全操作。

下一次启动时，`check_previous_crash()` 读取该数据块，通过 `backtrace` 将 IP 解析为符号，写出 `last-crash-report.txt` 并归档（保留最近 5 份报告）。

在非 unix 平台上为空操作。在基于 musl 的 Linux（release 构建）上，处理器仍会记录信号、地址和版本，但会跳过帧捕获，因为 musl 不提供 `backtrace()`。

## 限制

### 帧捕获尽力而为

帧捕获使用两种完全异步信号安全的技术：
1. 从内核传入的 `ucontext_t` 中直接提取崩溃指令指针。
2. 通过原始指针读取遍历帧指针链，捕获额外帧（x86_64 使用 RBP，aarch64 使用 x29）。

在未使用 `-C force-frame-pointers` 的 release 构建中，帧指针链可能不完整或为空（编译器默认会为优化省略帧指针）。崩溃 PC 始终会被捕获。在 debug/dev 构建中，默认保留帧指针，因此调用栈更完整。

### sigaltstack 是线程级的

备用信号栈只安装在线程调用 `install()` 的那个线程上。Tokio 工作线程不会继承它。工作线程上的栈溢出仍会触发处理器（`sigaction` 是进程级的），但由于没有备用栈保护，处理器本身可能在已溢出的栈上再次出错。

## 用法

```rust
use std::path::PathBuf;

let crash_dir = PathBuf::from("/home/user/.myapp/crash");

// check_previous_crash MUST be called before install(), because
// install() opens last-crash.bin with O_TRUNC.
if let Some(r) = xai_crash_handler::check_previous_crash(&crash_dir) {
    eprintln!("Crashed last session: {}", r.signal_name);
    eprintln!("Report: {}", r.report_path.display());
}

// install() before any threads or async runtime — sigaltstack is per-thread.
// Creates crash_dir if it does not exist.
xai_crash_handler::install(xai_crash_handler::CrashHandlerConfig {
    app_version: env!("CARGO_PKG_VERSION").to_string(),
    crash_dir,
});
```
