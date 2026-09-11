use crate::{
    Migrator, Plan, SessionInfo, SessionPreview, move_workspace, normalize_path,
    rollback_workspace_move, validate_workspace_move,
};
use anyhow::{Result, bail};
use chrono::{Local, TimeZone};
use eframe::egui;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Session,
    Workspace,
}

#[derive(Clone, Copy, Debug)]
enum TaskKind {
    Scan,
    Preview,
    Review,
    Apply,
    Doctor,
    Detail,
    Rename,
}

struct TaskResult {
    kind: TaskKind,
    result: Result<TaskPayload>,
}

enum TaskPayload {
    Sessions(Vec<SessionInfo>),
    Message(String),
    Review {
        input: ActionInput,
        summary: String,
    },
    Detail(SessionPreview),
    Renamed {
        session_id: String,
        title: String,
        message: String,
    },
}

#[derive(Clone)]
struct ActionInput {
    mode: Mode,
    codex_home: PathBuf,
    source: PathBuf,
    destination: PathBuf,
    session_id: Option<String>,
    move_files: bool,
}

struct RenameInput {
    session_id: String,
    current_title: String,
    new_title: String,
}

pub fn run() -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([920.0, 700.0])
            .with_min_inner_size([720.0, 520.0]),
        ..Default::default()
    };
    eframe::run_native(
        "MigraCoder",
        options,
        Box::new(|creation_context| Ok(Box::new(MigraCoderApp::new(creation_context)))),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))
}

struct MigraCoderApp {
    mode: Mode,
    codex_home: String,
    source: String,
    destination: String,
    search: String,
    filter_by_path: bool,
    recursive: bool,
    move_files: bool,
    sessions: Vec<SessionInfo>,
    selected_session: Option<String>,
    status: String,
    details: String,
    task: Option<Receiver<TaskResult>>,
    confirm_action: Option<ActionInput>,
    confirm_summary: String,
    session_detail: Option<SessionPreview>,
    rename_dialog: Option<RenameInput>,
}

impl MigraCoderApp {
    fn new(creation_context: &eframe::CreationContext<'_>) -> Self {
        configure_fonts(&creation_context.egui_ctx);
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let codex_home = env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"));
        let mut app = Self {
            mode: Mode::Session,
            codex_home: codex_home.display().to_string(),
            source: home.display().to_string(),
            destination: String::new(),
            search: String::new(),
            filter_by_path: false,
            recursive: false,
            move_files: true,
            sessions: Vec::new(),
            selected_session: None,
            status: "选择源路径并加载会话。".to_owned(),
            details: String::new(),
            task: None,
            confirm_action: None,
            confirm_summary: String::new(),
            session_detail: None,
            rename_dialog: None,
        };
        app.start_scan(&creation_context.egui_ctx);
        app
    }

    fn busy(&self) -> bool {
        self.task.is_some()
    }

    fn choose_source(&mut self) {
        let mut dialog = rfd::FileDialog::new();
        if let Some(path) = existing_parent(&self.source) {
            dialog = dialog.set_directory(path);
        }
        if let Some(path) = dialog.pick_folder() {
            self.source = path.display().to_string();
            self.sessions.clear();
            self.selected_session = None;
            self.status = "源路径已改变，请重新加载会话。".to_owned();
        }
    }

    fn choose_destination(&mut self) {
        let mut dialog = rfd::FileDialog::new();
        if let Some(path) = existing_parent(&self.destination) {
            dialog = dialog.set_directory(path);
        }
        if let Some(path) = dialog.pick_folder() {
            self.destination = path.display().to_string();
        }
    }

    fn start_scan(&mut self, context: &egui::Context) {
        let codex_home = PathBuf::from(self.codex_home.clone());
        let source = PathBuf::from(self.source.clone());
        let filter_by_path = self.filter_by_path;
        let recursive = self.recursive;
        let search = (!self.search.trim().is_empty()).then(|| self.search.trim().to_owned());
        self.status = "正在扫描会话…".to_owned();
        self.details.clear();
        self.selected_session = None;
        self.spawn_task(context, TaskKind::Scan, move || {
            let migrator = Migrator::new(codex_home)?;
            let sessions = if filter_by_path {
                migrator.find_sessions(source, recursive, search.as_deref())?
            } else {
                migrator.list_sessions(search.as_deref())?
            };
            Ok(TaskPayload::Sessions(sessions))
        });
    }

    fn action_input(&self) -> Result<ActionInput> {
        let codex_home = normalize_path(&self.codex_home)?;
        let source = normalize_path(&self.source)?;
        let mut destination = normalize_path(&self.destination)?;
        if self.mode == Mode::Session && !destination.is_dir() {
            bail!("单会话的新工作目录必须已经存在");
        }
        if self.mode == Mode::Workspace && self.move_files {
            destination = validate_workspace_move(&source, &destination)?.1;
        } else if self.mode == Mode::Workspace && !destination.is_dir() {
            bail!("仅修复指向时，目标目录必须已经存在");
        }
        let session_id = if self.mode == Mode::Session {
            Some(
                self.selected_session
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("请先点选一个会话"))?,
            )
        } else {
            None
        };
        Ok(ActionInput {
            mode: self.mode,
            codex_home,
            source,
            destination,
            session_id,
            move_files: self.move_files,
        })
    }

    fn start_preview(&mut self, context: &egui::Context) {
        match self.action_input() {
            Ok(input) => {
                self.status = "正在生成预览…".to_owned();
                self.details.clear();
                self.spawn_task(context, TaskKind::Preview, move || {
                    let migrator = Migrator::new(&input.codex_home)?;
                    let (_, plan) = create_plan(&migrator, &input)?;
                    Ok(TaskPayload::Message(plan_summary(&plan)))
                });
            }
            Err(error) => self.set_error(error),
        }
    }

    fn request_apply(&mut self, context: &egui::Context) {
        match self.action_input() {
            Ok(input) => {
                self.status = "正在检查实际变更…".to_owned();
                self.details.clear();
                let task_input = input.clone();
                self.spawn_task(context, TaskKind::Review, move || {
                    let migrator = Migrator::new(&task_input.codex_home)?;
                    let (_, plan) = create_plan(&migrator, &task_input)?;
                    if plan.replacements() == 0 {
                        bail!("没有找到需要修改的 Codex 指向，请检查源路径和会话")
                    }
                    Ok(TaskPayload::Review {
                        input,
                        summary: plan_summary(&plan),
                    })
                });
            }
            Err(error) => self.set_error(error),
        }
    }

    fn start_doctor(&mut self, context: &egui::Context) {
        let codex_home = PathBuf::from(self.codex_home.clone());
        self.status = "正在检查 Codex 数据…".to_owned();
        self.details.clear();
        self.spawn_task(context, TaskKind::Doctor, move || {
            let report = Migrator::new(codex_home)?.doctor()?;
            let mut lines = vec![
                format!("Codex 数据：{}", report.codex_home.display()),
                format!(
                    "会话：{}  工作区：{}  数据库：{}  备份：{}",
                    report.sessions, report.workspaces, report.databases, report.backups
                ),
            ];
            if report.healthy() {
                lines.push("检查通过。".to_owned());
            } else {
                lines.extend(
                    report
                        .warnings
                        .into_iter()
                        .map(|warning| format!("警告：{warning}")),
                );
            }
            Ok(TaskPayload::Message(lines.join("\n")))
        });
    }

    fn start_detail(&mut self, context: &egui::Context, session_id: String) {
        let codex_home = PathBuf::from(self.codex_home.clone());
        let search = (!self.search.trim().is_empty()).then(|| self.search.trim().to_owned());
        self.status = "正在读取会话内容…".to_owned();
        self.spawn_task(context, TaskKind::Detail, move || {
            let preview =
                Migrator::new(codex_home)?.session_preview(&session_id, 6, search.as_deref())?;
            Ok(TaskPayload::Detail(preview))
        });
    }

    fn start_rename(&mut self, context: &egui::Context, input: RenameInput) {
        let codex_home = PathBuf::from(self.codex_home.clone());
        self.status = "正在更新会话标题…".to_owned();
        self.details.clear();
        self.spawn_task(context, TaskKind::Rename, move || {
            let migrator = Migrator::new(codex_home)?;
            let plan = migrator.plan_session_title(&input.session_id, &input.new_title)?;
            let summary = format!(
                "标题：{} -> {}\n共 {} 个文件，{} 处更新\n{}",
                plan.old_title,
                plan.new_title,
                plan.files(),
                plan.replacements(),
                plan.descriptions().join("\n")
            );
            let backup = migrator
                .apply_session_title(&plan)?
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "无需备份".to_owned());
            Ok(TaskPayload::Renamed {
                session_id: plan.session_id,
                title: plan.new_title,
                message: format!("标题修改完成\n备份：{backup}\n\n{summary}"),
            })
        });
    }

    fn start_apply(&mut self, context: &egui::Context, input: ActionInput) {
        self.status = "正在执行迁移，请勿关闭窗口…".to_owned();
        self.details.clear();
        self.spawn_task(context, TaskKind::Apply, move || {
            let migrator = Migrator::new(&input.codex_home)?;
            let (effective_destination, plan) = create_plan(&migrator, &input)?;
            let moved = input.mode == Mode::Workspace && input.move_files;
            if moved {
                move_workspace(&input.source, &effective_destination)?;
            }
            let applied = migrator.apply(&plan);
            if let Err(error) = applied {
                if moved {
                    rollback_workspace_move(&input.source, &effective_destination)?;
                }
                return Err(error);
            }
            let backup = applied?
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "无需备份（没有匹配指向）".to_owned());
            let verification = match input.mode {
                Mode::Session => match migrator
                    .resolve_session(input.session_id.as_deref().unwrap_or_default())
                {
                    Ok(session) if session.cwd == effective_destination => {
                        "验证通过：会话已指向目标目录".to_owned()
                    }
                    Ok(session) => {
                        format!("验证警告：读取到的会话路径仍为 {}", session.cwd.display())
                    }
                    Err(error) => format!("验证警告：无法重新读取会话（{error:#}）"),
                },
                Mode::Workspace => match migrator.plan(&input.source, &effective_destination) {
                    Ok(plan) if plan.replacements() == 0 => "验证通过：旧路径指向已清除".to_owned(),
                    Ok(plan) => format!("验证警告：仍发现 {} 处旧路径指向", plan.replacements()),
                    Err(error) => format!("验证警告：无法重新扫描（{error:#}）"),
                },
            };
            Ok(TaskPayload::Message(format!(
                "迁移完成\n目标：{}\n备份：{}\n{}\n\n{}",
                effective_destination.display(),
                backup,
                verification,
                plan_summary(&plan)
            )))
        });
    }

    fn spawn_task<F>(&mut self, context: &egui::Context, kind: TaskKind, operation: F)
    where
        F: FnOnce() -> Result<TaskPayload> + Send + 'static,
    {
        let (sender, receiver) = mpsc::channel();
        let context = context.clone();
        thread::spawn(move || {
            let result = operation();
            let _ = sender.send(TaskResult { kind, result });
            context.request_repaint();
        });
        self.task = Some(receiver);
    }

    fn poll_task(&mut self) {
        let result = self
            .task
            .as_ref()
            .and_then(|receiver| receiver.try_recv().ok());
        let Some(result) = result else {
            return;
        };
        self.task = None;
        match result.result {
            Ok(TaskPayload::Sessions(sessions)) => {
                self.status = format!("找到 {} 个会话。", sessions.len());
                self.sessions = sessions;
            }
            Ok(TaskPayload::Message(message)) => {
                self.status = match result.kind {
                    TaskKind::Preview => "预览完成，尚未修改任何内容。".to_owned(),
                    TaskKind::Apply => "迁移成功。".to_owned(),
                    TaskKind::Scan => "操作完成。".to_owned(),
                    TaskKind::Doctor => "环境检查完成。".to_owned(),
                    TaskKind::Review => "变更检查完成。".to_owned(),
                    TaskKind::Detail => "会话内容已加载。".to_owned(),
                    TaskKind::Rename => "标题修改完成。".to_owned(),
                };
                self.details = message;
                if matches!(result.kind, TaskKind::Apply) {
                    self.sessions.clear();
                    self.selected_session = None;
                }
            }
            Ok(TaskPayload::Review { input, summary }) => {
                self.status = "变更检查完成，请确认后执行。".to_owned();
                self.details = summary.clone();
                self.confirm_summary = summary;
                self.confirm_action = Some(input);
            }
            Ok(TaskPayload::Detail(preview)) => {
                self.status = "会话内容已加载。".to_owned();
                self.session_detail = Some(preview);
            }
            Ok(TaskPayload::Renamed {
                session_id,
                title,
                message,
            }) => {
                if let Some(session) = self
                    .sessions
                    .iter_mut()
                    .find(|session| session.session_id == session_id)
                {
                    session.title = title.clone();
                }
                if let Some(preview) = self
                    .session_detail
                    .as_mut()
                    .filter(|preview| preview.session.session_id == session_id)
                {
                    preview.session.title = title;
                }
                self.status = "标题修改完成。".to_owned();
                self.details = message;
            }
            Err(error) => self.set_error(error),
        }
    }

    fn set_error(&mut self, error: anyhow::Error) {
        self.status = "操作失败。".to_owned();
        self.details = format!("{error:#}");
    }

    fn confirmation_window(&mut self, context: &egui::Context) {
        if self.confirm_action.is_none() {
            return;
        }
        let mut confirm = false;
        let mut cancel = false;
        egui::Window::new("确认迁移")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(context, |ui| {
                let input = self.confirm_action.as_ref().expect("checked above");
                match input.mode {
                    Mode::Session => {
                        ui.label("只会修改点选会话的工作目录，不会移动磁盘文件。");
                        ui.label(format!(
                            "会话：{}",
                            input.session_id.as_deref().unwrap_or_default()
                        ));
                    }
                    Mode::Workspace if input.move_files => {
                        ui.label("将移动整个目录，并更新该路径下的全部 Codex 会话。");
                    }
                    Mode::Workspace => {
                        ui.label("只修复整个路径的 Codex 指向，不移动磁盘文件。");
                    }
                }
                ui.label(format!("源：{}", input.source.display()));
                ui.label(format!("目标：{}", input.destination.display()));
                ui.separator();
                ui.monospace(&self.confirm_summary);
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("取消").clicked() {
                        cancel = true;
                    }
                    if ui.button("确认执行").clicked() {
                        confirm = true;
                    }
                });
            });
        if cancel {
            self.confirm_action = None;
            self.confirm_summary.clear();
        } else if confirm {
            let input = self.confirm_action.take().expect("confirmation exists");
            self.confirm_summary.clear();
            self.start_apply(context, input);
        }
    }

    fn session_detail_window(&mut self, context: &egui::Context) {
        let Some(preview) = self.session_detail.as_ref() else {
            return;
        };
        let mut open = true;
        let mut close = false;
        let mut rename = None;
        egui::Window::new("会话内容")
            .open(&mut open)
            .default_width(620.0)
            .min_width(440.0)
            .max_height(620.0)
            .collapsible(false)
            .show(context, |ui| {
                ui.heading(compact_title(&preview.session.title, 72));
                ui.add_space(4.0);
                egui::Grid::new("session_detail_metadata")
                    .num_columns(2)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        ui.weak("时间");
                        ui.label(format_time(preview.session.updated_at_ms));
                        ui.end_row();
                        ui.weak("状态");
                        ui.label(if preview.session.archived {
                            "已归档"
                        } else {
                            "活动"
                        });
                        ui.end_row();
                        ui.weak("工作目录");
                        ui.label(preview.session.cwd.display().to_string());
                        ui.end_row();
                        ui.weak("会话 ID");
                        ui.monospace(&preview.session.session_id);
                        ui.end_row();
                    });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("修改标题…").clicked() {
                        rename = Some(RenameInput {
                            session_id: preview.session.session_id.clone(),
                            current_title: preview.session.title.clone(),
                            new_title: preview.session.title.clone(),
                        });
                    }
                    if ui.button("复制会话 ID").clicked() {
                        context.copy_text(preview.session.session_id.clone());
                    }
                    if ui.button("复制 resume 命令").clicked() {
                        context.copy_text(resume_command(&preview.session));
                    }
                });
                ui.separator();
                ui.strong("话题内容（用户消息摘要）");
                egui::ScrollArea::vertical()
                    .max_height(360.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if preview.user_messages.is_empty() {
                            ui.weak("该会话没有可读取的用户消息摘要。");
                        }
                        for (index, message) in preview.user_messages.iter().enumerate() {
                            ui.group(|ui| {
                                ui.weak(format!("消息 {}", index + 1));
                                ui.label(message);
                            });
                            ui.add_space(5.0);
                        }
                    });
                ui.separator();
                if ui.button("关闭").clicked() {
                    close = true;
                }
            });
        if !open || close {
            self.session_detail = None;
        }
        if let Some(input) = rename {
            self.rename_dialog = Some(input);
        }
    }

    fn rename_window(&mut self, context: &egui::Context) {
        let Some(input) = self.rename_dialog.as_mut() else {
            return;
        };
        let mut save = false;
        let mut cancel = false;
        egui::Window::new("修改话题标题")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(context, |ui| {
                ui.weak("当前标题");
                ui.label(compact_title(&input.current_title, 100));
                ui.add_space(6.0);
                ui.label("新标题");
                let response = ui.add_sized(
                    [520.0, 28.0],
                    egui::TextEdit::singleline(&mut input.new_title)
                        .hint_text("输入简短、容易识别的话题标题"),
                );
                ui.weak(format!("{} / 160 个字符", input.new_title.chars().count()));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("取消").clicked() {
                        cancel = true;
                    }
                    let valid = !input.new_title.trim().is_empty()
                        && input.new_title.chars().count() <= 160
                        && input
                            .new_title
                            .split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" ")
                            != input.current_title;
                    if ui
                        .add_enabled(valid, egui::Button::new("保存标题"))
                        .clicked()
                        || (valid
                            && response.lost_focus()
                            && ui.input(|keys| keys.key_pressed(egui::Key::Enter)))
                    {
                        save = true;
                    }
                });
            });
        if cancel {
            self.rename_dialog = None;
        } else if save {
            let input = self.rename_dialog.take().expect("rename dialog exists");
            self.start_rename(context, input);
        }
    }
}

impl eframe::App for MigraCoderApp {
    fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_task();
        if self.busy() {
            context.request_repaint_after(Duration::from_millis(100));
            if context.input(|input| input.viewport().close_requested()) {
                context.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.status = "操作仍在进行，请等待完成后再关闭窗口。".to_owned();
            }
        }

        egui::TopBottomPanel::top("header").show(context, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.heading("MigraCoder");
                ui.label("迁移目录时保持 Codex 会话可恢复");
            });
            ui.add_space(6.0);
        });

        egui::CentralPanel::default().show(context, |ui| {
            ui.add_enabled_ui(!self.busy(), |ui| {
                let previous_mode = self.mode;
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.mode, Mode::Session, "单个会话");
                    ui.selectable_value(&mut self.mode, Mode::Workspace, "整个工作区");
                });
                if self.mode != previous_mode {
                    self.details.clear();
                    self.status = match self.mode {
                        Mode::Session => "选择源路径并加载会话。".to_owned(),
                        Mode::Workspace => "选择要迁移的工作区和目标位置。".to_owned(),
                    };
                }
                ui.separator();

                path_row(ui, "Codex 数据", &mut self.codex_home, None);
                ui.horizontal(|ui| {
                    if ui.small_button("检查环境").clicked() {
                        self.start_doctor(context);
                    }
                    ui.weak("检查会话、数据库和 MigraCoder 备份是否可识别");
                });

                let previous_source = self.source.clone();
                let mut pick_source = false;
                if self.mode == Mode::Session {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.filter_by_path, "按工作目录筛选");
                        if !self.filter_by_path {
                            ui.weak("当前浏览全部工作区；点选会话后会自动带入源路径");
                        }
                    });
                    ui.add_enabled_ui(self.filter_by_path, |ui| {
                        path_row(ui, "路径筛选", &mut self.source, Some(&mut pick_source));
                    });
                } else {
                    path_row(ui, "源路径", &mut self.source, Some(&mut pick_source));
                }
                if pick_source {
                    self.choose_source();
                    if self.mode == Mode::Session {
                        self.start_scan(context);
                    }
                } else if self.source != previous_source && self.mode == Mode::Session {
                    self.sessions.clear();
                    self.selected_session = None;
                    self.status = "路径筛选已改变，请重新加载会话。".to_owned();
                }

                if self.mode == Mode::Session {
                    ui.horizontal(|ui| {
                        ui.label("关键词");
                        let response = ui.add(
                            egui::TextEdit::singleline(&mut self.search)
                                .hint_text("标题、路径、会话 ID 或消息正文"),
                        );
                        if response.changed() {
                            self.selected_session = None;
                        }
                        ui.add_enabled_ui(self.filter_by_path, |ui| {
                            ui.checkbox(&mut self.recursive, "包含子目录");
                        });
                        if ui.button("加载会话").clicked()
                            || (response.lost_focus()
                                && ui.input(|input| input.key_pressed(egui::Key::Enter)))
                        {
                            self.start_scan(context);
                        }
                    });
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label(format!("会话列表（{}）", self.sessions.len()));
                        ui.weak("单击标题选择，双击标题或点“查看”打开内容");
                    });
                    let widths = session_table_widths(ui.available_width());
                    egui::Frame::new()
                        .fill(ui.visuals().faint_bg_color)
                        .inner_margin(egui::Margin::symmetric(4, 4))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                table_header(ui, "话题标题", widths.title);
                                table_header(ui, "工作目录", widths.path);
                                table_header(ui, "更新时间 ↓", widths.updated);
                                table_header(ui, "状态", widths.status);
                                table_header(ui, "操作", widths.action);
                            });
                        });
                    let mut open_detail = None;
                    egui::ScrollArea::vertical()
                        .max_height(280.0)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            if self.sessions.is_empty() {
                                ui.weak("尚未加载，或没有匹配会话。");
                            }
                            for (index, session) in self.sessions.iter().enumerate() {
                                let selected = self.selected_session.as_deref()
                                    == Some(session.session_id.as_str());
                                let time = format_time(session.updated_at_ms);
                                let title = compact_title(&session.title, 56);
                                let status = if session.archived {
                                    "已归档"
                                } else {
                                    "活动"
                                };
                                let path = session.cwd.display().to_string();
                                let mut select = false;
                                let row_fill = if selected {
                                    ui.visuals().selection.bg_fill
                                } else if index % 2 == 1 {
                                    ui.visuals().faint_bg_color
                                } else {
                                    egui::Color32::TRANSPARENT
                                };
                                egui::Frame::new()
                                    .fill(row_fill)
                                    .inner_margin(egui::Margin::symmetric(4, 2))
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            let response = ui
                                                .add_sized(
                                                    [widths.title, 26.0],
                                                    egui::Button::selectable(selected, title)
                                                        .frame(false)
                                                        .truncate(),
                                                )
                                                .on_hover_text(compact_title(&session.title, 240));
                                            select |= response.clicked();
                                            if response.double_clicked() {
                                                open_detail = Some(session.session_id.clone());
                                            }
                                            let response = ui
                                                .add_sized(
                                                    [widths.path, 26.0],
                                                    egui::Label::new(
                                                        egui::RichText::new(&path).monospace(),
                                                    )
                                                    .truncate()
                                                    .sense(egui::Sense::click()),
                                                )
                                                .on_hover_text(&path);
                                            select |= response.clicked();
                                            let response = ui.add_sized(
                                                [widths.updated, 26.0],
                                                egui::Label::new(&time).sense(egui::Sense::click()),
                                            );
                                            select |= response.clicked();
                                            let response = ui.add_sized(
                                                [widths.status, 26.0],
                                                egui::Label::new(status)
                                                    .sense(egui::Sense::click()),
                                            );
                                            select |= response.clicked();
                                            if ui
                                                .add_sized(
                                                    [widths.action, 24.0],
                                                    egui::Button::new("查看"),
                                                )
                                                .clicked()
                                            {
                                                open_detail = Some(session.session_id.clone());
                                                select = true;
                                            }
                                        });
                                    });
                                if select {
                                    self.selected_session = Some(session.session_id.clone());
                                    self.source = path;
                                }
                            }
                        });
                    if let Some(session_id) = open_detail {
                        self.start_detail(context, session_id);
                    }
                    let selected = self.selected_session.as_deref().and_then(|id| {
                        self.sessions
                            .iter()
                            .find(|session| session.session_id == id)
                            .map(|session| {
                                (
                                    compact_title(&session.title, 56),
                                    session.session_id.clone(),
                                )
                            })
                    });
                    if let Some((title, session_id)) = selected {
                        ui.horizontal(|ui| {
                            ui.strong(format!("已选择：{title}"));
                            if ui.button("查看内容…").clicked() {
                                self.start_detail(context, session_id);
                            }
                        });
                    }
                } else {
                    ui.checkbox(&mut self.move_files, "同时移动磁盘上的整个目录");
                    if self.move_files {
                        ui.weak("目标若是已有目录，将在其中保留源目录名（与 mv 相同）。");
                    } else {
                        ui.weak("仅修复 Codex 指向，适合目录已经由其他方式移动的情况。");
                    }
                }

                let mut pick_destination = false;
                path_row(
                    ui,
                    "目标路径",
                    &mut self.destination,
                    Some(&mut pick_destination),
                );
                if pick_destination {
                    self.choose_destination();
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("预览变更").clicked() {
                        self.start_preview(context);
                    }
                    if ui.button("检查并执行…").clicked() {
                        self.request_apply(context);
                    }
                });
            });

            ui.separator();
            ui.horizontal(|ui| {
                if self.busy() {
                    ui.spinner();
                }
                ui.label(&self.status);
            });
            if !self.details.is_empty() {
                ui.add(
                    egui::TextEdit::multiline(&mut self.details)
                        .desired_rows(9)
                        .font(egui::TextStyle::Monospace)
                        .interactive(false),
                );
            }
        });
        self.confirmation_window(context);
        self.session_detail_window(context);
        self.rename_window(context);
    }
}

#[derive(Clone, Copy)]
struct SessionTableWidths {
    title: f32,
    path: f32,
    updated: f32,
    status: f32,
    action: f32,
}

fn session_table_widths(available: f32) -> SessionTableWidths {
    let content = (available - 56.0).max(540.0);
    let updated = 108.0;
    let status = 54.0;
    let action = 58.0;
    let flexible = content - updated - status - action;
    let title = flexible * 0.48;
    SessionTableWidths {
        title,
        path: flexible - title,
        updated,
        status,
        action,
    }
}

fn table_header(ui: &mut egui::Ui, text: &str, width: f32) {
    ui.add_sized(
        [width, 24.0],
        egui::Label::new(egui::RichText::new(text).strong()).truncate(),
    );
}

fn path_row(ui: &mut egui::Ui, label: &str, value: &mut String, pick: Option<&mut bool>) {
    ui.horizontal(|ui| {
        ui.label(label);
        let picker_width = if pick.is_some() { 72.0 } else { 0.0 };
        let editor_width = (ui.available_width() - picker_width).max(120.0);
        ui.add_sized([editor_width, 24.0], egui::TextEdit::singleline(value));
        if let Some(pick) = pick
            && ui.button("选择…").clicked()
        {
            *pick = true;
        }
    });
}

fn existing_parent(value: &str) -> Option<PathBuf> {
    let path = PathBuf::from(value);
    if path.is_dir() {
        return Some(path);
    }
    path.ancestors()
        .find(|path| path.is_dir())
        .map(Path::to_path_buf)
}

fn create_plan(migrator: &Migrator, input: &ActionInput) -> Result<(PathBuf, Plan)> {
    match input.mode {
        Mode::Session => {
            let session_id = input
                .session_id
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("missing session id"))?;
            let (_, plan) = migrator.plan_session(session_id, &input.destination)?;
            Ok((input.destination.clone(), plan))
        }
        Mode::Workspace if input.move_files => {
            let (_, destination) = validate_workspace_move(&input.source, &input.destination)?;
            let plan = migrator.plan(&input.source, &destination)?;
            Ok((destination, plan))
        }
        Mode::Workspace => {
            if !input.destination.is_dir() {
                bail!("仅修复指向时，目标目录必须已经存在");
            }
            let plan = migrator.plan(&input.source, &input.destination)?;
            Ok((input.destination.clone(), plan))
        }
    }
}

fn plan_summary(plan: &Plan) -> String {
    let mut lines = vec![
        format!("Codex: {} -> {}", plan.old.display(), plan.new.display()),
        format!("共 {} 个文件，{} 处指向", plan.files(), plan.replacements()),
    ];
    lines.extend(
        plan.descriptions()
            .into_iter()
            .map(|line| format!("  {line}")),
    );
    lines.join("\n")
}

fn format_time(timestamp_ms: i64) -> String {
    Local
        .timestamp_millis_opt(timestamp_ms)
        .single()
        .map(|time| time.format("%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "未知时间".to_owned())
}

fn compact_title(title: &str, max_chars: usize) -> String {
    let compact = title.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut characters = compact.chars();
    let shortened = characters.by_ref().take(max_chars).collect::<String>();
    if characters.next().is_some() {
        format!("{shortened}…")
    } else {
        shortened
    }
}

fn resume_command(session: &SessionInfo) -> String {
    format!(
        "cd {} && codex resume {}",
        shell_quote(&session.cwd.display().to_string()),
        shell_quote(&session.session_id)
    )
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn configure_fonts(context: &egui::Context) {
    let mut candidates = vec![
        PathBuf::from("/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc"),
        PathBuf::from("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"),
        PathBuf::from("/usr/share/fonts/WindowsFonts/msyh.ttc"),
    ];
    if let Ok(output) = Command::new("fc-match")
        .args(["-f", "%{file}", "sans-serif:lang=zh-cn"])
        .output()
        && output.status.success()
    {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !path.is_empty() {
            candidates.insert(0, PathBuf::from(path));
        }
    }
    let Some((path, data)) = candidates
        .into_iter()
        .find_map(|path| fs::read(&path).ok().map(|data| (path, data)))
    else {
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    let name = format!("cjk:{}", path.display());
    fonts
        .font_data
        .insert(name.clone(), egui::FontData::from_owned(data).into());
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, name.clone());
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .push(name);
    context.set_fonts(fonts);
}
