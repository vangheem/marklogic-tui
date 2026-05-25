mod client;
mod config;
mod query_file;

use anyhow::{Context, Result, bail};
use client::{MarkLogicClient, SearchResult, ServerConfig};
use config::AppConfig;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{
        Clear as TerminalClear, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode,
    },
};
use query_file::{
    QueryExecutionKind, discover_query_files, display_query_path, editor_lines,
    is_supported_query_file, load_query_file, query_execution_kind, save_query_file,
    should_autosave,
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{
        Block, Borders, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap,
        block::Title,
    },
};
use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::runtime::Runtime;
use tui_textarea::{Input, TextArea};

#[derive(Debug, Clone, PartialEq)]
enum Focus {
    Command,
    Query,
    Results,
    Filter,
}

#[derive(Debug, Clone, PartialEq)]
enum EditMode {
    Navigate,
    Insert,
}

#[derive(Debug, Clone, PartialEq)]
enum AppMode {
    Normal,
    FullScreenView,
    ServerAdd,
    DatabaseSelect,
    ServerSelect,
    CollectionSelect,
    DeleteConfirm,
    QueryFileSelect,
    QueryFileCreate,
}

struct App {
    config: AppConfig,
    client: Option<MarkLogicClient>,
    focus: Focus,
    edit_mode: EditMode,
    mode: AppMode,
    command_input: String,
    query_editor: TextArea<'static>,
    query_visible: bool,
    needs_terminal_refresh: bool,
    query_root_dir: PathBuf,
    query_files: Vec<PathBuf>,
    active_query_file: Option<PathBuf>,
    query_file_list_state: ListState,
    new_query_file_input: String,
    query_dirty: bool,
    query_last_edit: Option<Instant>,
    query_autosave_interval: Duration,
    results_text: String,
    status_message: String,
    // Query results (individual parts from eval)
    query_results: Vec<String>,
    query_results_state: TableState,
    // Listing state
    records: Vec<SearchResult>,
    list_state: TableState,
    selected_indices: Vec<usize>,
    current_page: usize,
    page_size: usize,
    total_results: Option<usize>,
    current_collection: Option<String>,
    uri_filter: Option<String>,
    filter_input: String,
    last_esc: Option<Instant>,
    last_query_results_g: Option<Instant>,
    // Full screen view
    active_document: Option<client::DocumentDetail>,
    full_view_content: String,
    full_view_scroll: u16,
    // Server add wizard
    server_add_step: usize,
    server_add_fields: Vec<String>,
    // Autocomplete
    autocomplete_suggestions: Vec<&'static str>,
    autocomplete_selected: usize,
    // Database selection
    database_list: Vec<String>,
    database_list_state: ListState,
    // Server selection
    server_list: Vec<String>,
    server_list_state: ListState,
    // Collection selection
    collection_list: Vec<String>,
    collection_list_state: ListState,
    // Runtime for async
    rt: Runtime,
}

impl App {
    fn new() -> Result<Self> {
        let config = AppConfig::load()?;
        let rt = Runtime::new()?;
        let query_root_dir = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let query_files = discover_query_files(&query_root_dir)?;
        let active_query_file = query_files.first().cloned();
        let client = config.active_server_config().map(|s| {
            let mut c = MarkLogicClient::new(s.clone());
            if let Some(db) = &config.active_database {
                c.set_database(db.clone());
            }
            c
        });

        let mut app_result = Ok(Self {
            config,
            client,
            focus: Focus::Command,
            edit_mode: EditMode::Navigate,
            mode: AppMode::Normal,
            command_input: String::new(),
            query_editor: Self::new_query_editor(Vec::new()),
            query_visible: false,
            needs_terminal_refresh: false,
            query_root_dir,
            query_files,
            active_query_file,
            query_file_list_state: ListState::default(),
            new_query_file_input: String::new(),
            query_dirty: false,
            query_last_edit: None,
            query_autosave_interval: Duration::from_secs(2),
            results_text: String::new(),
            status_message: String::new(),
            query_results: Vec::new(),
            query_results_state: TableState::default(),
            records: Vec::new(),
            list_state: TableState::default(),
            selected_indices: Vec::new(),
            current_page: 0,
            page_size: 20,
            total_results: None,
            current_collection: None,
            uri_filter: None,
            filter_input: String::new(),
            last_esc: None,
            last_query_results_g: None,
            active_document: None,
            full_view_content: String::new(),
            full_view_scroll: 0,
            server_add_step: 0,
            server_add_fields: vec![String::new(); 5], // name, uri, user, pass, port
            autocomplete_suggestions: Vec::new(),
            autocomplete_selected: 0,
            database_list: Vec::new(),
            database_list_state: ListState::default(),
            server_list: Vec::new(),
            server_list_state: ListState::default(),
            collection_list: Vec::new(),
            collection_list_state: ListState::default(),
            rt,
        });

        if let Ok(ref mut app) = app_result {
            app.initialize_query_editor();
        }

        // Calculate initial page_size from terminal size
        if let Ok(ref mut app) = app_result {
            app.recalculate_page_size();
        }

        // Auto-list if server and database are configured
        if let Ok(ref mut app) = app_result {
            if app.client.is_some() && app.config.active_database.is_some() {
                app.fetch_list();
            }
        }

        app_result
    }

    const COMMANDS: &[(&str, &str)] = &[
        (":servers", "Manage servers [a=add, d=remove, Enter=switch]"),
        (":server-add", "Open the add server wizard"),
        (":databases", "List databases"),
        (":list", "List all documents (paged)"),
        (":collections", "Show collections"),
        (":list:<collection>", "List documents in collection"),
        (":clear", "Clear collection filter and reset page"),
        (":tdes", "List Template Driven Extraction templates"),
        (":query", "Show the active query file"),
        (":query-files", "Open the query file picker"),
        (":query-open", "Open the query file picker"),
    ];

    fn new_query_editor(lines: Vec<String>) -> TextArea<'static> {
        let mut ta = TextArea::new(if lines.is_empty() {
            vec![String::new()]
        } else {
            lines
        });
        ta.set_block(Block::default().borders(Borders::ALL).title(
            "Query [Alt+Enter or F5 to run, Ctrl+E to edit, Ctrl+S to save, Ctrl+O to switch]",
        ));
        ta
    }

    fn initialize_query_editor(&mut self) {
        if let Some(path) = self.active_query_file.clone() {
            match self.load_query_file(path) {
                Ok(()) => {}
                Err(e) => {
                    self.query_editor = Self::new_query_editor(Vec::new());
                    self.active_query_file = None;
                    self.status_message = e.to_string();
                }
            }
        } else {
            self.query_editor = Self::new_query_editor(Vec::new());
            self.status_message = format!(
                "No supported query files found in {}",
                self.query_root_dir.display()
            );
        }
    }

    fn update_autocomplete(&mut self) {
        if self.command_input.starts_with(':') && self.command_input.len() > 1 {
            let input = &self.command_input;
            self.autocomplete_suggestions = Self::COMMANDS
                .iter()
                .filter(|(cmd, _)| cmd.starts_with(input))
                .map(|(cmd, _)| *cmd)
                .collect();
        } else if self.command_input == ":" {
            self.autocomplete_suggestions = Self::COMMANDS.iter().map(|(cmd, _)| *cmd).collect();
        } else {
            self.autocomplete_suggestions.clear();
        }
        self.autocomplete_selected = 0;
    }

    fn refresh_query_files(&mut self) -> Result<()> {
        self.query_files = discover_query_files(&self.query_root_dir)?;
        if self.query_files.is_empty() {
            self.query_file_list_state.select(None);
        } else {
            let selected = self
                .active_query_file
                .as_ref()
                .and_then(|active| self.query_files.iter().position(|path| path == active))
                .unwrap_or(0);
            self.query_file_list_state.select(Some(selected));
            if self.active_query_file.is_none() {
                self.active_query_file = self.query_files.get(selected).cloned();
            }
        }
        Ok(())
    }

    fn active_query_file_label(&self) -> String {
        self.active_query_file
            .as_ref()
            .map(|path| display_query_path(path, &self.query_root_dir))
            .unwrap_or_else(|| "(no query file)".to_string())
    }

    fn query_title(&self) -> String {
        let dirty = if self.query_dirty { " *" } else { "" };
        let shortcuts = match self.edit_mode {
            EditMode::Navigate => "i:insert mode|F4:editor|o:switch file|n:new|r/F5:run",
            EditMode::Insert => "ESC:Normal mode|Ctrl-r:Run|Ctrl-o:switch file",
        };
        format!(
            "Query [{}{}] [{}]",
            self.active_query_file_label(),
            dirty,
            shortcuts
        )
    }

    fn edit_mode_label(&self) -> &'static str {
        match self.edit_mode {
            EditMode::Navigate => "NORMAL",
            EditMode::Insert => "INSERT",
        }
    }

    fn focus_command_panel(&mut self) {
        self.focus = Focus::Command;
        self.edit_mode = EditMode::Navigate;
    }

    fn focus_query_panel(&mut self) {
        self.query_visible = true;
        self.focus = Focus::Query;
        self.edit_mode = EditMode::Navigate;
    }

    fn focus_results_panel(&mut self) {
        self.focus = Focus::Results;
        self.edit_mode = EditMode::Navigate;
    }

    fn cycle_panel_focus(&mut self) {
        match self.focus {
            Focus::Command | Focus::Filter => self.focus_query_panel(),
            Focus::Query => self.focus_results_panel(),
            Focus::Results => self.focus_command_panel(),
        }
    }

    fn cycle_panel_focus_backward(&mut self) {
        match self.focus {
            Focus::Command | Focus::Filter => self.focus_results_panel(),
            Focus::Query => self.focus_command_panel(),
            Focus::Results => self.focus_query_panel(),
        }
    }

    fn open_filter_input(&mut self) {
        self.focus = Focus::Filter;
        self.filter_input = self.uri_filter.clone().unwrap_or_default();
    }

    fn enter_insert_mode(&mut self) {
        if self.focus == Focus::Query {
            self.edit_mode = EditMode::Insert;
        } else {
            self.status_message = "Insert mode only applies to the query editor.".to_string();
        }
    }

    fn exit_insert_mode(&mut self) {
        self.edit_mode = EditMode::Navigate;
        self.last_esc = None;
    }

    fn replace_query_contents(&mut self, contents: &str) {
        self.query_editor = Self::new_query_editor(editor_lines(contents));
        self.focus_query_panel();
    }

    fn open_query_in_external_editor(&mut self) {
        let original_contents = self.query_editor.lines().join("\n");
        let edit_result = edit_text_in_external_editor(
            &original_contents,
            self.active_query_file.as_deref(),
            "query",
        );
        self.needs_terminal_refresh = true;

        match edit_result {
            Ok(edited_contents) => {
                self.replace_query_contents(&edited_contents);
                match self.active_query_file.as_ref() {
                    Some(path) => match load_query_file(path) {
                        Ok(saved_contents) => {
                            self.query_dirty = edited_contents != saved_contents;
                            self.query_last_edit = self.query_dirty.then_some(Instant::now());
                            self.status_message = format!(
                                "Loaded external edits for {}",
                                display_query_path(path, &self.query_root_dir)
                            );
                        }
                        Err(e) => {
                            self.query_dirty = true;
                            self.query_last_edit = Some(Instant::now());
                            self.status_message = format!(
                                "Loaded external edits, but couldn't compare {}: {}",
                                display_query_path(path, &self.query_root_dir),
                                e
                            );
                        }
                    },
                    None => {
                        self.query_dirty = false;
                        self.query_last_edit = None;
                        self.status_message =
                            "Loaded external edits into the query pane.".to_string();
                    }
                }
            }
            Err(e) => {
                self.status_message = format!("External edit failed: {}", e);
            }
        }
    }

    fn current_query_extension(&self) -> String {
        self.active_query_file
            .as_ref()
            .and_then(|path| path.extension())
            .and_then(|ext| ext.to_str())
            .filter(|ext| !ext.is_empty())
            .unwrap_or("xqy")
            .to_string()
    }

    fn default_new_query_file_name(&self) -> String {
        let extension = self.current_query_extension();
        let mut index = 1usize;
        loop {
            let file_name = format!("query-{}.{}", index, extension);
            let path = self.query_root_dir.join(&file_name);
            if !path.exists() && !self.query_files.iter().any(|existing| existing == &path) {
                return file_name;
            }
            index += 1;
        }
    }

    fn open_query_file_create(&mut self) {
        self.new_query_file_input = self.default_new_query_file_name();
        self.mode = AppMode::QueryFileCreate;
    }

    fn create_new_query_file_from_input(&mut self) {
        let file_name = self.new_query_file_input.trim().to_string();
        if file_name.is_empty() {
            self.status_message = "Enter a file name.".to_string();
            return;
        }

        let file_path = Path::new(&file_name);
        if file_path.components().count() != 1 {
            self.status_message =
                "Enter a file name only, not a nested or absolute path.".to_string();
            return;
        }

        let path = self.query_root_dir.join(file_path);
        if !is_supported_query_file(&path) {
            self.status_message =
                "Unsupported query file extension. Use js, mjs, sparql, sql, sjs, or xqy."
                    .to_string();
            return;
        }
        if path.exists() {
            self.status_message = format!(
                "Query file already exists: {}",
                display_query_path(&path, &self.query_root_dir)
            );
            return;
        }

        match save_query_file(&path, "") {
            Ok(()) => {
                if let Err(e) = self.refresh_query_files() {
                    self.status_message =
                        format!("Created file but failed to refresh picker: {}", e);
                    return;
                }
                if let Err(e) = self.load_query_file(path.clone()) {
                    self.status_message = format!("Created file but failed to open it: {}", e);
                    return;
                }
                self.mode = AppMode::Normal;
                self.status_message = format!(
                    "Created query file: {}",
                    display_query_path(&path, &self.query_root_dir)
                );
            }
            Err(e) => {
                self.status_message = format!("Failed to create query file: {}", e);
            }
        }
    }

    fn set_query_results(&mut self, results: Vec<String>) {
        self.records.clear();
        self.query_results = results;
        self.query_results_state
            .select(if self.query_results.is_empty() {
                None
            } else {
                Some(0)
            });
        self.last_query_results_g = None;
        self.results_text.clear();
        self.status_message = format!("{} result(s)", self.query_results.len());
    }

    fn set_active_document(&mut self, detail: client::DocumentDetail) {
        self.full_view_content = format_document_detail(&detail);
        self.active_document = Some(detail);
        self.full_view_scroll = 0;
        self.mode = AppMode::FullScreenView;
    }

    fn open_document_in_external_editor(&mut self) {
        let Some(detail) = self.active_document.clone() else {
            self.status_message =
                "The current full-screen view is not an editable document.".to_string();
            return;
        };
        let edit_result = edit_text_in_external_editor(
            &detail.content,
            Some(Path::new(detail.uri.as_str())),
            "document",
        );
        self.needs_terminal_refresh = true;

        match edit_result {
            Ok(edited_contents) => {
                if edited_contents == detail.content {
                    self.status_message = format!("No document changes to save for {}", detail.uri);
                    return;
                }

                let Some(client) = &self.client else {
                    self.status_message = "No server connected.".to_string();
                    return;
                };

                let client = client.clone();
                match self
                    .rt
                    .block_on(client.update_document(&detail.uri, &edited_contents))
                {
                    Ok(()) => {
                        let mut updated_detail = detail;
                        updated_detail.content = edited_contents;
                        self.set_active_document(updated_detail.clone());
                        self.status_message = format!("Saved document: {}", updated_detail.uri);
                        if !self.records.is_empty() {
                            self.fetch_list();
                            self.mode = AppMode::FullScreenView;
                            self.active_document = Some(updated_detail);
                        }
                    }
                    Err(e) => {
                        self.status_message = format!("Document save failed: {}", e);
                    }
                }
            }
            Err(e) => {
                self.status_message = format!("External edit failed: {}", e);
            }
        }
    }

    fn load_query_file(&mut self, path: PathBuf) -> Result<()> {
        let contents = load_query_file(&path)?;
        self.query_editor = Self::new_query_editor(editor_lines(&contents));
        self.active_query_file = Some(path.clone());
        self.query_dirty = false;
        self.query_last_edit = None;
        self.focus_query_panel();
        self.status_message = format!(
            "Loaded query file: {}",
            display_query_path(&path, &self.query_root_dir)
        );
        Ok(())
    }

    fn save_query_file_if_dirty(&mut self) -> Result<()> {
        if !self.query_dirty {
            return Ok(());
        }

        let Some(path) = self.active_query_file.clone() else {
            return Ok(());
        };

        let contents = self.query_editor.lines().join("\n");
        save_query_file(&path, &contents)?;
        self.query_dirty = false;
        self.query_last_edit = None;
        self.status_message = format!(
            "Saved query file: {}",
            display_query_path(&path, &self.query_root_dir)
        );
        Ok(())
    }

    fn save_query_file_manually(&mut self) {
        let Some(path) = self.active_query_file.clone() else {
            self.status_message = "No query file selected.".to_string();
            return;
        };

        if !self.query_dirty {
            self.status_message = format!(
                "Query file already saved: {}",
                display_query_path(&path, &self.query_root_dir)
            );
            return;
        }

        if let Err(e) = self.save_query_file_if_dirty() {
            self.status_message = format!("Save error: {}", e);
        }
    }

    fn maybe_autosave(&mut self) {
        if should_autosave(
            self.query_dirty,
            self.query_last_edit,
            Instant::now(),
            self.query_autosave_interval,
        ) {
            if let Err(e) = self.save_query_file_if_dirty() {
                self.status_message = format!("Autosave failed: {}", e);
            }
        }
    }

    fn show_query_editor(&mut self) {
        if self.active_query_file.is_none() {
            if self.refresh_query_files().is_ok() {
                if let Some(path) = self.active_query_file.clone() {
                    if let Err(e) = self.load_query_file(path) {
                        self.status_message = e.to_string();
                    }
                }
            }
        }
        self.focus_query_panel();
        if self.active_query_file.is_none() {
            self.status_message = format!(
                "No supported query files found in {}",
                self.query_root_dir.display()
            );
        }
    }

    fn open_query_file_picker(&mut self) {
        match self.refresh_query_files() {
            Ok(()) => {
                if self.query_files.is_empty() {
                    self.status_message = format!(
                        "No supported query files found in {}",
                        self.query_root_dir.display()
                    );
                } else {
                    self.mode = AppMode::QueryFileSelect;
                }
            }
            Err(e) => {
                self.status_message = e.to_string();
            }
        }
    }

    fn status_line(&self) -> String {
        let mode = self.edit_mode_label();
        let server = self
            .config
            .active_server
            .as_deref()
            .unwrap_or("(no server)");
        let db = self
            .config
            .active_database
            .as_deref()
            .unwrap_or("(no database)");
        let collection = self
            .current_collection
            .as_deref()
            .map(|c| format!(" | Collection: {}", c))
            .unwrap_or_default();
        let query_file = format!(" | Query: {}", self.active_query_file_label());
        format!(
            "Mode: {} | Server: {} | Database: {}{}{} | {}",
            mode, server, db, collection, query_file, self.status_message
        )
    }

    fn execute_command(&mut self) {
        let cmd = self.command_input.trim().to_string();
        self.command_input.clear();

        if !cmd.starts_with(':') {
            // It's a filter for current results
            self.status_message = format!("Filter: {}", cmd);
            return;
        }

        let parts: Vec<&str> = cmd[1..].splitn(2, ' ').collect();
        let command = parts[0];
        let _arg = parts.get(1).map(|s| s.trim());

        match command {
            "servers" => self.cmd_servers(),
            "server-add" => self.open_server_add(),
            "databases" => self.cmd_databases(),
            "list" => {
                if let Err(e) = self.save_query_file_if_dirty() {
                    self.results_text = format!("Save error: {}", e);
                    return;
                }
                self.current_collection = None;
                self.uri_filter = None;
                self.filter_input.clear();
                self.current_page = 0;
                self.query_visible = false;
                self.focus_results_panel();
                self.fetch_list();
            }
            "collections" => self.cmd_collections(),
            "clear" => {
                self.current_collection = None;
                self.uri_filter = None;
                self.filter_input.clear();
                self.current_page = 0;
                self.status_message = "Cleared collection filter, URI filter, and page".to_string();
            }
            "tdes" => self.cmd_tdes(),
            "query" => self.show_query_editor(),
            "query-files" | "query-open" => self.open_query_file_picker(),
            _ => {
                // Check if it's list:collection-name
                if command.starts_with("list:") {
                    let col = &command[5..];
                    self.current_collection = Some(col.to_string());
                    self.current_page = 0;
                    self.fetch_list();
                } else {
                    self.results_text = format!("Unknown command: :{}", command);
                }
            }
        }
    }

    fn reconnect(&mut self) {
        self.client = self.config.active_server_config().map(|s| {
            let mut c = MarkLogicClient::new(s.clone());
            if let Some(db) = &self.config.active_database {
                c.set_database(db.clone());
            }
            c
        });
    }

    fn cmd_servers(&mut self) {
        let list: Vec<String> = self
            .config
            .servers
            .iter()
            .map(|s| {
                let active = if self.config.active_server.as_deref() == Some(&s.name) {
                    " (active)"
                } else {
                    ""
                };
                format!("{}{} - {}", s.name, active, s.uri)
            })
            .collect();
        if list.is_empty() {
            self.open_server_add();
            self.status_message = "No servers configured. Add your first server.".to_string();
        } else {
            self.server_list = list;
            self.server_list_state.select(Some(0));
            self.mode = AppMode::ServerSelect;
        }
    }

    fn open_server_add(&mut self) {
        self.mode = AppMode::ServerAdd;
        self.server_add_step = 0;
        self.server_add_fields = vec![String::new(); 5];
    }

    fn cmd_databases(&mut self) {
        if let Some(client) = &self.client {
            let client = client.clone();
            match self.rt.block_on(client.list_databases()) {
                Ok(dbs) => {
                    self.database_list = dbs;
                    // Pre-select the active database
                    let selected = self
                        .config
                        .active_database
                        .as_ref()
                        .and_then(|active| self.database_list.iter().position(|d| d == active))
                        .unwrap_or(0);
                    self.database_list_state.select(Some(selected));
                    self.mode = AppMode::DatabaseSelect;
                }
                Err(e) => {
                    self.results_text = format!("Error listing databases: {}", e);
                }
            }
        } else {
            self.results_text = "No server connected.".to_string();
        }
    }

    fn cmd_collections(&mut self) {
        if let Some(client) = &self.client {
            let client = client.clone();
            match self.rt.block_on(client.list_collections()) {
                Ok(cols) => {
                    self.collection_list = cols;
                    self.collection_list_state
                        .select(if self.collection_list.is_empty() {
                            None
                        } else {
                            Some(0)
                        });
                    self.mode = AppMode::CollectionSelect;
                }
                Err(e) => {
                    self.results_text = format!("Error listing collections: {}", e);
                }
            }
        } else {
            self.results_text = "No server connected.".to_string();
        }
    }

    fn cmd_tdes(&mut self) {
        if let Some(client) = &self.client {
            let client = client.clone();
            let script = r#"
                'use strict';
                const tdes = fn.collection("http://marklogic.com/xdmp/tde");
                const results = [];
                for (const tde of tdes) {
                    results.push(xdmp.nodeUri(tde));
                }
                JSON.stringify(results);
            "#;
            match self.rt.block_on(client.js_query(script)) {
                Ok(parts) => {
                    let mut uris: Vec<String> = Vec::new();
                    for part in &parts {
                        if let Ok(arr) = serde_json::from_str::<Vec<String>>(part) {
                            uris.extend(arr);
                        }
                    }
                    if uris.is_empty() {
                        self.records.clear();
                        self.query_results.clear();
                        self.results_text = "No TDEs found.".to_string();
                    } else {
                        self.set_query_results(uris);
                        self.status_message = format!("{} TDE(s) found", self.query_results.len());
                        self.focus_results_panel();
                    }
                }
                Err(e) => {
                    self.results_text = format!("Error listing TDEs: {}", e);
                }
            }
        } else {
            self.results_text = "No server connected.".to_string();
        }
    }

    fn recalculate_page_size(&mut self) {
        if let Ok((_, rows)) = crossterm::terminal::size() {
            // status(3) + command(3) = 6 rows of chrome
            let main_height = rows.saturating_sub(6);
            let results_height = if self.query_visible {
                (main_height as f32 * 0.7) as u16
            } else {
                main_height
            };
            let usable = results_height.saturating_sub(4) as usize; // borders + header
            if usable > 0 {
                self.page_size = usable;
            }
        }
    }

    fn fetch_list(&mut self) {
        self.recalculate_page_size();
        if let Some(client) = &self.client {
            let client = client.clone();
            let col = self.current_collection.as_deref();
            let dir = self.uri_filter.as_deref();
            let start = self.current_page * self.page_size + 1;
            match self
                .rt
                .block_on(client.search_documents(col, None, dir, start, self.page_size))
            {
                Ok(paged) => {
                    self.total_results = paged.total;
                    self.records = paged.results;
                    self.list_state.select(if self.records.is_empty() {
                        None
                    } else {
                        Some(0)
                    });
                    self.selected_indices.clear();
                    self.focus_results_panel();
                    let total_str = paged
                        .total
                        .map(|t| t.to_string())
                        .unwrap_or("?".to_string());
                    self.status_message =
                        format!("Page {} | Total: {}", self.current_page + 1, total_str);
                }
                Err(e) => {
                    self.results_text = format!("Error: {}", e);
                }
            }
        } else {
            self.results_text = "No server connected.".to_string();
        }
    }

    fn select_query_file(&mut self, path: PathBuf) {
        if let Err(e) = self.save_query_file_if_dirty() {
            self.results_text = format!("Save error: {}", e);
            return;
        }

        if let Err(e) = self.load_query_file(path) {
            self.results_text = format!("Load error: {}", e);
        }
    }

    fn execute_query(&mut self) {
        let Some(path) = self.active_query_file.clone() else {
            self.results_text = "No query file selected.".to_string();
            return;
        };

        if let Err(e) = self.save_query_file_if_dirty() {
            self.query_results.clear();
            self.results_text = format!("Save error: {}", e);
            return;
        }

        if let Some(client) = &self.client {
            let client = client.clone();
            let query = self.query_editor.lines().join("\n");
            let result = match query_execution_kind(&path) {
                QueryExecutionKind::JavaScript => self.rt.block_on(client.js_query(&query)),
                QueryExecutionKind::XQuery => self.rt.block_on(client.xquery_query(&query)),
                QueryExecutionKind::Deferred => {
                    self.query_results.clear();
                    self.results_text = format!(
                        "Running {} files is not implemented yet.",
                        path.extension()
                            .and_then(|ext| ext.to_str())
                            .unwrap_or("this")
                    );
                    self.status_message = format!(
                        "Execution for {} files is deferred.",
                        path.extension()
                            .and_then(|ext| ext.to_str())
                            .unwrap_or("this")
                    );
                    return;
                }
                QueryExecutionKind::Unsupported => {
                    self.query_results.clear();
                    self.results_text = format!(
                        "Unsupported query file type: {}",
                        display_query_path(&path, &self.query_root_dir)
                    );
                    return;
                }
            };

            match result {
                Ok(parts) => {
                    self.set_query_results(parts);
                }
                Err(e) => {
                    self.query_results.clear();
                    self.results_text = format!("Query error: {}", e);
                }
            }
        } else {
            self.results_text = "No server connected.".to_string();
        }
    }

    fn delete_selected(&mut self) {
        if self.selected_indices.is_empty() {
            self.status_message = "No records selected for deletion.".to_string();
            return;
        }
        self.mode = AppMode::DeleteConfirm;
    }

    fn confirm_delete(&mut self) {
        let uris: Vec<String> = self
            .selected_indices
            .iter()
            .filter_map(|&i| self.records.get(i).map(|r| r.uri.clone()))
            .collect();
        if let Some(client) = &self.client {
            let client = client.clone();
            match self.rt.block_on(client.delete_documents(&uris)) {
                Ok(()) => {
                    self.status_message = format!("Deleted {} documents.", uris.len());
                    self.selected_indices.clear();
                    self.fetch_list();
                }
                Err(e) => {
                    self.status_message = format!("Delete error: {}", e);
                }
            }
        }
        self.mode = AppMode::Normal;
    }

    fn delete_uris(&self) -> Vec<String> {
        self.selected_indices
            .iter()
            .filter_map(|&i| self.records.get(i).map(|r| r.uri.clone()))
            .collect()
    }

    fn open_record(&mut self) {
        if let Some(idx) = self.list_state.selected() {
            if let Some(record) = self.records.get(idx) {
                let uri = record.uri.clone();
                if let Some(client) = &self.client {
                    let client = client.clone();
                    match self.rt.block_on(client.get_document(&uri)) {
                        Ok(detail) => {
                            self.set_active_document(detail);
                        }
                        Err(e) => {
                            self.active_document = None;
                            self.full_view_content = format!("Error loading document: {}", e);
                            self.full_view_scroll = 0;
                            self.mode = AppMode::FullScreenView;
                        }
                    }
                } else {
                    self.active_document = None;
                    self.full_view_content = format!("URI: {}", uri);
                    self.full_view_scroll = 0;
                    self.mode = AppMode::FullScreenView;
                }
            }
        }
    }

    fn snippet(record: &SearchResult) -> String {
        let cols = if record.collections.is_empty() {
            String::new()
        } else {
            format!(" [{}]", record.collections.join(", "))
        };
        format!("{}{}", record.uri, cols)
    }
}

/// Format a DocumentDetail for the full-screen view with metadata header + content
/// Create a single-line snippet from a result string, truncated to max_len
fn make_snippet(text: &str, max_len: usize) -> String {
    // Collapse to single line
    let oneline: String = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if oneline.len() > max_len {
        format!("{}...", &oneline[..max_len])
    } else {
        oneline
    }
}

fn format_document_detail(detail: &client::DocumentDetail) -> String {
    let mut output = String::new();
    output.push_str("━━━ Metadata ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");
    output.push_str(&format!("URI:          {}\n", detail.uri));
    output.push_str(&format!(
        "Collections:  {}\n",
        if detail.collections.is_empty() {
            "(none)".to_string()
        } else {
            detail.collections.join(", ")
        }
    ));
    if let Some(q) = detail.quality {
        output.push_str(&format!("Quality:      {}\n", q));
    }
    if !detail.permissions.is_empty() {
        output.push_str(&format!(
            "Permissions:  {}\n",
            detail.permissions.join(", ")
        ));
    }
    output.push_str("━━━ Content ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n\n");
    output.push_str(&format_document_content(&detail.content));
    output
}

/// Format document content based on type
fn format_document_content(text: &str) -> String {
    let trimmed = text.trim();
    // Try JSON
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return serde_json::to_string_pretty(&val).unwrap_or_else(|_| text.to_string());
    }
    // Try XML - do basic indentation
    if trimmed.starts_with('<') {
        return format_xml(trimmed);
    }
    // Fallback: plain text
    text.to_string()
}

/// Basic XML indentation formatter
fn format_xml(xml: &str) -> String {
    let mut result = String::new();
    let mut indent: usize = 0;
    let mut i = 0;
    let bytes = xml.as_bytes();

    while i < bytes.len() {
        if bytes[i] == b'<' {
            // Find end of tag
            let tag_start = i;
            while i < bytes.len() && bytes[i] != b'>' {
                i += 1;
            }
            if i < bytes.len() {
                i += 1; // include '>'
            }
            let tag = &xml[tag_start..i];

            if tag.starts_with("</") {
                // Closing tag - decrease indent
                indent = indent.saturating_sub(1);
                result.push_str(&"  ".repeat(indent));
                result.push_str(tag);
                result.push('\n');
            } else if tag.ends_with("/>") || tag.starts_with("<?") || tag.starts_with("<!") {
                // Self-closing or processing instruction
                result.push_str(&"  ".repeat(indent));
                result.push_str(tag);
                result.push('\n');
            } else {
                // Opening tag
                result.push_str(&"  ".repeat(indent));
                result.push_str(tag);
                result.push('\n');
                indent += 1;
            }
        } else {
            // Text content
            let text_start = i;
            while i < bytes.len() && bytes[i] != b'<' {
                i += 1;
            }
            let text_content = xml[text_start..i].trim();
            if !text_content.is_empty() {
                result.push_str(&"  ".repeat(indent));
                result.push_str(text_content);
                result.push('\n');
            }
        }
    }
    result
}

fn ui(f: &mut Frame, app: &mut App) {
    match app.mode {
        AppMode::FullScreenView => ui_fullscreen(f, app),
        AppMode::ServerAdd => ui_server_add(f, app),
        AppMode::ServerSelect => ui_server_select(f, app),
        AppMode::DatabaseSelect => ui_database_select(f, app),
        AppMode::CollectionSelect => ui_collection_select(f, app),
        AppMode::DeleteConfirm => ui_delete_confirm(f, app),
        AppMode::QueryFileSelect => ui_query_file_select(f, app),
        AppMode::QueryFileCreate => ui_query_file_create(f, app),
        AppMode::Normal => ui_normal(f, app),
    }
}

fn panel_block<T>(title: T, shortcut: &'static str) -> Block<'static>
where
    T: Into<Line<'static>>,
{
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title(Title::from(Line::from(shortcut)).alignment(Alignment::Right))
}

fn ui_normal(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // status
            Constraint::Length(3), // command
            Constraint::Min(0),    // main area
        ])
        .split(f.area());

    // Status row: split into status (left) and keybinds (right)
    let status_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[0]);

    let status = Paragraph::new(app.status_line()).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Status")
            .border_style(Style::default().fg(Color::Blue)),
    );
    f.render_widget(status, status_chunks[0]);

    let help_text = "1 command  2 query  3 results  i insert  Esc normal  / filter";
    let keys = Paragraph::new(help_text).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Help")
            .border_style(Style::default().fg(Color::Yellow)),
    );
    f.render_widget(keys, status_chunks[1]);

    // Command / Filter input
    if app.focus == Focus::Filter {
        let filter_display = format!("/{}", app.filter_input);
        let filter_widget = Paragraph::new(filter_display.as_str()).block(
            panel_block("Filter URI [Enter apply, Esc cancel]", "[1]")
                .border_style(Style::default().fg(Color::Green)),
        );
        f.render_widget(filter_widget, chunks[1]);
        let cursor_x = chunks[1].x + app.filter_input.len() as u16 + 2; // +1 border +1 for '/'
        let cursor_y = chunks[1].y + 1;
        f.set_cursor_position((cursor_x, cursor_y));
    } else {
        let cmd_style = Style::default().fg(Color::Magenta);
        let title = if let Some(ref filter) = app.uri_filter {
            format!(
                "Command [: commands, Enter run, Tab complete] | Filter: {}",
                filter
            )
        } else {
            "Command [: commands, Enter run, Tab complete]".to_string()
        };
        let command = Paragraph::new(app.command_input.as_str())
            .block(panel_block(title, "[1]").border_style(cmd_style));
        f.render_widget(command, chunks[1]);

        // Show cursor in command input when focused
        if app.focus == Focus::Command {
            let cursor_x = chunks[1].x + app.command_input.len() as u16 + 1;
            let cursor_y = chunks[1].y + 1;
            f.set_cursor_position((cursor_x, cursor_y));
        }
    }

    // Main area: query on top (if visible), results on bottom
    let main_chunks = if app.query_visible {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
            .split(chunks[2])
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0)])
            .split(chunks[2])
    };

    let results_area = if app.query_visible {
        main_chunks[1]
    } else {
        main_chunks[0]
    };

    if app.query_visible {
        // Query input
        let border_style = if app.focus == Focus::Query {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        };
        app.query_editor
            .set_block(panel_block(app.query_title(), "[2]").border_style(border_style));
        app.query_editor.set_cursor_line_style(Style::default());
        if app.focus == Focus::Query && app.edit_mode == EditMode::Insert {
            app.query_editor
                .set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
        } else {
            app.query_editor.set_cursor_style(Style::default());
        }
        f.render_widget(&app.query_editor, main_chunks[0]);
    } // end if query_visible

    // Results area
    let results_style = if app.focus == Focus::Results {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };

    // Dynamic page size based on available height (subtract borders + header row)
    let available_height = results_area.height.saturating_sub(4) as usize; // 2 borders + 1 header + 1 buffer
    if available_height > 0 {
        app.page_size = available_height;
    }

    if !app.records.is_empty() {
        // Show as table with aligned columns
        let rows: Vec<Row> = app
            .records
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let selected = app.selected_indices.contains(&i);
                let check = if selected { "[x]" } else { "[ ]" };
                let cols = if r.collections.is_empty() {
                    String::new()
                } else {
                    r.collections.join(", ")
                };
                let style = if selected {
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                Row::new(vec![check.to_string(), r.uri.clone(), cols]).style(style)
            })
            .collect();

        let title = "Results [Space select, Enter view, Ctrl+D delete, n/p page, / filter]";
        let header = Row::new(vec!["", "URI", "Collections"]).style(
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::Cyan),
        );
        let widths = [
            Constraint::Length(3),
            Constraint::Percentage(45),
            Constraint::Percentage(50),
        ];
        let table = Table::new(rows, widths)
            .header(header)
            .block(panel_block(title, "[3]").border_style(results_style))
            .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
            .row_highlight_style(Style::default().bg(Color::Blue).fg(Color::White));
        f.render_stateful_widget(table, results_area, &mut app.list_state);
    } else if !app.query_results.is_empty() {
        // Show query results as navigable list with snippets
        let rows: Vec<Row> = app
            .query_results
            .iter()
            .enumerate()
            .map(|(i, result)| {
                // width minus borders(2), index col(4), padding(3)
                let max_width = results_area.width.saturating_sub(9) as usize;
                let snippet = make_snippet(result, max_width);
                let num = format!("{}", i + 1);
                Row::new(vec![num, snippet])
            })
            .collect();

        let title = "Query Results [Enter view full, j/k navigate, Tab focus query]";
        let header = Row::new(vec!["#", "Result"]).style(
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::Cyan),
        );
        let widths = [Constraint::Length(4), Constraint::Min(0)];
        let table = Table::new(rows, widths)
            .header(header)
            .block(panel_block(title, "[3]").border_style(results_style))
            .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
            .row_highlight_style(Style::default().bg(Color::Blue).fg(Color::White));
        f.render_stateful_widget(table, results_area, &mut app.query_results_state);
    } else {
        let results = Paragraph::new(app.results_text.as_str())
            .block(panel_block("Results", "[3]").border_style(results_style))
            .wrap(Wrap { trim: false });
        f.render_widget(results, results_area);
    }

    // Autocomplete popup (rendered last so it draws on top of query area)
    if !app.autocomplete_suggestions.is_empty() && app.focus == Focus::Command {
        let items: Vec<ListItem> = app
            .autocomplete_suggestions
            .iter()
            .enumerate()
            .map(|(i, &cmd)| {
                let desc = App::COMMANDS
                    .iter()
                    .find(|(c, _)| *c == cmd)
                    .map(|(_, d)| *d)
                    .unwrap_or("");
                let text = format!(" {} - {}", cmd, desc);
                let style = if i == app.autocomplete_selected {
                    Style::default().bg(Color::DarkGray).fg(Color::White)
                } else {
                    Style::default().fg(Color::Gray)
                };
                ListItem::new(text).style(style)
            })
            .collect();
        let height = (items.len() as u16 + 2).min(12); // +2 for borders
        let popup_area = Rect {
            x: chunks[1].x,
            y: chunks[1].y + chunks[1].height,
            width: chunks[1].width.min(70),
            height,
        };
        let list =
            List::new(items).block(Block::default().borders(Borders::ALL).title("Suggestions"));
        f.render_widget(Clear, popup_area);
        f.render_widget(list, popup_area);
    }
}

fn ui_fullscreen(f: &mut Frame, app: &App) {
    let title = if app.active_document.is_some() {
        "Document View [Esc to close, Ctrl+E to edit, j/k or arrows to scroll]"
    } else {
        "Document View [Esc to close, j/k or arrows to scroll]"
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let para = Paragraph::new(app.full_view_content.as_str())
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((app.full_view_scroll, 0));
    f.render_widget(para, f.area());
}

fn ui_database_select(f: &mut Frame, app: &mut App) {
    let area = centered_rect(50, 60, f.area());
    f.render_widget(Clear, area);

    let items: Vec<ListItem> = app
        .database_list
        .iter()
        .map(|db| {
            let marker = if app.config.active_database.as_deref() == Some(db.as_str()) {
                " (active)"
            } else {
                ""
            };
            ListItem::new(format!("  {}{}", db, marker))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Select Database [Enter=select, Esc=cancel]"),
        )
        .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut app.database_list_state);
}

fn ui_collection_select(f: &mut Frame, app: &mut App) {
    let area = centered_rect(50, 60, f.area());
    f.render_widget(Clear, area);

    let items: Vec<ListItem> = app
        .collection_list
        .iter()
        .map(|col| {
            let marker = if app.current_collection.as_deref() == Some(col.as_str()) {
                " (active)"
            } else {
                ""
            };
            ListItem::new(format!("  {}{}", col, marker))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Select Collection [Enter=list, Esc=cancel]"),
        )
        .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut app.collection_list_state);
}

fn ui_query_file_select(f: &mut Frame, app: &mut App) {
    let area = centered_rect(60, 60, f.area());
    f.render_widget(Clear, area);

    let items: Vec<ListItem> = app
        .query_files
        .iter()
        .map(|path| {
            let marker = if app.active_query_file.as_ref() == Some(path) {
                " (active)"
            } else {
                ""
            };
            ListItem::new(format!(
                "  {}{}",
                display_query_path(path, &app.query_root_dir),
                marker
            ))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Select Query File [Enter=load, Esc=cancel]"),
        )
        .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut app.query_file_list_state);
}

fn ui_query_file_create(f: &mut Frame, app: &App) {
    let area = centered_rect(60, 20, f.area());
    f.render_widget(Clear, area);

    let input = Paragraph::new(app.new_query_file_input.as_str()).block(
        Block::default()
            .borders(Borders::ALL)
            .title("New Query File [Enter=create, Esc=cancel]"),
    );
    f.render_widget(input, area);
    f.set_cursor_position((
        area.x + app.new_query_file_input.len() as u16 + 1,
        area.y + 1,
    ));
}

fn ui_delete_confirm(f: &mut Frame, app: &App) {
    let uris = app.delete_uris();
    let area = centered_rect(70, 60, f.area());
    f.render_widget(Clear, area);

    let mut lines = vec![format!("Delete {} document(s)?", uris.len()), String::new()];
    for uri in &uris {
        lines.push(format!("  {}", uri));
    }
    lines.push(String::new());
    lines.push("Press 'y' to confirm, 'n' or Esc to cancel".to_string());

    let text = lines.join("\n");
    let para = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Confirm Delete")
                .border_style(Style::default().fg(Color::Red)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn ui_server_select(f: &mut Frame, app: &mut App) {
    let area = centered_rect(60, 60, f.area());
    f.render_widget(Clear, area);

    let items: Vec<ListItem> = app
        .server_list
        .iter()
        .map(|s| ListItem::new(format!("  {}", s)))
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Servers [Enter=switch, a=add, d=remove, Esc=close]"),
        )
        .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut app.server_list_state);
}

fn ui_server_add(f: &mut Frame, app: &App) {
    let labels = [
        "Name",
        "URI (e.g. http://localhost)",
        "Username",
        "Password",
        "Port (default 8003)",
    ];
    let area = centered_rect(60, 50, f.area());
    f.render_widget(Clear, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            labels
                .iter()
                .map(|_| Constraint::Length(3))
                .chain(std::iter::once(Constraint::Min(0)))
                .collect::<Vec<_>>(),
        )
        .split(area);

    for (i, label) in labels.iter().enumerate() {
        let style = if i == app.server_add_step {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        };
        let display = if i == 3 {
            "*".repeat(app.server_add_fields[i].len())
        } else {
            app.server_add_fields[i].clone()
        };
        let p = Paragraph::new(display).block(
            Block::default()
                .borders(Borders::ALL)
                .title(*label)
                .border_style(style),
        );
        f.render_widget(p, chunks[i]);
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn handle_event(app: &mut App) -> Result<bool> {
    if event::poll(std::time::Duration::from_millis(100))? {
        if let Event::Key(key) = event::read()? {
            // Global quit
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                if let Err(e) = app.save_query_file_if_dirty() {
                    app.status_message = format!("Save error: {}", e);
                    return Ok(false);
                }
                return Ok(true);
            }

            match app.mode {
                AppMode::FullScreenView => return handle_fullscreen_key(app, key),
                AppMode::ServerAdd => return handle_server_add_key(app, key),
                AppMode::ServerSelect => return handle_server_select_key(app, key),
                AppMode::DatabaseSelect => return handle_database_select_key(app, key),
                AppMode::CollectionSelect => return handle_collection_select_key(app, key),
                AppMode::DeleteConfirm => return handle_delete_confirm_key(app, key),
                AppMode::QueryFileSelect => return handle_query_file_select_key(app, key),
                AppMode::QueryFileCreate => return handle_query_file_create_key(app, key),
                AppMode::Normal => {}
            }

            if app.query_visible
                && key.code == KeyCode::Char('o')
                && key.modifiers.contains(KeyModifiers::CONTROL)
            {
                app.open_query_file_picker();
                return Ok(false);
            }

            // These Ctrl shortcuts remain active in both navigation and insert modes.
            if app.query_visible
                && key.code == KeyCode::Char('s')
                && key.modifiers.contains(KeyModifiers::CONTROL)
            {
                app.save_query_file_manually();
                return Ok(false);
            }

            if handle_query_insert_transition_key(app, key) {
                return Ok(false);
            }

            // Double-Esc in navigation mode: clear all filters and re-fetch
            if key.code == KeyCode::Esc {
                if app.edit_mode == EditMode::Navigate {
                    if let Some(last) = app.last_esc {
                        if last.elapsed().as_millis() < 500 {
                            if let Err(e) = app.save_query_file_if_dirty() {
                                app.status_message = format!("Save error: {}", e);
                                return Ok(false);
                            }
                            app.last_esc = None;
                            app.current_collection = None;
                            app.uri_filter = None;
                            app.filter_input.clear();
                            app.current_page = 0;
                            app.query_visible = false;
                            app.focus_results_panel();
                            app.status_message = "Cleared all filters".to_string();
                            app.fetch_list();
                            return Ok(false);
                        }
                    }
                    app.last_esc = Some(Instant::now());
                } else {
                    app.last_esc = None;
                }
            } else {
                app.last_esc = None;
            }

            if app.edit_mode == EditMode::Navigate && handle_navigation_mode_key(app, key) {
                return Ok(false);
            }

            if app.edit_mode == EditMode::Navigate
                && app.focus != Focus::Command
                && app.focus != Focus::Filter
                && key.code == KeyCode::Char(':')
            {
                app.focus_command_panel();
                app.command_input = ":".to_string();
                app.update_autocomplete();
                return Ok(false);
            }

            match app.focus {
                Focus::Command => handle_command_key(app, key),
                Focus::Query => handle_query_key(app, key),
                Focus::Results => handle_results_key(app, key),
                Focus::Filter => handle_filter_key(app, key),
            }
        }
    }
    Ok(false)
}

fn handle_navigation_mode_key(app: &mut App, key: KeyEvent) -> bool {
    if !key.modifiers.is_empty() {
        return false;
    }

    let is_navigation_surface = matches!(app.focus, Focus::Query | Focus::Results);

    match key.code {
        KeyCode::Char('1') if is_navigation_surface => {
            app.focus_command_panel();
            true
        }
        KeyCode::Char('2') if is_navigation_surface => {
            app.show_query_editor();
            app.edit_mode = EditMode::Navigate;
            true
        }
        KeyCode::Char('3') if is_navigation_surface => {
            app.focus_results_panel();
            true
        }
        KeyCode::Tab if is_navigation_surface => {
            app.cycle_panel_focus();
            true
        }
        KeyCode::BackTab if is_navigation_surface => {
            app.cycle_panel_focus_backward();
            true
        }
        KeyCode::Char('i') if app.focus == Focus::Query => {
            app.enter_insert_mode();
            true
        }
        KeyCode::Char('/') if app.focus == Focus::Results => {
            app.open_filter_input();
            true
        }
        _ => false,
    }
}

fn handle_query_insert_transition_key(app: &mut App, key: KeyEvent) -> bool {
    if app.edit_mode != EditMode::Insert || app.focus != Focus::Query {
        return false;
    }

    match key.code {
        KeyCode::Esc => {
            app.exit_insert_mode();
            true
        }
        KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.exit_insert_mode();
            app.cycle_panel_focus();
            true
        }
        _ => false,
    }
}

fn map_query_navigation_key(key: KeyEvent) -> Option<Input> {
    if !key.modifiers.is_empty() {
        return None;
    }

    match key.code {
        KeyCode::Up
        | KeyCode::Down
        | KeyCode::Left
        | KeyCode::Right
        | KeyCode::Home
        | KeyCode::End
        | KeyCode::PageUp
        | KeyCode::PageDown => Some(Input::from(key)),
        KeyCode::Char('h') => Some(Input::from(KeyEvent::new(
            KeyCode::Left,
            KeyModifiers::NONE,
        ))),
        KeyCode::Char('j') => Some(Input::from(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        ))),
        KeyCode::Char('k') => Some(Input::from(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))),
        KeyCode::Char('l') => Some(Input::from(KeyEvent::new(
            KeyCode::Right,
            KeyModifiers::NONE,
        ))),
        _ => None,
    }
}

fn handle_query_key(app: &mut App, key: KeyEvent) {
    if key.code == KeyCode::F(4) {
        app.open_query_in_external_editor();
    } else if key.code == KeyCode::F(5) {
        app.execute_query();
    } else if app.edit_mode == EditMode::Navigate && key.modifiers.is_empty() {
        match key.code {
            KeyCode::Char('o') => app.open_query_file_picker(),
            KeyCode::Char('n') => app.open_query_file_create(),
            KeyCode::Char('r') => app.execute_query(),
            _ => {
                if let Some(input) = map_query_navigation_key(key) {
                    app.query_editor.input(input);
                }
            }
        }
    } else if key.code == KeyCode::Char('r') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.execute_query();
    } else if (key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::ALT))
        || (key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::CONTROL))
    {
        app.execute_query();
    } else if app.edit_mode == EditMode::Insert {
        let before = app.query_editor.lines().join("\n");
        app.query_editor.input(Input::from(key));
        if app.query_editor.lines().join("\n") != before {
            app.query_dirty = true;
            app.query_last_edit = Some(Instant::now());
        }
    }
}

fn temp_editor_path(active_path: Option<&Path>) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut file_name = format!("marklogic-tui-edit-{}-{}", std::process::id(), unique);
    if let Some(extension) = active_path
        .and_then(|path| path.extension())
        .and_then(|ext| ext.to_str())
        .filter(|ext| !ext.is_empty())
    {
        file_name.push('.');
        file_name.push_str(extension);
    } else {
        file_name.push_str(".tmp");
    }
    env::temp_dir().join(file_name)
}

fn edit_text_in_external_editor(
    original_contents: &str,
    active_path: Option<&Path>,
    label: &str,
) -> Result<String> {
    if env::var_os("EDITOR").is_none() {
        bail!("$EDITOR is not set");
    }

    let temp_path = temp_editor_path(active_path);
    fs::write(&temp_path, original_contents).with_context(|| {
        format!(
            "Failed to create temporary {} file: {}",
            label,
            temp_path.display()
        )
    })?;

    let edit_result = suspend_tui_for_external_editor(&temp_path).and_then(|_| {
        fs::read_to_string(&temp_path).with_context(|| {
            format!(
                "Failed to read edited {} from temporary file: {}",
                label,
                temp_path.display()
            )
        })
    });
    let _ = fs::remove_file(&temp_path);
    edit_result
}

fn suspend_tui_for_external_editor(path: &Path) -> Result<()> {
    disable_raw_mode().context("Failed to suspend raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, LeaveAlternateScreen).context("Failed to leave alternate screen")?;

    let edit_result = run_external_editor(path);
    let resume_result = (|| -> Result<()> {
        enable_raw_mode().context("Failed to restore raw mode")?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, TerminalClear(ClearType::All))
            .context("Failed to restore alternate screen")?;
        Ok(())
    })();

    match (edit_result, resume_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(edit_err), Ok(())) => Err(edit_err),
        (Ok(()), Err(resume_err)) => Err(resume_err),
        (Err(edit_err), Err(resume_err)) => {
            Err(edit_err.context(format!("Also failed to restore terminal: {}", resume_err)))
        }
    }
}

fn run_external_editor(path: &Path) -> Result<()> {
    let status = external_editor_command(path)
        .status()
        .with_context(|| format!("Failed to launch $EDITOR for {}", path.display()))?;
    if !status.success() {
        bail!("$EDITOR exited with status {}", status);
    }
    Ok(())
}

fn external_editor_command(path: &Path) -> Command {
    #[cfg(windows)]
    {
        let mut command = Command::new("cmd");
        command
            .arg("/C")
            .arg(format!(r#"%EDITOR% "{}""#, path.display()));
        command
    }

    #[cfg(not(windows))]
    {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(r#"exec $EDITOR "$1""#)
            .arg("sh")
            .arg(path);
        command
    }
}

#[cfg(test)]
mod tests {
    use super::{
        App, AppMode, EditMode, Focus, handle_command_key, handle_filter_key,
        handle_navigation_mode_key, handle_query_insert_transition_key, handle_results_key,
        map_query_navigation_key, temp_editor_path,
    };
    use crate::config::AppConfig;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::widgets::{ListState, TableState};
    use std::{
        fs,
        path::{Path, PathBuf},
        time::Duration,
    };
    use tokio::runtime::Runtime;

    fn test_app() -> App {
        App {
            config: AppConfig::default(),
            client: None,
            focus: Focus::Command,
            edit_mode: EditMode::Navigate,
            mode: AppMode::Normal,
            command_input: String::new(),
            query_editor: App::new_query_editor(Vec::new()),
            query_visible: false,
            needs_terminal_refresh: false,
            query_root_dir: PathBuf::from("."),
            query_files: Vec::new(),
            active_query_file: None,
            query_file_list_state: ListState::default(),
            new_query_file_input: String::new(),
            query_dirty: false,
            query_last_edit: None,
            query_autosave_interval: Duration::from_secs(2),
            results_text: String::new(),
            status_message: String::new(),
            query_results: Vec::new(),
            query_results_state: TableState::default(),
            records: Vec::new(),
            list_state: TableState::default(),
            selected_indices: Vec::new(),
            current_page: 0,
            page_size: 20,
            total_results: None,
            current_collection: None,
            uri_filter: None,
            filter_input: String::new(),
            last_esc: None,
            last_query_results_g: None,
            active_document: None,
            full_view_content: String::new(),
            full_view_scroll: 0,
            server_add_step: 0,
            server_add_fields: vec![String::new(); 5],
            autocomplete_suggestions: Vec::new(),
            autocomplete_selected: 0,
            database_list: Vec::new(),
            database_list_state: ListState::default(),
            server_list: Vec::new(),
            server_list_state: ListState::default(),
            collection_list: Vec::new(),
            collection_list_state: ListState::default(),
            rt: Runtime::new().unwrap(),
        }
    }

    #[test]
    fn temp_editor_path_preserves_active_file_extension() {
        let temp_path = temp_editor_path(Some(Path::new("query.xqy")));
        assert_eq!(
            temp_path.extension().and_then(|ext| ext.to_str()),
            Some("xqy")
        );
    }

    #[test]
    fn temp_editor_path_falls_back_to_tmp_without_active_extension() {
        let temp_path = temp_editor_path(None);
        assert_eq!(
            temp_path.extension().and_then(|ext| ext.to_str()),
            Some("tmp")
        );
    }

    #[test]
    fn modal_input_numeric_shortcuts_switch_panels() {
        let mut app = test_app();
        app.focus = Focus::Results;

        assert!(handle_navigation_mode_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE)
        ));
        assert_eq!(app.focus, Focus::Command);
        assert_eq!(app.edit_mode, EditMode::Navigate);

        app.focus_results_panel();
        assert!(handle_navigation_mode_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE)
        ));
        assert_eq!(app.focus, Focus::Query);
        assert_eq!(app.edit_mode, EditMode::Navigate);
        assert!(app.query_visible);

        assert!(handle_navigation_mode_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE)
        ));
        assert_eq!(app.focus, Focus::Results);
        assert_eq!(app.edit_mode, EditMode::Navigate);
    }

    #[test]
    fn modal_input_numeric_shortcuts_do_not_hijack_command_input() {
        let mut app = test_app();
        app.focus = Focus::Command;

        assert!(!handle_navigation_mode_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE)
        ));
        assert_eq!(app.focus, Focus::Command);
    }

    #[test]
    fn modal_input_command_tab_cycles_when_input_is_empty() {
        let mut app = test_app();
        app.focus = Focus::Command;

        handle_command_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

        assert_eq!(app.focus, Focus::Query);
    }

    #[test]
    fn modal_input_command_tab_keeps_completion_once_typing_starts() {
        let mut app = test_app();
        app.focus = Focus::Command;
        app.command_input = ":q".to_string();
        app.update_autocomplete();

        handle_command_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

        assert_eq!(app.focus, Focus::Command);
        assert_eq!(app.command_input, ":query");
    }

    #[test]
    fn modal_input_insert_mode_can_be_entered_and_exited() {
        let mut app = test_app();
        app.focus_query_panel();

        assert!(handle_navigation_mode_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)
        ));
        assert_eq!(app.edit_mode, EditMode::Insert);
        assert_eq!(app.focus, Focus::Query);

        app.exit_insert_mode();
        assert_eq!(app.edit_mode, EditMode::Navigate);
        assert_eq!(app.focus, Focus::Query);
    }

    #[test]
    fn modal_input_ctrl_t_exits_insert_and_cycles_panel() {
        let mut app = test_app();
        app.focus_query_panel();
        app.edit_mode = EditMode::Insert;

        assert!(handle_query_insert_transition_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)
        ));
        assert_eq!(app.edit_mode, EditMode::Navigate);
        assert_eq!(app.focus, Focus::Results);
    }

    #[test]
    fn modal_input_filter_returns_to_results_on_escape() {
        let mut app = test_app();
        app.focus_results_panel();

        assert!(handle_navigation_mode_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)
        ));
        assert_eq!(app.focus, Focus::Filter);
        assert_eq!(app.edit_mode, EditMode::Navigate);

        handle_filter_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.focus, Focus::Results);
        assert_eq!(app.edit_mode, EditMode::Navigate);
    }

    #[test]
    fn modal_input_i_only_enters_insert_in_query_panel() {
        let mut app = test_app();
        app.focus = Focus::Command;

        assert!(!handle_navigation_mode_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)
        ));
        assert_eq!(app.edit_mode, EditMode::Navigate);

        app.focus_query_panel();
        assert!(handle_navigation_mode_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)
        ));
        assert_eq!(app.edit_mode, EditMode::Insert);
    }

    #[test]
    fn modal_input_tab_cycles_panels_in_navigation_mode() {
        let mut app = test_app();
        app.focus_query_panel();

        assert!(handle_navigation_mode_key(
            &mut app,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)
        ));
        assert_eq!(app.focus, Focus::Results);

        assert!(handle_navigation_mode_key(
            &mut app,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)
        ));
        assert_eq!(app.focus, Focus::Command);
    }

    #[test]
    fn modal_input_query_navigation_maps_vim_keys() {
        assert!(
            map_query_navigation_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE))
                .is_some()
        );
        assert!(
            map_query_navigation_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))
                .is_some()
        );
        assert!(
            map_query_navigation_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE))
                .is_some()
        );
        assert!(
            map_query_navigation_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE))
                .is_some()
        );
        assert!(
            map_query_navigation_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
                .is_none()
        );
    }

    #[test]
    fn modal_input_query_title_reflects_mode_specific_shortcuts() {
        let mut app = test_app();
        app.focus_query_panel();
        assert!(
            app.query_title()
                .contains("i:insert mode|F4:editor|o:switch file|n:new|r/F5:run")
        );

        app.edit_mode = EditMode::Insert;
        assert!(
            app.query_title()
                .contains("ESC:Normal mode|Ctrl-r:Run|Ctrl-o:switch file")
        );
    }

    #[test]
    fn modal_input_new_query_file_default_uses_current_extension() {
        let mut app = test_app();
        let temp_dir = std::env::temp_dir().join(format!(
            "marklogic-tui-query-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&temp_dir).unwrap();
        fs::write(temp_dir.join("query-1.xqy"), "").unwrap();
        app.query_root_dir = temp_dir.clone();
        app.query_files = vec![temp_dir.join("query-1.xqy")];
        app.active_query_file = Some(temp_dir.join("current.xqy"));

        assert_eq!(app.default_new_query_file_name(), "query-2.xqy");

        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn modal_input_new_query_file_defaults_to_xqy_without_active_file() {
        let mut app = test_app();
        let temp_dir = std::env::temp_dir().join(format!(
            "marklogic-tui-query-default-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&temp_dir).unwrap();
        app.query_root_dir = temp_dir.clone();

        assert_eq!(app.default_new_query_file_name(), "query-1.xqy");

        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn modal_input_new_query_file_rejects_unsupported_extensions() {
        let mut app = test_app();
        app.mode = AppMode::QueryFileCreate;
        app.new_query_file_input = "query-1.txt".to_string();

        app.create_new_query_file_from_input();

        assert_eq!(app.mode, AppMode::QueryFileCreate);
        assert!(
            app.status_message
                .contains("Unsupported query file extension")
        );
    }

    #[test]
    fn modal_input_setting_query_results_keeps_query_focus() {
        let mut app = test_app();
        app.focus_query_panel();

        app.set_query_results(vec!["result".to_string()]);

        assert_eq!(app.focus, Focus::Query);
        assert_eq!(app.query_results, vec!["result".to_string()]);
    }

    #[test]
    fn modal_input_query_results_support_vim_page_navigation() {
        let mut app = test_app();
        app.focus_results_panel();
        app.page_size = 10;
        app.set_query_results((0..30).map(|i| format!("result-{i}")).collect());

        handle_results_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
        );
        assert_eq!(app.query_results_state.selected(), Some(10));

        handle_results_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE),
        );
        assert_eq!(app.query_results_state.selected(), Some(0));
    }

    #[test]
    fn modal_input_query_results_support_vim_end_and_top_navigation() {
        let mut app = test_app();
        app.focus_results_panel();
        app.set_query_results((0..5).map(|i| format!("result-{i}")).collect());

        handle_results_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE),
        );
        assert_eq!(app.query_results_state.selected(), Some(4));

        handle_results_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
        );
        handle_results_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
        );
        assert_eq!(app.query_results_state.selected(), Some(0));
    }
}

fn handle_command_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => {
            if !app.autocomplete_suggestions.is_empty() && app.command_input.starts_with(':') {
                // Accept the selected suggestion
                let suggestion = app.autocomplete_suggestions[app.autocomplete_selected];
                // Take just the command part (before any <arg> placeholder)
                let cmd_part = suggestion.split(' ').next().unwrap_or(suggestion);
                app.command_input = cmd_part.to_string();
                // If the command takes an argument, add a space
                if suggestion.contains('<') {
                    app.command_input.push(' ');
                    app.autocomplete_suggestions.clear();
                } else {
                    app.autocomplete_suggestions.clear();
                    app.execute_command();
                }
            } else {
                app.execute_command();
            }
        }
        KeyCode::Tab => {
            if app.command_input.is_empty() {
                app.cycle_panel_focus();
                return;
            }
            // Accept the current suggestion into the input
            if !app.autocomplete_suggestions.is_empty() {
                let suggestion = app.autocomplete_suggestions[app.autocomplete_selected];
                let cmd_part = suggestion.split(' ').next().unwrap_or(suggestion);
                app.command_input = cmd_part.to_string();
                if suggestion.contains('<') {
                    app.command_input.push(' ');
                }
                app.autocomplete_suggestions.clear();
            }
        }
        KeyCode::BackTab => {
            if app.command_input.is_empty() {
                app.cycle_panel_focus_backward();
                return;
            }
            if !app.autocomplete_suggestions.is_empty() {
                app.autocomplete_selected = if app.autocomplete_selected == 0 {
                    app.autocomplete_suggestions.len() - 1
                } else {
                    app.autocomplete_selected - 1
                };
            }
        }
        KeyCode::Up => {
            if !app.autocomplete_suggestions.is_empty() {
                app.autocomplete_selected = if app.autocomplete_selected == 0 {
                    app.autocomplete_suggestions.len() - 1
                } else {
                    app.autocomplete_selected - 1
                };
            }
        }
        KeyCode::Down => {
            if !app.autocomplete_suggestions.is_empty() {
                app.autocomplete_selected =
                    (app.autocomplete_selected + 1) % app.autocomplete_suggestions.len();
            }
        }
        KeyCode::Esc => {
            app.autocomplete_suggestions.clear();
        }
        KeyCode::Backspace => {
            app.command_input.pop();
            app.update_autocomplete();
        }
        KeyCode::Char(c) => {
            // '/' as first character switches to filter mode
            if c == '/' && app.command_input.is_empty() {
                app.open_filter_input();
                return;
            }
            app.command_input.push(c);
            app.update_autocomplete();
        }
        _ => {}
    }
}

fn handle_filter_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => {
            // Apply filter and re-fetch
            let filter = app.filter_input.trim().to_string();
            if filter.is_empty() {
                app.uri_filter = None;
            } else {
                app.uri_filter = Some(filter);
            }
            app.current_page = 0;
            app.focus_results_panel();
            app.fetch_list();
        }
        KeyCode::Esc => {
            // Cancel filter editing
            app.focus_results_panel();
        }
        KeyCode::Backspace => {
            app.filter_input.pop();
        }
        KeyCode::Char(c) => {
            app.filter_input.push(c);
        }
        _ => {}
    }
}

fn handle_results_key(app: &mut App, key: KeyEvent) {
    if key.code == KeyCode::Char('d') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if !app.records.is_empty() {
            app.delete_selected();
        }
        return;
    }
    if key.code == KeyCode::Char(':') {
        // handled globally now
        return;
    }

    // Determine which list we're navigating
    let is_query_results = !app.query_results.is_empty() && app.records.is_empty();
    let page_step = app.page_size.max(1);

    if is_query_results {
        if key.code != KeyCode::Char('g') {
            app.last_query_results_g = None;
        }

        match key.code {
            KeyCode::Char('d') => {
                if let Some(sel) = app.query_results_state.selected() {
                    let next = (sel + page_step).min(app.query_results.len().saturating_sub(1));
                    app.query_results_state.select(Some(next));
                }
                return;
            }
            KeyCode::Char('u') => {
                if let Some(sel) = app.query_results_state.selected() {
                    app.query_results_state
                        .select(Some(sel.saturating_sub(page_step)));
                }
                return;
            }
            KeyCode::Char('G') => {
                app.query_results_state
                    .select(Some(app.query_results.len().saturating_sub(1)));
                return;
            }
            KeyCode::Char('g') => {
                if let Some(last_g) = app.last_query_results_g {
                    if last_g.elapsed().as_millis() < 500 {
                        app.query_results_state.select(Some(0));
                        app.last_query_results_g = None;
                        return;
                    }
                }
                app.last_query_results_g = Some(Instant::now());
                return;
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            if is_query_results {
                if let Some(sel) = app.query_results_state.selected() {
                    if sel > 0 {
                        app.query_results_state.select(Some(sel - 1));
                    }
                }
            } else {
                if let Some(sel) = app.list_state.selected() {
                    if sel > 0 {
                        app.list_state.select(Some(sel - 1));
                    }
                }
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if is_query_results {
                if let Some(sel) = app.query_results_state.selected() {
                    if sel < app.query_results.len().saturating_sub(1) {
                        app.query_results_state.select(Some(sel + 1));
                    }
                }
            } else {
                if let Some(sel) = app.list_state.selected() {
                    if sel < app.records.len().saturating_sub(1) {
                        app.list_state.select(Some(sel + 1));
                    }
                }
            }
        }
        KeyCode::Char(' ') => {
            // Toggle selection (records only)
            if !is_query_results {
                if let Some(sel) = app.list_state.selected() {
                    if app.selected_indices.contains(&sel) {
                        app.selected_indices.retain(|&i| i != sel);
                    } else {
                        app.selected_indices.push(sel);
                    }
                }
            }
        }
        KeyCode::Enter => {
            if is_query_results {
                // Open full view of selected query result
                if let Some(sel) = app.query_results_state.selected() {
                    if let Some(result) = app.query_results.get(sel) {
                        app.active_document = None;
                        app.full_view_content = format_document_content(result);
                        app.full_view_scroll = 0;
                        app.mode = AppMode::FullScreenView;
                    }
                }
            } else {
                app.open_record();
            }
        }
        KeyCode::Tab => {
            if is_query_results && app.query_visible {
                app.focus_query_panel();
            }
        }
        KeyCode::BackTab => {
            app.focus_command_panel();
        }
        KeyCode::Char('n') => {
            if !is_query_results {
                let max_page = app
                    .total_results
                    .map(|t| t.saturating_sub(1) / app.page_size)
                    .unwrap_or(usize::MAX);
                if app.current_page < max_page {
                    app.current_page += 1;
                    app.fetch_list();
                }
            }
        }
        KeyCode::Char('p') => {
            if !is_query_results {
                if app.current_page > 0 {
                    app.current_page -= 1;
                    app.fetch_list();
                }
            }
        }
        _ => {}
    }
}

fn handle_database_select_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.mode = AppMode::Normal;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Some(sel) = app.database_list_state.selected() {
                if sel > 0 {
                    app.database_list_state.select(Some(sel - 1));
                }
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Some(sel) = app.database_list_state.selected() {
                if sel < app.database_list.len().saturating_sub(1) {
                    app.database_list_state.select(Some(sel + 1));
                }
            }
        }
        KeyCode::Enter => {
            if let Some(sel) = app.database_list_state.selected() {
                if let Some(db) = app.database_list.get(sel) {
                    let db_name = db.clone();
                    app.config.active_database = Some(db_name.clone());
                    app.config.save().ok();
                    if let Some(c) = &mut app.client {
                        c.set_database(db_name.clone());
                    }
                    app.status_message = format!("Database: {}", db_name);
                    app.mode = AppMode::Normal;
                    app.current_collection = None;
                    app.current_page = 0;
                    app.fetch_list();
                }
            }
        }
        _ => {}
    }
    Ok(false)
}

fn handle_collection_select_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.mode = AppMode::Normal;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Some(sel) = app.collection_list_state.selected() {
                if sel > 0 {
                    app.collection_list_state.select(Some(sel - 1));
                }
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Some(sel) = app.collection_list_state.selected() {
                if sel < app.collection_list.len().saturating_sub(1) {
                    app.collection_list_state.select(Some(sel + 1));
                }
            }
        }
        KeyCode::Enter => {
            if let Some(sel) = app.collection_list_state.selected() {
                if let Some(col) = app.collection_list.get(sel) {
                    app.current_collection = Some(col.clone());
                    app.current_page = 0;
                    app.mode = AppMode::Normal;
                    app.fetch_list();
                }
            }
        }
        _ => {}
    }
    Ok(false)
}

fn handle_query_file_select_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.mode = AppMode::Normal;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Some(sel) = app.query_file_list_state.selected() {
                if sel > 0 {
                    app.query_file_list_state.select(Some(sel - 1));
                }
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Some(sel) = app.query_file_list_state.selected() {
                if sel < app.query_files.len().saturating_sub(1) {
                    app.query_file_list_state.select(Some(sel + 1));
                }
            }
        }
        KeyCode::Enter => {
            if let Some(sel) = app.query_file_list_state.selected() {
                if let Some(path) = app.query_files.get(sel).cloned() {
                    app.mode = AppMode::Normal;
                    app.select_query_file(path);
                }
            }
        }
        _ => {}
    }
    Ok(false)
}

fn handle_query_file_create_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.mode = AppMode::Normal;
        }
        KeyCode::Enter => {
            app.create_new_query_file_from_input();
        }
        KeyCode::Backspace => {
            app.new_query_file_input.pop();
        }
        KeyCode::Char(c) => {
            app.new_query_file_input.push(c);
        }
        _ => {}
    }
    Ok(false)
}

fn handle_delete_confirm_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            app.confirm_delete();
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            app.mode = AppMode::Normal;
        }
        _ => {}
    }
    Ok(false)
}

fn handle_fullscreen_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.mode = AppMode::Normal;
        }
        KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.open_document_in_external_editor();
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.full_view_scroll = app.full_view_scroll.saturating_add(1);
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.full_view_scroll = app.full_view_scroll.saturating_sub(1);
        }
        KeyCode::PageDown | KeyCode::Char(' ') => {
            app.full_view_scroll = app.full_view_scroll.saturating_add(20);
        }
        KeyCode::PageUp => {
            app.full_view_scroll = app.full_view_scroll.saturating_sub(20);
        }
        _ => {}
    }
    Ok(false)
}

fn handle_server_select_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.mode = AppMode::Normal;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Some(sel) = app.server_list_state.selected() {
                if sel > 0 {
                    app.server_list_state.select(Some(sel - 1));
                }
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Some(sel) = app.server_list_state.selected() {
                if sel < app.server_list.len().saturating_sub(1) {
                    app.server_list_state.select(Some(sel + 1));
                }
            }
        }
        KeyCode::Enter => {
            if let Some(sel) = app.server_list_state.selected() {
                if let Some(server) = app.config.servers.get(sel) {
                    let name = server.name.clone();
                    app.config.active_server = Some(name.clone());
                    app.config.save().ok();
                    app.reconnect();
                    app.mode = AppMode::Normal;
                    app.status_message = format!("Switched to server: {}", name);
                }
            }
        }
        KeyCode::Char('a') => {
            app.open_server_add();
        }
        KeyCode::Char('d') => {
            if let Some(sel) = app.server_list_state.selected() {
                if let Some(server) = app.config.servers.get(sel) {
                    let name = server.name.clone();
                    app.config.remove_server(&name);
                    app.config.save().ok();
                    app.status_message = format!("Removed server: {}", name);
                    app.reconnect();
                    app.cmd_servers();
                    if app.server_list.is_empty() {
                        app.mode = AppMode::Normal;
                    }
                }
            }
        }
        _ => {}
    }
    Ok(false)
}

fn handle_server_add_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.mode = AppMode::ServerSelect;
            app.cmd_servers();
        }
        KeyCode::Enter => {
            // Submit the form
            let port: u16 = app.server_add_fields[4].parse().unwrap_or(8003);
            let server = ServerConfig {
                name: app.server_add_fields[0].clone(),
                uri: app.server_add_fields[1].clone(),
                username: app.server_add_fields[2].clone(),
                password: app.server_add_fields[3].clone(),
                port,
            };
            if server.name.is_empty() || server.uri.is_empty() {
                app.status_message = "Name and URI are required.".to_string();
            } else {
                app.status_message = format!("Server '{}' added.", app.server_add_fields[0]);
                app.config.add_server(server);
                app.config.save().ok();
                app.reconnect();
                app.cmd_servers();
            }
        }
        KeyCode::Tab => {
            // Next field
            app.server_add_step = (app.server_add_step + 1) % 5;
        }
        KeyCode::BackTab => {
            // Previous field
            app.server_add_step = if app.server_add_step == 0 {
                4
            } else {
                app.server_add_step - 1
            };
        }
        KeyCode::Backspace => {
            app.server_add_fields[app.server_add_step].pop();
        }
        KeyCode::Char(c) => {
            app.server_add_fields[app.server_add_step].push(c);
        }
        _ => {}
    }
    Ok(false)
}

fn main() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new()?;

    loop {
        if app.needs_terminal_refresh {
            terminal.clear()?;
            app.needs_terminal_refresh = false;
        }
        terminal.draw(|f| ui(f, &mut app))?;
        if handle_event(&mut app)? {
            break;
        }
        app.maybe_autosave();
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(())
}
