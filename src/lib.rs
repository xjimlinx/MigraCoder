use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, MAIN_DB, params, types::Value as SqlValue};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionAction {
    Archive,
    Unarchive,
    Delete,
}

impl SessionAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Archive => "归档",
            Self::Unarchive => "取消归档",
            Self::Delete => "删除",
        }
    }
}

#[derive(Debug)]
pub struct SessionActionPlan {
    pub session: SessionInfo,
    pub action: SessionAction,
    pub rollout_source: Option<PathBuf>,
    pub rollout_destination: Option<PathBuf>,
    index_change: Option<TextChange>,
    database_paths: Vec<PathBuf>,
}

impl SessionActionPlan {
    pub fn files(&self) -> usize {
        usize::from(self.rollout_source.is_some())
            + usize::from(self.index_change.is_some())
            + self.database_paths.len()
    }

    pub fn descriptions(&self) -> Vec<String> {
        let mut descriptions = Vec::new();
        if let Some(source) = &self.rollout_source {
            if let Some(destination) = &self.rollout_destination {
                descriptions.push(format!(
                    "move   {} -> {}",
                    source.display(),
                    destination.display()
                ));
            } else {
                descriptions.push(format!("delete {}", source.display()));
            }
        }
        if let Some(change) = &self.index_change {
            descriptions.push(format!("index  {}", change.path.display()));
        }
        descriptions.extend(
            self.database_paths
                .iter()
                .map(|path| format!("sqlite {}", path.display())),
        );
        descriptions
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageRole {
    User,
    Assistant,
}

impl MessageRole {
    pub fn label(self) -> &'static str {
        match self {
            Self::User => "用户",
            Self::Assistant => "AI",
        }
    }
}

#[derive(Debug)]
pub struct SessionMessage {
    pub role: MessageRole,
    pub content: String,
    pub truncated: bool,
    pub timestamp: Option<String>,
}

#[derive(Debug)]
pub struct SessionPreview {
    pub session: SessionInfo,
    pub messages: Vec<SessionMessage>,
    pub next_offset: Option<u64>,
    pub skipped_oversized_records: usize,
}

const MAX_JSONL_RECORD_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug)]
pub struct DoctorReport {
    pub codex_home: PathBuf,
    pub opencode_home: Option<PathBuf>,
    pub opencode_sessions: usize,
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
    pub opencode_home: Option<PathBuf>,
}

impl Migrator {
    pub fn new(codex_home: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            codex_home: normalize_path(codex_home)?,
            opencode_home: None,
        })
    }

    pub fn with_opencode(mut self, opencode_home: Option<PathBuf>) -> Result<Self> {
        self.opencode_home = match opencode_home {
            Some(path) => Some(normalize_path(path)?),
            None => None,
        };
        Ok(self)
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
        self.plan_opencode(&mut plan)?;
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

    pub fn list_opencode_sessions(&self, search: Option<&str>) -> Result<Vec<SessionInfo>> {
        self.collect_opencode_sessions(None, false, search)
    }

    pub fn find_opencode_sessions(
        &self,
        path: impl AsRef<Path>,
        recursive: bool,
        search: Option<&str>,
    ) -> Result<Vec<SessionInfo>> {
        let target = normalize_path(path)?;
        self.collect_opencode_sessions(Some(target), recursive, search)
    }

    fn collect_opencode_sessions(
        &self,
        target: Option<PathBuf>,
        recursive: bool,
        search: Option<&str>,
    ) -> Result<Vec<SessionInfo>> {
        let Some(path) = self.opencode_database() else {
            return Ok(Vec::new());
        };
        let connection = open_read_only(&path)?;
        if !table_names(&connection)?.contains("session") {
            return Ok(Vec::new());
        }
        let needle = search.map(str::to_lowercase);
        let mut statement = connection.prepare(
            "SELECT id, directory, title, slug, time_updated, time_archived FROM session",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<i64>>(5)?,
            ))
        })?;
        let mut matches = Vec::new();
        for row in rows {
            let (id, directory, title, slug, updated, archived) = row?;
            if let Some(target) = &target {
                let cwd = Path::new(&directory);
                let path_matches =
                    cwd == target || (recursive && cwd.starts_with(target) && cwd != target);
                if !path_matches {
                    continue;
                }
            }
            let mut title = title.unwrap_or_default();
            if title.is_empty() {
                title = slug.clone().unwrap_or_else(|| id.clone());
            }
            let mut match_excerpt = String::new();
            if let Some(needle) = &needle {
                let Some(excerpt) =
                    opencode_session_excerpt(&id, &title, &directory, slug.as_deref(), needle)
                else {
                    continue;
                };
                match_excerpt = excerpt;
            }
            matches.push(SessionInfo {
                session_id: id,
                cwd: PathBuf::from(directory),
                title,
                rollout_path: None,
                updated_at_ms: updated.unwrap_or(0),
                archived: archived.is_some(),
                match_excerpt,
            });
        }
        matches.sort_by_key(|session| std::cmp::Reverse(session.updated_at_ms));
        Ok(matches)
    }

    pub fn resolve_opencode_session(&self, reference: &str) -> Result<SessionInfo> {
        let sessions = self.list_opencode_sessions(None)?;
        if let Some(session) = sessions
            .iter()
            .find(|session| session.session_id == reference)
        {
            return Ok(session.clone());
        }
        let matches: Vec<_> = sessions
            .into_iter()
            .filter(|session| session.session_id.starts_with(reference))
            .collect();
        match matches.as_slice() {
            [] => bail!("opencode session not found: {reference}"),
            [session] => Ok(session.clone()),
            many => {
                let ids = many
                    .iter()
                    .take(5)
                    .map(|session| session.session_id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!("opencode session prefix is ambiguous: {reference} ({ids})")
            }
        }
    }

    pub fn opencode_session_preview(
        &self,
        reference: &str,
        limit: usize,
        search: Option<&str>,
    ) -> Result<SessionPreview> {
        let session = self.resolve_opencode_session(reference)?;
        let Some(path) = self.opencode_database() else {
            return Ok(SessionPreview {
                session,
                messages: Vec::new(),
                next_offset: None,
                skipped_oversized_records: 0,
            });
        };
        let connection = open_read_only(&path)?;
        let mut message_statement = connection
            .prepare("SELECT id FROM message WHERE session_id = ?1 ORDER BY time_created, id")?;
        let message_rows =
            message_statement.query_map([&session.session_id], |row| row.get::<_, String>(0))?;
        let mut messages = Vec::new();
        let mut part_statement = connection
            .prepare("SELECT data FROM part WHERE message_id = ?1 ORDER BY time_created, id")?;
        for message_id in message_rows {
            let message_id = message_id?;
            let is_user: bool = match connection.query_row(
                "SELECT data FROM message WHERE id = ?1",
                [&message_id],
                |row| row.get::<_, String>(0),
            ) {
                Ok(data) => {
                    serde_json::from_str::<Value>(&data)
                        .ok()
                        .and_then(|value| {
                            value.get("role").and_then(Value::as_str).map(str::to_owned)
                        })
                        .as_deref()
                        == Some("user")
                }
                Err(_) => false,
            };
            if !is_user {
                continue;
            }
            let mut text = String::new();
            let parts = part_statement.query_map([&message_id], |row| row.get::<_, String>(0))?;
            for part in parts {
                let Ok(value) = serde_json::from_str::<Value>(&part?) else {
                    continue;
                };
                if value.get("type").and_then(Value::as_str) == Some("text")
                    && let Some(piece) = value.get("text").and_then(Value::as_str)
                {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(piece);
                }
            }
            let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
            if compact.is_empty() {
                continue;
            }
            if let Some(search) = search
                && !compact.to_lowercase().contains(&search.to_lowercase())
            {
                continue;
            }
            let (content, truncated) = truncate_message_text(&compact, 600);
            messages.push(SessionMessage {
                role: MessageRole::User,
                content,
                truncated,
                timestamp: None,
            });
            if messages.len() >= limit {
                break;
            }
        }
        Ok(SessionPreview {
            session,
            messages,
            next_offset: None,
            skipped_oversized_records: 0,
        })
    }

    pub fn plan_opencode_session(
        &self,
        reference: &str,
        new: impl AsRef<Path>,
    ) -> Result<(SessionInfo, Plan)> {
        let new = normalize_path(new)?;
        let session = self.resolve_opencode_session(reference)?;
        if session.cwd == new {
            bail!("source and destination resolve to the same path");
        }
        let mut plan = Plan::new(session.cwd.clone(), new);
        self.plan_opencode_session_rows(&mut plan, &session.session_id)?;
        Ok((session, plan))
    }

    fn plan_opencode_session_rows(&self, plan: &mut Plan, session_id: &str) -> Result<()> {
        let Some(path) = self.opencode_database() else {
            return Ok(());
        };
        let connection = open_read_only(&path)?;
        let tables = table_names(&connection)?;
        let mut updates = Vec::new();
        if tables.contains("session") {
            for (column, relative) in [("directory", false), ("path", true)] {
                if !has_column(&connection, "session", column)? {
                    continue;
                }
                let current: Option<String> = connection.query_row(
                    &format!("SELECT \"{column}\" FROM session WHERE id = ?1"),
                    [session_id],
                    |row| row.get(0),
                )?;
                let Some(current) = current else {
                    continue;
                };
                let replaced = if relative {
                    replace_relative_path(&current, &plan.old, &plan.new, true)
                } else {
                    replace_path(&current, &plan.old, &plan.new, true)
                };
                if let Some(value) = replaced {
                    updates.push(RowUpdate {
                        table: "session",
                        column,
                        identity_column: "id",
                        identity: SqlValue::Text(session_id.to_owned()),
                        value,
                    });
                }
            }
        }
        if tables.contains("event") && has_column(&connection, "event", "data")? {
            let mut statement = connection.prepare(
                "SELECT id, data FROM event \
                 WHERE aggregate_id = ?1 AND type LIKE 'session.%'",
            )?;
            let rows = statement.query_map([session_id], |row| {
                Ok((row.get::<_, SqlValue>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (id, current) = row?;
                if let Some(value) = replace_opencode_event_paths(&current, &plan.old, &plan.new) {
                    updates.push(RowUpdate {
                        table: "event",
                        column: "data",
                        identity_column: "id",
                        identity: id,
                        value,
                    });
                }
            }
        }
        if !updates.is_empty() {
            plan.database_changes.push(DatabaseChange { path, updates });
        }
        Ok(())
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
                opencode_home: self.opencode_home.clone(),
                opencode_sessions: 0,
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
        let mut opencode_sessions = 0;
        if let Some(opencode_home) = &self.opencode_home {
            match opencode_session_count(opencode_home) {
                Ok(count) => opencode_sessions = count,
                Err(error) => warnings.push(format!("opencode 数据无法读取：{error:#}")),
            }
        }
        Ok(DoctorReport {
            codex_home: self.codex_home.clone(),
            opencode_home: self.opencode_home.clone(),
            opencode_sessions,
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
        offset: u64,
        max_messages: usize,
        max_chars: usize,
    ) -> Result<SessionPreview> {
        if !(1..=100).contains(&max_messages) {
            bail!("page size must be between 1 and 100 messages");
        }
        if !(200..=50_000).contains(&max_chars) {
            bail!("message size must be between 200 and 50000 characters");
        }
        let session = self.resolve_session(reference)?;
        let mut messages = Vec::new();
        let mut next_offset = None;
        let mut skipped_oversized_records = 0;
        if let Some(path) = session
            .rollout_path
            .as_deref()
            .filter(|path| path.is_file())
        {
            let file = File::open(path)?;
            let file_len = file.metadata()?.len();
            if offset > file_len {
                bail!("message cursor is past the end of the session file");
            }
            let mut reader = BufReader::new(file);
            reader.seek(SeekFrom::Start(offset))?;
            let mut position = offset;
            let mut line = Vec::new();
            while messages.len() < max_messages {
                let (bytes, oversized) =
                    read_bounded_line(&mut reader, &mut line, MAX_JSONL_RECORD_BYTES)?;
                if bytes == 0 {
                    break;
                }
                position += bytes as u64;
                if oversized {
                    skipped_oversized_records += 1;
                    continue;
                }
                let Ok(value) = serde_json::from_slice::<Value>(&line) else {
                    continue;
                };
                let Some((role, text)) = conversation_message_from_record(&value) else {
                    continue;
                };
                if role == MessageRole::User
                    && text.trim_start().starts_with("<environment_context>")
                {
                    continue;
                }
                let (content, truncated) = truncate_message_text(&text, max_chars);
                if content.trim().is_empty() {
                    continue;
                }
                messages.push(SessionMessage {
                    role,
                    content,
                    truncated,
                    timestamp: value
                        .get("timestamp")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                });
            }
            if position < file_len {
                next_offset = Some(position);
            }
        }
        Ok(SessionPreview {
            session,
            messages,
            next_offset,
            skipped_oversized_records,
        })
    }

    pub fn plan_session_action(
        &self,
        reference: &str,
        action: SessionAction,
    ) -> Result<SessionActionPlan> {
        let session = self.resolve_session(reference)?;
        match action {
            SessionAction::Archive if session.archived => bail!("session is already archived"),
            SessionAction::Unarchive if !session.archived => bail!("session is not archived"),
            _ => {}
        }

        let rollout_source = session.rollout_path.clone().filter(|path| path.is_file());
        let rollout_destination = match (action, rollout_source.as_deref()) {
            (SessionAction::Archive, Some(source)) => Some(
                self.codex_home.join("archived_sessions").join(
                    source
                        .file_name()
                        .ok_or_else(|| anyhow!("invalid rollout path"))?,
                ),
            ),
            (SessionAction::Unarchive, Some(source)) => {
                Some(self.active_rollout_path(source, session.updated_at_ms)?)
            }
            _ => None,
        };
        if let (Some(source), Some(destination)) =
            (rollout_source.as_deref(), rollout_destination.as_deref())
            && source != destination
            && destination.exists()
        {
            bail!(
                "rollout destination already exists: {}",
                destination.display()
            );
        }

        let index_change = if action == SessionAction::Delete {
            self.plan_session_index_delete(&session.session_id)?
        } else {
            None
        };
        let database_paths = self.session_action_databases(&session.session_id, action)?;
        if rollout_source.is_none() && index_change.is_none() && database_paths.is_empty() {
            bail!("no writable Codex data was found for this session");
        }
        Ok(SessionActionPlan {
            session,
            action,
            rollout_source,
            rollout_destination,
            index_change,
            database_paths,
        })
    }

    pub fn apply_session_action(&self, plan: &SessionActionPlan) -> Result<PathBuf> {
        let backup_dir = self.create_session_action_backup(plan)?;
        let result = (|| -> Result<()> {
            if let (Some(source), Some(destination)) =
                (&plan.rollout_source, &plan.rollout_destination)
                && source != destination
            {
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::rename(source, destination).with_context(|| {
                    format!(
                        "failed to move rollout {} -> {}",
                        source.display(),
                        destination.display()
                    )
                })?;
            }

            for path in &plan.database_paths {
                match plan.action {
                    SessionAction::Archive | SessionAction::Unarchive => {
                        update_session_archive_database(
                            path,
                            &plan.session.session_id,
                            plan.action == SessionAction::Archive,
                            plan.rollout_destination.as_deref(),
                        )?;
                    }
                    SessionAction::Delete => {
                        delete_session_database_rows(path, &plan.session.session_id)?;
                    }
                }
            }
            if let Some(change) = &plan.index_change {
                atomic_write(&change.path, change.content.as_bytes())?;
            }
            if plan.action == SessionAction::Delete
                && let Some(source) = &plan.rollout_source
                && source.exists()
            {
                fs::remove_file(source)?;
            }
            Ok(())
        })();

        if let Err(original) = result {
            if let Some(destination) = &plan.rollout_destination
                && destination.exists()
            {
                let _ = fs::remove_file(destination);
            }
            if let Err(restore) = self.restore_backup(&backup_dir) {
                return Err(anyhow!(
                    "{} failed: {original:#}; backup restore also failed: {restore:#}",
                    plan.action.label()
                ));
            }
            return Err(original);
        }
        Ok(backup_dir)
    }

    pub fn apply_session_actions(&self, plans: &[SessionActionPlan]) -> Result<Vec<PathBuf>> {
        let mut completed: Vec<(SessionActionPlan, PathBuf)> = Vec::new();
        for plan in plans {
            let result = self
                .plan_session_action(&plan.session.session_id, plan.action)
                .and_then(|effective| {
                    self.apply_session_action(&effective)
                        .map(|backup| (effective, backup))
                });
            match result {
                Ok(completed_action) => completed.push(completed_action),
                Err(original) => {
                    let mut restore_errors = Vec::new();
                    for (completed_plan, backup) in completed.iter().rev() {
                        if let Some(destination) = &completed_plan.rollout_destination
                            && destination.exists()
                            && let Err(error) = fs::remove_file(destination)
                        {
                            restore_errors
                                .push(format!("cannot remove {}: {error}", destination.display()));
                        }
                        if let Err(error) = self.restore_backup(backup) {
                            restore_errors.push(format!("{}: {error:#}", backup.display()));
                        }
                    }
                    if restore_errors.is_empty() {
                        return Err(original);
                    }
                    return Err(anyhow!(
                        "batch session operation failed: {original:#}; rollback also failed: {}",
                        restore_errors.join("; ")
                    ));
                }
            }
        }
        Ok(completed.into_iter().map(|(_, backup)| backup).collect())
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

    pub fn apply_session_repoints(
        &self,
        references: &[String],
        new: impl AsRef<Path>,
    ) -> Result<Vec<PathBuf>> {
        let new = normalize_path(new)?;
        let mut backups = Vec::new();
        for reference in references {
            let result = self
                .plan_session(reference, &new)
                .and_then(|(_, plan)| self.apply(&plan));
            match result {
                Ok(Some(backup)) => backups.push(backup),
                Ok(None) => {}
                Err(original) => {
                    let mut restore_errors = Vec::new();
                    for backup in backups.iter().rev() {
                        if let Err(error) = self.restore_backup(backup) {
                            restore_errors.push(format!("{}: {error:#}", backup.display()));
                        }
                    }
                    if restore_errors.is_empty() {
                        return Err(original);
                    }
                    return Err(anyhow!(
                        "batch migration failed: {original:#}; rollback also failed: {}",
                        restore_errors.join("; ")
                    ));
                }
            }
        }
        Ok(backups)
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

    fn active_rollout_path(&self, source: &Path, updated_at_ms: i64) -> Result<PathBuf> {
        let file_name = source
            .file_name()
            .ok_or_else(|| anyhow!("invalid rollout path"))?;
        let name = file_name.to_string_lossy();
        let date = name
            .strip_prefix("rollout-")
            .and_then(|rest| rest.get(..10))
            .filter(|date| {
                date.as_bytes().get(4) == Some(&b'-')
                    && date.as_bytes().get(7) == Some(&b'-')
                    && date
                        .chars()
                        .enumerate()
                        .all(|(index, ch)| index == 4 || index == 7 || ch.is_ascii_digit())
            })
            .map(str::to_owned)
            .or_else(|| {
                DateTime::<Utc>::from_timestamp_millis(updated_at_ms)
                    .map(|time| time.format("%Y-%m-%d").to_string())
            })
            .ok_or_else(|| anyhow!("cannot determine rollout date"))?;
        Ok(self
            .codex_home
            .join("sessions")
            .join(&date[0..4])
            .join(&date[5..7])
            .join(&date[8..10])
            .join(file_name))
    }

    fn plan_session_index_delete(&self, session_id: &str) -> Result<Option<TextChange>> {
        let path = self.codex_home.join("session_index.jsonl");
        if !path.is_file() {
            return Ok(None);
        }
        let original = fs::read_to_string(&path)?;
        let trailing_newline = original.ends_with('\n');
        let mut removed = 0;
        let lines = original
            .lines()
            .filter(|line| {
                let matches = serde_json::from_str::<Value>(line)
                    .ok()
                    .and_then(|value| value.get("id").and_then(Value::as_str).map(str::to_owned))
                    .as_deref()
                    == Some(session_id);
                removed += usize::from(matches);
                !matches
            })
            .collect::<Vec<_>>();
        if removed == 0 {
            return Ok(None);
        }
        let mut content = lines.join("\n");
        if trailing_newline && !content.is_empty() {
            content.push('\n');
        }
        Ok(Some(TextChange {
            path,
            content,
            replacements: removed,
        }))
    }

    fn session_action_databases(
        &self,
        session_id: &str,
        action: SessionAction,
    ) -> Result<Vec<PathBuf>> {
        let mut matches = Vec::new();
        for path in self.all_databases()? {
            let connection = open_read_only(&path)?;
            let tables = table_names(&connection)?;
            let affected = if action == SessionAction::Delete {
                database_contains_session(&connection, &tables, session_id)?
            } else if tables.contains("threads")
                && has_column(&connection, "threads", "id")?
                && has_column(&connection, "threads", "archived")?
            {
                connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM threads WHERE id=?1)",
                    [session_id],
                    |row| row.get::<_, bool>(0),
                )?
            } else {
                false
            };
            if affected {
                matches.push(path);
            }
        }
        Ok(matches)
    }

    fn create_session_action_backup(&self, plan: &SessionActionPlan) -> Result<PathBuf> {
        let mut text_changes = Vec::new();
        if let Some(path) = &plan.rollout_source {
            text_changes.push(TextChange {
                path: path.clone(),
                content: String::new(),
                replacements: 1,
            });
        }
        if let Some(change) = &plan.index_change {
            text_changes.push(TextChange {
                path: change.path.clone(),
                content: String::new(),
                replacements: change.replacements,
            });
        }
        let database_changes = plan
            .database_paths
            .iter()
            .map(|path| DatabaseChange {
                path: path.clone(),
                updates: Vec::new(),
            })
            .collect::<Vec<_>>();
        self.create_backup_for_changes(
            &text_changes,
            &database_changes,
            json!({
                "operation": match plan.action {
                    SessionAction::Archive => "archive-session",
                    SessionAction::Unarchive => "unarchive-session",
                    SessionAction::Delete => "delete-session",
                },
                "session_id": plan.session.session_id,
                "title": plan.session.title,
            }),
        )
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

    fn opencode_database(&self) -> Option<PathBuf> {
        let home = self.opencode_home.as_ref()?;
        let path = home.join("opencode.db");
        path.is_file().then_some(path)
    }

    fn plan_opencode(&self, plan: &mut Plan) -> Result<()> {
        let Some(path) = self.opencode_database() else {
            return Ok(());
        };
        let connection = open_read_only(&path)?;
        let tables = table_names(&connection)?;
        let mut updates = Vec::new();

        for (table, identity, column) in [
            ("project", "id", "worktree"),
            ("project_directory", "rowid", "directory"),
            ("workspace", "id", "directory"),
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

        if tables.contains("project") && has_column(&connection, "project", "sandboxes")? {
            let mut statement = connection.prepare(
                "SELECT id, sandboxes FROM project WHERE sandboxes IS NOT NULL AND sandboxes <> ''",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, SqlValue>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (id, current) = row?;
                if let Some(value) = replace_json_paths(&current, &plan.old, &plan.new, false) {
                    updates.push(RowUpdate {
                        table: "project",
                        column: "sandboxes",
                        identity_column: "id",
                        identity: id,
                        value,
                    });
                }
            }
        }

        let mut changed_sessions = Vec::new();
        if tables.contains("session") {
            let has_directory = has_column(&connection, "session", "directory")?;
            let has_path = has_column(&connection, "session", "path")?;
            if has_directory || has_path {
                let directory_column = if has_directory {
                    "\"directory\""
                } else {
                    "NULL"
                };
                let path_column = if has_path { "\"path\"" } else { "NULL" };
                let mut statement = connection.prepare(&format!(
                    "SELECT id, {directory_column}, {path_column} FROM session"
                ))?;
                let rows = statement.query_map([], |row| {
                    Ok((
                        row.get::<_, SqlValue>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                })?;
                for row in rows {
                    let (id, directory, path) = row?;
                    let mut changed = false;
                    if let Some(current) = directory
                        && let Some(value) = replace_path(&current, &plan.old, &plan.new, false)
                    {
                        updates.push(RowUpdate {
                            table: "session",
                            column: "directory",
                            identity_column: "id",
                            identity: id.clone(),
                            value,
                        });
                        changed = true;
                    }
                    if let Some(current) = path
                        && let Some(value) =
                            replace_relative_path(&current, &plan.old, &plan.new, false)
                    {
                        updates.push(RowUpdate {
                            table: "session",
                            column: "path",
                            identity_column: "id",
                            identity: id.clone(),
                            value,
                        });
                        changed = true;
                    }
                    if changed && let SqlValue::Text(session_id) = &id {
                        changed_sessions.push(session_id.clone());
                    }
                }
            }
        }

        if tables.contains("event")
            && has_column(&connection, "event", "data")?
            && has_column(&connection, "event", "aggregate_id")?
        {
            let mut statement = connection.prepare(
                "SELECT id, data FROM event \
                 WHERE aggregate_id = ?1 AND type LIKE 'session.%'",
            )?;
            for session_id in &changed_sessions {
                let rows = statement.query_map([session_id], |row| {
                    Ok((row.get::<_, SqlValue>(0)?, row.get::<_, String>(1)?))
                })?;
                for row in rows {
                    let (id, current) = row?;
                    if let Some(value) =
                        replace_opencode_event_paths(&current, &plan.old, &plan.new)
                    {
                        updates.push(RowUpdate {
                            table: "event",
                            column: "data",
                            identity_column: "id",
                            identity: id,
                            value,
                        });
                    }
                }
            }
        }

        if !updates.is_empty() {
            plan.database_changes.push(DatabaseChange { path, updates });
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
                    if session
                        .rollout_path
                        .as_ref()
                        .is_none_or(|rollout| !rollout.is_file())
                    {
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
            let (role, text) = conversation_message_from_record(&value)
                .map(|(role, text)| (Some(role), text))
                .unwrap_or_else(|| (None, user_text_from_record(&value)));
            if text.to_lowercase().contains(&needle) {
                let excerpt = compact_excerpt(&text, 180);
                return Ok(Some(match role {
                    Some(role) => format!("{}：{excerpt}", role.label()),
                    None => excerpt,
                }));
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

    fn backup_relative(&self, path: &Path) -> Result<PathBuf> {
        if let Ok(relative) = path.strip_prefix(&self.codex_home) {
            return Ok(PathBuf::from("codex").join(relative));
        }
        if let Some(opencode_home) = &self.opencode_home
            && let Ok(relative) = path.strip_prefix(opencode_home)
        {
            return Ok(PathBuf::from("opencode").join(relative));
        }
        bail!(
            "cannot back up {}: not under a known data directory",
            path.display()
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
            let relative = self.backup_relative(&change.path)?;
            let destination = backup_dir.join(&relative);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&change.path, &destination)?;
            files.push(json!({"path": relative, "source": change.path, "kind": "file"}));
        }
        for change in database_changes {
            let relative = self.backup_relative(&change.path)?;
            let destination = backup_dir.join(&relative);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            backup_database(&change.path, &destination)?;
            files.push(json!({"path": relative, "source": change.path, "kind": "sqlite"}));
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
            #[serde(default)]
            source: Option<PathBuf>,
            kind: String,
        }
        let manifest: Manifest =
            serde_json::from_slice(&fs::read(backup_dir.join("manifest.json"))?)?;
        for item in manifest.files {
            let source = backup_dir.join(&item.path);
            let destination = item
                .source
                .clone()
                .unwrap_or_else(|| self.codex_home.join(&item.path));
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
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

fn replace_relative_path(value: &str, old: &Path, new: &Path, exact: bool) -> Option<String> {
    let old = path_string(old);
    let new = path_string(new);
    let old = old.trim_start_matches('/');
    let new = new.trim_start_matches('/');
    if !old.is_empty() && value == old {
        return Some(new.to_owned());
    }
    if !exact
        && !old.is_empty()
        && let Some(suffix) = value.strip_prefix(&format!("{old}/"))
    {
        return Some(format!("{new}/{suffix}"));
    }
    None
}

fn replace_opencode_event_paths(text: &str, old: &Path, new: &Path) -> Option<String> {
    let mut value: Value = serde_json::from_str(text).ok()?;
    let mut changes = 0;
    if let Some(info) = value.get_mut("info").and_then(Value::as_object_mut) {
        if let Some(current) = info
            .get("directory")
            .and_then(Value::as_str)
            .map(str::to_owned)
            && let Some(replaced) = replace_path(&current, old, new, false)
        {
            info.insert("directory".to_owned(), Value::String(replaced));
            changes += 1;
        }
        if let Some(current) = info.get("path").and_then(Value::as_str).map(str::to_owned)
            && let Some(replaced) = replace_relative_path(&current, old, new, false)
        {
            info.insert("path".to_owned(), Value::String(replaced));
            changes += 1;
        }
    }
    (changes > 0)
        .then(|| serde_json::to_string(&value).expect("serializing JSON value cannot fail"))
}

fn opencode_session_count(home: &Path) -> Result<usize> {
    let path = home.join("opencode.db");
    if !path.is_file() {
        bail!("找不到 opencode 数据库：{}", path.display());
    }
    let connection = open_read_only(&path)?;
    if !table_names(&connection)?.contains("session") {
        return Ok(0);
    }
    let count: i64 = connection.query_row("SELECT COUNT(*) FROM session", [], |row| row.get(0))?;
    Ok(count.max(0) as usize)
}

fn opencode_session_excerpt(
    id: &str,
    title: &str,
    directory: &str,
    slug: Option<&str>,
    needle: &str,
) -> Option<String> {
    if title.to_lowercase().contains(needle) {
        return Some(title.to_owned());
    }
    if id.to_lowercase().contains(needle) {
        return Some(format!("会话 ID：{id}"));
    }
    if directory.to_lowercase().contains(needle) {
        return Some(format!("工作目录：{directory}"));
    }
    if let Some(slug) = slug
        && slug.to_lowercase().contains(needle)
    {
        return Some(format!("slug：{slug}"));
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

fn conversation_message_from_record(record: &Value) -> Option<(MessageRole, String)> {
    if record.get("type").and_then(Value::as_str) != Some("event_msg") {
        return None;
    }
    let payload = record.get("payload")?.as_object()?;
    let (role, content) = match payload.get("type").and_then(Value::as_str) {
        Some("item_completed") => {
            let item = payload.get("item")?.as_object()?;
            let role = match item.get("type").and_then(Value::as_str) {
                Some("UserMessage") => MessageRole::User,
                Some("AgentMessage") => MessageRole::Assistant,
                _ => return None,
            };
            (role, item.get("content")?)
        }
        Some("user_message") => (MessageRole::User, payload.get("message")?),
        Some("agent_message") => (MessageRole::Assistant, payload.get("message")?),
        _ => return None,
    };
    let text = text_from_message_content(content);
    (!text.trim().is_empty()).then_some((role, text))
}

fn read_bounded_line(
    reader: &mut impl BufRead,
    output: &mut Vec<u8>,
    max_bytes: usize,
) -> std::io::Result<(usize, bool)> {
    output.clear();
    let mut total = 0;
    let mut oversized = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok((total, oversized));
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |index| index + 1);
        if !oversized && output.len() + consumed <= max_bytes {
            output.extend_from_slice(&available[..consumed]);
        } else {
            oversized = true;
            output.clear();
        }
        reader.consume(consumed);
        total += consumed;
        if newline.is_some() {
            return Ok((total, oversized));
        }
    }
}

fn text_from_message_content(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| {
                item.get("text")
                    .or_else(|| item.get("content"))
                    .and_then(Value::as_str)
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn truncate_message_text(text: &str, max_chars: usize) -> (String, bool) {
    let mut characters = text.trim().chars();
    let shortened = characters.by_ref().take(max_chars).collect::<String>();
    if characters.next().is_some() {
        (format!("{shortened}\n\n…（该消息过长，已截断）"), true)
    } else {
        (shortened, false)
    }
}

fn compact_excerpt(text: &str, max_chars: usize) -> String {
    let mut result = String::new();
    let mut previous_was_space = true;
    let mut count = 0;
    let mut truncated = false;
    for character in text.chars() {
        if character.is_whitespace() {
            if !previous_was_space && count < max_chars {
                result.push(' ');
                count += 1;
            }
            previous_was_space = true;
        } else if count < max_chars {
            result.push(character);
            count += 1;
            previous_was_space = false;
        } else {
            truncated = true;
            break;
        }
    }
    if truncated {
        result.push('…');
    }
    result.trim_end().to_owned()
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
    let query = format!("PRAGMA table_info({})", quoted_identifier(table));
    let mut statement = connection.prepare(&query)?;
    let rows = statement.query_map([], |row| row.get(1))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn has_column(connection: &Connection, table: &str, column: &str) -> Result<bool> {
    Ok(columns(connection, table)?.contains(column))
}

fn session_identity_columns(table: &str, columns: &HashSet<String>) -> Vec<&'static str> {
    let mut result = Vec::new();
    if table == "threads" && columns.contains("id") {
        result.push("id");
    }
    for column in ["thread_id", "parent_thread_id", "child_thread_id"] {
        if columns.contains(column) {
            result.push(column);
        }
    }
    result
}

fn quoted_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn database_contains_session(
    connection: &Connection,
    tables: &HashSet<String>,
    session_id: &str,
) -> Result<bool> {
    for table in tables {
        let columns = columns(connection, table)?;
        let identities = session_identity_columns(table, &columns);
        if identities.is_empty() {
            continue;
        }
        let predicates = identities
            .iter()
            .map(|column| format!("{} = ?", quoted_identifier(column)))
            .collect::<Vec<_>>()
            .join(" OR ");
        let query = format!(
            "SELECT EXISTS(SELECT 1 FROM {} WHERE {predicates})",
            quoted_identifier(table)
        );
        let parameters = std::iter::repeat_n(session_id, identities.len());
        if connection.query_row(&query, rusqlite::params_from_iter(parameters), |row| {
            row.get::<_, bool>(0)
        })? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn update_session_archive_database(
    path: &Path,
    session_id: &str,
    archived: bool,
    rollout_path: Option<&Path>,
) -> Result<()> {
    let mut connection = Connection::open(path)?;
    let columns = columns(&connection, "threads")?;
    if !columns.contains("id") || !columns.contains("archived") {
        return Ok(());
    }
    let mut assignments = vec!["archived = ?".to_owned()];
    let mut values = vec![SqlValue::Integer(i64::from(archived))];
    if columns.contains("archived_at") {
        assignments.push("archived_at = ?".to_owned());
        values.push(if archived {
            SqlValue::Integer(Utc::now().timestamp())
        } else {
            SqlValue::Null
        });
    }
    if columns.contains("rollout_path")
        && let Some(rollout_path) = rollout_path
    {
        assignments.push("rollout_path = ?".to_owned());
        values.push(SqlValue::Text(path_string(rollout_path)));
    }
    values.push(SqlValue::Text(session_id.to_owned()));
    let query = format!("UPDATE threads SET {} WHERE id = ?", assignments.join(", "));
    let transaction = connection.transaction()?;
    transaction.execute(&query, rusqlite::params_from_iter(values))?;
    transaction.commit()?;
    Ok(())
}

fn delete_session_database_rows(path: &Path, session_id: &str) -> Result<()> {
    let mut connection = Connection::open(path)?;
    let tables = table_names(&connection)?;
    let mut ordered = tables.into_iter().collect::<Vec<_>>();
    ordered.sort_by_key(|table| table == "threads");
    let transaction = connection.transaction()?;
    for table in ordered {
        let columns = columns(&transaction, &table)?;
        let identities = session_identity_columns(&table, &columns);
        if identities.is_empty() {
            continue;
        }
        let predicates = identities
            .iter()
            .map(|column| format!("{} = ?", quoted_identifier(column)))
            .collect::<Vec<_>>()
            .join(" OR ");
        let query = format!(
            "DELETE FROM {} WHERE {predicates}",
            quoted_identifier(&table)
        );
        let parameters = std::iter::repeat_n(session_id, identities.len());
        transaction.execute(&query, rusqlite::params_from_iter(parameters))?;
    }
    transaction.commit()?;
    Ok(())
}

fn apply_database(change: &DatabaseChange) -> Result<()> {
    let mut connection = Connection::open(&change.path)?;
    connection.busy_timeout(std::time::Duration::from_secs(10))?;
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

    pub(super) fn create_fixture() -> Result<(tempfile::TempDir, Migrator, PathBuf)> {
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
            "{\"id\":\"one\",\"thread_name\":\"Fixture session\",\"updated_at\":\"2026-01-02T00:00:00Z\"}\n\
             {\"id\":\"two\",\"thread_name\":\"Other work\",\"updated_at\":\"2026-01-02T00:00:00Z\"}\n",
        )?;
        let session_path = session_dir.join("rollout.jsonl");
        let records = [
            json!({"type":"session_meta","payload":{"id":"one","cwd":"/example/old-repo","prompt":"/example/old-repo"}}),
            json!({"type":"turn_context","payload":{"cwd":"/example/old-repo","workspace_roots":["/example/old-repo"]}}),
            json!({"type":"response_item","payload":{"text":"/example/old-repo"}}),
            json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"text","text":"repair the sample database"}]}}}),
            json!({"timestamp":"2026-01-02T00:00:01Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","content":[{"type":"text","text":"I can help repair that database."}]}}}),
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
                archived_at INTEGER, sandbox_policy TEXT NOT NULL, name TEXT
             );
             CREATE TABLE thread_artifacts (
                id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, payload TEXT NOT NULL
             );",
        )?;
        state.execute(
            "INSERT INTO threads
             (id, cwd, title, rollout_path, updated_at_ms, archived, sandbox_policy, name)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
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
            "INSERT INTO threads
             (id, cwd, title, rollout_path, updated_at_ms, archived, sandbox_policy, name)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
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
        state.execute(
            "INSERT INTO thread_artifacts VALUES ('artifact-one', 'one', 'saved')",
            [],
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
    fn bounded_line_reader_discards_oversized_records() -> Result<()> {
        let data = format!("{}\n{{\"ok\":true}}\n", "x".repeat(128));
        let mut reader = BufReader::new(std::io::Cursor::new(data.into_bytes()));
        let mut output = Vec::new();
        let (bytes, oversized) = read_bounded_line(&mut reader, &mut output, 32)?;
        assert_eq!(bytes, 129);
        assert!(oversized);
        assert!(output.is_empty());
        let (_, oversized) = read_bounded_line(&mut reader, &mut output, 32)?;
        assert!(!oversized);
        assert_eq!(output, b"{\"ok\":true}\n");
        Ok(())
    }

    #[test]
    fn message_text_is_truncated_to_the_requested_granularity() {
        let (text, truncated) = truncate_message_text(&"内容".repeat(250), 200);
        assert!(truncated);
        assert!(text.starts_with(&"内容".repeat(100)));
        assert!(text.ends_with("已截断）"));
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
        let preview = migrator.session_preview("one", 0, 1, 1_000)?;
        assert_eq!(preview.messages.len(), 1);
        assert_eq!(preview.messages[0].role, MessageRole::User);
        assert_eq!(preview.messages[0].content, "repair the sample database");
        let next = preview.next_offset.expect("there should be another page");
        let preview = migrator.session_preview("one", next, 1, 1_000)?;
        assert_eq!(preview.messages.len(), 1);
        assert_eq!(preview.messages[0].role, MessageRole::Assistant);
        assert_eq!(
            preview.messages[0].content,
            "I can help repair that database."
        );
        assert_eq!(
            preview.messages[0].timestamp.as_deref(),
            Some("2026-01-02T00:00:01Z")
        );
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
    fn archives_and_unarchives_one_session() -> Result<()> {
        let (_temporary, migrator, session_path) = create_fixture()?;
        let archive = migrator.plan_session_action("one", SessionAction::Archive)?;
        let archived_path = archive
            .rollout_destination
            .clone()
            .expect("archive destination");
        let backup = migrator.apply_session_action(&archive)?;
        assert!(backup.join("manifest.json").is_file());
        assert!(!session_path.exists());
        assert!(archived_path.is_file());

        let archived = migrator.resolve_session("one")?;
        assert!(archived.archived);
        assert_eq!(
            archived.rollout_path.as_deref(),
            Some(archived_path.as_path())
        );
        let state = Connection::open(migrator.codex_home.join("state_5.sqlite"))?;
        let (flag, archived_at): (i64, Option<i64>) = state.query_row(
            "SELECT archived, archived_at FROM threads WHERE id='one'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(flag, 1);
        assert!(archived_at.is_some());
        drop(state);

        let unarchive = migrator.plan_session_action("one", SessionAction::Unarchive)?;
        let active_path = unarchive
            .rollout_destination
            .clone()
            .expect("active destination");
        migrator.apply_session_action(&unarchive)?;
        assert!(!archived_path.exists());
        assert!(active_path.is_file());
        let active = migrator.resolve_session("one")?;
        assert!(!active.archived);
        assert_eq!(active.rollout_path.as_deref(), Some(active_path.as_path()));
        Ok(())
    }

    #[test]
    fn deletes_only_the_selected_session_and_keeps_a_backup() -> Result<()> {
        let (_temporary, migrator, session_path) = create_fixture()?;
        let plan = migrator.plan_session_action("one", SessionAction::Delete)?;
        assert!(
            plan.descriptions()
                .iter()
                .any(|line| line.starts_with("delete "))
        );
        let backup = migrator.apply_session_action(&plan)?;
        assert!(backup.join("manifest.json").is_file());
        assert!(!session_path.exists());
        assert!(migrator.resolve_session("one").is_err());
        assert_eq!(migrator.resolve_session("two")?.title, "Other work");

        let index = fs::read_to_string(migrator.codex_home.join("session_index.jsonl"))?;
        assert!(!index.contains("\"id\":\"one\""));
        let state = Connection::open(migrator.codex_home.join("state_5.sqlite"))?;
        let threads: i64 =
            state.query_row("SELECT count(*) FROM threads WHERE id='one'", [], |row| {
                row.get(0)
            })?;
        let artifacts: i64 = state.query_row(
            "SELECT count(*) FROM thread_artifacts WHERE thread_id='one'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!((threads, artifacts), (0, 0));
        let catalog = Connection::open(migrator.codex_home.join("sqlite/codex-dev.db"))?;
        let catalog_rows: i64 = catalog.query_row(
            "SELECT count(*) FROM local_thread_catalog WHERE thread_id='one'",
            [],
            |row| row.get(0),
        )?;
        let run_rows: i64 = catalog.query_row(
            "SELECT count(*) FROM automation_runs WHERE thread_id='one'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!((catalog_rows, run_rows), (0, 0));
        Ok(())
    }

    #[test]
    fn batch_repoints_and_deletes_multiple_sessions() -> Result<()> {
        let (_temporary, migrator, _session_path) = create_fixture()?;
        let references = vec!["one".to_owned(), "two".to_owned()];
        let backups = migrator.apply_session_repoints(&references, "/example/batch-target")?;
        assert_eq!(backups.len(), 2);
        assert!(references.iter().all(|id| {
            migrator
                .resolve_session(id)
                .is_ok_and(|session| session.cwd == Path::new("/example/batch-target"))
        }));

        let plans = references
            .iter()
            .map(|id| migrator.plan_session_action(id, SessionAction::Delete))
            .collect::<Result<Vec<_>>>()?;
        let backups = migrator.apply_session_actions(&plans)?;
        assert_eq!(backups.len(), 2);
        assert!(
            references
                .iter()
                .all(|id| migrator.resolve_session(id).is_err())
        );
        let index = fs::read_to_string(migrator.codex_home.join("session_index.jsonl"))?;
        assert!(index.trim().is_empty());
        Ok(())
    }

    #[test]
    fn failed_batches_roll_back_completed_sessions() -> Result<()> {
        let (_temporary, migrator, session_path) = create_fixture()?;
        let duplicate = vec!["one".to_owned(), "one".to_owned()];
        assert!(
            migrator
                .apply_session_repoints(&duplicate, "/example/batch-target")
                .is_err()
        );
        assert_eq!(
            migrator.resolve_session("one")?.cwd,
            Path::new("/example/old-repo")
        );
        assert!(session_path.is_file());

        let plans = vec![
            migrator.plan_session_action("one", SessionAction::Archive)?,
            migrator.plan_session_action("one", SessionAction::Archive)?,
        ];
        assert!(migrator.apply_session_actions(&plans).is_err());
        assert!(!migrator.resolve_session("one")?.archived);
        assert!(session_path.is_file());
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

    pub(super) fn create_opencode_fixture(opencode_home: &Path) -> Result<()> {
        fs::create_dir_all(opencode_home)?;
        let database = Connection::open(opencode_home.join("opencode.db"))?;
        database.execute_batch(
            "CREATE TABLE project (id TEXT, worktree TEXT, sandboxes TEXT);
             CREATE TABLE project_directory (project_id TEXT, directory TEXT, type TEXT);
             CREATE TABLE session (id TEXT, project_id TEXT, directory TEXT, path TEXT, title TEXT, slug TEXT, time_updated INTEGER, time_archived INTEGER);
             CREATE TABLE workspace (id TEXT, directory TEXT);
             CREATE TABLE event (id TEXT, aggregate_id TEXT, type TEXT, data TEXT);
             CREATE TABLE message (id TEXT, session_id TEXT, time_created INTEGER, data TEXT);
             CREATE TABLE part (id TEXT, message_id TEXT, session_id TEXT, time_created INTEGER, data TEXT);",
        )?;
        database.execute(
            "INSERT INTO project VALUES ('p1', '/example/old-repo', '[\"/example/old-repo\"]')",
            [],
        )?;
        database.execute(
            "INSERT INTO project_directory VALUES ('p1', '/example/old-repo', 'main')",
            [],
        )?;
        database.execute(
            "INSERT INTO session VALUES ('s1', 'p1', '/example/old-repo/sub', 'example/old-repo/sub', 'Sample session', 'sample-session', 2000, NULL)",
            [],
        )?;
        database.execute(
            "INSERT INTO session VALUES ('s2', 'p1', '/example/old-repo', 'example/old-repo', 'Main session', 'main-session', 1000, NULL)",
            [],
        )?;
        database.execute(
            "INSERT INTO workspace VALUES ('w1', '/example/old-repo')",
            [],
        )?;
        database.execute(
            "INSERT INTO event VALUES ('e1', 's1', 'session.created.1', ?1)",
            params![
                r#"{"sessionID":"s1","info":{"id":"s1","directory":"/example/old-repo","path":"example/old-repo","title":"/example/old-repo"}}"#
            ],
        )?;
        database.execute(
            "INSERT INTO event VALUES ('e2', 's1', 'message.updated.1', ?1)",
            params![r#"{"sessionID":"s1","info":{"directory":"/example/old-repo"}}"#],
        )?;
        database.execute(
            "INSERT INTO event VALUES ('e3', 's2', 'session.created.1', ?1)",
            params![
                r#"{"sessionID":"s2","info":{"id":"s2","directory":"/example/old-repo","path":"example/old-repo","title":"Main session"}}"#
            ],
        )?;
        database.execute(
            "INSERT INTO message VALUES ('m1', 's1', 1, '{\"role\":\"user\"}')",
            [],
        )?;
        database.execute(
            r#"INSERT INTO message VALUES ('m2', 's1', 2, '{"role":"assistant"}')"#,
            [],
        )?;
        database.execute(
            r#"INSERT INTO part VALUES ('p1', 'm1', 's1', 1, '{"type":"text","text":"please repoint the sample repo"}')"#,
            [],
        )?;
        database.execute(
            r#"INSERT INTO part VALUES ('p2', 'm2', 's1', 2, '{"type":"text","text":"ok"}')"#,
            [],
        )?;
        Ok(())
    }

    #[test]
    fn codex_batch_operations_leave_opencode_data_unchanged() -> Result<()> {
        let (temporary, migrator, _session_path) = create_fixture()?;
        let opencode_home = temporary.path().join("share/opencode");
        create_opencode_fixture(&opencode_home)?;
        let database_path = opencode_home.join("opencode.db");
        let original_database = fs::read(&database_path)?;
        let migrator = migrator.with_opencode(Some(opencode_home))?;
        let references = vec!["one".to_owned(), "two".to_owned()];

        migrator.apply_session_repoints(&references, "/example/batch-target")?;
        for action in [
            SessionAction::Archive,
            SessionAction::Unarchive,
            SessionAction::Delete,
        ] {
            let plans = references
                .iter()
                .map(|id| migrator.plan_session_action(id, action))
                .collect::<Result<Vec<_>>>()?;
            migrator.apply_session_actions(&plans)?;
        }

        assert!(migrator.list_sessions(None)?.is_empty());
        assert_eq!(fs::read(database_path)?, original_database);
        assert_eq!(migrator.list_opencode_sessions(None)?.len(), 2);
        Ok(())
    }

    #[test]
    fn mixed_workspace_backup_restores_both_data_directories() -> Result<()> {
        let (temporary, migrator, session_path) = create_fixture()?;
        let opencode_home = temporary.path().join("share/opencode");
        create_opencode_fixture(&opencode_home)?;
        let migrator = migrator.with_opencode(Some(opencode_home.clone()))?;

        let plan = migrator.plan("/example/old-repo", "/example/new-repo")?;
        let backup = migrator.apply(&plan)?.expect("workspace backup");
        assert_eq!(
            migrator.resolve_session("one")?.cwd,
            Path::new("/example/new-repo")
        );
        assert_eq!(
            migrator.resolve_opencode_session("s2")?.cwd,
            Path::new("/example/new-repo")
        );

        fs::remove_file(&session_path)?;
        fs::remove_dir_all(&opencode_home)?;
        migrator.restore_backup(&backup)?;

        assert!(session_path.is_file());
        assert_eq!(
            migrator.resolve_session("one")?.cwd,
            Path::new("/example/old-repo")
        );
        assert_eq!(
            migrator.resolve_opencode_session("s2")?.cwd,
            Path::new("/example/old-repo")
        );
        let preview = migrator.opencode_session_preview("s1", 6, None)?;
        assert_eq!(
            preview.messages[0].content,
            "please repoint the sample repo"
        );
        Ok(())
    }

    #[test]
    fn repoints_opencode_structure_and_event_log() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let codex_home = temporary.path().join(".codex");
        fs::create_dir_all(&codex_home)?;
        let opencode_home = temporary.path().join("share/opencode");
        create_opencode_fixture(&opencode_home)?;

        let migrator = Migrator::new(&codex_home)?.with_opencode(Some(opencode_home.clone()))?;
        let plan = migrator.plan("/example/old-repo", "/example/new-repo")?;
        assert!(plan.replacements() > 0);
        let backup = migrator.apply(&plan)?.expect("backup should be created");
        assert!(backup.join("manifest.json").is_file());
        assert!(backup.join("opencode/opencode.db").is_file());

        let verification = migrator.plan("/example/old-repo", "/example/new-repo")?;
        assert_eq!(verification.replacements(), 0);

        let database = Connection::open(opencode_home.join("opencode.db"))?;
        let worktree: String =
            database.query_row("SELECT worktree FROM project WHERE id='p1'", [], |row| {
                row.get(0)
            })?;
        assert_eq!(worktree, "/example/new-repo");
        let sandboxes: String =
            database.query_row("SELECT sandboxes FROM project WHERE id='p1'", [], |row| {
                row.get(0)
            })?;
        assert!(sandboxes.contains("/example/new-repo"));
        let directory: String = database.query_row(
            "SELECT directory FROM project_directory WHERE project_id='p1'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(directory, "/example/new-repo");
        let session: String =
            database.query_row("SELECT directory FROM session WHERE id='s1'", [], |row| {
                row.get(0)
            })?;
        assert_eq!(session, "/example/new-repo/sub");
        let path: String =
            database.query_row("SELECT path FROM session WHERE id='s1'", [], |row| {
                row.get(0)
            })?;
        assert_eq!(path, "example/new-repo/sub");
        let workspace: String =
            database.query_row("SELECT directory FROM workspace WHERE id='w1'", [], |row| {
                row.get(0)
            })?;
        assert_eq!(workspace, "/example/new-repo");

        let session_event: String =
            database.query_row("SELECT data FROM event WHERE id='e1'", [], |row| row.get(0))?;
        let session_event: Value = serde_json::from_str(&session_event)?;
        assert_eq!(session_event["info"]["directory"], "/example/new-repo");
        assert_eq!(session_event["info"]["path"], "example/new-repo");
        assert_eq!(session_event["info"]["title"], "/example/old-repo");
        let message_event: String =
            database.query_row("SELECT data FROM event WHERE id='e2'", [], |row| row.get(0))?;
        assert!(message_event.contains("/example/old-repo"));
        Ok(())
    }

    #[test]
    fn lists_and_previews_opencode_sessions() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let codex_home = temporary.path().join(".codex");
        fs::create_dir_all(&codex_home)?;
        let opencode_home = temporary.path().join("share/opencode");
        create_opencode_fixture(&opencode_home)?;
        let migrator = Migrator::new(&codex_home)?.with_opencode(Some(opencode_home.clone()))?;

        let sessions = migrator.list_opencode_sessions(None)?;
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, "s1");
        assert_eq!(sessions[0].cwd, PathBuf::from("/example/old-repo/sub"));
        assert_eq!(sessions[0].title, "Sample session");

        let by_path = migrator.list_opencode_sessions(Some("old-repo"))?;
        assert_eq!(by_path.len(), 2);
        assert!(
            by_path
                .iter()
                .all(|session| session.match_excerpt.contains("工作目录"))
        );
        let by_slug = migrator.list_opencode_sessions(Some("main-session"))?;
        assert_eq!(by_slug.len(), 1);
        assert_eq!(by_slug[0].session_id, "s2");

        let exact = migrator.find_opencode_sessions("/example/old-repo", false, None)?;
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].session_id, "s2");
        let recursive = migrator.find_opencode_sessions("/example/old-repo", true, None)?;
        assert_eq!(recursive.len(), 2);

        let preview = migrator.opencode_session_preview("s1", 6, None)?;
        assert_eq!(preview.messages.len(), 1);
        assert_eq!(
            preview.messages[0].content,
            "please repoint the sample repo"
        );
        let preview = migrator.opencode_session_preview("s1", 6, Some("sample"))?;
        assert_eq!(preview.messages.len(), 1);
        let empty = migrator.opencode_session_preview("s1", 6, Some("nomatch"))?;
        assert!(empty.messages.is_empty());
        Ok(())
    }

    #[test]
    fn repoints_single_opencode_session() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let codex_home = temporary.path().join(".codex");
        fs::create_dir_all(&codex_home)?;
        let opencode_home = temporary.path().join("share/opencode");
        create_opencode_fixture(&opencode_home)?;
        let migrator = Migrator::new(&codex_home)?.with_opencode(Some(opencode_home.clone()))?;

        let (_session, plan) = migrator.plan_opencode_session("s2", "/example/new-repo")?;
        assert!(plan.replacements() > 0);
        migrator.apply(&plan)?;

        let database = Connection::open(opencode_home.join("opencode.db"))?;
        let s2: String =
            database.query_row("SELECT directory FROM session WHERE id='s2'", [], |row| {
                row.get(0)
            })?;
        assert_eq!(s2, "/example/new-repo");
        let s1: String =
            database.query_row("SELECT directory FROM session WHERE id='s1'", [], |row| {
                row.get(0)
            })?;
        assert_eq!(s1, "/example/old-repo/sub");
        let worktree: String =
            database.query_row("SELECT worktree FROM project WHERE id='p1'", [], |row| {
                row.get(0)
            })?;
        assert_eq!(worktree, "/example/old-repo");
        let event: String =
            database.query_row("SELECT data FROM event WHERE id='e3'", [], |row| row.get(0))?;
        let event: Value = serde_json::from_str(&event)?;
        assert_eq!(event["info"]["directory"], "/example/new-repo");
        Ok(())
    }
}
