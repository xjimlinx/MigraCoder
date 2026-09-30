# MigraCoder

迁移文件夹或 Git 仓库时，同步修改本机 Codex 保存的工作区路径，避免迁移后
`codex resume --last` 因仍按旧目录筛选而找不到会话。

工具使用 Rust 编写，编译后是单个原生可执行文件，支持 Codex CLI 与桌面端目前使用的几类本地数据：

- `sessions/` 和 `archived_sessions/` 中的会话工作目录；
- `state_*.sqlite` 与桌面端 thread catalog 中的 `cwd`、project root 和 sandbox 权限路径；
- Codex 自动化的工作目录与历史运行源路径；
- `.codex-global-state.json` 中的工作区、权限和项目路径；
- `config.toml` 中按路径保存的项目配置（例如 `trust_level`）。

同时也支持 opencode 的本地会话数据，默认自动检测 `~/.local/share/opencode`（或
`$XDG_DATA_HOME/opencode`），更新其 `opencode.db` 中的：

- `project` 的工作目录和 sandbox 路径；
- `project_directory` 的项目目录映射；
- `session` 的工作目录和相对路径；
- `workspace` 的目录；
- 事件日志（`event`）中会话创建/更新事件的工作目录与相对路径。

opencode 的项目标识保存在仓库的 `.git/opencode` 中并随目录一起移动，因此移动后会话
仍然归属同一项目。历史对话、提示词和工具命令正文不会被替换（包括事件日志中的标题等
正文内容）。

> 迁移前请关闭 opencode，避免它与本工具同时写入 `opencode.db`。

## 图形界面

启动原生 GUI：

```bash
make gui
# 或 ./migracoder gui
```

界面提供两种流程：

- **会话管理**：顶部可在 **Codex** / **opencode** 两个标签间切换，分别浏览各自的会话。两类会话都支持复选框多选、全选当前结果，以及批量重定向工作目录、归档、取消归档或删除。Codex 可分页阅读单条会话；opencode 可查看内容、复制 `opencode --session` 命令。
- **整个工作区**：选择源目录和目标位置，可选择移动磁盘目录，或只修复 Codex 与 opencode 指向。

扫描和迁移在后台执行；“检查并执行”会先生成真实变更计划，再显示确认窗口。每次真实修改仍会创建一致性备份，完成后会自动复查旧路径指向。界面可勾选是否同时处理 opencode 会话，并填写其数据目录。

Codex 会话详情同时展示用户和 AI 消息，并按 JSONL 字节游标增量读取，不会先把整个会话载入内存。默认每页 10 条、每条最多 4000 字，可在详情窗口选择 5–50 条的分页粒度和 1k–20k 字的单条上限后重新加载。系统提示、工具输出和思考过程不会展示。opencode 会话详情展示前 5–50 条用户消息摘要。

两类会话详情中都可以归档、取消归档、删除、复制会话 ID 或对应的恢复命令；Codex 还支持修改话题标题。修改前会创建备份。Codex 操作同步更新会话文件、索引及状态数据库；opencode 操作更新其 SQLite 数据库，删除时清理关联消息、内容、事件等记录，只删除选中的会话，保留未选中的子会话。列表下方提供批量操作栏，危险操作会显示数量和会话清单后再次确认；批量执行中途失败时，已完成项目会逆序回滚。

修改目录、标题或归档状态后，当前搜索结果会原地更新，保留关键词、路径筛选、列表顺序和勾选状态，便于继续处理剩余会话；删除只移除对应行。即使修改后的目录或标题不再匹配原关键词，该会话仍保留在当前结果中。点击“加载会话”可按当前条件重新搜索。

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

在终端中分页阅读用户和 AI 消息：

```bash
./migracoder show-session <会话ID>
./migracoder show-session <会话ID> --limit 20 --max-chars 10000
```

如果还有下一页，命令末尾会输出包含 `--offset` 游标的下一页命令。每次只保留当前页内容。

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

归档单个会话，或将它恢复到活动列表：

```bash
./migracoder archive-session <会话ID> --dry-run
./migracoder archive-session <会话ID>
./migracoder unarchive-session <会话ID>
```

删除前可以先预览。正式删除会交互确认；脚本或其他非交互环境必须显式传入
`--yes`。操作会先备份 rollout、会话索引和包含关联记录的数据库：

```bash
./migracoder delete-session <会话ID> --dry-run
./migracoder delete-session <会话ID>
./migracoder delete-session <会话ID> --yes
```

这些命令只处理指定会话，不会归档或删除同一目录下的其他会话。执行时建议关闭
Codex 客户端，避免它同时写入本地状态。

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

自定义 opencode 数据目录，或只处理 Codex：

```bash
migracoder --opencode-home /path/to/opencode repoint /old/repo /new/repo
migracoder --no-opencode repoint /old/repo /new/repo
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
