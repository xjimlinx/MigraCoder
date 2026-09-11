use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, MAIN_DB, params, types::Value as SqlValue};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use tempfile::NamedTempFile;
use walkdir::WalkDir;

pub mod gui;

#[derive(Debug)]
pub struct TextChange {
    pub path: PathBuf,
    pub content: String,
    pub replacements: usize,
}

#[derive(Debug)]
pub struct RowUpdate {
    table: &'static str,
    column: &'static str,
    identity_column: &'static str,
    identity: SqlValue,
    value: String,
}

#[derive(Debug)]
pub struct DatabaseChange {
    pub path: PathBuf,
    updates: Vec<RowUpdate>,
}

impl DatabaseChange {
    pub fn replacements(&self) -> usize {
        self.updates.len()
    }
}

#[derive(Debug)]
pub struct Plan {
    pub old: PathBuf,
    pub new: PathBuf,
    pub text_changes: Vec<TextChange>,
    pub database_changes: Vec<DatabaseChange>,
}

#[derive(Debug)]
pub struct TitlePlan {
    pub session_id: String,
    pub old_title: String,
    pub new_title: String,
    text_changes: Vec<TextChange>,
    database_changes: Vec<DatabaseChange>,
}

impl TitlePlan {
    pub fn replacements(&self) -> usize {
        self.text_changes
            .iter()
            .map(|change| change.replacements)
            .sum::<usize>()
            + self
                .database_changes
                .iter()
                .map(DatabaseChange::replacements)
                .sum::<usize>()
    }

    pub fn files(&self) -> usize {
        self.text_changes.len() + self.database_changes.len()
    }

    pub fn descriptions(&self) -> Vec<String> {
        let mut result = Vec::new();
        for change in &self.text_changes {
            result.push(format!("text   {}", change.path.display()));
        }
        for change in &self.database_changes {
            result.push(format!(
                "sqlite {} ({})",
                change.path.display(),
                change.replacements()
            ));
        }
        result
    }
}

impl Plan {
    fn new(old: PathBuf, new: PathBuf) -> Self {
        Self {
            old,
            new,
            text_changes: Vec::new(),
            database_changes: Vec::new(),
        }
    }

    pub fn replacements(&self) -> usize {
        self.text_changes
            .iter()
            .map(|change| change.replacements)
            .sum::<usize>()
            + self
                .database_changes
                .iter()
                .map(DatabaseChange::replacements)
                .sum::<usize>()
    }

    pub fn files(&self) -> usize {
        self.text_changes.len() + self.database_changes.len()
    }

    pub fn descriptions(&self) -> Vec<String> {
        let mut result = Vec::new();
        for change in &self.text_changes {
            result.push(format!(
                "text   {} ({})",
                change.path.display(),
                change.replacements
            ));
        }
        for change in &self.database_changes {
            result.push(format!(
                "sqlite {} ({})",
                change.path.display(),
                change.replacements()
            ));
        }
        result
    }
}

#[derive(Clone, Debug)]
pub struct SessionInfo {
    pub session_id: String,
    pub cwd: PathBuf,
    pub title: String,
    pub rollout_path: Option<PathBuf>,
    pub updated_at_ms: i64,
    pub archived: bool,
    pub match_excerpt: String,
}

#[derive(Debug)]
pub struct SessionPreview {
    pub session: SessionInfo,
    pub user_messages: Vec<String>,
}

#[derive(Debug)]
pub struct DoctorReport {
    pub codex_home: PathBuf,
    pub sessions: usize,
    pub workspaces: usize,
    pub databases: usize,
    pub backups: usize,
    pub warnings: Vec<String>,
}

impl DoctorReport {
    pub fn healthy(&self) -> bool {
        self.warnings.is_empty()
    }
}

#[derive(Debug)]
pub struct Migrator {
    pub codex_home: PathBuf,
}

impl Migrator {
    pub fn new(codex_home: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            codex_home: normalize_path(codex_home)?,
        })
    }

    pub fn plan(&self, old: impl AsRef<Path>, new: impl AsRef<Path>) -> Result<Plan> {
        let old = normalize_path(old)?;
        let new = normalize_path(new)?;
        if old == new {
            bail!("source and destination resolve to the same path");
        }
        let mut plan = Plan::new(old, new);
        self.plan_config(&mut plan)?;
        self.plan_global_state(&mut plan)?;
        self.plan_sessions(&mut plan)?;
        self.plan_databases(&mut plan)?;
        Ok(plan)
    }

    pub fn find_sessions(
        &self,
        path: impl AsRef<Path>,
        recursive: bool,
        search: Option<&str>,
    ) -> Result<Vec<SessionInfo>> {
        let target = normalize_path(path)?;
        let mut matches = Vec::new();
        for mut session in self.load_sessions()?.into_values() {
            let path_matches = session.cwd == target
                || (recursive && session.cwd.starts_with(&target) && session.cwd != target);
            if !path_matches {
                continue;
            }
            if let Some(search) = search {
                let Some(excerpt) = self.matching_session_excerpt(&session, search)? else {
                    continue;
                };
                session.match_excerpt = excerpt;
            }
            matches.push(session);
        }
        matches.sort_by_key(|session| std::cmp::Reverse(session.updated_at_ms));
        Ok(matches)
    }

    pub fn list_sessions(&self, search: Option<&str>) -> Result<Vec<SessionInfo>> {
        let mut matches = Vec::new();
        for mut session in self.load_sessions()?.into_values() {
            if let Some(search) = search {
                let Some(excerpt) = self.matching_session_excerpt(&session, search)? else {
                    continue;
                };
                session.match_excerpt = excerpt;
            }
            matches.push(session);
        }
        matches.sort_by_key(|session| std::cmp::Reverse(session.updated_at_ms));
        Ok(matches)
    }

    pub fn doctor(&self) -> Result<DoctorReport> {
        let mut warnings = Vec::new();
        if !self.codex_home.is_dir() {
            warnings.push(format!(
                "Codex 数据目录不存在：{}",
                self.codex_home.display()
            ));
            return Ok(DoctorReport {
                codex_home: self.codex_home.clone(),
                sessions: 0,
                workspaces: 0,
                databases: 0,
                backups: 0,
                warnings,
            });
        }

        let sessions = self.list_sessions(None)?;
        let workspaces = sessions
            .iter()
            .map(|session| session.cwd.clone())
            .collect::<HashSet<_>>()
            .len();
        let databases = self.all_databases()?.len();
        let backups_root = self.codex_home.join("migracoder-backups");
        let backups = if backups_root.is_dir() {
            fs::read_dir(&backups_root)?
                .filter_map(Result::ok)
                .filter(|entry| entry.path().join("manifest.json").is_file())
                .count()
        } else {
            0
        };
        if sessions.is_empty() {
            warnings.push("没有发现可识别的 Codex 会话".to_owned());
        }
        if databases == 0 {
            warnings.push("没有发现 Codex 状态数据库，将只检查文本数据".to_owned());
        }
        Ok(DoctorReport {
            codex_home: self.codex_home.clone(),
            sessions: sessions.len(),
            workspaces,
            databases,
            backups,
            warnings,
        })
    }

    pub fn resolve_session(&self, reference: &str) -> Result<SessionInfo> {
        let sessions = self.load_sessions()?;
        if let Some(session) = sessions.get(reference) {
            return Ok(session.clone());
        }
        let matches: Vec<_> = sessions
            .values()
            .filter(|session| session.session_id.starts_with(reference))
            .cloned()
            .collect();
        match matches.as_slice() {
            [] => bail!("session not found: {reference}"),
            [session] => Ok(session.clone()),
            many => {
                let ids = many
                    .iter()
                    .take(5)
                    .map(|item| item.session_id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!("session prefix is ambiguous: {reference} ({ids})")
            }
        }
    }

    pub fn session_preview(
        &self,
        reference: &str,
        max_messages: usize,
        search: Option<&str>,
    ) -> Result<SessionPreview> {
        let session = self.resolve_session(reference)?;
        let mut messages = Vec::new();
        if let Some(path) = session
            .rollout_path
            .as_deref()
            .filter(|path| path.is_file())
        {
            for line in BufReader::new(File::open(path)?).lines() {
                let Ok(value) = serde_json::from_str::<Value>(&line?) else {
                    continue;
                };
                let text = compact_preview_text(&user_text_from_record(&value), 600);
                if text.is_empty()
                    || text.starts_with("<environment_context>")
                    || messages.last() == Some(&text)
                {
                    continue;
                }
                messages.push(text);
            }
        }
        if max_messages == 0 {
            messages.clear();
        } else if messages.len() > max_messages {
            let mut selected = HashSet::from([0]);
            if max_messages > 1
                && let Some(needle) = search
                    .map(str::to_lowercase)
                    .filter(|text| !text.is_empty())
                && let Some(index) = messages
                    .iter()
                    .position(|message| message.to_lowercase().contains(&needle))
            {
                selected.insert(index);
            }
            for index in (0..messages.len()).rev() {
                if selected.len() >= max_messages {
                    break;
                }
                selected.insert(index);
            }
            let mut indices = selected.into_iter().collect::<Vec<_>>();
            indices.sort_unstable();
            messages = indices
                .into_iter()
                .map(|index| messages[index].clone())
                .collect();
        }
        Ok(SessionPreview {
            session,
            user_messages: messages,
        })
    }

    pub fn plan_session(
        &self,
        reference: &str,
        new: impl AsRef<Path>,
    ) -> Result<(SessionInfo, Plan)> {
        let session = self.resolve_session(reference)?;
        let old = normalize_path(&session.cwd)?;
        let new = normalize_path(new)?;
        if old == new {
            bail!("session already points to the destination");
        }
        let mut plan = Plan::new(old, new);
        if let Some(path) = session
            .rollout_path
            .as_deref()
            .filter(|path| path.is_file())
        {
            self.plan_one_session_file(&mut plan, path)?;
        }
        self.plan_thread_global_state(&mut plan, &session.session_id)?;
        self.plan_session_databases(&mut plan, &session.session_id)?;
        Ok((session, plan))
    }

    pub fn plan_session_title(&self, reference: &str, title: &str) -> Result<TitlePlan> {
        let session = self.resolve_session(reference)?;
        let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
        if title.is_empty() {
            bail!("title cannot be empty");
        }
        if title.chars().count() > 160 {
            bail!("title is too long (maximum 160 characters)");
        }
        if session.title == title {
            bail!("session already has this title");
        }
        let mut plan = TitlePlan {
            session_id: session.session_id.clone(),
            old_title: session.title,
            new_title: title,
            text_changes: Vec::new(),
            database_changes: Vec::new(),
        };

        let index = self.codex_home.join("session_index.jsonl");
        if index.is_file() {
            let original = fs::read_to_string(&index)?;
            let mut content = original.clone();
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(&serde_json::to_string(&json!({
                "id": plan.session_id,
                "thread_name": plan.new_title,
                "updated_at": Utc::now().to_rfc3339(),
            }))?);
            content.push('\n');
            plan.text_changes.push(TextChange {
                path: index,
                content,
                replacements: 1,
            });
        }
        self.plan_session_title_databases(&mut plan)?;
        if plan.files() == 0 {
            bail!("no writable Codex title store was found for this session");
        }
        Ok(plan)
    }

    pub fn apply(&self, plan: &Plan) -> Result<Option<PathBuf>> {
        if plan.files() == 0 {
            return Ok(None);
        }
        let backup_dir = self.create_backup(plan)?;
        let result = (|| -> Result<()> {
            for change in &plan.text_changes {
                atomic_write(&change.path, change.content.as_bytes())?;
            }
            for change in &plan.database_changes {
                apply_database(change)?;
            }
            Ok(())
        })();
        if let Err(original) = result {
            if let Err(restore) = self.restore_backup(&backup_dir) {
                return Err(anyhow!(
                    "migration failed: {original:#}; backup restore also failed: {restore:#}"
                ));
            }
            return Err(original);
        }
        Ok(Some(backup_dir))
    }

    pub fn apply_session_title(&self, plan: &TitlePlan) -> Result<Option<PathBuf>> {
        if plan.files() == 0 {
            return Ok(None);
        }
        let backup_dir = self.create_title_backup(plan)?;
        let result = (|| -> Result<()> {
            for change in &plan.text_changes {
                atomic_write(&change.path, change.content.as_bytes())?;
            }
            for change in &plan.database_changes {
                apply_database(change)?;
            }
            Ok(())
        })();
        if let Err(original) = result {
            if let Err(restore) = self.restore_backup(&backup_dir) {
                return Err(anyhow!(
                    "title update failed: {original:#}; backup restore also failed: {restore:#}"
                ));
            }
            return Err(original);
        }
        Ok(Some(backup_dir))
    }

    fn session_roots(&self) -> [PathBuf; 2] {
        [
            self.codex_home.join("sessions"),
            self.codex_home.join("archived_sessions"),
        ]
    }

    fn state_databases(&self) -> Result<Vec<PathBuf>> {
        let mut paths = Vec::new();
        if self.codex_home.is_dir() {
            for entry in fs::read_dir(&self.codex_home)? {
                let path = entry?.path();
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("");
                if path.is_file() && name.starts_with("state_") && name.ends_with(".sqlite") {
                    paths.push(path);
                }
            }
        }
        paths.sort();
        paths.reverse();
        Ok(paths)
    }

    fn all_databases(&self) -> Result<Vec<PathBuf>> {
        let mut paths = self.state_databases()?;
        let sqlite_dir = self.codex_home.join("sqlite");
        if sqlite_dir.is_dir() {
            for entry in fs::read_dir(sqlite_dir)? {
                let path = entry?.path();
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("");
                if path.is_file() && name.starts_with("codex") && name.ends_with(".db") {
                    paths.push(path);
                }
            }
        }
        Ok(paths)
    }

    fn plan_config(&self, plan: &mut Plan) -> Result<()> {
        let path = self.codex_home.join("config.toml");
        if !path.is_file() {
            return Ok(());
        }
        let original = fs::read_to_string(&path)?;
        let (content, count) = replace_config_paths(&original, &plan.old, &plan.new);
        if count > 0 {
            plan.text_changes.push(TextChange {
                path,
                content,
                replacements: count,
            });
        }
        Ok(())
    }

    fn plan_global_state(&self, plan: &mut Plan) -> Result<()> {
        let path = self.codex_home.join(".codex-global-state.json");
        if !path.is_file() {
            return Ok(());
        }
        let original = fs::read_to_string(&path)?;
        let mut value: Value = serde_json::from_str(&original)
            .with_context(|| format!("invalid JSON in {}", path.display()))?;
        let count = replace_state_paths(&mut value, &plan.old, &plan.new, false);
        self.add_json_change(plan, path, &original, &value, count)
    }

    fn plan_thread_global_state(&self, plan: &mut Plan, thread_id: &str) -> Result<()> {
        let path = self.codex_home.join(".codex-global-state.json");
        if !path.is_file() {
            return Ok(());
        }
        let original = fs::read_to_string(&path)?;
        let mut value: Value = serde_json::from_str(&original)
            .with_context(|| format!("invalid JSON in {}", path.display()))?;
        let count =
            replace_thread_state_paths(&mut value, thread_id, &plan.old, &plan.new, false, false);
        self.add_json_change(plan, path, &original, &value, count)
    }

    fn add_json_change(
        &self,
        plan: &mut Plan,
        path: PathBuf,
        original: &str,
        value: &Value,
        count: usize,
    ) -> Result<()> {
        if count == 0 {
            return Ok(());
        }
        let pretty = original.trim_start().starts_with("{\n");
        let mut content = if pretty {
            serde_json::to_string_pretty(value)?
        } else {
            serde_json::to_string(value)?
        };
        if original.ends_with('\n') {
            content.push('\n');
        }
        plan.text_changes.push(TextChange {
            path,
            content,
            replacements: count,
        });
        Ok(())
    }

    fn plan_sessions(&self, plan: &mut Plan) -> Result<()> {
        for root in self.session_roots() {
            if !root.is_dir() {
                continue;
            }
            for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
                let path = entry.path();
                if entry.file_type().is_file()
                    && path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
                {
                    self.plan_session_file(plan, path, false)?;
                }
            }
        }
        Ok(())
    }

    fn plan_one_session_file(&self, plan: &mut Plan, path: &Path) -> Result<()> {
        self.plan_session_file(plan, path, true)
    }

    fn plan_session_file(&self, plan: &mut Plan, path: &Path, exact: bool) -> Result<()> {
        let original = fs::read_to_string(path)?;
        let trailing_newline = original.ends_with('\n');
        let mut output = Vec::new();
        let mut changes = 0;
        for (index, line) in original.lines().enumerate() {
            if line.trim().is_empty() {
                output.push(String::new());
                continue;
            }
            let mut record: Value = serde_json::from_str(line)
                .with_context(|| format!("invalid JSONL in {}:{}", path.display(), index + 1))?;
            let changed = replace_session_record(&mut record, &plan.old, &plan.new, exact);
            if changed > 0 {
                output.push(serde_json::to_string(&record)?);
                changes += changed;
            } else {
                output.push(line.to_owned());
            }
        }
        if changes > 0 {
            let mut content = output.join("\n");
            if trailing_newline {
                content.push('\n');
            }
            plan.text_changes.push(TextChange {
                path: path.to_path_buf(),
                content,
                replacements: changes,
            });
        }
        Ok(())
    }

    fn plan_databases(&self, plan: &mut Plan) -> Result<()> {
        for path in self.all_databases()? {
            let connection = open_read_only(&path)?;
            let tables = table_names(&connection)?;
            let mut updates = Vec::new();
            for (table, identity, column) in [
                ("threads", "id", "cwd"),
                ("project_roots", "rowid", "path"),
                ("local_thread_catalog", "rowid", "cwd"),
                ("automation_runs", "rowid", "source_cwd"),
            ] {
                if !tables.contains(table) || !has_column(&connection, table, column)? {
                    continue;
                }
                let query = format!(
                    "SELECT {identity}, \"{column}\" FROM \"{table}\" \
                     WHERE \"{column}\" IS NOT NULL AND \"{column}\" <> ''"
                );
                let mut statement = connection.prepare(&query)?;
                let rows = statement.query_map([], |row| {
                    Ok((row.get::<_, SqlValue>(0)?, row.get::<_, String>(1)?))
                })?;
                for row in rows {
                    let (id, current) = row?;
                    if let Some(value) = replace_path(&current, &plan.old, &plan.new, false) {
                        updates.push(RowUpdate {
                            table,
                            column,
                            identity_column: identity,
                            identity: id,
                            value,
                        });
                    }
                }
            }
            for (table, identity, column) in [
                ("threads", "id", "sandbox_policy"),
                ("automations", "rowid", "cwds"),
            ] {
                if !tables.contains(table) || !has_column(&connection, table, column)? {
                    continue;
                }
                let query = format!(
                    "SELECT {identity}, \"{column}\" FROM \"{table}\" \
                     WHERE \"{column}\" IS NOT NULL AND \"{column}\" <> ''"
                );
                let mut statement = connection.prepare(&query)?;
                let rows = statement.query_map([], |row| {
                    Ok((row.get::<_, SqlValue>(0)?, row.get::<_, String>(1)?))
                })?;
                for row in rows {
                    let (id, current) = row?;
                    if let Some(value) = replace_json_paths(&current, &plan.old, &plan.new, false) {
                        updates.push(RowUpdate {
                            table,
                            column,
                            identity_column: identity,
                            identity: id,
                            value,
                        });
                    }
                }
            }
            if !updates.is_empty() {
                plan.database_changes.push(DatabaseChange { path, updates });
            }
        }
        Ok(())
    }

    fn plan_session_databases(&self, plan: &mut Plan, thread_id: &str) -> Result<()> {
        for path in self.all_databases()? {
            let connection = open_read_only(&path)?;
            let tables = table_names(&connection)?;
            let mut updates = Vec::new();
            if tables.contains("threads")
                && has_column(&connection, "threads", "id")?
                && has_column(&connection, "threads", "cwd")?
            {
                let mut statement =
                    connection.prepare("SELECT id, cwd FROM threads WHERE id = ?1")?;
                let rows = statement.query_map([thread_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                for row in rows {
                    let (id, current) = row?;
                    if let Some(value) = replace_path(&current, &plan.old, &plan.new, true) {
                        updates.push(RowUpdate {
                            table: "threads",
                            column: "cwd",
                            identity_column: "id",
                            identity: SqlValue::Text(id),
                            value,
                        });
                    }
                }
            }
            if tables.contains("threads")
                && has_column(&connection, "threads", "id")?
                && has_column(&connection, "threads", "sandbox_policy")?
            {
                let mut statement =
                    connection.prepare("SELECT id, sandbox_policy FROM threads WHERE id = ?1")?;
                let rows = statement.query_map([thread_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                for row in rows {
                    let (id, current) = row?;
                    if let Some(value) = replace_json_paths(&current, &plan.old, &plan.new, true) {
                        updates.push(RowUpdate {
                            table: "threads",
                            column: "sandbox_policy",
                            identity_column: "id",
                            identity: SqlValue::Text(id),
                            value,
                        });
                    }
                }
            }
            if tables.contains("local_thread_catalog")
                && has_column(&connection, "local_thread_catalog", "thread_id")?
                && has_column(&connection, "local_thread_catalog", "cwd")?
            {
                let mut statement = connection
                    .prepare("SELECT rowid, cwd FROM local_thread_catalog WHERE thread_id = ?1")?;
                let rows = statement.query_map([thread_id], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })?;
                for row in rows {
                    let (id, current) = row?;
                    if let Some(value) = replace_path(&current, &plan.old, &plan.new, true) {
                        updates.push(RowUpdate {
                            table: "local_thread_catalog",
                            column: "cwd",
                            identity_column: "rowid",
                            identity: SqlValue::Integer(id),
                            value,
                        });
                    }
                }
            }
            if !updates.is_empty() {
                plan.database_changes.push(DatabaseChange { path, updates });
            }
        }
        Ok(())
    }

    fn plan_session_title_databases(&self, plan: &mut TitlePlan) -> Result<()> {
        for path in self.all_databases()? {
            let connection = open_read_only(&path)?;
            let tables = table_names(&connection)?;
            let mut updates = Vec::new();
            if tables.contains("threads") && has_column(&connection, "threads", "id")? {
                for column in ["title", "name"] {
                    if !has_column(&connection, "threads", column)? {
                        continue;
                    }
                    let query = format!("SELECT id, \"{column}\" FROM threads WHERE id = ?1");
                    let mut statement = connection.prepare(&query)?;
                    let rows = statement.query_map([&plan.session_id], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                    })?;
                    for row in rows {
                        let (id, current) = row?;
                        if current.as_deref() != Some(plan.new_title.as_str()) {
                            updates.push(RowUpdate {
                                table: "threads",
                                column,
                                identity_column: "id",
                                identity: SqlValue::Text(id),
                                value: plan.new_title.clone(),
                            });
                        }
                    }
                }
            }
            if tables.contains("local_thread_catalog")
                && has_column(&connection, "local_thread_catalog", "thread_id")?
                && has_column(&connection, "local_thread_catalog", "display_title")?
            {
                let mut statement = connection.prepare(
                    "SELECT rowid, display_title FROM local_thread_catalog WHERE thread_id = ?1",
                )?;
                let rows = statement.query_map([&plan.session_id], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?))
                })?;
                for row in rows {
                    let (id, current) = row?;
                    if current.as_deref() != Some(plan.new_title.as_str()) {
                        updates.push(RowUpdate {
                            table: "local_thread_catalog",
                            column: "display_title",
                            identity_column: "rowid",
                            identity: SqlValue::Integer(id),
                            value: plan.new_title.clone(),
                        });
                    }
                }
            }
            if !updates.is_empty() {
                plan.database_changes.push(DatabaseChange { path, updates });
            }
        }
        Ok(())
    }

    fn load_sessions(&self) -> Result<HashMap<String, SessionInfo>> {
        let names = self.load_session_names()?;
        let mut sessions = HashMap::new();
        for path in self.state_databases()? {
            let connection = open_read_only(&path)?;
            if !table_names(&connection)?.contains("threads") {
                continue;
            }
            let columns = columns(&connection, "threads")?;
            if !columns.contains("id") || !columns.contains("cwd") {
                continue;
            }
            let title = if columns.contains("title") {
                "title"
            } else {
                "'' AS title"
            };
            let rollout = if columns.contains("rollout_path") {
                "rollout_path"
            } else {
                "'' AS rollout_path"
            };
            let updated = if columns.contains("updated_at_ms") {
                "COALESCE(updated_at_ms, 0) AS updated_at_ms"
            } else if columns.contains("updated_at") {
                "updated_at * 1000 AS updated_at_ms"
            } else {
                "0 AS updated_at_ms"
            };
            let archived = if columns.contains("archived") {
                "archived"
            } else {
                "0 AS archived"
            };
            let query =
                format!("SELECT id, cwd, {title}, {rollout}, {updated}, {archived} FROM threads");
            let mut statement = connection.prepare(&query)?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })?;
            for row in rows {
                let (id, cwd, title, rollout, updated, archived) = row?;
                sessions.entry(id.clone()).or_insert_with(|| SessionInfo {
                    session_id: id.clone(),
                    cwd: PathBuf::from(cwd),
                    title: names
                        .get(&id)
                        .cloned()
                        .or_else(|| (!title.is_empty()).then_some(title))
                        .unwrap_or_else(|| "(无标题)".to_owned()),
                    rollout_path: (!rollout.is_empty()).then(|| PathBuf::from(rollout)),
                    updated_at_ms: updated,
                    archived: archived != 0,
                    match_excerpt: String::new(),
                });
            }
        }

        for root in self.session_roots() {
            if !root.is_dir() {
                continue;
            }
            for entry in WalkDir::new(&root).into_iter().filter_map(Result::ok) {
                let path = entry.path();
                if !entry.file_type().is_file()
                    || path.extension().and_then(|ext| ext.to_str()) != Some("jsonl")
                {
                    continue;
                }
                let Some((id, cwd, timestamp)) = read_session_metadata(path)? else {
                    continue;
                };
                if let Some(session) = sessions.get_mut(&id) {
                    if session.rollout_path.is_none() {
                        session.rollout_path = Some(path.to_path_buf());
                    }
                    continue;
                }
                let modified = fs::metadata(path)
                    .and_then(|meta| meta.modified())
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|duration| duration.as_millis() as i64)
                    .unwrap_or(0);
                sessions.insert(
                    id.clone(),
                    SessionInfo {
                        session_id: id.clone(),
                        cwd,
                        title: names
                            .get(&id)
                            .cloned()
                            .unwrap_or_else(|| "(无标题)".to_owned()),
                        rollout_path: Some(path.to_path_buf()),
                        updated_at_ms: timestamp.max(modified),
                        archived: root.ends_with("archived_sessions"),
                        match_excerpt: String::new(),
                    },
                );
            }
        }
        Ok(sessions)
    }

    fn load_session_names(&self) -> Result<HashMap<String, String>> {
        let path = self.codex_home.join("session_index.jsonl");
        let mut names = HashMap::new();
        if !path.is_file() {
            return Ok(names);
        }
        for line in BufReader::new(File::open(path)?).lines() {
            let Ok(value) = serde_json::from_str::<Value>(&line?) else {
                continue;
            };
            if let (Some(id), Some(name)) = (
                value.get("id").and_then(Value::as_str),
                value.get("thread_name").and_then(Value::as_str),
            ) {
                names.insert(id.to_owned(), name.to_owned());
            }
        }
        Ok(names)
    }

    fn matching_session_excerpt(
        &self,
        session: &SessionInfo,
        search: &str,
    ) -> Result<Option<String>> {
        let needle = search.to_lowercase();
        if session.title.to_lowercase().contains(&needle) {
            return Ok(Some(session.title.clone()));
        }
        if session.session_id.to_lowercase().contains(&needle) {
            return Ok(Some(format!("会话 ID：{}", session.session_id)));
        }
        let cwd = session.cwd.display().to_string();
        if cwd.to_lowercase().contains(&needle) {
            return Ok(Some(format!("工作目录：{cwd}")));
        }
        let Some(path) = session
            .rollout_path
            .as_deref()
            .filter(|path| path.is_file())
        else {
            return Ok(None);
        };
        for line in BufReader::new(File::open(path)?).lines() {
            let Ok(value) = serde_json::from_str::<Value>(&line?) else {
                continue;
            };
            let text = user_text_from_record(&value);
            if text.to_lowercase().contains(&needle) {
                let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
                return Ok(Some(compact.chars().take(180).collect()));
            }
        }
        Ok(None)
    }

    fn create_backup(&self, plan: &Plan) -> Result<PathBuf> {
        self.create_backup_for_changes(
            &plan.text_changes,
            &plan.database_changes,
            json!({
                "operation": "repoint",
                "old": plan.old,
                "new": plan.new,
            }),
        )
    }

    fn create_title_backup(&self, plan: &TitlePlan) -> Result<PathBuf> {
        self.create_backup_for_changes(
            &plan.text_changes,
            &plan.database_changes,
            json!({
                "operation": "rename-session",
                "session_id": plan.session_id,
                "old_title": plan.old_title,
                "new_title": plan.new_title,
            }),
        )
    }

    fn create_backup_for_changes(
        &self,
        text_changes: &[TextChange],
        database_changes: &[DatabaseChange],
        mut manifest: Value,
    ) -> Result<PathBuf> {
        let timestamp = Utc::now().format("%Y%m%dT%H%M%S.%6fZ").to_string();
        let backup_dir = self.codex_home.join("migracoder-backups").join(timestamp);
        fs::create_dir_all(&backup_dir)?;
        let mut files = Vec::new();
        for change in text_changes {
            let relative = change.path.strip_prefix(&self.codex_home)?;
            let destination = backup_dir.join(relative);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&change.path, &destination)?;
            files.push(json!({"path": relative, "kind": "file"}));
        }
        for change in database_changes {
            let relative = change.path.strip_prefix(&self.codex_home)?;
            let destination = backup_dir.join(relative);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            backup_database(&change.path, &destination)?;
            files.push(json!({"path": relative, "kind": "sqlite"}));
        }
        let fields = manifest
            .as_object_mut()
            .ok_or_else(|| anyhow!("backup metadata must be a JSON object"))?;
        fields.insert("version".to_owned(), json!(1));
        fields.insert("created_at".to_owned(), json!(Utc::now().to_rfc3339()));
        fields.insert("files".to_owned(), Value::Array(files));
        let content = format!("{}\n", serde_json::to_string_pretty(&manifest)?);
        atomic_write(&backup_dir.join("manifest.json"), content.as_bytes())?;
        Ok(backup_dir)
    }

    fn restore_backup(&self, backup_dir: &Path) -> Result<()> {
        #[derive(Deserialize)]
        struct Manifest {
            files: Vec<ManifestFile>,
        }
        #[derive(Deserialize)]
        struct ManifestFile {
            path: PathBuf,
            kind: String,
        }
        let manifest: Manifest =
            serde_json::from_slice(&fs::read(backup_dir.join("manifest.json"))?)?;
        for item in manifest.files {
            let source = backup_dir.join(&item.path);
            let destination = self.codex_home.join(&item.path);
            if item.kind == "sqlite" {
                restore_database(&source, &destination)?;
            } else {
                fs::copy(source, destination)?;
            }
        }
        Ok(())
    }
}

pub fn normalize_path(path: impl AsRef<Path>) -> Result<PathBuf> {
    let raw = path.as_ref();
    let expanded = if raw == Path::new("~") {
        PathBuf::from(env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?)
    } else if let Ok(rest) = raw.strip_prefix("~/") {
        PathBuf::from(env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?).join(rest)
    } else {
        raw.to_path_buf()
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        env::current_dir()?.join(expanded)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    if normalized == Path::new("/") {
        bail!("refusing to migrate the filesystem root");
    }
    Ok(normalized)
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn replace_path(value: &str, old: &Path, new: &Path, exact: bool) -> Option<String> {
    let old = path_string(old);
    let new = path_string(new);
    if value == old {
        return Some(new);
    }
    let file_old = format!("file://{old}");
    if value == file_old {
        return Some(format!("file://{new}"));
    }
    if !exact {
        if let Some(suffix) = value.strip_prefix(&(old.clone() + "/")) {
            return Some(format!("{new}/{suffix}"));
        }
        if let Some(suffix) = value.strip_prefix(&(file_old + "/")) {
            return Some(format!("file://{new}/{suffix}"));
        }
    }
    None
}

fn replace_all_json_paths(value: &mut Value, old: &Path, new: &Path, exact: bool) -> usize {
    match value {
        Value::String(text) => {
            if let Some(replaced) = replace_path(text, old, new, exact) {
                *text = replaced;
                1
            } else {
                0
            }
        }
        Value::Array(items) => items
            .iter_mut()
            .map(|item| replace_all_json_paths(item, old, new, exact))
            .sum(),
        Value::Object(items) => items
            .values_mut()
            .map(|item| replace_all_json_paths(item, old, new, exact))
            .sum(),
        _ => 0,
    }
}

fn replace_json_paths(text: &str, old: &Path, new: &Path, exact: bool) -> Option<String> {
    let mut value = serde_json::from_str::<Value>(text).ok()?;
    let changes = replace_all_json_paths(&mut value, old, new, exact);
    (changes > 0)
        .then(|| serde_json::to_string(&value).expect("serializing JSON value cannot fail"))
}

fn is_path_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|character| *character != '-' && *character != '_')
        .flat_map(char::to_lowercase)
        .collect();
    matches!(
        normalized.as_str(),
        "cwd"
            | "path"
            | "paths"
            | "root"
            | "roots"
            | "rootpaths"
            | "workdir"
            | "workspacepath"
            | "workspacepaths"
            | "workspaceroots"
            | "writableroots"
            | "projectsources"
            | "runtimeworkspaceroots"
            | "threadworkspaceroothints"
            | "threadwritableroots"
            | "threadprojectlessoutputdirectories"
    ) || normalized.ends_with("directory")
        || normalized.ends_with("directories")
}

fn replace_state_paths(value: &mut Value, old: &Path, new: &Path, path_context: bool) -> usize {
    if path_context {
        return replace_all_json_paths(value, old, new, false);
    }
    match value {
        Value::Array(items) => items
            .iter_mut()
            .map(|item| replace_state_paths(item, old, new, false))
            .sum(),
        Value::Object(items) => items
            .iter_mut()
            .map(|(key, item)| replace_state_paths(item, old, new, is_path_key(key)))
            .sum(),
        _ => 0,
    }
}

fn replace_thread_state_paths(
    value: &mut Value,
    thread_id: &str,
    old: &Path,
    new: &Path,
    selected: bool,
    path_context: bool,
) -> usize {
    match value {
        Value::String(text) => {
            if selected
                && path_context
                && let Some(replaced) = replace_path(text, old, new, true)
            {
                *text = replaced;
                return 1;
            }
            0
        }
        Value::Array(items) => items
            .iter_mut()
            .map(|item| {
                replace_thread_state_paths(item, thread_id, old, new, selected, path_context)
            })
            .sum(),
        Value::Object(items) => items
            .iter_mut()
            .map(|(key, item)| {
                let prefix = key.split(':').next().unwrap_or(key);
                replace_thread_state_paths(
                    item,
                    thread_id,
                    old,
                    new,
                    selected || key.contains(thread_id),
                    path_context || is_path_key(key) || is_path_key(prefix),
                )
            })
            .sum(),
        _ => 0,
    }
}

fn replace_config_paths(text: &str, old: &Path, new: &Path) -> (String, usize) {
    let old = path_string(old);
    let new = path_string(new);
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    let mut count = 0;
    while let Some(index) = rest.find(&old) {
        let end = index + old.len();
        output.push_str(&rest[..index]);
        let following = rest[end..].chars().next();
        let boundary = following.is_none_or(|character| {
            matches!(
                character,
                '/' | '\\' | '"' | '\'' | ' ' | '\t' | '\r' | '\n' | ',' | ']' | '}' | '='
            )
        });
        if boundary {
            output.push_str(&new);
            count += 1;
        } else {
            output.push_str(&old);
        }
        rest = &rest[end..];
    }
    output.push_str(rest);
    (output, count)
}

fn replace_session_record(record: &mut Value, old: &Path, new: &Path, exact: bool) -> usize {
    let Some(record_type) = record.get("type").and_then(Value::as_str) else {
        return 0;
    };
    if record_type != "session_meta" && record_type != "turn_context" {
        return 0;
    }
    let Some(payload) = record.get_mut("payload").and_then(Value::as_object_mut) else {
        return 0;
    };
    let mut count = 0;
    for key in ["cwd", "workspace_roots"] {
        if let Some(value) = payload.get_mut(key) {
            count += replace_all_json_paths(value, old, new, exact);
        }
    }
    count
}

fn user_text_from_record(record: &Value) -> String {
    let Some(payload) = record.get("payload").and_then(Value::as_object) else {
        return String::new();
    };
    let content = if record.get("type").and_then(Value::as_str) == Some("event_msg")
        && payload.get("type").and_then(Value::as_str) == Some("item_completed")
        && payload
            .get("item")
            .and_then(Value::as_object)
            .and_then(|item| item.get("type"))
            .and_then(Value::as_str)
            == Some("UserMessage")
    {
        payload.get("item").and_then(|item| item.get("content"))
    } else if record.get("type").and_then(Value::as_str) == Some("event_msg")
        && payload.get("type").and_then(Value::as_str) == Some("user_message")
    {
        payload.get("message")
    } else if record.get("type").and_then(Value::as_str) == Some("response_item")
        && payload.get("type").and_then(Value::as_str) == Some("message")
        && payload.get("role").and_then(Value::as_str) == Some("user")
    {
        payload.get("content")
    } else {
        None
    };
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

fn compact_preview_text(text: &str, max_chars: usize) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut characters = compact.chars();
    let shortened = characters.by_ref().take(max_chars).collect::<String>();
    if characters.next().is_some() {
        format!("{shortened}…")
    } else {
        shortened
    }
}

fn read_session_metadata(path: &Path) -> Result<Option<(String, PathBuf, i64)>> {
    for line in BufReader::new(File::open(path)?).lines() {
        let value: Value = serde_json::from_str(&line?)?;
        if value.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let Some(payload) = value.get("payload").and_then(Value::as_object) else {
            return Ok(None);
        };
        let id = payload
            .get("id")
            .or_else(|| payload.get("session_id"))
            .and_then(Value::as_str);
        let cwd = payload.get("cwd").and_then(Value::as_str);
        let (Some(id), Some(cwd)) = (id, cwd) else {
            return Ok(None);
        };
        let timestamp = payload
            .get("timestamp")
            .or_else(|| value.get("timestamp"))
            .and_then(Value::as_str)
            .and_then(|timestamp| DateTime::parse_from_rfc3339(timestamp).ok())
            .map(|timestamp| timestamp.timestamp_millis())
            .unwrap_or(0);
        return Ok(Some((id.to_owned(), PathBuf::from(cwd), timestamp)));
    }
    Ok(None)
}

fn open_read_only(path: &Path) -> Result<Connection> {
    Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("could not inspect {}", path.display()))
}

fn table_names(connection: &Connection) -> Result<HashSet<String>> {
    let mut statement = connection.prepare("SELECT name FROM sqlite_master WHERE type='table'")?;
    let rows = statement.query_map([], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn columns(connection: &Connection, table: &str) -> Result<HashSet<String>> {
    let query = format!("PRAGMA table_info(\"{table}\")");
    let mut statement = connection.prepare(&query)?;
    let rows = statement.query_map([], |row| row.get(1))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn has_column(connection: &Connection, table: &str, column: &str) -> Result<bool> {
    Ok(columns(connection, table)?.contains(column))
}

fn apply_database(change: &DatabaseChange) -> Result<()> {
    let mut connection = Connection::open(&change.path)?;
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    for update in &change.updates {
        let query = format!(
            "UPDATE \"{}\" SET \"{}\" = ?1 WHERE \"{}\" = ?2",
            update.table, update.column, update.identity_column
        );
        transaction.execute(&query, params![update.value, update.identity])?;
    }
    transaction.commit()?;
    Ok(())
}

fn backup_database(source: &Path, destination: &Path) -> Result<()> {
    let source_connection = Connection::open(source)?;
    source_connection.backup(MAIN_DB, destination, None)?;
    Ok(())
}

fn restore_database(source: &Path, destination: &Path) -> Result<()> {
    let mut destination_connection = Connection::open(destination)?;
    destination_connection.restore(MAIN_DB, source, None::<fn(rusqlite::backup::Progress)>)?;
    Ok(())
}

fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| anyhow!("path has no parent"))?;
    let mut temporary = NamedTempFile::new_in(parent)?;
    temporary.write_all(content)?;
    temporary.as_file().sync_all()?;
    if let Ok(metadata) = fs::metadata(path) {
        fs::set_permissions(temporary.path(), metadata.permissions())?;
    }
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("could not replace {}", path.display()))?;
    Ok(())
}

pub fn validate_workspace_move(
    old: impl AsRef<Path>,
    new: impl AsRef<Path>,
) -> Result<(PathBuf, PathBuf)> {
    let source = normalize_path(old)?;
    if !source.is_dir() {
        bail!(
            "source does not exist or is not a directory: {}",
            source.display()
        );
    }
    let requested = normalize_path(new)?;
    let destination = if requested.is_dir() {
        requested.join(
            source
                .file_name()
                .ok_or_else(|| anyhow!("source has no name"))?,
        )
    } else {
        requested
    };
    if destination.exists() || fs::symlink_metadata(&destination).is_ok() {
        bail!("destination already exists: {}", destination.display());
    }
    if destination.starts_with(&source) {
        bail!("destination cannot be inside the source directory");
    }
    Ok((source, destination))
}

pub fn move_workspace(old: impl AsRef<Path>, new: impl AsRef<Path>) -> Result<PathBuf> {
    let (source, destination) = validate_workspace_move(old, new)?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::rename(&source, &destination) {
        Ok(()) => Ok(destination),
        Err(error) if error.kind() == std::io::ErrorKind::CrossesDevices => {
            let status = Command::new("mv")
                .arg("--")
                .arg(&source)
                .arg(&destination)
                .status()?;
            if !status.success() {
                bail!("mv failed with status {status}");
            }
            Ok(destination)
        }
        Err(error) => Err(error.into()),
    }
}

pub fn rollback_workspace_move(old: &Path, new: &Path) -> Result<()> {
    if !old.exists() && new.exists() {
        if let Some(parent) = old.parent() {
            fs::create_dir_all(parent)?;
        }
        let status = Command::new("mv").arg("--").arg(new).arg(old).status()?;
        if !status.success() {
            bail!("could not roll workspace back: mv exited with {status}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_fixture() -> Result<(tempfile::TempDir, Migrator, PathBuf)> {
        let temporary = tempfile::tempdir()?;
        let codex_home = temporary.path().join(".codex");
        let session_dir = codex_home.join("sessions/2026/01/02");
        fs::create_dir_all(&session_dir)?;
        fs::write(
            codex_home.join("config.toml"),
            "[projects.\"/example/old-repo\"]\ntrust_level = \"trusted\"\n",
        )?;
        fs::write(
            codex_home.join(".codex-global-state.json"),
            "{\n  \"roots\": [\"/example/old-repo\"],\n  \"command-history\": [[\"tool\", \"/example/old-repo\"]]\n}\n",
        )?;
        fs::write(
            codex_home.join("session_index.jsonl"),
            "{\"id\":\"one\",\"thread_name\":\"Fixture session\",\"updated_at\":\"2026-01-02T00:00:00Z\"}\n",
        )?;
        let session_path = session_dir.join("rollout.jsonl");
        let records = [
            json!({"type":"session_meta","payload":{"id":"one","cwd":"/example/old-repo","prompt":"/example/old-repo"}}),
            json!({"type":"turn_context","payload":{"cwd":"/example/old-repo","workspace_roots":["/example/old-repo"]}}),
            json!({"type":"response_item","payload":{"text":"/example/old-repo"}}),
            json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"text","text":"repair the sample database"}]}}}),
        ];
        let rollout = records
            .iter()
            .map(serde_json::to_string)
            .collect::<serde_json::Result<Vec<_>>>()?
            .join("\n")
            + "\n";
        fs::write(&session_path, rollout)?;

        let state = Connection::open(codex_home.join("state_5.sqlite"))?;
        state.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY, cwd TEXT NOT NULL, title TEXT NOT NULL,
                rollout_path TEXT NOT NULL, updated_at_ms INTEGER NOT NULL, archived INTEGER NOT NULL,
                sandbox_policy TEXT NOT NULL, name TEXT
             );",
        )?;
        state.execute(
            "INSERT INTO threads VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                "one",
                "/example/old-repo",
                "Fixture session",
                path_string(&session_path),
                1000_i64,
                0_i64,
                r#"{"type":"workspace-write","writable_roots":["/example/old-repo"]}"#,
                "Fixture session"
            ],
        )?;
        state.execute(
            "INSERT INTO threads VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                "two",
                "/example/old-repo",
                "Other work",
                "/tmp/nonexistent-rollout.jsonl",
                500_i64,
                0_i64,
                r#"{"type":"workspace-write","writable_roots":["/example/old-repo"]}"#,
                "Other work"
            ],
        )?;
        drop(state);

        fs::create_dir(codex_home.join("sqlite"))?;
        let catalog = Connection::open(codex_home.join("sqlite/codex-dev.db"))?;
        catalog.execute_batch(
            "CREATE TABLE local_thread_catalog (
                thread_id TEXT NOT NULL, cwd TEXT NOT NULL, display_title TEXT
             );
             CREATE TABLE automations (id TEXT NOT NULL, cwds TEXT NOT NULL);
             CREATE TABLE automation_runs (thread_id TEXT NOT NULL, source_cwd TEXT);",
        )?;
        catalog.execute(
            "INSERT INTO local_thread_catalog VALUES ('one', '/example/old-repo', 'Fixture session')",
            [],
        )?;
        catalog.execute(
            r#"INSERT INTO automations VALUES ('daily', '["/example/old-repo"]')"#,
            [],
        )?;
        catalog.execute(
            "INSERT INTO automation_runs VALUES ('one', '/example/old-repo')",
            [],
        )?;
        drop(catalog);

        let migrator = Migrator::new(&codex_home)?;
        Ok((temporary, migrator, session_path))
    }

    #[test]
    fn path_replacement_obeys_component_boundary() {
        let old = Path::new("/example/old-repo");
        let new = Path::new("/example/new-repo");
        assert_eq!(
            replace_path("/example/old-repo", old, new, false).unwrap(),
            "/example/new-repo"
        );
        assert_eq!(
            replace_path("/example/old-repo/child", old, new, false).unwrap(),
            "/example/new-repo/child"
        );
        assert!(replace_path("/example/old-repo-similar", old, new, false).is_none());
        assert!(replace_path("/example/old-repo/child", old, new, true).is_none());
    }

    #[test]
    fn existing_destination_uses_mv_semantics() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source");
        let container = temporary.path().join("existing");
        fs::create_dir(&source)?;
        fs::create_dir(&container)?;
        fs::write(source.join("file.txt"), "ok")?;
        let destination = move_workspace(&source, &container)?;
        assert_eq!(destination, container.join("source"));
        assert_eq!(fs::read_to_string(destination.join("file.txt"))?, "ok");
        Ok(())
    }

    #[test]
    fn lists_sessions_by_path_and_user_message() -> Result<()> {
        let (_temporary, migrator, _session_path) = create_fixture()?;
        let sessions = migrator.find_sessions("/example/old-repo", false, Some("database"))?;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "one");
        assert!(sessions[0].match_excerpt.contains("database"));
        let preview = migrator.session_preview("one", 3, Some("database"))?;
        assert_eq!(preview.user_messages, ["repair the sample database"]);
        Ok(())
    }

    #[test]
    fn lists_all_sessions_and_reports_environment() -> Result<()> {
        let (_temporary, migrator, _session_path) = create_fixture()?;
        let sessions = migrator.list_sessions(Some("old-repo"))?;
        assert_eq!(sessions.len(), 2);
        assert!(
            sessions
                .iter()
                .all(|session| session.match_excerpt.contains("工作目录"))
        );

        let report = migrator.doctor()?;
        assert!(report.healthy());
        assert_eq!(report.sessions, 2);
        assert_eq!(report.workspaces, 1);
        assert_eq!(report.databases, 2);
        assert_eq!(report.backups, 0);
        Ok(())
    }

    #[test]
    fn renames_session_in_index_and_database_caches() -> Result<()> {
        let (_temporary, migrator, _session_path) = create_fixture()?;
        let plan = migrator.plan_session_title("one", "  Database   migration  ")?;
        assert_eq!(plan.old_title, "Fixture session");
        assert_eq!(plan.new_title, "Database migration");
        assert_eq!(plan.files(), 3);
        assert_eq!(plan.replacements(), 4);
        let backup = migrator
            .apply_session_title(&plan)?
            .expect("backup should be created");
        assert!(backup.join("manifest.json").is_file());

        assert_eq!(migrator.resolve_session("one")?.title, "Database migration");
        let state = Connection::open(migrator.codex_home.join("state_5.sqlite"))?;
        let (title, name): (String, String) = state.query_row(
            "SELECT title, name FROM threads WHERE id='one'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(title, "Database migration");
        assert_eq!(name, "Database migration");
        let catalog = Connection::open(migrator.codex_home.join("sqlite/codex-dev.db"))?;
        let display_title: String = catalog.query_row(
            "SELECT display_title FROM local_thread_catalog WHERE thread_id='one'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(display_title, "Database migration");
        Ok(())
    }

    #[test]
    fn repoints_only_selected_session() -> Result<()> {
        let (_temporary, migrator, session_path) = create_fixture()?;
        let (_session, plan) = migrator.plan_session("one", "/example/new-repo")?;
        assert_eq!(plan.files(), 3);
        assert_eq!(plan.replacements(), 6);
        let backup = migrator.apply(&plan)?.expect("backup should be created");
        assert!(backup.join("manifest.json").is_file());

        let state = Connection::open(migrator.codex_home.join("state_5.sqlite"))?;
        let one: String = state.query_row("SELECT cwd FROM threads WHERE id='one'", [], |row| {
            row.get(0)
        })?;
        let two: String = state.query_row("SELECT cwd FROM threads WHERE id='two'", [], |row| {
            row.get(0)
        })?;
        assert_eq!(one, "/example/new-repo");
        assert_eq!(two, "/example/old-repo");
        let policy: String = state.query_row(
            "SELECT sandbox_policy FROM threads WHERE id='one'",
            [],
            |row| row.get(0),
        )?;
        assert!(policy.contains("/example/new-repo"));

        let config = fs::read_to_string(migrator.codex_home.join("config.toml"))?;
        assert!(config.contains("/example/old-repo"));
        let global = fs::read_to_string(migrator.codex_home.join(".codex-global-state.json"))?;
        assert!(global.contains("/example/old-repo"));

        let records: Vec<Value> = fs::read_to_string(session_path)?
            .lines()
            .map(serde_json::from_str)
            .collect::<serde_json::Result<_>>()?;
        assert_eq!(records[0]["payload"]["cwd"], "/example/new-repo");
        assert_eq!(records[1]["payload"]["cwd"], "/example/new-repo");
        assert_eq!(records[2]["payload"]["text"], "/example/old-repo");
        Ok(())
    }

    #[test]
    fn full_migration_leaves_no_old_path_references() -> Result<()> {
        let (_temporary, migrator, _session_path) = create_fixture()?;
        let plan = migrator.plan("/example/old-repo", "/example/new-repo")?;
        assert!(plan.replacements() > 0);
        migrator.apply(&plan)?;

        let verification = migrator.plan("/example/old-repo", "/example/new-repo")?;
        assert_eq!(verification.replacements(), 0);
        let state = Connection::open(migrator.codex_home.join("state_5.sqlite"))?;
        let stale_policies: i64 = state.query_row(
            "SELECT count(*) FROM threads WHERE sandbox_policy LIKE '%/example/old-repo%'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(stale_policies, 0);
        let automation = Connection::open(migrator.codex_home.join("sqlite/codex-dev.db"))?;
        let cwds: String =
            automation.query_row("SELECT cwds FROM automations", [], |row| row.get(0))?;
        let source_cwd: String =
            automation.query_row("SELECT source_cwd FROM automation_runs", [], |row| {
                row.get(0)
            })?;
        assert!(cwds.contains("/example/new-repo"));
        assert_eq!(source_cwd, "/example/new-repo");
        Ok(())
    }
}
