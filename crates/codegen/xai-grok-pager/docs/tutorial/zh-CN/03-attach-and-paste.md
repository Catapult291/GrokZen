# 附加文件、图像并粘贴

你为 Grok 指定的上下文越准确，结果就越好。将内容放入提示的三种方式：

## 使用 `@` 提及文件

输入 `@` 打开模糊文件选择器，也支持行范围：

```
@src/main.rs          attach a file
@src/main.rs:10-50    attach specific lines
@!.env                reach hidden files with @!
```

## 粘贴图像

直接将截图粘贴到提示中：在 macOS 上按 `Cmd+V`，Linux 上按 `Ctrl+V`，
Windows 上按 `Alt+V`。错误对话框、设计稿和图表都很适合这样处理。

## 自行运行 Shell 命令

在空提示中输入 `!` 可直接运行 Shell 命令——输出会落入回滚区，
Grok 也能在那里看到。

*深入了解：`/docs Getting Started`*
