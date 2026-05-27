mod client;
mod config;
mod events;
mod external_editor;
mod query_file;
mod query_result_cache;
mod tracked_folder;
mod ui;

use anyhow::Result;
use clap::Parser;
use client::{MarkLogicClient, SearchResult, ServerConfig};
use config::AppConfig;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use edtui::{
    EditorEventHandler, EditorState, EditorTheme, EditorView, LineNumbers, Lines, SyntaxHighlighter,
};
#[cfg(test)]
use events::{
    handle_command_key, handle_filter_key, handle_fullscreen_key, handle_navigation_mode_key,
    handle_query_insert_transition_key, handle_query_key, handle_results_key,
    handle_return_to_start_page_key, handle_server_delete_confirm_key,
    handle_servers_interface_key, map_query_navigation_key,
};
#[cfg(test)]
use external_editor::temp_editor_path;
use query_file::{
    QueryExecutionKind, discover_query_files, display_query_path, editor_lines,
    is_supported_query_file, load_query_file, query_execution_kind, save_query_file,
    should_autosave,
};
use query_result_cache::{
    QueryResultSnapshot, load_query_result_snapshot, move_query_result_cache,
    remove_query_result_cache, save_query_result_snapshot,
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState,
        Widget, Wrap,
    },
};
use ratatui_textarea::{Input, TextArea};
use std::{
    env, fs, io,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::runtime::Runtime;
use tracked_folder::{TrackedFolderEntry, TrackedFolderStore, canonicalize_folder};
use ui::{display_folder_path, format_document_detail, resolve_folder_input};

#[derive(Parser, Debug)]
struct Args {
    /// Use an alternative inline editor. Supported value: "edtui" (Vim-inspired, with syntax highlighting).
    #[arg(long, value_name = "EDITOR")]
    inline_editor: Option<String>,
}

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
    StartPage,
    Normal,
    FullScreenView,
    Interface(AppInterface),
    ServerForm,
    ServerDeleteConfirm,
    CollectionSelect,
    DeleteConfirm,
    QueryFileSelect,
    QueryFileCreate,
    QueryFileRename,
    QueryFileDeleteConfirm,
    TrackedFolderSelect,
    TrackedFolderAdd,
    TrackedFolderDeleteConfirm,
    TrackedFolderCacheClearConfirm,
    HelpOverlay,
}

#[derive(Debug, Clone, PartialEq)]
enum AppInterface {
    Servers,
}

#[derive(Debug, Clone, PartialEq)]
enum ServerFormMode {
    Add,
    Edit,
}

#[derive(Debug, Clone, PartialEq)]
enum ServersInterfaceFocus {
    Servers,
    Databases,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TrackedFolderMenuItem {
    Header(&'static str),
    Folder(TrackedFolderEntry),
}

struct App {
    config: AppConfig,
    client: Option<MarkLogicClient>,
    focus: Focus,
    edit_mode: EditMode,
    mode: AppMode,
    previous_mode: Option<AppMode>,
    modal_origin_focus: Option<Focus>,
    interface_origin_focus: Option<Focus>,
    command_input: String,
    query_editor: TextArea<'static>,
    query_visible: bool,
    needs_terminal_refresh: bool,
    should_quit: bool,
    query_root_dir: PathBuf,
    query_result_cache_dir: PathBuf,
    query_files: Vec<PathBuf>,
    active_query_file: Option<PathBuf>,
    query_file_list_state: ListState,
    tracked_folders: TrackedFolderStore,
    tracked_folder_items: Vec<TrackedFolderMenuItem>,
    tracked_folder_list_state: ListState,
    tracked_folder_input: String,
    tracked_folder_delete_target: Option<PathBuf>,
    tracked_folder_cache_clear_target: Option<PathBuf>,
    file_list_visible: bool,
    file_delete_target: Option<PathBuf>,
    rename_file_input: String,
    rename_file_target: Option<PathBuf>,
    new_query_file_input: String,
    query_dirty: bool,
    query_last_edit: Option<Instant>,
    query_autosave_interval: Duration,
    results_text: String,
    status_message: String,
    transient_status_message: Option<String>,
    transient_status_expires_at: Option<Instant>,
    // Query results (individual parts from eval)
    query_results: Vec<String>,
    query_results_timestamp: Option<SystemTime>,
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
    last_fullscreen_g: Option<Instant>,
    fullscreen_area_height: u16,
    // Server management interface
    server_form_mode: ServerFormMode,
    server_form_step: usize,
    server_form_fields: Vec<String>,
    server_edit_target: Option<String>,
    server_delete_target: Option<String>,
    servers_interface_focus: ServersInterfaceFocus,
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
    // Edtui alternative editor
    use_edtui: bool,
    edtui_state: EditorState,
    edtui_handler: EditorEventHandler,
}

impl App {
    fn new(use_edtui: bool) -> Result<Self> {
        let config = AppConfig::load()?;
        let rt = Runtime::new()?;
        let mut tracked_folders = TrackedFolderStore::load()?;
        let query_root_dir =
            canonicalize_folder(&env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))?;
        tracked_folders.record_access(&query_root_dir)?;
        tracked_folders.save()?;
        let query_result_cache_dir = query_root_dir.join(".marklogic-tui");
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
            mode: AppMode::StartPage,
            previous_mode: None,
            modal_origin_focus: None,
            interface_origin_focus: None,
            command_input: String::new(),
            query_editor: Self::new_query_editor(Vec::new()),
            query_visible: false,
            needs_terminal_refresh: false,
            should_quit: false,
            query_root_dir,
            query_result_cache_dir,
            query_files,
            active_query_file,
            query_file_list_state: ListState::default(),
            tracked_folders,
            tracked_folder_items: Vec::new(),
            tracked_folder_list_state: ListState::default(),
            tracked_folder_input: String::new(),
            tracked_folder_delete_target: None,
            tracked_folder_cache_clear_target: None,
            file_list_visible: true,
            file_delete_target: None,
            rename_file_input: String::new(),
            rename_file_target: None,
            new_query_file_input: String::new(),
            query_dirty: false,
            query_last_edit: None,
            query_autosave_interval: Duration::from_secs(2),
            results_text: String::new(),
            status_message: String::new(),
            transient_status_message: None,
            transient_status_expires_at: None,
            query_results: Vec::new(),
            query_results_timestamp: None,
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
            last_fullscreen_g: None,
            fullscreen_area_height: 0,
            server_form_mode: ServerFormMode::Add,
            server_form_step: 0,
            server_form_fields: vec![String::new(); 5], // name, uri, user, pass, port
            server_edit_target: None,
            server_delete_target: None,
            servers_interface_focus: ServersInterfaceFocus::Servers,
            autocomplete_suggestions: Vec::new(),
            autocomplete_selected: 0,
            database_list: Vec::new(),
            database_list_state: ListState::default(),
            server_list: Vec::new(),
            server_list_state: ListState::default(),
            collection_list: Vec::new(),
            collection_list_state: ListState::default(),
            rt,
            use_edtui,
            edtui_state: EditorState::default(),
            edtui_handler: EditorEventHandler::default(),
        });

        if let Ok(ref mut app) = app_result {
            app.initialize_query_editor();
            app.rebuild_tracked_folder_items();
        }

        // Calculate initial page_size from terminal size
        if let Ok(ref mut app) = app_result {
            app.recalculate_page_size();
        }

        // Start in the centered command-first home screen.
        if let Ok(ref mut app) = app_result {
            app.enter_start_page();
        }

        app_result
    }

    const COMMANDS: &[(&str, &str)] = &[
        (
            ":servers",
            "Open the server management interface [a=add, e=edit, d=remove]",
        ),
        (":server-add", "Open server management in add mode"),
        (":databases", "List databases"),
        (":list", "List all documents (paged)"),
        (":collections", "Show collections"),
        (":list:<collection>", "List documents in collection"),
        (":clear", "Clear collection filter and reset page"),
        (":tdes", "List Template Driven Extraction templates"),
        (":query", "Show the active query file"),
        (":query-files", "Open the query file picker"),
        (":query-open", "Open the query file picker"),
        (":folders", "Open the tracked folder selector"),
        (":quit", "Quit the application"),
    ];

    fn new_query_editor(lines: Vec<String>) -> TextArea<'static> {
        let mut ta = TextArea::new(if lines.is_empty() {
            vec![String::new()]
        } else {
            lines
        });
        ta.set_block(
            Block::default().borders(Borders::ALL).title(
                "Query [Alt+Enter or F5 to run, e to edit, Ctrl+S to save, Ctrl+O to switch]",
            ),
        );
        ta
    }

    fn query_text(&self) -> String {
        if self.use_edtui {
            // Lines::iter() yields (Option<&char>, Index2) for each position.
            // Group by row index to reconstruct the text.
            let mut rows: Vec<String> = Vec::new();
            let mut cur_row = usize::MAX;
            for (opt_char, idx) in self.edtui_state.lines.iter() {
                if idx.row != cur_row {
                    rows.push(String::new());
                    cur_row = idx.row;
                }
                if let Some(c) = opt_char {
                    if let Some(last) = rows.last_mut() {
                        last.push(*c);
                    }
                }
            }
            if rows.is_empty() {
                rows.push(String::new());
            }
            rows.join("\n")
        } else {
            self.query_editor.lines().join("\n")
        }
    }

    fn initialize_query_editor(&mut self) {
        if let Some(path) = self.active_query_file.clone() {
            match self.load_query_file(path) {
                Ok(()) => {}
                Err(e) => {
                    self.query_editor = Self::new_query_editor(Vec::new());
                    if self.use_edtui {
                        self.edtui_state = EditorState::default();
                    }
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
        let normalized_input = if self.command_input.is_empty() {
            String::new()
        } else if self.command_input.starts_with(':') {
            self.command_input.clone()
        } else {
            format!(":{}", self.command_input)
        };

        if normalized_input.len() > 1 {
            self.autocomplete_suggestions = Self::COMMANDS
                .iter()
                .filter(|(cmd, _)| cmd.starts_with(&normalized_input))
                .map(|(cmd, _)| *cmd)
                .collect();
        } else if normalized_input == ":" {
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
        }
        Ok(())
    }

    fn rebuild_tracked_folder_items(&mut self) {
        let preferred_selection = self
            .selected_tracked_folder_path()
            .unwrap_or_else(|| self.query_root_dir.clone());
        let mut items = Vec::new();

        let favorites = self.tracked_folders.favorite_entries();
        if !favorites.is_empty() {
            items.push(TrackedFolderMenuItem::Header("Favorites"));
            items.extend(favorites.into_iter().map(TrackedFolderMenuItem::Folder));
        }

        let recents = self.tracked_folders.recent_entries();
        if !recents.is_empty() {
            items.push(TrackedFolderMenuItem::Header("Recent"));
            items.extend(recents.into_iter().map(TrackedFolderMenuItem::Folder));
        }

        self.tracked_folder_items = items;
        let selected = self
            .tracked_folder_item_index(&preferred_selection)
            .or_else(|| self.first_tracked_folder_item_index());
        self.tracked_folder_list_state.select(selected);
    }

    fn first_tracked_folder_item_index(&self) -> Option<usize> {
        self.tracked_folder_items
            .iter()
            .position(|item| matches!(item, TrackedFolderMenuItem::Folder(_)))
    }

    fn last_tracked_folder_item_index(&self) -> Option<usize> {
        self.tracked_folder_items
            .iter()
            .rposition(|item| matches!(item, TrackedFolderMenuItem::Folder(_)))
    }

    fn tracked_folder_item_index(&self, path: &Path) -> Option<usize> {
        let path_string = path.to_string_lossy().to_string();
        self.tracked_folder_items.iter().position(|item| {
            matches!(item, TrackedFolderMenuItem::Folder(entry) if entry.path == path_string)
        })
    }

    fn selected_tracked_folder_path(&self) -> Option<PathBuf> {
        let selected = self.tracked_folder_list_state.selected()?;
        match self.tracked_folder_items.get(selected)? {
            TrackedFolderMenuItem::Folder(entry) => Some(PathBuf::from(&entry.path)),
            TrackedFolderMenuItem::Header(_) => None,
        }
    }

    fn move_tracked_folder_selection(&mut self, delta: isize) {
        if self.tracked_folder_items.is_empty() {
            self.tracked_folder_list_state.select(None);
            return;
        }

        let len = self.tracked_folder_items.len() as isize;
        let start = self
            .tracked_folder_list_state
            .selected()
            .map(|idx| idx as isize);
        let mut index = start.unwrap_or(if delta >= 0 { -1 } else { len });

        loop {
            index += delta;
            if index < 0 || index >= len {
                break;
            }
            if matches!(
                self.tracked_folder_items.get(index as usize),
                Some(TrackedFolderMenuItem::Folder(_))
            ) {
                self.tracked_folder_list_state.select(Some(index as usize));
                return;
            }
        }
    }

    fn open_tracked_folder_picker(&mut self) {
        self.rebuild_tracked_folder_items();
        if let Some(index) = self.tracked_folder_item_index(&self.query_root_dir) {
            self.tracked_folder_list_state.select(Some(index));
        }
        self.open_modal(AppMode::TrackedFolderSelect);
    }

    fn open_tracked_folder_add(&mut self) {
        self.tracked_folder_input.clear();
        self.open_modal(AppMode::TrackedFolderAdd);
    }

    fn add_tracked_folder_from_input(&mut self) {
        let input = self.tracked_folder_input.trim().to_string();
        if input.is_empty() {
            self.status_message = "Enter a folder path.".to_string();
            return;
        }

        let candidate = resolve_folder_input(&self.query_root_dir, &input);
        match self.tracked_folders.add_folder(&candidate) {
            Ok(path) => {
                if let Err(e) = self.switch_query_root(path) {
                    self.status_message = format!("Failed to switch folder: {}", e);
                    self.mode = AppMode::TrackedFolderAdd;
                }
            }
            Err(e) => {
                self.status_message = format!("Failed to add folder: {}", e);
            }
        }
    }

    fn switch_query_root(&mut self, path: PathBuf) -> Result<()> {
        let canonical_root = canonicalize_folder(&path)?;

        if canonical_root == self.query_root_dir {
            self.tracked_folders.record_access(&canonical_root)?;
            self.tracked_folders.save()?;
            self.rebuild_tracked_folder_items();
            self.close_modal_with_focus(Focus::Query);
            self.status_message = format!(
                "Folder already active: {}",
                display_folder_path(&canonical_root)
            );
            return Ok(());
        }

        self.save_query_file_if_dirty()?;

        self.query_root_dir = canonical_root.clone();
        self.query_result_cache_dir = canonical_root.join(".marklogic-tui");
        self.active_query_file = None;
        self.query_dirty = false;
        self.query_last_edit = None;

        self.tracked_folders.record_access(&canonical_root)?;
        self.tracked_folders.save()?;

        self.refresh_query_files()?;

        if let Some(path) = self.query_files.first().cloned() {
            self.load_query_file(path)?;
            self.status_message = format!(
                "Switched query folder: {}",
                display_folder_path(&canonical_root)
            );
        } else {
            self.query_editor = Self::new_query_editor(Vec::new());
            self.active_query_file = None;
            self.clear_query_results_view();
            self.focus_query_panel();
            self.status_message = format!(
                "Switched query folder: {} (no supported query files found)",
                display_folder_path(&canonical_root)
            );
        }

        self.rebuild_tracked_folder_items();
        self.close_modal_with_focus(Focus::Query);
        Ok(())
    }

    fn switch_to_selected_tracked_folder(&mut self) {
        let Some(path) = self.selected_tracked_folder_path() else {
            self.status_message = "No folder selected.".to_string();
            return;
        };

        if let Err(e) = self.switch_query_root(path) {
            self.status_message = format!("Failed to switch folder: {}", e);
            self.mode = AppMode::TrackedFolderSelect;
        }
    }

    fn toggle_selected_tracked_folder_favorite(&mut self) {
        let Some(path) = self.selected_tracked_folder_path() else {
            self.status_message = "No folder selected.".to_string();
            return;
        };

        let Some(is_favorite) = self.tracked_folders.toggle_favorite(&path) else {
            self.status_message = "No folder selected.".to_string();
            return;
        };

        match self.tracked_folders.save() {
            Ok(()) => {
                self.rebuild_tracked_folder_items();
                self.status_message = if is_favorite {
                    format!("Added favorite folder: {}", display_folder_path(&path))
                } else {
                    format!("Removed favorite folder: {}", display_folder_path(&path))
                };
            }
            Err(e) => {
                self.status_message = format!("Failed to update favorite folder: {}", e);
            }
        }
    }

    fn start_delete_selected_tracked_folder(&mut self) {
        self.tracked_folder_delete_target = self.selected_tracked_folder_path();
        if self.tracked_folder_delete_target.is_some() {
            self.mode = AppMode::TrackedFolderDeleteConfirm;
        } else {
            self.status_message = "No folder selected.".to_string();
        }
    }

    fn confirm_delete_selected_tracked_folder(&mut self) {
        let Some(path) = self.tracked_folder_delete_target.take() else {
            self.mode = AppMode::TrackedFolderSelect;
            return;
        };

        if !self.tracked_folders.remove_folder(&path) {
            self.status_message =
                format!("Tracked folder not found: {}", display_folder_path(&path));
            self.mode = AppMode::TrackedFolderSelect;
            return;
        }

        match self.tracked_folders.save() {
            Ok(()) => {
                self.rebuild_tracked_folder_items();
                self.status_message = if path == self.query_root_dir {
                    format!(
                        "Removed tracked folder: {} (current folder stays active)",
                        display_folder_path(&path)
                    )
                } else {
                    format!("Removed tracked folder: {}", display_folder_path(&path))
                };
            }
            Err(e) => {
                self.status_message = format!("Failed to remove tracked folder: {}", e);
            }
        }

        self.mode = AppMode::TrackedFolderSelect;
    }

    fn start_clear_selected_tracked_folder_cache(&mut self) {
        self.tracked_folder_cache_clear_target = self.selected_tracked_folder_path();
        if self.tracked_folder_cache_clear_target.is_some() {
            self.mode = AppMode::TrackedFolderCacheClearConfirm;
        } else {
            self.status_message = "No folder selected.".to_string();
        }
    }

    fn confirm_clear_selected_tracked_folder_cache(&mut self) {
        let Some(path) = self.tracked_folder_cache_clear_target.take() else {
            self.mode = AppMode::TrackedFolderSelect;
            return;
        };

        let cache_dir = path.join(".marklogic-tui");
        if !cache_dir.exists() {
            self.status_message =
                format!("No cached results found for {}", display_folder_path(&path));
            self.mode = AppMode::TrackedFolderSelect;
            return;
        }

        match fs::remove_dir_all(&cache_dir) {
            Ok(()) => {
                self.status_message =
                    format!("Cleared cached results for {}", display_folder_path(&path));
            }
            Err(e) => {
                self.status_message = format!("Failed to clear cached results: {}", e);
            }
        }

        self.mode = AppMode::TrackedFolderSelect;
    }

    fn active_query_file_label(&self) -> String {
        self.active_query_file
            .as_ref()
            .map(|path| display_query_path(path, &self.query_root_dir))
            .unwrap_or_else(|| "(no query file)".to_string())
    }

    fn query_title(&self) -> String {
        let dirty = if self.query_dirty { "*" } else { "" };
        format!("Query: {}{}", self.active_query_file_label(), dirty)
    }

    fn edit_mode_label(&self) -> &'static str {
        match self.edit_mode {
            EditMode::Navigate => "NORMAL",
            EditMode::Insert => "INSERT",
        }
    }

    fn status_mode_label(&self) -> &'static str {
        match (&self.mode, &self.server_form_mode) {
            (AppMode::Interface(AppInterface::Servers), _) => "SERVERS",
            (AppMode::ServerForm, ServerFormMode::Add) => "SERVER ADD",
            (AppMode::ServerForm, ServerFormMode::Edit) => "SERVER EDIT",
            (AppMode::ServerDeleteConfirm, _) => "SERVER DELETE",
            _ => self.edit_mode_label(),
        }
    }

    fn edit_mode_style(&self) -> Style {
        match self.edit_mode {
            EditMode::Navigate => Style::default().fg(Color::White),
            EditMode::Insert => Style::default().fg(Color::White).bg(Color::Red),
        }
    }

    fn start_page_mode_style(&self) -> Style {
        Style::default()
            .fg(self.edit_mode_accent_color())
            .add_modifier(Modifier::BOLD)
    }

    fn edit_mode_accent_color(&self) -> Color {
        match self.edit_mode {
            EditMode::Navigate => Color::Cyan,
            EditMode::Insert => Color::Red,
        }
    }

    fn status_identity_spans(&self) -> Vec<Span<'static>> {
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

        let gray = Style::default().fg(Color::DarkGray);

        vec![
            Span::styled(" · ", gray),
            Span::styled(server.to_string(), gray),
            Span::styled(" · ", gray),
            Span::styled(db.to_string(), gray),
        ]
    }

    fn focus_command_panel(&mut self) {
        self.focus = Focus::Command;
        self.edit_mode = EditMode::Navigate;
    }

    fn apply_focus(&mut self, focus: Focus) {
        match focus {
            Focus::Command => self.focus_command_panel(),
            Focus::Query => self.focus_query_panel(),
            Focus::Results => self.focus_results_panel(),
            Focus::Filter => {
                self.focus = Focus::Filter;
                self.edit_mode = EditMode::Navigate;
            }
        }
    }

    fn is_modal_mode(mode: &AppMode) -> bool {
        matches!(
            mode,
            AppMode::ServerForm
                | AppMode::ServerDeleteConfirm
                | AppMode::CollectionSelect
                | AppMode::DeleteConfirm
                | AppMode::QueryFileSelect
                | AppMode::QueryFileCreate
                | AppMode::QueryFileRename
                | AppMode::QueryFileDeleteConfirm
                | AppMode::TrackedFolderSelect
                | AppMode::TrackedFolderAdd
                | AppMode::TrackedFolderDeleteConfirm
                | AppMode::TrackedFolderCacheClearConfirm
        )
    }

    fn is_interface_mode(mode: &AppMode) -> bool {
        matches!(mode, AppMode::Interface(_))
    }

    fn open_interface(&mut self, interface: AppInterface) {
        if !Self::is_interface_mode(&self.mode) {
            self.interface_origin_focus = Some(self.focus.clone());
        }
        self.modal_origin_focus = None;
        self.mode = AppMode::Interface(interface);
        self.edit_mode = EditMode::Navigate;
    }

    fn close_interface_restore_focus(&mut self) {
        let target_focus = self
            .interface_origin_focus
            .take()
            .unwrap_or_else(|| self.focus.clone());
        self.mode = AppMode::Normal;
        self.apply_focus(target_focus);
    }

    fn open_modal(&mut self, mode: AppMode) {
        if !Self::is_modal_mode(&mode) {
            self.mode = mode;
            return;
        }

        if !Self::is_modal_mode(&self.mode) {
            self.modal_origin_focus = Some(self.focus.clone());
        }

        self.mode = mode;
    }

    fn close_modal_restore_focus(&mut self) {
        let target_focus = self
            .modal_origin_focus
            .take()
            .unwrap_or_else(|| self.focus.clone());
        self.mode = AppMode::Normal;
        self.apply_focus(target_focus);
    }

    fn close_modal_with_focus(&mut self, focus: Focus) {
        self.modal_origin_focus = None;
        self.mode = AppMode::Normal;
        self.apply_focus(focus);
    }

    fn enter_start_page(&mut self) {
        self.mode = AppMode::StartPage;
        self.modal_origin_focus = None;
        self.interface_origin_focus = None;
        self.focus_command_panel();
        self.query_visible = false;
        self.autocomplete_suggestions.clear();
        self.autocomplete_selected = 0;
        self.command_input.clear();
    }

    fn focus_query_panel(&mut self) {
        self.query_visible = true;
        self.focus = Focus::Query;
        self.edit_mode = EditMode::Navigate;
        if self.query_file_list_state.selected().is_none() && !self.query_files.is_empty() {
            let selected = self
                .active_query_file
                .as_ref()
                .and_then(|active| self.query_files.iter().position(|path| path == active))
                .unwrap_or(0);
            self.query_file_list_state.select(Some(selected));
        }
    }

    fn focus_results_panel(&mut self) {
        self.focus = Focus::Results;
        self.edit_mode = EditMode::Navigate;
    }

    fn toggle_file_list(&mut self) {
        self.file_list_visible = !self.file_list_visible;
    }

    fn cycle_panel_focus(&mut self) {
        match self.focus {
            Focus::Command | Focus::Filter => self.focus_query_panel(),
            Focus::Query => self.focus_results_panel(),
            Focus::Results => self.focus_query_panel(),
        }
    }

    fn cycle_panel_focus_backward(&mut self) {
        match self.focus {
            Focus::Command | Focus::Filter => self.focus_results_panel(),
            Focus::Query => self.focus_results_panel(),
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
        if self.use_edtui {
            self.edtui_state = EditorState::new(Lines::from(contents));
        }
        self.focus_query_panel();
    }

    fn open_query_in_external_editor(&mut self) {
        let original_contents = self.query_text();
        let edit_result = external_editor::edit_text_in_external_editor(
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
        self.open_modal(AppMode::QueryFileCreate);
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
                self.close_modal_with_focus(Focus::Query);
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

    fn delete_query_file(&mut self, path: PathBuf) {
        if let Err(e) = fs::remove_file(&path) {
            self.status_message = format!("Failed to delete file: {}", e);
            return;
        }

        let cache_warning =
            remove_query_result_cache(&self.query_result_cache_dir, &self.query_root_dir, &path)
                .err()
                .map(|e| e.to_string());

        self.status_message = format!(
            "Deleted query file: {}",
            display_query_path(&path, &self.query_root_dir)
        );
        if let Some(warning) = cache_warning {
            self.status_message
                .push_str(&format!(" (cache cleanup warning: {})", warning));
        }
        if self.active_query_file.as_ref() == Some(&path) {
            self.active_query_file = None;
            self.query_editor = Self::new_query_editor(Vec::new());
            self.query_dirty = false;
            self.query_last_edit = None;
        }
        if let Err(e) = self.refresh_query_files() {
            self.status_message = format!("Deleted file but failed to refresh list: {}", e);
        }
    }

    fn start_rename_query_file(&mut self) {
        if let Some(sel) = self.query_file_list_state.selected() {
            if let Some(path) = self.query_files.get(sel).cloned() {
                let name = display_query_path(&path, &self.query_root_dir);
                self.rename_file_input = name;
                self.rename_file_target = Some(path);
                self.open_modal(AppMode::QueryFileRename);
            }
        }
    }

    fn rename_query_file(&mut self) {
        let Some(old_path) = self.rename_file_target.take() else {
            self.close_modal_with_focus(Focus::Query);
            return;
        };
        let new_name = self.rename_file_input.trim().to_string();
        if new_name.is_empty() {
            self.status_message = "Enter a file name.".to_string();
            self.close_modal_with_focus(Focus::Query);
            return;
        }

        let file_path = Path::new(&new_name);
        if file_path.components().count() != 1 {
            self.status_message =
                "Enter a file name only, not a nested or absolute path.".to_string();
            self.close_modal_with_focus(Focus::Query);
            return;
        }

        let new_path = self.query_root_dir.join(file_path);
        if new_path.exists() {
            self.status_message = format!(
                "File already exists: {}",
                display_query_path(&new_path, &self.query_root_dir)
            );
            self.close_modal_with_focus(Focus::Query);
            return;
        }

        if let Err(e) = fs::rename(&old_path, &new_path) {
            self.status_message = format!("Failed to rename file: {}", e);
            self.close_modal_with_focus(Focus::Query);
            return;
        }

        if self.active_query_file.as_ref() == Some(&old_path) {
            self.active_query_file = Some(new_path.clone());
        }

        let cache_move_warning = move_query_result_cache(
            &self.query_result_cache_dir,
            &self.query_root_dir,
            &old_path,
            &new_path,
        )
        .err()
        .map(|e| e.to_string());

        if let Err(e) = self.refresh_query_files() {
            self.status_message = format!("Renamed file but failed to refresh list: {}", e);
        } else {
            self.status_message = format!(
                "Renamed to: {}",
                display_query_path(&new_path, &self.query_root_dir)
            );
            if let Some(warning) = cache_move_warning {
                self.status_message
                    .push_str(&format!(" (cache move warning: {})", warning));
            }
        }
        self.close_modal_with_focus(Focus::Query);
    }

    fn set_query_results(&mut self, results: Vec<String>) {
        self.records.clear();
        self.list_state.select(None);
        self.selected_indices.clear();
        self.total_results = None;
        self.query_results = results;
        self.query_results_timestamp = Some(SystemTime::now());
        self.query_results_state
            .select(if self.query_results.is_empty() {
                None
            } else {
                Some(0)
            });
        self.last_query_results_g = None;
        self.results_text.clear();
    }

    fn clear_query_results_view(&mut self) {
        self.records.clear();
        self.list_state.select(None);
        self.selected_indices.clear();
        self.total_results = None;
        self.query_results.clear();
        self.query_results_timestamp = None;
        self.query_results_state.select(None);
        self.last_query_results_g = None;
        self.results_text.clear();
    }

    fn persist_query_results_for_path(&mut self, path: &Path) {
        let snapshot = QueryResultSnapshot {
            query_results: self.query_results.clone(),
            selected_index: self.query_results_state.selected(),
            created_at: self
                .query_results_timestamp
                .map(|ts| ts.duration_since(UNIX_EPOCH).unwrap().as_secs()),
        };

        if let Err(e) = save_query_result_snapshot(
            &self.query_result_cache_dir,
            &self.query_root_dir,
            path,
            &snapshot,
        ) {
            self.status_message = format!("Query executed, but failed to cache results: {}", e);
        }
    }

    fn restore_query_results_for_path(&mut self, path: &Path) -> Result<Option<usize>> {
        let snapshot =
            load_query_result_snapshot(&self.query_result_cache_dir, &self.query_root_dir, path)?;

        if let Some(snapshot) = snapshot {
            self.records.clear();
            self.list_state.select(None);
            self.selected_indices.clear();
            self.total_results = None;
            self.query_results = snapshot.query_results;
            self.query_results_timestamp = snapshot
                .created_at
                .map(|secs| UNIX_EPOCH + Duration::from_secs(secs));
            let selected = snapshot
                .selected_index
                .filter(|idx| *idx < self.query_results.len())
                .or_else(|| {
                    if self.query_results.is_empty() {
                        None
                    } else {
                        Some(0)
                    }
                });
            self.query_results_state.select(selected);
            self.last_query_results_g = None;
            self.results_text.clear();
            Ok(Some(self.query_results.len()))
        } else {
            self.clear_query_results_view();
            Ok(None)
        }
    }

    fn set_active_document(&mut self, detail: client::DocumentDetail) {
        self.full_view_content = format_document_detail(&detail);
        self.active_document = Some(detail);
        self.full_view_scroll = 0;
        self.mode = AppMode::FullScreenView;
    }

    fn open_document_in_external_editor(&mut self) {
        let (original_content, path, is_document) = if let Some(ref detail) = self.active_document {
            (
                detail.content.clone(),
                Some(Path::new(detail.uri.as_str())),
                true,
            )
        } else {
            (self.full_view_content.clone(), None, false)
        };

        let label = if is_document { "document" } else { "content" };
        let edit_result =
            external_editor::edit_text_in_external_editor(&original_content, path, label);
        self.needs_terminal_refresh = true;

        match edit_result {
            Ok(edited_contents) => {
                if edited_contents == original_content {
                    let msg = if is_document {
                        format!(
                            "No document changes to save for {}",
                            self.active_document.as_ref().unwrap().uri
                        )
                    } else {
                        "No changes to save.".to_string()
                    };
                    self.status_message = msg;
                    return;
                }

                if is_document {
                    let Some(client) = &self.client else {
                        self.status_message = "No server connected.".to_string();
                        return;
                    };

                    let client = client.clone();
                    let uri = self.active_document.as_ref().unwrap().uri.clone();
                    match self
                        .rt
                        .block_on(client.update_document(&uri, &edited_contents))
                    {
                        Ok(()) => {
                            let mut updated_detail = self.active_document.clone().unwrap();
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
                } else {
                    self.full_view_content = edited_contents;
                    self.status_message = "Content updated.".to_string();
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
        if self.use_edtui {
            self.edtui_state = EditorState::new(Lines::from(contents.as_str()));
        }
        self.active_query_file = Some(path.clone());
        let restored_results = self.restore_query_results_for_path(&path);
        self.query_dirty = false;
        self.query_last_edit = None;
        self.focus_query_panel();
        self.status_message = match restored_results {
            Ok(Some(_count)) => String::new(),
            Ok(None) => String::new(),
            Err(e) => {
                self.clear_query_results_view();
                format!(
                    "Loaded query file: {} (failed to restore cached results: {})",
                    display_query_path(&path, &self.query_root_dir),
                    e
                )
            }
        };
        Ok(())
    }

    fn save_query_file_if_dirty(&mut self) -> Result<()> {
        if !self.query_dirty {
            return Ok(());
        }

        let Some(path) = self.active_query_file.clone() else {
            return Ok(());
        };

        let contents = self.query_text();
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

    fn set_transient_status_message(&mut self, message: String, duration: Duration) {
        self.status_message = message.clone();
        self.transient_status_message = Some(message);
        self.transient_status_expires_at = Some(Instant::now() + duration);
    }

    fn clear_expired_status_message(&mut self) {
        let Some(expires_at) = self.transient_status_expires_at else {
            return;
        };

        if Instant::now() < expires_at {
            return;
        }

        if self
            .transient_status_message
            .as_ref()
            .is_some_and(|message| message == &self.status_message)
        {
            self.status_message.clear();
        }
        self.transient_status_message = None;
        self.transient_status_expires_at = None;
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
                    self.open_modal(AppMode::QueryFileSelect);
                }
            }
            Err(e) => {
                self.status_message = e.to_string();
            }
        }
    }

    fn status_line(&self) -> Line<'static> {
        let mode_span = Span::styled(self.status_mode_label(), self.edit_mode_style());
        let mut spans = vec![Span::raw("  "), mode_span];
        spans.extend(self.status_identity_spans());
        Line::from(spans)
    }

    fn execute_command(&mut self) {
        let previous_mode = self.mode.clone();
        let mut cmd = self.command_input.trim().to_string();
        self.command_input.clear();

        if cmd.is_empty() {
            return;
        }

        if self.mode == AppMode::StartPage {
            self.mode = AppMode::Normal;
        }

        if !cmd.starts_with(':') && self.focus == Focus::Command {
            cmd = format!(":{}", cmd);
        }

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
            "folders" => self.open_tracked_folder_picker(),
            "quit" => {
                if let Err(e) = self.save_query_file_if_dirty() {
                    self.results_text = format!("Save error: {}", e);
                    return;
                }
                self.should_quit = true;
            }
            _ => {
                // Check if it's list:collection-name
                if command.starts_with("list:") {
                    let col = &command[5..];
                    self.current_collection = Some(col.to_string());
                    self.current_page = 0;
                    self.fetch_list();
                } else {
                    self.mode = previous_mode;
                    self.set_transient_status_message(
                        format!("Unknown command: :{}", command),
                        Duration::from_secs(5),
                    );
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
        self.open_servers_interface();
    }

    fn rebuild_server_list(&mut self) {
        let preferred = self
            .selected_server_name()
            .or_else(|| self.config.active_server.clone());

        self.server_list = self
            .config
            .servers
            .iter()
            .map(|s| {
                let active = if self.config.active_server.as_deref() == Some(&s.name) {
                    " (active)"
                } else {
                    ""
                };
                format!("{}{} - {}:{}", s.name, active, s.uri, s.port)
            })
            .collect();

        let selected = preferred
            .as_ref()
            .and_then(|name| self.config.servers.iter().position(|s| &s.name == name))
            .or_else(|| {
                if self.config.servers.is_empty() {
                    None
                } else {
                    Some(0)
                }
            });
        self.server_list_state.select(selected);
    }

    fn selected_server_name(&self) -> Option<String> {
        let selected = self.server_list_state.selected()?;
        self.config.servers.get(selected).map(|s| s.name.clone())
    }

    fn move_server_selection(&mut self, delta: isize) {
        if self.config.servers.is_empty() {
            self.server_list_state.select(None);
            return;
        }

        let len = self.config.servers.len() as isize;
        let current = self
            .server_list_state
            .selected()
            .map(|idx| idx as isize)
            .unwrap_or(if delta >= 0 { -1 } else { len });
        let next = (current + delta).clamp(0, len - 1) as usize;
        self.server_list_state.select(Some(next));
    }

    fn open_servers_interface(&mut self) {
        self.rebuild_server_list();
        self.refresh_database_list_for_interface();
        self.servers_interface_focus = ServersInterfaceFocus::Servers;
        self.open_interface(AppInterface::Servers);
        if self.config.servers.is_empty() {
            self.status_message = "No servers configured. Press 'a' to add one.".to_string();
        }
    }

    fn refresh_database_list_for_interface(&mut self) {
        if let Some(client) = &self.client {
            let client = client.clone();
            match self.rt.block_on(client.list_databases()) {
                Ok(dbs) => {
                    self.database_list = dbs;
                    let selected = self
                        .config
                        .active_database
                        .as_ref()
                        .and_then(|active| self.database_list.iter().position(|d| d == active))
                        .or_else(|| {
                            if self.database_list.is_empty() {
                                None
                            } else {
                                Some(0)
                            }
                        });
                    self.database_list_state.select(selected);
                }
                Err(e) => {
                    self.database_list.clear();
                    self.database_list_state.select(None);
                    self.status_message = format!("Error listing databases: {}", e);
                }
            }
        } else {
            self.database_list.clear();
            self.database_list_state.select(None);
        }
    }

    fn move_database_selection(&mut self, delta: isize) {
        if self.database_list.is_empty() {
            self.database_list_state.select(None);
            return;
        }

        let len = self.database_list.len() as isize;
        let current = self
            .database_list_state
            .selected()
            .map(|idx| idx as isize)
            .unwrap_or(if delta >= 0 { -1 } else { len });
        let next = (current + delta).clamp(0, len - 1) as usize;
        self.database_list_state.select(Some(next));
    }

    fn activate_selected_database(&mut self) {
        let Some(selected) = self.database_list_state.selected() else {
            self.status_message = "No database selected.".to_string();
            return;
        };
        let Some(db_name) = self.database_list.get(selected).cloned() else {
            self.status_message = "No database selected.".to_string();
            return;
        };

        self.config.active_database = Some(db_name.clone());
        match self.config.save() {
            Ok(()) => {
                if let Some(c) = &mut self.client {
                    c.set_database(db_name.clone());
                }
                self.current_collection = None;
                self.current_page = 0;
                self.status_message = format!("Database: {}", db_name);
            }
            Err(e) => {
                self.status_message = format!("Failed to save active database: {}", e);
            }
        }
    }

    fn open_server_add(&mut self) {
        if self.mode != AppMode::Interface(AppInterface::Servers) {
            self.open_servers_interface();
        }
        self.server_form_mode = ServerFormMode::Add;
        self.server_form_step = 0;
        self.server_form_fields = vec![String::new(); 5];
        self.server_edit_target = None;
        self.mode = AppMode::ServerForm;
    }

    fn open_server_edit(&mut self) {
        let Some(selected) = self.server_list_state.selected() else {
            self.status_message = "No server selected.".to_string();
            return;
        };
        let Some(server) = self.config.servers.get(selected) else {
            self.status_message = "No server selected.".to_string();
            return;
        };

        self.server_form_mode = ServerFormMode::Edit;
        self.server_form_step = 0;
        self.server_form_fields = vec![
            server.name.clone(),
            server.uri.clone(),
            server.username.clone(),
            server.password.clone(),
            server.port.to_string(),
        ];
        self.server_edit_target = Some(server.name.clone());
        self.mode = AppMode::ServerForm;
    }

    fn submit_server_form(&mut self) {
        let name = self.server_form_fields[0].trim().to_string();
        let uri = self.server_form_fields[1].trim().to_string();
        let username = self.server_form_fields[2].trim().to_string();
        let password = self.server_form_fields[3].clone();
        let port_input = self.server_form_fields[4].trim();

        if name.is_empty() || uri.is_empty() {
            self.status_message = "Name and URI are required.".to_string();
            return;
        }

        let port = if port_input.is_empty() {
            8003
        } else {
            match port_input.parse::<u16>() {
                Ok(port) => port,
                Err(_) => {
                    self.status_message = "Port must be a number from 0 to 65535.".to_string();
                    return;
                }
            }
        };

        let old_name = self.server_edit_target.clone();
        let was_edit = self.server_form_mode == ServerFormMode::Edit;
        let active_before = self.config.active_server.clone();
        let was_active = old_name
            .as_deref()
            .and_then(|target| {
                self.config
                    .active_server
                    .as_deref()
                    .map(|active| active == target)
            })
            .unwrap_or(false);

        if let Some(old_name) = old_name.as_deref() {
            if old_name != name {
                self.config.remove_server(old_name);
            }
        }

        let server = ServerConfig {
            name: name.clone(),
            uri,
            username,
            password,
            port,
        };
        self.config.add_server(server);
        if was_edit && was_active {
            self.config.active_server = Some(name.clone());
        } else if was_edit {
            self.config.active_server = active_before;
        }

        match self.config.save() {
            Ok(()) => {
                self.reconnect();
                self.rebuild_server_list();
                if let Some(index) = self.config.servers.iter().position(|s| s.name == name) {
                    self.server_list_state.select(Some(index));
                }
                self.server_edit_target = None;
                self.mode = AppMode::Interface(AppInterface::Servers);
                self.status_message = if was_edit {
                    format!("Updated server: {}", name)
                } else {
                    format!("Added server: {}", name)
                };
            }
            Err(e) => {
                self.status_message = format!("Failed to save server config: {}", e);
            }
        }
    }

    fn activate_selected_server(&mut self) {
        let Some(name) = self.selected_server_name() else {
            self.status_message = "No server selected.".to_string();
            return;
        };

        self.config.active_server = Some(name.clone());
        match self.config.save() {
            Ok(()) => {
                self.reconnect();
                self.current_collection = None;
                self.current_page = 0;
                self.rebuild_server_list();
                self.refresh_database_list_for_interface();
                self.status_message = format!("Switched to server: {}", name);
            }
            Err(e) => {
                self.status_message = format!("Failed to save active server: {}", e);
            }
        }
    }

    fn start_delete_selected_server(&mut self) {
        self.server_delete_target = self.selected_server_name();
        if self.server_delete_target.is_some() {
            self.mode = AppMode::ServerDeleteConfirm;
        } else {
            self.status_message = "No server selected.".to_string();
        }
    }

    fn confirm_delete_selected_server(&mut self) {
        let Some(name) = self.server_delete_target.take() else {
            self.mode = AppMode::Interface(AppInterface::Servers);
            return;
        };

        self.config.remove_server(&name);
        match self.config.save() {
            Ok(()) => {
                self.reconnect();
                self.rebuild_server_list();
                self.status_message = format!("Removed server: {}", name);
            }
            Err(e) => {
                self.status_message = format!("Failed to remove server: {}", e);
            }
        }
        self.mode = AppMode::Interface(AppInterface::Servers);
    }

    fn cmd_databases(&mut self) {
        self.open_servers_interface();
        self.servers_interface_focus = ServersInterfaceFocus::Databases;
        if self.client.is_none() {
            self.status_message = "No server connected. Select or add a server first.".to_string();
        } else if self.database_list.is_empty() && self.status_message.is_empty() {
            self.status_message = "No databases returned for the active server.".to_string();
        }
    }

    fn cmd_collections(&mut self) {
        self.focus_results_panel();

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
                    self.open_modal(AppMode::CollectionSelect);
                }
                Err(e) => {
                    self.clear_query_results_view();
                    self.results_text = format!("Error listing collections: {}", e);
                }
            }
        } else {
            self.clear_query_results_view();
            self.results_text = "No server connected.".to_string();
        }
    }

    fn cmd_tdes(&mut self) {
        self.focus_results_panel();

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
                        self.clear_query_results_view();
                        self.results_text = "No TDEs found.".to_string();
                    } else {
                        self.set_query_results(uris);
                        self.status_message = format!("{} TDE(s) found", self.query_results.len());
                    }
                }
                Err(e) => {
                    self.clear_query_results_view();
                    self.results_text = format!("Error listing TDEs: {}", e);
                }
            }
        } else {
            self.clear_query_results_view();
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

        // Enter document-list mode for the results pane and drop any stale query output.
        self.records.clear();
        self.list_state.select(None);
        self.selected_indices.clear();
        self.total_results = None;
        self.query_results.clear();
        self.query_results_timestamp = None;
        self.query_results_state.select(None);
        self.last_query_results_g = None;

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
                    self.focus_results_panel();
                    self.results_text = if self.records.is_empty() {
                        "No documents found.".to_string()
                    } else {
                        String::new()
                    };
                    self.status_message = String::new();
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
            let query = self.query_text();
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
                    self.persist_query_results_for_path(&path);
                }
                Err(e) => {
                    self.query_results.clear();
                    self.query_results_state.select(None);
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
        self.open_modal(AppMode::DeleteConfirm);
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
        self.close_modal_with_focus(Focus::Results);
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

#[cfg(test)]
mod tests {
    use super::{
        App, AppInterface, AppMode, EditMode, Focus, ServerFormMode, ServersInterfaceFocus,
        TrackedFolderEntry, TrackedFolderMenuItem, display_folder_path, handle_command_key,
        handle_filter_key, handle_fullscreen_key, handle_navigation_mode_key,
        handle_query_insert_transition_key, handle_query_key, handle_results_key,
        handle_return_to_start_page_key, handle_server_delete_confirm_key,
        handle_servers_interface_key, map_query_navigation_key, resolve_folder_input,
        temp_editor_path,
    };
    use crate::client::ServerConfig;
    use crate::config::AppConfig;
    use crate::tracked_folder::TrackedFolderStore;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use edtui::{EditorEventHandler, EditorState};
    use ratatui::style::{Color, Style};
    use ratatui::widgets::{ListState, TableState};
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{Duration, SystemTime},
    };
    use tokio::runtime::Runtime;

    fn test_app() -> App {
        App {
            config: AppConfig::default(),
            client: None,
            focus: Focus::Results,
            edit_mode: EditMode::Navigate,
            mode: AppMode::Normal,
            previous_mode: None,
            modal_origin_focus: None,
            interface_origin_focus: None,
            command_input: String::new(),
            query_editor: App::new_query_editor(Vec::new()),
            query_visible: false,
            needs_terminal_refresh: false,
            should_quit: false,
            query_root_dir: PathBuf::from("."),
            query_result_cache_dir: PathBuf::from(".marklogic-tui"),
            query_files: Vec::new(),
            active_query_file: None,
            query_file_list_state: ListState::default(),
            tracked_folders: TrackedFolderStore::default(),
            tracked_folder_items: Vec::new(),
            tracked_folder_list_state: ListState::default(),
            tracked_folder_input: String::new(),
            tracked_folder_delete_target: None,
            tracked_folder_cache_clear_target: None,
            file_list_visible: true,
            file_delete_target: None,
            rename_file_input: String::new(),
            rename_file_target: None,
            new_query_file_input: String::new(),
            query_dirty: false,
            query_last_edit: None,
            query_autosave_interval: Duration::from_secs(2),
            results_text: String::new(),
            status_message: String::new(),
            transient_status_message: None,
            transient_status_expires_at: None,
            query_results: Vec::new(),
            query_results_timestamp: None,
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
            server_form_mode: ServerFormMode::Add,
            server_form_step: 0,
            server_form_fields: vec![String::new(); 5],
            server_edit_target: None,
            server_delete_target: None,
            servers_interface_focus: ServersInterfaceFocus::Servers,
            autocomplete_suggestions: Vec::new(),
            autocomplete_selected: 0,
            database_list: Vec::new(),
            database_list_state: ListState::default(),
            server_list: Vec::new(),
            server_list_state: ListState::default(),
            collection_list: Vec::new(),
            collection_list_state: ListState::default(),
            fullscreen_area_height: 0,
            last_fullscreen_g: None,
            rt: Runtime::new().unwrap(),
            use_edtui: false,
            edtui_state: EditorState::default(),
            edtui_handler: EditorEventHandler::default(),
        }
    }

    fn temp_test_dir(name: &str) -> PathBuf {
        let unique = format!(
            "marklogic-tui-main-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::env::temp_dir().join(unique)
    }

    fn test_server(name: &str, uri: &str) -> ServerConfig {
        ServerConfig {
            name: name.to_string(),
            uri: uri.to_string(),
            username: "admin".to_string(),
            password: "admin".to_string(),
            port: 8003,
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
        assert_eq!(app.focus, Focus::Query);
        assert_eq!(app.edit_mode, EditMode::Navigate);
        assert!(app.query_visible);

        assert!(handle_navigation_mode_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE)
        ));
        assert_eq!(app.focus, Focus::Results);
        assert_eq!(app.edit_mode, EditMode::Navigate);

        assert!(handle_navigation_mode_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE)
        ));
        assert_eq!(app.focus, Focus::Command);
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
    fn modal_input_command_escape_closes_and_focuses_next_panel() {
        let mut app = test_app();
        app.focus = Focus::Command;
        app.command_input = ":q".to_string();
        app.update_autocomplete();

        handle_command_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert_eq!(app.focus, Focus::Query);
        assert!(app.command_input.is_empty());
        assert!(app.autocomplete_suggestions.is_empty());
    }

    #[test]
    fn start_page_escape_keeps_command_focus() {
        let mut app = test_app();
        app.mode = AppMode::StartPage;
        app.focus = Focus::Command;
        app.command_input = ":q".to_string();
        app.update_autocomplete();

        handle_command_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert_eq!(app.mode, AppMode::StartPage);
        assert_eq!(app.focus, Focus::Command);
        assert!(app.command_input.is_empty());
        assert!(app.autocomplete_suggestions.is_empty());
    }

    #[test]
    fn start_page_execute_command_switches_to_normal_mode() {
        let mut app = test_app();
        app.mode = AppMode::StartPage;
        app.focus = Focus::Command;
        app.command_input = ":clear".to_string();

        app.execute_command();

        assert_eq!(app.mode, AppMode::Normal);
    }

    #[test]
    fn unknown_command_sets_transient_status_without_touching_results() {
        let mut app = test_app();
        app.mode = AppMode::Normal;
        app.focus_results_panel();
        app.results_text = "existing results".to_string();
        app.command_input = ":bogus".to_string();

        app.execute_command();

        assert_eq!(app.mode, AppMode::Normal);
        assert_eq!(app.results_text, "existing results");
        assert_eq!(app.status_message, "Unknown command: :bogus");
        assert_eq!(
            app.transient_status_message.as_deref(),
            Some("Unknown command: :bogus")
        );
        assert!(app.transient_status_expires_at.is_some());
    }

    #[test]
    fn unknown_command_from_start_page_stays_on_start_page() {
        let mut app = test_app();
        app.mode = AppMode::StartPage;
        app.focus_command_panel();
        app.command_input = ":bogus".to_string();

        app.execute_command();

        assert_eq!(app.mode, AppMode::StartPage);
        assert_eq!(app.status_message, "Unknown command: :bogus");
    }

    #[test]
    fn transient_status_clears_after_expiry() {
        let mut app = test_app();
        app.set_transient_status_message("Unknown command: :bogus".to_string(), Duration::ZERO);

        app.clear_expired_status_message();

        assert!(app.status_message.is_empty());
        assert!(app.transient_status_message.is_none());
        assert!(app.transient_status_expires_at.is_none());
    }

    #[test]
    fn quit_command_sets_quit_flag() {
        let mut app = test_app();
        app.command_input = ":quit".to_string();

        app.execute_command();

        assert!(app.should_quit);
    }

    #[test]
    fn quit_command_leaves_start_page_and_sets_quit_flag() {
        let mut app = test_app();
        app.mode = AppMode::StartPage;
        app.focus = Focus::Command;
        app.command_input = ":quit".to_string();

        app.execute_command();

        assert_eq!(app.mode, AppMode::Normal);
        assert!(app.should_quit);
    }

    #[test]
    fn enter_start_page_resets_focus_and_command_state() {
        let mut app = test_app();
        app.mode = AppMode::Normal;
        app.focus = Focus::Results;
        app.query_visible = true;
        app.command_input = ":list".to_string();
        app.autocomplete_suggestions = vec![":list"];
        app.autocomplete_selected = 0;

        app.enter_start_page();

        assert_eq!(app.mode, AppMode::StartPage);
        assert_eq!(app.focus, Focus::Command);
        assert!(!app.query_visible);
        assert!(app.command_input.is_empty());
        assert!(app.autocomplete_suggestions.is_empty());
        assert_eq!(app.autocomplete_selected, 0);
    }

    #[test]
    fn q_in_navigation_mode_returns_to_start_page() {
        let mut app = test_app();
        app.mode = AppMode::Normal;
        app.focus_results_panel();
        app.query_visible = true;

        assert!(
            handle_return_to_start_page_key(
                &mut app,
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)
            )
            .unwrap()
        );

        assert_eq!(app.mode, AppMode::StartPage);
        assert_eq!(app.focus, Focus::Command);
        assert!(!app.query_visible);
    }

    #[test]
    fn q_in_insert_mode_does_not_return_to_start_page() {
        let mut app = test_app();
        app.mode = AppMode::Normal;
        app.focus_query_panel();
        app.edit_mode = EditMode::Insert;

        assert!(
            !handle_return_to_start_page_key(
                &mut app,
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)
            )
            .unwrap()
        );

        assert_eq!(app.mode, AppMode::Normal);
        assert_eq!(app.edit_mode, EditMode::Insert);
    }

    #[test]
    fn q_in_command_input_remains_text_input() {
        let mut app = test_app();
        app.mode = AppMode::Normal;
        app.focus_command_panel();

        assert!(
            !handle_return_to_start_page_key(
                &mut app,
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)
            )
            .unwrap()
        );

        assert_eq!(app.mode, AppMode::Normal);
        assert_eq!(app.focus, Focus::Command);
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
        assert_eq!(app.focus, Focus::Query);
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
        // query_title returns the file name with optional dirty marker
        assert!(app.query_title().starts_with("Query: "));

        app.active_query_file = Some(PathBuf::from("test.xqy"));
        app.query_dirty = true;
        assert_eq!(app.query_title(), "Query: test.xqy*");
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
    fn resolve_folder_input_supports_relative_and_home_paths() {
        let current_root = Path::new("/tmp/project");

        assert_eq!(
            resolve_folder_input(current_root, "nested"),
            Path::new("/tmp/project/nested")
        );

        if let Some(home_dir) = dirs::home_dir() {
            assert_eq!(resolve_folder_input(current_root, "~"), home_dir);
            assert_eq!(
                resolve_folder_input(current_root, "~/queries"),
                home_dir.join("queries")
            );
        }
    }

    #[test]
    fn rebuild_tracked_folder_items_groups_favorites_before_recents() {
        let mut app = test_app();
        app.query_root_dir = PathBuf::from("/tmp/current");
        app.tracked_folders = TrackedFolderStore {
            folders: vec![
                TrackedFolderEntry {
                    path: "/tmp/recent-a".to_string(),
                    favorite: false,
                    last_accessed: Some(2),
                },
                TrackedFolderEntry {
                    path: "/tmp/favorite".to_string(),
                    favorite: true,
                    last_accessed: Some(3),
                },
                TrackedFolderEntry {
                    path: "/tmp/recent-b".to_string(),
                    favorite: false,
                    last_accessed: Some(4),
                },
            ],
        };

        app.rebuild_tracked_folder_items();

        assert!(matches!(
            app.tracked_folder_items.first(),
            Some(TrackedFolderMenuItem::Header("Favorites"))
        ));
        assert!(matches!(
            app.tracked_folder_items.get(1),
            Some(TrackedFolderMenuItem::Folder(entry)) if entry.path == "/tmp/favorite"
        ));
        assert!(matches!(
            app.tracked_folder_items.get(2),
            Some(TrackedFolderMenuItem::Header("Recent"))
        ));
        assert!(matches!(
            app.tracked_folder_items.get(3),
            Some(TrackedFolderMenuItem::Folder(entry)) if entry.path == "/tmp/recent-b"
        ));
        assert!(matches!(
            app.tracked_folder_items.get(4),
            Some(TrackedFolderMenuItem::Folder(entry)) if entry.path == "/tmp/recent-a"
        ));
    }

    #[test]
    fn query_file_list_f_key_opens_tracked_folder_picker() {
        let mut app = test_app();
        app.focus_query_panel();
        app.file_list_visible = true;
        app.tracked_folders = TrackedFolderStore {
            folders: vec![TrackedFolderEntry {
                path: "/tmp/folder".to_string(),
                favorite: false,
                last_accessed: Some(1),
            }],
        };

        handle_query_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE),
        );

        assert_eq!(app.mode, AppMode::TrackedFolderSelect);
        assert!(matches!(
            app.tracked_folder_items.get(1),
            Some(TrackedFolderMenuItem::Folder(entry)) if entry.path == "/tmp/folder"
        ));
    }

    #[test]
    fn folders_command_opens_tracked_folder_picker() {
        let mut app = test_app();
        app.command_input = ":folders".to_string();
        app.tracked_folders = TrackedFolderStore {
            folders: vec![TrackedFolderEntry {
                path: "/tmp/folder".to_string(),
                favorite: false,
                last_accessed: Some(1),
            }],
        };

        app.execute_command();

        assert_eq!(app.mode, AppMode::TrackedFolderSelect);
        assert!(matches!(
            app.tracked_folder_items.get(1),
            Some(TrackedFolderMenuItem::Folder(entry)) if entry.path == "/tmp/folder"
        ));
    }

    #[test]
    fn switch_query_root_refreshes_supported_files_and_cache_dir() {
        let mut app = test_app();
        let temp_dir = temp_test_dir("switch-query-root");
        let folder_one = temp_dir.join("one");
        let folder_two = temp_dir.join("two");
        fs::create_dir_all(&folder_one).unwrap();
        fs::create_dir_all(&folder_two).unwrap();
        fs::write(folder_two.join("query-2.xqy"), "xquery version \"1.0-ml\";").unwrap();
        app.query_root_dir = folder_one.clone();
        app.tracked_folders = TrackedFolderStore::default();
        let canonical_folder_two = fs::canonicalize(&folder_two).unwrap();

        app.switch_query_root(folder_two.clone()).unwrap();

        assert_eq!(app.query_root_dir, canonical_folder_two);
        assert_eq!(
            app.query_result_cache_dir,
            app.query_root_dir.join(".marklogic-tui")
        );
        assert_eq!(
            app.query_files,
            vec![app.query_root_dir.join("query-2.xqy")]
        );
        assert_eq!(
            app.active_query_file,
            Some(app.query_root_dir.join("query-2.xqy"))
        );
        assert!(
            app.tracked_folders
                .folders
                .iter()
                .any(|entry| entry.path == display_folder_path(&app.query_root_dir))
        );

        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn clear_selected_tracked_folder_cache_removes_marklogic_tui_dir() {
        let mut app = test_app();
        let temp_dir = temp_test_dir("clear-folder-cache");
        let folder = temp_dir.join("queries");
        let cache_dir = folder.join(".marklogic-tui");
        fs::create_dir_all(&cache_dir).unwrap();
        fs::write(cache_dir.join("results-v1.json"), "{}").unwrap();
        app.tracked_folder_cache_clear_target = Some(folder.clone());

        app.confirm_clear_selected_tracked_folder_cache();

        assert!(!cache_dir.exists());
        assert_eq!(app.mode, AppMode::TrackedFolderSelect);

        fs::remove_dir_all(temp_dir).unwrap();
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
    fn query_file_switch_restores_cached_results_per_file() {
        let mut app = test_app();
        let temp_dir = temp_test_dir("query-cache-switch");
        fs::create_dir_all(&temp_dir).unwrap();

        let query_one = temp_dir.join("query-1.xqy");
        let query_two = temp_dir.join("query-2.xqy");
        fs::write(&query_one, "xquery version \"1.0-ml\";").unwrap();
        fs::write(&query_two, "xquery version \"1.0-ml\";").unwrap();

        app.query_root_dir = temp_dir.clone();
        app.query_result_cache_dir = temp_dir.join(".marklogic-tui");
        app.query_files = vec![query_one.clone(), query_two.clone()];
        app.query_file_list_state.select(Some(0));

        app.set_query_results(vec!["result-a".to_string()]);
        app.persist_query_results_for_path(&query_one);

        app.set_query_results(vec!["result-b1".to_string(), "result-b2".to_string()]);
        app.query_results_state.select(Some(1));
        app.persist_query_results_for_path(&query_two);

        app.load_query_file(query_one.clone()).unwrap();
        assert_eq!(app.query_results, vec!["result-a".to_string()]);
        assert_eq!(app.query_results_state.selected(), Some(0));

        app.load_query_file(query_two.clone()).unwrap();
        assert_eq!(
            app.query_results,
            vec!["result-b1".to_string(), "result-b2".to_string()]
        );
        assert_eq!(app.query_results_state.selected(), Some(1));

        app.load_query_file(query_one).unwrap();
        assert_eq!(app.query_results, vec!["result-a".to_string()]);
        assert_eq!(app.query_results_state.selected(), Some(0));

        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn query_result_cache_persists_timestamp() {
        let mut app = test_app();
        let temp_dir = temp_test_dir("query-cache-timestamp");
        fs::create_dir_all(&temp_dir).unwrap();

        let query_path = temp_dir.join("query-1.xqy");
        fs::write(&query_path, "xquery version \"1.0-ml\";").unwrap();

        app.query_root_dir = temp_dir.clone();
        app.query_result_cache_dir = temp_dir.join(".marklogic-tui");

        // Set results with a specific timestamp
        app.set_query_results(vec!["result".to_string()]);
        let before_save = app.query_results_timestamp.unwrap();
        app.persist_query_results_for_path(&query_path);

        // Clear and restore
        app.clear_query_results_view();
        assert!(app.query_results_timestamp.is_none());

        app.load_query_file(query_path).unwrap();
        assert_eq!(app.query_results, vec!["result".to_string()]);
        let after_restore = app.query_results_timestamp.unwrap();

        // Timestamp should be preserved (within 1 second tolerance)
        let diff = after_restore
            .duration_since(before_save)
            .unwrap_or_default()
            .as_secs();
        assert_eq!(
            diff, 0,
            "Timestamp should be preserved through cache round-trip"
        );

        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn load_query_file_success_does_not_set_status_message() {
        let mut app = test_app();
        let temp_dir = temp_test_dir("query-load-status-message");
        fs::create_dir_all(&temp_dir).unwrap();

        let query_path = temp_dir.join("query-1.xqy");
        fs::write(&query_path, "xquery version \"1.0-ml\";").unwrap();

        app.query_root_dir = temp_dir.clone();
        app.query_result_cache_dir = temp_dir.join(".marklogic-tui");

        app.load_query_file(query_path.clone()).unwrap();
        assert!(app.status_message.is_empty());

        app.set_query_results(vec!["result".to_string()]);
        app.persist_query_results_for_path(&query_path);
        app.load_query_file(query_path).unwrap();
        assert!(app.status_message.is_empty());

        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn load_query_file_without_cached_results_clears_previous_query_results() {
        let mut app = test_app();
        let temp_dir = temp_test_dir("query-cache-miss");
        fs::create_dir_all(&temp_dir).unwrap();

        let query_one = temp_dir.join("query-1.xqy");
        let query_two = temp_dir.join("query-2.xqy");
        fs::write(&query_one, "xquery version \"1.0-ml\";").unwrap();
        fs::write(&query_two, "xquery version \"1.0-ml\";").unwrap();

        app.query_root_dir = temp_dir.clone();
        app.query_result_cache_dir = temp_dir.join(".marklogic-tui");
        app.query_files = vec![query_one.clone(), query_two.clone()];

        app.set_query_results(vec!["cached-result".to_string()]);
        app.persist_query_results_for_path(&query_one);

        app.load_query_file(query_one).unwrap();
        assert_eq!(app.query_results, vec!["cached-result".to_string()]);
        assert_eq!(app.query_results_state.selected(), Some(0));

        app.load_query_file(query_two).unwrap();
        assert!(app.query_results.is_empty());
        assert_eq!(app.query_results_state.selected(), None);

        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn fetch_list_clears_stale_query_results_before_list_mode() {
        let mut app = test_app();
        app.focus_results_panel();
        app.set_query_results(vec!["stale-query-result".to_string()]);
        app.results_text = "old query text".to_string();
        app.query_results_timestamp = Some(SystemTime::now());
        assert!(!app.query_results.is_empty());

        app.focus_results_panel();
        assert_eq!(app.focus, Focus::Results);

        app.fetch_list();

        assert!(app.query_results.is_empty());
        assert_eq!(app.query_results_state.selected(), None);
        assert_eq!(app.results_text, "No server connected.");
        assert_eq!(app.query_results_timestamp, None);
        assert_eq!(app.focus, Focus::Results);
    }

    #[test]
    fn cmd_tdes_without_server_keeps_results_focus() {
        let mut app = test_app();
        app.focus = Focus::Command;

        app.cmd_tdes();

        assert_eq!(app.focus, Focus::Results);
        assert_eq!(app.results_text, "No server connected.");
    }

    #[test]
    fn cmd_collections_without_server_keeps_results_focus() {
        let mut app = test_app();
        app.focus = Focus::Command;

        app.cmd_collections();

        assert_eq!(app.focus, Focus::Results);
        assert_eq!(app.results_text, "No server connected.");
    }

    #[test]
    fn modal_cancel_restores_original_focus() {
        let mut app = test_app();
        app.focus_query_panel();
        app.open_servers_interface();

        handle_servers_interface_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();

        assert_eq!(app.mode, AppMode::Normal);
        assert_eq!(app.focus, Focus::Query);
    }

    #[test]
    fn servers_command_opens_interface_even_when_empty() {
        let mut app = test_app();
        app.command_input = ":servers".to_string();

        app.execute_command();

        assert_eq!(app.mode, AppMode::Interface(AppInterface::Servers));
        assert_eq!(app.server_list_state.selected(), None);
        assert!(
            app.status_message.contains("No servers configured"),
            "empty state should guide the user toward adding a server"
        );
    }

    #[test]
    fn servers_interface_navigation_supports_top_and_bottom() {
        let mut app = test_app();
        app.config.servers = vec![
            test_server("one", "http://one.example"),
            test_server("two", "http://two.example"),
            test_server("three", "http://three.example"),
        ];
        app.open_servers_interface();

        handle_servers_interface_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(app.server_list_state.selected(), Some(2));

        handle_servers_interface_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(app.server_list_state.selected(), Some(0));
    }

    #[test]
    fn server_add_validation_keeps_form_open_for_missing_uri() {
        let mut app = test_app();
        app.open_server_add();
        app.server_form_fields[0] = "local".to_string();

        app.submit_server_form();

        assert_eq!(app.mode, AppMode::ServerForm);
        assert!(app.config.servers.is_empty());
        assert_eq!(app.status_message, "Name and URI are required.");
    }

    #[test]
    fn server_edit_prefills_selected_server() {
        let mut app = test_app();
        app.config.servers = vec![test_server("local", "http://localhost")];
        app.open_servers_interface();

        app.open_server_edit();

        assert_eq!(app.mode, AppMode::ServerForm);
        assert_eq!(app.server_form_mode, ServerFormMode::Edit);
        assert_eq!(app.server_edit_target.as_deref(), Some("local"));
        assert_eq!(app.server_form_fields[0], "local");
        assert_eq!(app.server_form_fields[1], "http://localhost");
        assert_eq!(app.server_form_fields[4], "8003");
    }

    #[test]
    fn server_delete_cancel_returns_to_servers_interface() {
        let mut app = test_app();
        app.config.servers = vec![test_server("local", "http://localhost")];
        app.open_servers_interface();
        app.start_delete_selected_server();

        handle_server_delete_confirm_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();

        assert_eq!(app.mode, AppMode::Interface(AppInterface::Servers));
        assert!(app.server_delete_target.is_none());
        assert_eq!(app.config.servers.len(), 1);
    }

    #[test]
    fn databases_command_opens_servers_interface_with_database_focus() {
        let mut app = test_app();
        app.command_input = ":databases".to_string();

        app.execute_command();

        assert_eq!(app.mode, AppMode::Interface(AppInterface::Servers));
        assert_eq!(
            app.servers_interface_focus,
            ServersInterfaceFocus::Databases
        );
        assert!(
            app.status_message.contains("No server connected"),
            "database command should now guide users through the Servers interface"
        );
    }

    #[test]
    fn servers_interface_tab_toggles_between_servers_and_databases() {
        let mut app = test_app();
        app.open_servers_interface();

        handle_servers_interface_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(
            app.servers_interface_focus,
            ServersInterfaceFocus::Databases
        );

        handle_servers_interface_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.servers_interface_focus, ServersInterfaceFocus::Servers);
    }

    #[test]
    fn servers_interface_database_enter_sets_active_database() {
        let mut app = test_app();
        app.mode = AppMode::Interface(AppInterface::Servers);
        app.servers_interface_focus = ServersInterfaceFocus::Databases;
        app.database_list = vec!["Documents".to_string(), "Schemas".to_string()];
        app.database_list_state.select(Some(1));

        handle_servers_interface_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        assert_eq!(app.config.active_database.as_deref(), Some("Schemas"));
        assert_eq!(app.status_message, "Database: Schemas");
    }

    #[test]
    fn server_switch_keeps_servers_interface_open() {
        let mut app = test_app();
        app.focus_command_panel();
        app.config.servers = vec![test_server("local", "http://localhost")];
        app.open_servers_interface();

        handle_servers_interface_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        assert_eq!(app.mode, AppMode::Interface(AppInterface::Servers));
        assert_eq!(app.config.active_server.as_deref(), Some("local"));
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

    #[test]
    fn status_identity_spans_excludes_collection_and_uses_gray() {
        let mut app = test_app();
        app.config.active_server = Some("dockerlocal".to_string());
        app.config.active_database = Some("Documents".to_string());
        app.current_collection = Some("/test/retention/set/r".to_string());

        let spans = app.status_identity_spans();
        assert_eq!(spans.len(), 4);

        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !text.contains("/test/retention/set/r"),
            "collection should not appear in identity spans"
        );

        let gray = Style::default().fg(Color::DarkGray);
        for span in &spans {
            assert_eq!(span.style, gray, "identity spans should be gray");
        }
    }

    #[test]
    fn status_line_excludes_collection_and_status_message() {
        let mut app = test_app();
        app.config.active_server = Some("dockerlocal".to_string());
        app.config.active_database = Some("Documents".to_string());
        app.current_collection = Some("/test/retention/set/r".to_string());
        app.status_message = "Something happened".to_string();

        let line = app.status_line();
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();

        assert!(text.contains("NORMAL"), "mode should appear");
        assert!(text.contains("dockerlocal"), "server should appear");
        assert!(text.contains("Documents"), "database should appear");
        assert!(
            !text.contains("/test/retention/set/r"),
            "collection should not appear"
        );
        assert!(
            !text.contains("Something happened"),
            "status message should not appear"
        );
    }

    #[test]
    fn status_line_stops_at_database() {
        let mut app = test_app();
        app.config.active_server = Some("srv".to_string());
        app.config.active_database = Some("db".to_string());

        let line = app.status_line();
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();

        // Expected: "  NORMAL · srv · db"
        assert!(text.starts_with("  NORMAL"));
        assert!(text.ends_with("db"));
        let parts: Vec<&str> = text.split(" · ").collect();
        assert_eq!(
            parts.len(),
            3,
            "should be exactly 3 parts: mode, server, database"
        );
    }

    #[test]
    fn fullscreen_e_triggers_editor_when_document_active() {
        let mut app = test_app();
        app.mode = AppMode::FullScreenView;
        app.active_document = Some(crate::client::DocumentDetail {
            uri: "/test/doc.xml".to_string(),
            content: "<root/>".to_string(),
            collections: vec![],
            permissions: vec![],
            quality: None,
        });

        // Ensure $EDITOR is unset so the test does not launch a real editor.
        let old_editor = std::env::var_os("EDITOR");
        unsafe { std::env::remove_var("EDITOR") };

        handle_fullscreen_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        )
        .unwrap();

        // Restore $EDITOR in case other tests rely on it.
        if let Some(val) = old_editor {
            unsafe { std::env::set_var("EDITOR", val) };
        }

        // open_document_in_external_editor always sets this flag, even on error.
        assert!(
            app.needs_terminal_refresh,
            "external editor should set refresh flag"
        );
    }

    #[test]
    fn fullscreen_e_edits_content_without_active_document() {
        let mut app = test_app();
        app.mode = AppMode::FullScreenView;
        app.active_document = None;
        app.full_view_content = "test content".to_string();

        let old_editor = std::env::var_os("EDITOR");
        unsafe { std::env::remove_var("EDITOR") };

        handle_fullscreen_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        )
        .unwrap();

        if let Some(val) = old_editor {
            unsafe { std::env::set_var("EDITOR", val) };
        }

        assert!(
            app.needs_terminal_refresh,
            "e should open editor even without active_document"
        );
    }

    #[test]
    fn fullscreen_question_opens_help_overlay() {
        let mut app = test_app();
        app.mode = AppMode::FullScreenView;

        handle_fullscreen_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
        )
        .unwrap();

        assert_eq!(app.mode, AppMode::HelpOverlay);
        assert_eq!(app.previous_mode, Some(AppMode::FullScreenView));
    }

    #[test]
    fn fullscreen_ctrl_e_no_longer_edits() {
        let mut app = test_app();
        app.mode = AppMode::FullScreenView;
        app.active_document = Some(crate::client::DocumentDetail {
            uri: "/test/doc.xml".to_string(),
            content: "<root/>".to_string(),
            collections: vec![],
            permissions: vec![],
            quality: None,
        });

        handle_fullscreen_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
        )
        .unwrap();

        assert!(
            !app.needs_terminal_refresh,
            "Ctrl+E should no longer trigger editor"
        );
    }

    #[test]
    fn query_panel_e_opens_editor_in_navigate_mode() {
        let mut app = test_app();
        app.focus = Focus::Query;
        app.edit_mode = EditMode::Navigate;
        app.query_editor = App::new_query_editor(vec!["test content".to_string()]);

        let old_editor = std::env::var_os("EDITOR");
        unsafe { std::env::remove_var("EDITOR") };

        handle_query_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        );

        if let Some(val) = old_editor {
            unsafe { std::env::set_var("EDITOR", val) };
        }

        assert!(
            app.needs_terminal_refresh,
            "e in navigate mode should open external editor"
        );
    }

    #[test]
    fn query_panel_e_does_not_open_editor_in_insert_mode() {
        let mut app = test_app();
        app.focus = Focus::Query;
        app.edit_mode = EditMode::Insert;
        app.query_editor = App::new_query_editor(vec!["test content".to_string()]);

        handle_query_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        );

        assert!(
            !app.needs_terminal_refresh,
            "e in insert mode should type letter, not open editor"
        );
        let text: String = app.query_text();
        assert!(text.contains('e'), "e should be inserted in insert mode");
    }

    #[test]
    fn query_results_fullscreen_e_edits_content() {
        let mut app = test_app();
        app.mode = AppMode::FullScreenView;
        app.active_document = None;
        app.full_view_content = "query result content".to_string();

        let old_editor = std::env::var_os("EDITOR");
        unsafe { std::env::remove_var("EDITOR") };

        handle_fullscreen_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        )
        .unwrap();

        if let Some(val) = old_editor {
            unsafe { std::env::set_var("EDITOR", val) };
        }

        assert!(
            app.needs_terminal_refresh,
            "e should open editor for query results fullscreen"
        );
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let use_edtui = args.inline_editor.as_deref() == Some("edtui");

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(use_edtui)?;

    loop {
        if app.needs_terminal_refresh {
            terminal.clear()?;
            app.needs_terminal_refresh = false;
        }
        terminal.draw(|f| ui::ui(f, &mut app))?;
        if events::handle_event(&mut app)? {
            break;
        }
        app.maybe_autosave();
        app.clear_expired_status_message();
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(())
}
