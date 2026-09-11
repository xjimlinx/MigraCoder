# MigraCoder

迁移文件夹或 Git 仓库时，同步修改本机 Codex 保存的工作区路径，避免迁移后
`codex resume --last` 因仍按旧目录筛选而找不到会话。

工具使用 Rust 编写，编译后是单个原生可执行文件，支持 Codex CLI 与桌面端目前使用的几类本地数据：

- `sessions/` 和 `archived_sessions/` 中的会话工作目录；
- `state_*.sqlite` 与桌面端 thread catalog 中的 `cwd`、project root 和 sandbox 权限路径；
- Codex 自动化的工作目录与历史运行源路径；
- `.codex-global-state.json` 中的工作区、权限和项目路径；
- `config.toml` 中按路径保存的项目配置（例如 `trust_level`）。

历史对话、提示词和工具命令正文不会被替换。

## 图形界面

启动原生 GUI：

```bash
make gui
# 或 ./migracoder gui
```

界面提供两种流程：

- **单个会话**：默认浏览全部工作区，可按标题、路径、会话 ID 或正文搜索；会话以固定表头的表格展示话题标题、工作目录、更新时间、状态和操作。单击一行选择，双击标题或按“查看”打开摘要窗口，点选后会自动带入源路径。
- **整个工作区**：选择源目录和目标位置，可选择移动磁盘目录，或只修复 Codex 指向。

扫描和迁移在后台执行；“检查并执行”会先生成真实变更计划，再显示确认窗口。每次真实修改仍会创建一致性备份，完成后会自动复查旧路径指向。

会话详情按需读取，最多展示首条、搜索命中和最近的用户消息摘要；还可以修改话题标题、复制会话 ID 或对应的 `codex resume` 命令，不会在主列表展开整段提示词。标题修改会同步写入 Codex 会话索引、状态数据库和桌面端标题缓存，并在修改前备份。

安装 GUI、独立的 `migracoder-gui` 命令和桌面启动项：

```bash
make install-gui
```

CLI、GUI 与 Fish 补全一次安装：

```bash
make install-all
```

## 查看和迁移单个会话

显示工作目录恰好为指定路径的会话：

```bash
./migracoder sessions ~
```

省略路径可以浏览所有工作区；搜索也会匹配工作目录和会话 ID：

```bash
./migracoder sessions
./migracoder sessions --search "database migration"
```

可以按标题和用户消息正文过滤；只有加上 `--recursive` 才会包含子目录：

```bash
./migracoder sessions ~ --search "database migration"
./migracoder sessions ~/Code --recursive
```

从列表中复制会话 ID，然后只修改这一条会话的指向：

```bash
./migracoder repoint-session <会话ID> ~/Projects/new-location --dry-run
./migracoder repoint-session <会话ID> ~/Projects/new-location
```

`repoint-session` 不会移动目录，也不会修改同一旧路径下的其他会话。

也可以在命令行预览并修改单个会话的标题：

```bash
./migracoder rename-session <会话ID> "新的话题标题" --dry-run
./migracoder rename-session <会话ID> "新的话题标题"
```

## 直接运行

需要 Rust 1.88 或更高版本。仓库根目录的启动器会自动编译并运行：

```bash
./migracoder --help
```

例如：

```bash
./migracoder move /旧路径/repo /新路径/repo --dry-run
```

## 使用 Makefile

常用目标：

```bash
make help
make build
make release
make check
make completions
make gui
```

默认安装 CLI、GUI 和桌面启动项：

```bash
make install
```

只安装 CLI：

```bash
make install-cli
```

安装二进制和当前常用 Shell 的补全：

```bash
make install-fish
# 或 make install-bash / make install-zsh
```

一次安装 Fish、Bash 和 Zsh 补全：

```bash
make install-completions
```

可以覆盖安装前缀，例如：

```bash
make install PREFIX=/usr/local DESTDIR=/tmp/package-root
```

与 `mv` 一样，如果第二个参数是已存在的目录，会保留源目录名。例如：

```bash
./migracoder move ~/Projects/sample-repo ~/Archive
# 实际迁移到 ~/Archive/sample-repo
```

## 安装为全局命令

通过 Cargo 安装，不受 Arch Linux PEP 668 限制：

```bash
cargo install --path .
```

也可以构建 release 二进制后复制到任意 PATH 目录：

```bash
cargo build --release
install -Dm755 target/release/migracoder ~/.local/bin/migracoder
```

## 使用

先预览，然后一次完成目录移动与 Codex 指向更新：

```bash
migracoder move /旧路径/repo /新路径/repo --dry-run
migracoder move /旧路径/repo /新路径/repo
```

如果目录已经由文件管理器、`mv` 或其他工具移走，只修复 Codex：

```bash
migracoder repoint /旧路径/repo /新路径/repo --dry-run
migracoder repoint /旧路径/repo /新路径/repo
```

迁移前检查本机 Codex 数据是否可识别，迁移后确认旧路径引用已经清除：

```bash
migracoder doctor
migracoder verify /旧路径/repo /新路径/repo
```

自定义 Codex 数据目录：

```bash
migracoder --codex-home /path/to/codex-home repoint /old/repo /new/repo
```

## Shell 自动补全

支持 Fish、Bash、Zsh、Nushell、Elvish 和 PowerShell。查看可用类型：

```bash
migracoder completions --help
```

Fish 持久安装：

```fish
mkdir -p ~/.config/fish/completions
migracoder completions fish > ~/.config/fish/completions/migracoder.fish
```

Bash 持久安装：

```bash
mkdir -p ~/.local/share/bash-completion/completions
migracoder completions bash > ~/.local/share/bash-completion/completions/migracoder
```

Zsh 持久安装（确保该目录已加入 `fpath`）：

```zsh
mkdir -p ~/.local/share/zsh/site-functions
migracoder completions zsh > ~/.local/share/zsh/site-functions/_migracoder
```

其他 Shell 可以生成到标准输出后按各自配置加载：

```bash
migracoder completions nushell
migracoder completions elvish
migracoder completions powershell
```

每次真实修改前，相关文件都会一致性备份到
`$CODEX_HOME/migracoder-backups/<UTC 时间>/`。建议运行迁移命令时关闭其他正在写入同一
Codex 数据目录的客户端。完成后可以验证：

```bash
cd /新路径/repo
codex resume --last
```

临时情况下，Codex 自带的 `codex resume --all -C /新路径/repo` 也能从任意目录选择并以
新目录启动，但它不会永久重写旧会话的工作区路径。

## 开发

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```
