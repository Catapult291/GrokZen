---
name: re
description: 逆向工程总入口（路由）：APK/DEX/JAR 反编译、native 静态/动态、进程内存扫描、JS/OLLVM/.NET 与协议还原。
when-to-use: 要反编译或分析 APK/DEX/JAR/SO/DLL/EXE/ELF 时；要 IDA/radare2/xdbg/memscan 时；要处理 OLLVM、JS 签名、.NET 或自定义协议时。
metadata:
  spec-version: "1.0"
  role: router
---

<!-- spec: SKILL_ROUTER_SPEC@1.0 -->

## 定位

本 skill 是**逆向工程域路由（总入口）**，不是操作手册。按目标类型打开**一个**模块文件即可；模块内才是完整手册。

## 索引

模块目录（两种路径等价，前者不依赖当前 cwd）：

- `~/.grok/skill-store/re/modules/<模块>/`
- `<项目>/.grok/skills/re/modules/<模块>/`（junction 挂载点）

| 目标/场景 | 子文档 | 何时选 |
|-----------|--------|--------|
| APK/DEX/JAR 反编译 | `modules/jadx-cli/jadx-cli.md` | 要 CLI 全量落盘或 headless MCP 精读时 |
| 浏览器自动化 / 页面抓包 / 运行时 JS hook | `modules/jsh-cli/jsh-cli.md` | 要真实浏览器操作或 hook 前端 JS 时 |
| 前端 JS 去混淆 / Node 补环境复现签名 | `modules/js-reverse/js-reverse.md` | 签名参数要带回 Node 跑通、混淆需先分类时 |
| native 静态（反编译/xref/调用图） | `modules/ida-reverse/ida-reverse.md` | exe/dll/so/elf 等要 IDA Pro 深度分析时 |
| native 轻量/脚本化静态、patch | `modules/radare2/radare2.md` | 要 r2 跨架构、固件、快速反汇编或 patch 时 |
| Windows 动态调试 | `modules/xdbg-cli/xdbg-cli.md` | 要断点/单步/trace/读写寄存器与内存时 |
| 进程内存扫描 / CE 式差集 | `modules/reverse-memscan/reverse-memscan.md` | 要搜特征码或定位运行中变化字段时 |
| OLLVM / JS VM / .NET / 自定义协议 | `modules/reverse-extras/reverse-extras.md` | 疑似混淆分类、托管逆向或协议/PCAP 时 |

## 选路

1. **先分清分析对象坐在哪一层。** APK 内 Java/Kotlin → `jadx-cli`；APK 内 `.so` → `ida-reverse` 或 `radare2`；网页签名 JS → `js-reverse` + `jsh-cli`。
2. **静态优先，动态按需。** 静态（`ida-reverse` / `radare2` / `jadx-cli`）拿到假设后，再决定是否 `xdbg-cli` 或 `reverse-memscan`。
3. **混淆识别先于脱混淆。** 疑似 OLLVM / JSVMP / 风控 VM / .NET → 先读 `reverse-extras`，再回对应工具模块。
4. **表内无法区分时**问清目标文件类型与是否需要运行时，不要一次打开多个模块。

## 纪律

- 只打开命中的那**一个**模块文件；不要把索引当成要一次读完的清单。
- 跨模块协作（如 IDA 地址对回 xdbg/memscan）在具体任务中按需追加，仍不必预读全表。
- 动态调试（attach/launch）与内存写入前确认用户意图；默认只做静态分析。
- 需要 IDA 实例时先 `list_instances` + `analysis_wait`；需要 jadx MCP 时先 `load_apk`。

## 未命中

- listing 中其他 skill（含非逆向域）可**直接**使用，无需先打开本路由。
- 意图不属逆向域时，直接执行任务或问一句；不要为了「用上 re」而误开模块。
- 本路由**不**限制、不独占任何子 skill；若将来出现同名独立 skill，以 listing 实际条目为准，可不经本路由调用。

---

附（部署说明，非门禁）：

- 本技能由 `skilllink` 以目录 junction 按项目挂载；未挂载的项目不出现本条目。
- 模块文件名（`jadx-cli` 等）指上表路径；在形态 B 下它们默认**不是**独立 listing skill。
