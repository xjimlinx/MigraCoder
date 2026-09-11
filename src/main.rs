use anyhow::{Result, bail};
use chrono::{Local, TimeZone};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum, ValueHint};
use clap_complete::{Shell, generate};
use clap_complete_nushell::Nushell;
use migracoder::{
    Migrator, SessionInfo, move_workspace, normalize_path, rollback_workspace_move,
    validate_workspace_move,
};
use std::env;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(
    name = "migracoder",
    version,
    about = "移动工作区时同步更新 Codex 的本地会话指向。"
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

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    #[command(about = "移动目录，并同步更新 Codex 指向")]
    Move {
        #[arg(value_hint = ValueHint::DirPath)]
        old: PathBuf,
        #[arg(value_hint = ValueHint::DirPath)]
        new: PathBuf,
        #[arg(long)]
        dry_run: bool,
    },
    #[command(about = "目录已移动，仅修复 Codex 指向")]
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
    #[command(name = "repoint-session", about = "只修改一个会话的工作目录")]
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
    #[command(about = "生成 Shell 自动补全脚本")]
    Completions {
        #[arg(value_enum)]
        shell: CompletionShell,
    },
    #[command(about = "启动图形界面")]
    Gui,
    #[command(about = "检查 Codex 数据目录及可识别的数据")]
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
    let codex_home = cli.codex_home.unwrap_or_else(default_codex_home);
    let migrator = Migrator::new(codex_home)?;
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
                println!("验证通过：未发现仍指向 {} 的 Codex 数据", old.display());
                println!("目标：{}", new.display());
            } else {
                print_plan(&plan);
                bail!(
                    "验证失败：仍有 {} 处 Codex 指向使用旧路径",
                    plan.replacements()
                );
            }
        }
        Commands::Completions { .. } => unreachable!("handled before Codex initialization"),
        Commands::Gui => unreachable!("handled before Codex initialization"),
    }
    Ok(())
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

fn print_plan(plan: &migracoder::Plan) {
    println!("Codex: {} -> {}", plan.old.display(), plan.new.display());
    for description in plan.descriptions() {
        println!("  {description}");
    }
    println!("共 {} 个文件，{} 处指向", plan.files(), plan.replacements());
}

fn finish_apply(migrator: &Migrator, plan: &migracoder::Plan, new: &Path) -> Result<()> {
    match migrator.apply(plan)? {
        Some(backup) => println!("Codex 指向已更新；备份：{}", backup.display()),
        None => println!("未找到需要更新的 Codex 指向"),
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
