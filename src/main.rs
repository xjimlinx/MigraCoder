use anyhow::{Result, bail};
use chrono::{Local, TimeZone};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum, ValueHint};
use clap_complete::{Shell, generate};
use clap_complete_nushell::Nushell;
use migracoder::{
    Migrator, SessionAction, SessionActionPlan, SessionInfo, move_workspace, normalize_path,
    rollback_workspace_move, validate_workspace_move,
};
use std::env;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(
    name = "migracoder",
    version,
    about = "移动工作区时同步更新 Codex 与 opencode 的本地会话指向。"
)]
struct Cli {
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        value_hint = ValueHint::DirPath,
        help = "Codex 数据目录"
    )]
    codex_home: Option<PathBuf>,

    #[arg(
        long,
        global = true,
        value_name = "PATH",
        value_hint = ValueHint::DirPath,
        help = "opencode 数据目录"
    )]
    opencode_home: Option<PathBuf>,

    #[arg(long, global = true, help = "跳过 opencode 会话")]
    no_opencode: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    #[command(about = "移动目录，并同步更新 Codex 与 opencode 指向")]
    Move {
        #[arg(value_hint = ValueHint::DirPath)]
        old: PathBuf,
        #[arg(value_hint = ValueHint::DirPath)]
        new: PathBuf,
        #[arg(long)]
        dry_run: bool,
    },
    #[command(about = "目录已移动，仅修复 Codex 与 opencode 指向")]
    Repoint {
        #[arg(value_hint = ValueHint::DirPath)]
        old: PathBuf,
        #[arg(value_hint = ValueHint::DirPath)]
        new: PathBuf,
        #[arg(long)]
        dry_run: bool,
    },
    #[command(about = "显示指定路径关联的 Codex 会话")]
    Sessions {
        #[arg(value_hint = ValueHint::DirPath, help = "路径；省略时显示全部会话")]
        path: Option<PathBuf>,
        #[arg(long, help = "同时显示子目录关联的会话")]
        recursive: bool,
        #[arg(
            long,
            value_name = "TEXT",
            help = "按标题、路径、会话 ID 和用户消息过滤"
        )]
        search: Option<String>,
    },
    #[command(name = "show-session", about = "分页显示 Codex 会话中的用户与 AI 消息")]
    ShowSession {
        session: String,
        #[arg(long, default_value_t = 0, help = "从上次输出的字节游标继续读取")]
        offset: u64,
        #[arg(long, default_value_t = 10, help = "本页最多显示的消息数（1–100）")]
        limit: usize,
        #[arg(
            long,
            default_value_t = 4_000,
            help = "每条消息最多显示的字符数（200–50000）"
        )]
        max_chars: usize,
    },
    #[command(name = "repoint-session", about = "只修改一个 Codex 会话的工作目录")]
    RepointSession {
        session: String,
        #[arg(value_hint = ValueHint::DirPath)]
        new: PathBuf,
        #[arg(long)]
        dry_run: bool,
    },
    #[command(name = "rename-session", about = "修改一个 Codex 会话的标题")]
    RenameSession {
        session: String,
        title: String,
        #[arg(long)]
        dry_run: bool,
    },
    #[command(name = "archive-session", about = "归档一个 Codex 会话")]
    ArchiveSession {
        session: String,
        #[arg(long)]
        dry_run: bool,
    },
    #[command(name = "unarchive-session", about = "将一个 Codex 会话移出归档")]
    UnarchiveSession {
        session: String,
        #[arg(long)]
        dry_run: bool,
    },
    #[command(
        name = "delete-session",
        about = "删除一个 Codex 会话（操作前自动备份）"
    )]
    DeleteSession {
        session: String,
        #[arg(long)]
        dry_run: bool,
        #[arg(long, help = "跳过交互确认")]
        yes: bool,
    },
    #[command(about = "生成 Shell 自动补全脚本")]
    Completions {
        #[arg(value_enum)]
        shell: CompletionShell,
    },
    #[command(about = "启动图形界面")]
    Gui,
    #[command(about = "检查 Codex 与 opencode 数据目录及可识别的数据")]
    Doctor,
    #[command(about = "确认旧路径的 Codex 指向是否已经清除")]
    Verify {
        #[arg(value_hint = ValueHint::DirPath)]
        old: PathBuf,
        #[arg(value_hint = ValueHint::DirPath)]
        new: PathBuf,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CompletionShell {
    Bash,
    Elvish,
    Fish,
    Nushell,
    Powershell,
    Zsh,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("migracoder: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    if let Commands::Completions { shell } = &cli.command {
        generate_completions(*shell);
        return Ok(());
    }
    if matches!(&cli.command, Commands::Gui) {
        return migracoder::gui::run();
    }
    let opencode_home = if cli.no_opencode {
        None
    } else {
        match &cli.opencode_home {
            Some(path) => Some(normalize_path(path)?),
            None => default_opencode_home(),
        }
    };
    let codex_home = cli.codex_home.unwrap_or_else(default_codex_home);
    let migrator = Migrator::new(codex_home)?.with_opencode(opencode_home)?;
    match cli.command {
        Commands::Sessions {
            path,
            recursive,
            search,
        } => {
            let path = path.map(normalize_path).transpose()?;
            let sessions = match path.as_deref() {
                Some(path) => migrator.find_sessions(path, recursive, search.as_deref())?,
                None => migrator.list_sessions(search.as_deref())?,
            };
            print_sessions(path.as_deref(), &sessions);
        }
        Commands::ShowSession {
            session,
            offset,
            limit,
            max_chars,
        } => {
            let preview = migrator.session_preview(&session, offset, limit, max_chars)?;
            println!(
                "会话：{}  {}",
                preview.session.session_id, preview.session.title
            );
            println!("工作目录：{}", preview.session.cwd.display());
            for (index, message) in preview.messages.iter().enumerate() {
                let truncated = if message.truncated {
                    "（已截断）"
                } else {
                    ""
                };
                println!(
                    "\n[{}] {}{}\n{}",
                    index + 1,
                    message.role.label(),
                    truncated,
                    safe_terminal_text(&message.content)
                );
            }
            if preview.messages.is_empty() {
                println!("本页没有可显示的用户或 AI 消息");
            }
            if preview.skipped_oversized_records > 0 {
                println!(
                    "警告：为控制内存，已跳过 {} 条超过 8 MiB 的 JSONL 记录",
                    preview.skipped_oversized_records
                );
            }
            if let Some(next) = preview.next_offset {
                println!(
                    "\n下一页：{}",
                    next_page_command(
                        &migrator,
                        &preview.session.session_id,
                        next,
                        limit,
                        max_chars
                    )
                );
            } else {
                println!("\n已到会话末尾");
            }
        }
        Commands::RepointSession {
            session,
            new,
            dry_run,
        } => {
            let new = normalize_path(new)?;
            let (session, plan) = migrator.plan_session(&session, &new)?;
            println!("会话：{}  {}", session.session_id, session.title);
            print_plan(&plan);
            if dry_run {
                println!("dry-run：未修改任何内容");
                return Ok(());
            }
            if !new.is_dir() {
                bail!(
                    "new workspace does not exist or is not a directory: {}",
                    new.display()
                );
            }
            match migrator.apply(&plan)? {
                Some(backup) => println!("该会话的 Codex 指向已更新；备份：{}", backup.display()),
                None => println!("该会话没有需要更新的 Codex 指向"),
            }
            println!(
                "可验证：cd '{}' && codex resume {}",
                new.display(),
                session.session_id
            );
        }
        Commands::RenameSession {
            session,
            title,
            dry_run,
        } => {
            let plan = migrator.plan_session_title(&session, &title)?;
            println!("会话：{}", plan.session_id);
            println!("标题：{} -> {}", plan.old_title, plan.new_title);
            for description in plan.descriptions() {
                println!("  {description}");
            }
            println!("共 {} 个文件，{} 处更新", plan.files(), plan.replacements());
            if dry_run {
                println!("dry-run：未修改任何内容");
                return Ok(());
            }
            match migrator.apply_session_title(&plan)? {
                Some(backup) => println!("标题已更新；备份：{}", backup.display()),
                None => println!("该会话没有需要更新的标题数据"),
            }
        }
        Commands::ArchiveSession { session, dry_run } => {
            run_session_action(&migrator, &session, SessionAction::Archive, dry_run)?;
        }
        Commands::UnarchiveSession { session, dry_run } => {
            run_session_action(&migrator, &session, SessionAction::Unarchive, dry_run)?;
        }
        Commands::DeleteSession {
            session,
            dry_run,
            yes,
        } => {
            let plan = migrator.plan_session_action(&session, SessionAction::Delete)?;
            print_session_action_plan(&plan);
            if dry_run {
                println!("dry-run：未修改任何内容");
                return Ok(());
            }
            if !yes && !confirm_session_delete(&plan)? {
                println!("已取消，未修改任何内容");
                return Ok(());
            }
            let backup = migrator.apply_session_action(&plan)?;
            println!("会话已删除；恢复备份：{}", backup.display());
        }
        Commands::Repoint { old, new, dry_run } => {
            let new = normalize_path(new)?;
            let plan = migrator.plan(old, &new)?;
            print_plan(&plan);
            if dry_run {
                println!("dry-run：未修改任何内容");
                return Ok(());
            }
            if !new.is_dir() {
                bail!(
                    "new workspace does not exist or is not a directory: {}",
                    new.display()
                );
            }
            finish_apply(&migrator, &plan, &new)?;
        }
        Commands::Move { old, new, dry_run } => {
            let (old, destination) = validate_workspace_move(&old, &new)?;
            let plan = migrator.plan(&old, &destination)?;
            print_plan(&plan);
            if dry_run {
                println!(
                    "目录：将移动 {} -> {}",
                    old.display(),
                    destination.display()
                );
                println!("dry-run：未修改任何内容");
                return Ok(());
            }
            move_workspace(&old, &destination)?;
            println!("目录移动完成");
            if let Err(error) = finish_apply(&migrator, &plan, &destination) {
                rollback_workspace_move(&old, &destination)?;
                return Err(error);
            }
        }
        Commands::Doctor => {
            let report = migrator.doctor()?;
            println!("Codex 数据：{}", report.codex_home.display());
            println!(
                "会话：{}  工作区：{}  数据库：{}  备份：{}",
                report.sessions, report.workspaces, report.databases, report.backups
            );
            match &report.opencode_home {
                Some(home) => println!(
                    "opencode 数据：{}（{} 个会话）",
                    home.display(),
                    report.opencode_sessions
                ),
                None => println!("opencode 数据：未启用"),
            }
            if report.healthy() {
                println!("检查通过");
            } else {
                for warning in report.warnings {
                    println!("警告：{warning}");
                }
            }
        }
        Commands::Verify { old, new } => {
            let old = normalize_path(old)?;
            let new = normalize_path(new)?;
            let plan = migrator.plan(&old, &new)?;
            if plan.replacements() == 0 {
                println!("验证通过：未发现仍指向 {} 的数据", old.display());
                println!("目标：{}", new.display());
            } else {
                print_plan(&plan);
                bail!("验证失败：仍有 {} 处指向使用旧路径", plan.replacements());
            }
        }
        Commands::Completions { .. } => unreachable!("handled before Codex initialization"),
        Commands::Gui => unreachable!("handled before Codex initialization"),
    }
    Ok(())
}

fn run_session_action(
    migrator: &Migrator,
    reference: &str,
    action: SessionAction,
    dry_run: bool,
) -> Result<()> {
    let plan = migrator.plan_session_action(reference, action)?;
    print_session_action_plan(&plan);
    if dry_run {
        println!("dry-run：未修改任何内容");
        return Ok(());
    }
    let backup = migrator.apply_session_action(&plan)?;
    println!("会话已{}；恢复备份：{}", action.label(), backup.display());
    Ok(())
}

fn print_session_action_plan(plan: &SessionActionPlan) {
    println!("会话：{}  {}", plan.session.session_id, plan.session.title);
    println!("操作：{}", plan.action.label());
    for description in plan.descriptions() {
        println!("  {description}");
    }
    println!("共涉及 {} 个文件", plan.files());
}

fn confirm_session_delete(plan: &SessionActionPlan) -> Result<bool> {
    if !io::stdin().is_terminal() {
        bail!("非交互环境删除会话时必须传入 --yes");
    }
    print!(
        "确认永久删除会话“{}”（{}）？[y/N] ",
        plan.session.title, plan.session.session_id
    );
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn generate_completions(shell: CompletionShell) {
    let mut command = Cli::command();
    let name = "migracoder";
    match shell {
        CompletionShell::Bash => generate(Shell::Bash, &mut command, name, &mut io::stdout()),
        CompletionShell::Elvish => generate(Shell::Elvish, &mut command, name, &mut io::stdout()),
        CompletionShell::Fish => generate(Shell::Fish, &mut command, name, &mut io::stdout()),
        CompletionShell::Nushell => generate(Nushell, &mut command, name, &mut io::stdout()),
        CompletionShell::Powershell => {
            generate(Shell::PowerShell, &mut command, name, &mut io::stdout())
        }
        CompletionShell::Zsh => generate(Shell::Zsh, &mut command, name, &mut io::stdout()),
    }
}

fn default_codex_home() -> PathBuf {
    env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
        .unwrap_or_else(|| PathBuf::from(".codex"))
}

fn default_opencode_home() -> Option<PathBuf> {
    let data_home = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))?;
    let home = data_home.join("opencode");
    home.is_dir().then_some(home)
}

fn print_plan(plan: &migracoder::Plan) {
    println!("计划：{} -> {}", plan.old.display(), plan.new.display());
    for description in plan.descriptions() {
        println!("  {description}");
    }
    println!("共 {} 个文件，{} 处指向", plan.files(), plan.replacements());
}

fn finish_apply(migrator: &Migrator, plan: &migracoder::Plan, new: &Path) -> Result<()> {
    match migrator.apply(plan)? {
        Some(backup) => println!("指向已更新；备份：{}", backup.display()),
        None => println!("未找到需要更新的指向"),
    }
    println!("可验证：cd '{}' && codex resume --last", new.display());
    Ok(())
}

fn print_sessions(path: Option<&Path>, sessions: &[SessionInfo]) {
    match path {
        Some(path) => println!("路径：{}", path.display()),
        None => println!("路径：全部工作区"),
    }
    if sessions.is_empty() {
        println!("未找到匹配的 Codex 会话");
        return;
    }
    println!("找到 {} 个会话：", sessions.len());
    for session in sessions {
        let updated = Local
            .timestamp_millis_opt(session.updated_at_ms)
            .single()
            .map(|time| time.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| "未知时间".to_owned());
        let status = if session.archived {
            "已归档"
        } else {
            "活动"
        };
        let title = session
            .title
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        println!(
            "  {}  {}  {}  {}\n    {}",
            session.session_id,
            updated,
            status,
            title,
            session.cwd.display()
        );
        if !session.match_excerpt.is_empty() && session.match_excerpt != session.title {
            println!("    命中：{}", session.match_excerpt);
        }
    }
}

fn safe_terminal_text(text: &str) -> String {
    text.chars()
        .filter(|character| matches!(character, '\n' | '\r' | '\t') || !character.is_control())
        .collect()
}

fn next_page_command(
    migrator: &Migrator,
    session_id: &str,
    offset: u64,
    limit: usize,
    max_chars: usize,
) -> String {
    let opencode_option = match &migrator.opencode_home {
        Some(home) => format!("--opencode-home {}", shell_quote(&home.to_string_lossy())),
        None => "--no-opencode".to_owned(),
    };
    format!(
        "migracoder --codex-home {} {opencode_option} show-session {} --offset {offset} --limit {limit} --max-chars {max_chars}",
        shell_quote(&migrator.codex_home.to_string_lossy()),
        shell_quote(session_id),
    )
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
