# Linux x86_64 GNU 使用说明

此软件包是 Grok Build 简体中文社区版的 Linux x86_64 GNU CI 预览构建，
目标为 x86_64-unknown-linux-gnu。它不是 SpaceXAI 官方发行版。

## 校验与运行

Actions Artifact 内含 tar.gz 和同名 sha256 文件。下载并解压 Artifact 后，
先在这两个文件所在目录依次运行：

    sha256sum -c grok-zh-*-linux-x86_64-gnu.tar.gz.sha256
    mkdir grok-zh-linux
    tar -xzf grok-zh-*-linux-x86_64-gnu.tar.gz -C grok-zh-linux
    cd grok-zh-linux
    sha256sum -c SHA256SUMS.txt
    ./grok-zh --version
    ./grok-zh

若要从任意目录启动，可把解包目录加入 PATH，或自行把 grok-zh 复制到
当前用户拥有且已在 PATH 中的目录。此预览包不会修改 shell 配置、不会使用
sudo 安装，也不会覆盖官方 grok 命令。

## 支持边界

- CI 会验证产物为 x86_64 ELF、检查包内外 SHA-256，并执行解包后的
  grok-zh --version。
- 该产物当前只上传为 GitHub Actions Artifact，不加入 GitHub Release。
- 当前社区 Release 自动更新器只支持 Windows x64 GNU 和 macOS ARM64，并对
  Release 资产集合进行严格校验；在客户端和发布协议共同升级前，Linux 预览包
  不会冒充可自动更新的正式资产。
- 语音输入需要系统中可用的 PipeWire、PulseAudio 或 ALSA 录音工具；构建与
  基础命令运行不依赖这些工具。
