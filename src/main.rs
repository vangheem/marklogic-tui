mod client;
mod config;
mod query_file;
mod query_result_cache;
mod tracked_folder;

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
        Block, Borders, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap,
    },
};
use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::runtime::Runtime;
use tracked_folder::{TrackedFolderEntry, TrackedFolderStore, canonicalize_folder};
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
    StartPage,
    Normal,
    FullScreenView,
    ServerAdd,
    DatabaseSelect,
    ServerSelect,
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
        (":folders", "Open the tracked folder selector"),
        (":quit", "Quit the application"),
    ];

    fn new_query_editor(lines: Vec<String>) -> TextArea<'static> {
        let mut ta = TextArea::new(if lines.is_empty() {
            vec![String::new()]
        } else {
            lines
        });
        ta.set_block(Block::default().borders(Borders::ALL).title(
            "Query [Alt+Enter or F5 to run, e to edit, Ctrl+S to save, Ctrl+O to switch]",
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
            AppMode::ServerAdd
                | AppMode::DatabaseSelect
                | AppMode::ServerSelect
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
        let edit_result = edit_text_in_external_editor(&original_content, path, label);
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
                            self.status_message =
                                format!("Saved document: {}", updated_detail.uri);
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
                    self.open_modal(AppMode::QueryFileSelect);
                }
            }
            Err(e) => {
                self.status_message = e.to_string();
            }
        }
    }

    fn status_line(&self) -> Line<'static> {
        let mode_span = Span::styled(self.edit_mode_label(), self.edit_mode_style());
        let mut spans = vec![Span::raw("  "), mode_span];
        spans.extend(self.status_identity_spans());
        Line::from(spans)
    }

    fn execute_command(&mut self) {
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
            self.open_modal(AppMode::ServerSelect);
        }
    }

    fn open_server_add(&mut self) {
        self.open_modal(AppMode::ServerAdd);
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
                    self.open_modal(AppMode::DatabaseSelect);
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
        AppMode::StartPage => ui_start_page(f, app),
        AppMode::FullScreenView => ui_fullscreen(f, app),
        AppMode::ServerAdd => ui_server_add(f, app),
        AppMode::ServerSelect => ui_server_select(f, app),
        AppMode::DatabaseSelect => ui_database_select(f, app),
        AppMode::CollectionSelect => ui_collection_select(f, app),
        AppMode::DeleteConfirm => ui_delete_confirm(f, app),
        AppMode::QueryFileSelect => ui_query_file_select(f, app),
        AppMode::QueryFileCreate => ui_query_file_create(f, app),
        AppMode::QueryFileRename => ui_query_file_rename(f, app),
        AppMode::QueryFileDeleteConfirm => ui_query_file_delete_confirm(f, app),
        AppMode::TrackedFolderSelect => ui_tracked_folder_select(f, app),
        AppMode::TrackedFolderAdd => ui_tracked_folder_add(f, app),
        AppMode::TrackedFolderDeleteConfirm => ui_tracked_folder_delete_confirm(f, app),
        AppMode::TrackedFolderCacheClearConfirm => ui_tracked_folder_cache_clear_confirm(f, app),
        AppMode::HelpOverlay => ui_help(f, app),
        AppMode::Normal => ui_normal(f, app),
    }
}

fn panel_block<T>(title: T, shortcut: &'static str) -> Block<'static>
where
    T: Into<Line<'static>>,
{
    let title_line: Line<'static> = Line::from(vec![
        Span::raw(shortcut),
        Span::raw("─ "),
        title.into().spans.into_iter().next().unwrap_or_default(),
    ]);
    Block::default().borders(Borders::ALL).title(title_line)
}

fn format_relative_time(timestamp: SystemTime) -> String {
    let elapsed = SystemTime::now()
        .duration_since(timestamp)
        .unwrap_or_default();
    let secs = elapsed.as_secs();
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else if secs < 604800 {
        format!("{}d ago", secs / 86400)
    } else {
        format!("{}w ago", secs / 604800)
    }
}

fn display_folder_path(path: &Path) -> String {
    path.display().to_string()
}

fn resolve_folder_input(current_root: &Path, input: &str) -> PathBuf {
    if let Some(home_dir) = dirs::home_dir() {
        if input == "~" {
            return home_dir;
        }
        if let Some(stripped) = input.strip_prefix("~/") {
            return home_dir.join(stripped);
        }
    }

    let input_path = Path::new(input);
    if input_path.is_absolute() {
        input_path.to_path_buf()
    } else {
        current_root.join(input_path)
    }
}

fn keybinding_items(
    focus: &Focus,
    edit_mode: &EditMode,
    file_list_visible: bool,
) -> Vec<(&'static str, &'static str)> {
    let mut items = Vec::new();
    match focus {
        Focus::Command => {
            items.push(("Enter", "Execute"));
            items.push(("Tab", "Complete"));
            items.push(("Esc", "Close"));
        }
        Focus::Query => {
            if *edit_mode == EditMode::Insert {
                items.push(("Esc", "Normal"));
                items.push(("^R", "Run"));
                items.push(("^S", "Save"));
            } else {
                items.push(("i", "Insert"));
                items.push(("r", "Run"));
                items.push(("e", "Editor"));
                items.push(("n", "New"));
                if file_list_visible {
                    items.push(("f", "Folders"));
                }
            }
        }
        Focus::Results => {
            items.push(("Enter", "Open"));
            items.push(("Space", "Select"));
            items.push(("n/p", "Page"));
            items.push(("/", "Filter"));
            items.push(("^D", "Delete"));
        }
        Focus::Filter => {
            items.push(("Enter", "Apply"));
            items.push(("Esc", "Cancel"));
        }
    }
    items.push(("?", "Help"));
    items
}

fn keybindings_line(focus: &Focus, edit_mode: &EditMode, file_list_visible: bool) -> Line<'static> {
    let items = keybinding_items(focus, edit_mode, file_list_visible);
    let key_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let label_style = Style::default().fg(Color::Gray);
    let divider_style = Style::default().fg(Color::DarkGray);

    let mut spans = Vec::new();
    for (i, (key, label)) in items.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", divider_style));
        }
        spans.push(Span::styled(key, key_style));
        spans.push(Span::styled(format!(" {}", label), label_style));
    }

    Line::from(spans)
}

fn ui_start_page(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let mut prompt_width = area.width.saturating_sub(8);
    prompt_width = prompt_width.min(72);
    if prompt_width < 20 {
        prompt_width = area.width.saturating_sub(2);
    }
    if prompt_width == 0 {
        prompt_width = area.width.max(1);
    }

    let prompt_height = 4;

    let prompt_area = Rect {
        x: area.x + area.width.saturating_sub(prompt_width) / 2,
        y: area.y + area.height.saturating_sub(prompt_height) / 2,
        width: prompt_width,
        height: prompt_height,
    };

    let bottom_y = area.y + area.height.saturating_sub(1);
    let title_y = prompt_area.y.saturating_sub(2);
    let footer_y = prompt_area
        .y
        .saturating_add(prompt_area.height)
        .saturating_add(1)
        .min(bottom_y);

    let title = Paragraph::new(format!("MARKLOGIC TUI v{}", env!("CARGO_PKG_VERSION")))
        .alignment(Alignment::Center)
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    f.render_widget(
        title,
        Rect {
            x: area.x,
            y: title_y,
            width: area.width,
            height: 1,
        },
    );

    let footer = Paragraph::new("Press '?' for help on any screen")
        .alignment(Alignment::Center)
        .style(Style::default().fg(Color::DarkGray));
    f.render_widget(
        footer,
        Rect {
            x: area.x,
            y: footer_y,
            width: area.width,
            height: 1,
        },
    );

    let panel_bg = Color::Rgb(84, 84, 84);
    let command_style = Style::default().fg(Color::White).bg(panel_bg);
    f.render_widget(Block::default().style(command_style), prompt_area);
    let bar_style = Style::default()
        .fg(app.edit_mode_accent_color())
        .bg(panel_bg);

    let command_input = app
        .command_input
        .strip_prefix(':')
        .unwrap_or(app.command_input.as_str());

    let bar_glyph = "▌";
    let top_blank = Line::from(vec![Span::styled(bar_glyph, bar_style)]);
    f.render_widget(
        Paragraph::new(top_blank).style(command_style),
        Rect {
            x: prompt_area.x,
            y: prompt_area.y,
            width: prompt_area.width,
            height: 1,
        },
    );

    let command_line = Line::from(vec![
        Span::styled(bar_glyph, bar_style),
        Span::raw(" "),
        Span::raw(command_input.to_string()),
    ]);
    f.render_widget(
        Paragraph::new(command_line).style(command_style),
        Rect {
            x: prompt_area.x,
            y: prompt_area.y.saturating_add(1),
            width: prompt_area.width,
            height: 1,
        },
    );

    let middle_blank = Line::from(vec![Span::styled(bar_glyph, bar_style)]);
    f.render_widget(
        Paragraph::new(middle_blank).style(command_style),
        Rect {
            x: prompt_area.x,
            y: prompt_area.y.saturating_add(2),
            width: prompt_area.width,
            height: 1,
        },
    );

    let mut status_spans = vec![
        Span::styled(bar_glyph, bar_style),
        Span::raw(" "),
        Span::styled(app.edit_mode_label(), app.start_page_mode_style()),
    ];
    status_spans.extend(app.status_identity_spans());
    let status_line = Line::from(status_spans);
    f.render_widget(
        Paragraph::new(status_line).style(command_style),
        Rect {
            x: prompt_area.x,
            y: prompt_area.y.saturating_add(3),
            width: prompt_area.width,
            height: 1,
        },
    );

    if app.focus == Focus::Command {
        let cursor_offset =
            (command_input.chars().count() as u16 + 2).min(prompt_area.width.saturating_sub(1));
        let cursor_x = prompt_area.x + cursor_offset;
        f.set_cursor_position((cursor_x, prompt_area.y.saturating_add(1)));
    }

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

        let height = (items.len() as u16 + 2).min(12);
        let popup_y = if prompt_area
            .y
            .saturating_add(prompt_area.height)
            .saturating_add(height)
            <= bottom_y.saturating_add(1)
        {
            prompt_area.y.saturating_add(prompt_area.height)
        } else {
            prompt_area.y.saturating_sub(height)
        };
        let popup_area = Rect {
            x: prompt_area.x,
            y: popup_y,
            width: prompt_area.width,
            height,
        };
        let list =
            List::new(items).block(Block::default().borders(Borders::ALL).title("Suggestions"));
        f.render_widget(Clear, popup_area);
        f.render_widget(list, popup_area);
    }
}

fn ui_normal(f: &mut Frame, app: &mut App) {
    // Command line is hidden unless focused, filtering, or has content
    let cmd_visible = app.focus == Focus::Command
        || app.focus == Focus::Filter
        || !app.command_input.is_empty()
        || app.uri_filter.is_some()
        || !app.status_message.is_empty();
    let cmd_height = if cmd_visible { 1 } else { 0 };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),          // merged status + keybindings bar
            Constraint::Min(0),             // main area
            Constraint::Length(cmd_height), // command (at bottom, collapsible)
        ])
        .split(f.area());

    // Merged top bar: status on left, keybindings on right
    let top_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[0]);
    let status = Paragraph::new(app.status_line());
    f.render_widget(status, top_chunks[0]);
    let kb_line = keybindings_line(&app.focus, &app.edit_mode, app.file_list_visible);
    let kb_bar = Paragraph::new(kb_line).alignment(Alignment::Right);
    f.render_widget(kb_bar, top_chunks[1]);

    // Content area: query on top (if visible), results on bottom
    let main_chunks = if app.query_visible {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
            .split(chunks[1])
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0)])
            .split(chunks[1])
    };

    // Command / Filter input (at bottom)
    if cmd_visible {
        if app.focus == Focus::Filter {
            let filter_style = Style::default().fg(Color::White).bg(Color::Rgb(0, 90, 0));
            f.render_widget(Block::default().style(filter_style), chunks[2]);
            let filter_display = format!("/{}", app.filter_input);
            let filter_widget = Paragraph::new(filter_display.as_str()).style(filter_style);
            f.render_widget(filter_widget, chunks[2]);
            let cursor_x = chunks[2].x + app.filter_input.chars().count() as u16 + 1;
            let cursor_y = chunks[2].y;
            f.set_cursor_position((cursor_x, cursor_y));
        } else if !app.command_input.is_empty() || app.focus == Focus::Command {
            let command_style = Style::default().fg(Color::White).bg(Color::DarkGray);
            f.render_widget(Block::default().style(command_style), chunks[2]);

            let command_input = app
                .command_input
                .strip_prefix(':')
                .unwrap_or(app.command_input.as_str());
            let mut command_line = format!(":{}", command_input);
            if let Some(ref filter) = app.uri_filter {
                if !filter.is_empty() {
                    command_line.push_str(&format!("  [filter: {}]", filter));
                }
            }
            let command = Paragraph::new(command_line).style(command_style);
            f.render_widget(command, chunks[2]);

            // Show cursor in command input when focused
            if app.focus == Focus::Command {
                let cursor_x = chunks[2].x + command_input.chars().count() as u16 + 1;
                let cursor_y = chunks[2].y;
                f.set_cursor_position((cursor_x, cursor_y));
            }
        } else if !app.status_message.is_empty() {
            let status_style = Style::default().fg(Color::White).bg(Color::DarkGray);
            f.render_widget(Block::default().style(status_style), chunks[2]);
            let status_widget = Paragraph::new(app.status_message.as_str()).style(status_style);
            f.render_widget(status_widget, chunks[2]);
        }
    }

    let results_area = if app.query_visible {
        main_chunks[1]
    } else {
        main_chunks[0]
    };

    if app.query_visible {
        // Query panel: optionally split horizontally for file list
        let should_show_file_list = app.file_list_visible && app.edit_mode == EditMode::Navigate;
        let (editor_area, file_list_area) = if should_show_file_list {
            let hchunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(0), Constraint::Length(30)])
                .split(main_chunks[0]);
            (hchunks[0], Some(hchunks[1]))
        } else {
            (main_chunks[0], None)
        };

        // Query editor (left side, or full width if no file list)
        let border_style = if app.focus == Focus::Query {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let editor_bg = if app.edit_mode == EditMode::Insert {
            Color::Black
        } else {
            Color::Rgb(35, 35, 35)
        };
        let query_block = if should_show_file_list {
            let title_line = Line::from(vec![Span::raw("[1]─ "), Span::raw(app.query_title())]);
            Block::default()
                .borders(Borders::LEFT | Borders::TOP | Borders::BOTTOM)
                .title(title_line)
                .border_style(border_style)
                .style(Style::default().bg(editor_bg))
        } else {
            panel_block(app.query_title(), "[1]")
                .border_style(border_style)
                .style(Style::default().bg(editor_bg))
        };
        app.query_editor.set_block(query_block);
        app.query_editor.set_cursor_line_style(Style::default());
        if app.focus == Focus::Query && app.edit_mode == EditMode::Insert {
            app.query_editor
                .set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
        } else {
            app.query_editor.set_cursor_style(Style::default());
        }
        f.render_widget(&app.query_editor, editor_area);

        // File list (right side)
        if let Some(area) = file_list_area {
            let file_list_style =
                if app.focus == Focus::Query && app.edit_mode == EditMode::Navigate {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::DarkGray)
                };

            let items: Vec<ListItem> = app
                .query_files
                .iter()
                .map(|path| {
                    let is_active = app.active_query_file.as_ref() == Some(path);
                    let name = display_query_path(path, &app.query_root_dir);
                    let marker = if is_active { " *" } else { "" };
                    let text = format!("{}{}", name, marker);
                    let style = if is_active {
                        Style::default().fg(Color::Cyan)
                    } else {
                        Style::default()
                    };
                    ListItem::new(text).style(style)
                })
                .collect();

            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::TOP | Borders::RIGHT | Borders::BOTTOM)
                        .title("Files [f=folders]")
                        .border_style(file_list_style)
                        .style(Style::default().bg(Color::Black)),
                )
                .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
                .highlight_symbol("> ");
            f.render_stateful_widget(list, area, &mut app.query_file_list_state);
        }
        f.render_widget(&app.query_editor, editor_area);
    } // end if query_visible

    // Results area
    let results_style = if app.focus == Focus::Results {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
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
                } else if i % 2 == 0 {
                    Style::default().bg(Color::Rgb(30, 30, 30))
                } else {
                    Style::default()
                };
                Row::new(vec![check.to_string(), r.uri.clone(), cols]).style(style)
            })
            .collect();

        let title = "Results";
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
        let total_str = app
            .total_results
            .map(|t| t.to_string())
            .unwrap_or("?".to_string());
        let page_title = format!("Page {} | Total: {}", app.current_page + 1, total_str);
        let table = Table::new(rows, widths)
            .header(header)
            .block(
                panel_block(title, "[2]")
                    .border_style(results_style)
                    .title(Line::from(page_title).alignment(Alignment::Right)),
            )
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
                let style = if i % 2 == 0 {
                    Style::default().bg(Color::Rgb(30, 30, 30))
                } else {
                    Style::default()
                };
                Row::new(vec![num, snippet]).style(style)
            })
            .collect();

        let count = app.total_results.unwrap_or(app.query_results.len());
        let title = format!("Query Results ({})", count);
        let mut block = panel_block(title, "[2]").border_style(results_style);
        if let Some(ts) = app.query_results_timestamp {
            block = block.title(Line::from(format_relative_time(ts)).alignment(Alignment::Right));
        }
        let header = Row::new(vec!["#", "Result"]).style(
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::Cyan),
        );
        let widths = [Constraint::Length(4), Constraint::Min(0)];
        let table = Table::new(rows, widths)
            .header(header)
            .block(block)
            .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
            .row_highlight_style(Style::default().bg(Color::Blue).fg(Color::White));
        f.render_stateful_widget(table, results_area, &mut app.query_results_state);
    } else {
        let results = Paragraph::new(app.results_text.as_str())
            .block(panel_block("Results", "[2]").border_style(results_style))
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
            x: chunks[2].x,
            y: chunks[2].y.saturating_sub(height),
            width: chunks[2].width.min(70),
            height,
        };
        let list =
            List::new(items).block(Block::default().borders(Borders::ALL).title("Suggestions"));
        f.render_widget(Clear, popup_area);
        f.render_widget(list, popup_area);
    }
}

fn ui_fullscreen(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(f.area());

    // Top row: status left, keybindings right
    let top_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[0]);

    let total_lines = app.full_view_content.lines().count() as u16;
    let content_height = chunks[1].height.saturating_sub(2);
    let max_scroll = total_lines.saturating_sub(content_height);
    let pct = if max_scroll == 0 {
        100
    } else {
        ((app.full_view_scroll as f32 / max_scroll as f32) * 100.0) as u16
    };
    app.fullscreen_area_height = chunks[1].height;

    let status = Paragraph::new(app.status_line());
    f.render_widget(status, top_chunks[0]);

    let mut kb_parts = vec![
        "Close: Esc/q".to_string(),
        "j/k: scroll".to_string(),
        "d/u: page".to_string(),
        "gg/G: top/bottom".to_string(),
        "e: edit".to_string(),
        "?: help".to_string(),
    ];
    kb_parts.push(format!("{}%", pct));
    let kb_bar = Paragraph::new(kb_parts.join(" | "))
        .style(Style::default().fg(Color::Cyan))
        .alignment(Alignment::Right);
    f.render_widget(kb_bar, top_chunks[1]);

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Document View");
    let para = Paragraph::new(app.full_view_content.as_str())
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((app.full_view_scroll, 0));
    f.render_widget(para, chunks[1]);
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
                .title("Select Database"),
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
                .title("Select Collection"),
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
                .title("Select Query File"),
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
            .title("New Query File"),
    );
    f.render_widget(input, area);
    f.set_cursor_position((
        area.x + app.new_query_file_input.len() as u16 + 1,
        area.y + 1,
    ));
}

fn ui_query_file_rename(f: &mut Frame, app: &App) {
    let area = centered_rect(60, 20, f.area());
    f.render_widget(Clear, area);

    let input = Paragraph::new(app.rename_file_input.as_str())
        .block(Block::default().borders(Borders::ALL).title("Rename File"));
    f.render_widget(input, area);
    f.set_cursor_position((area.x + app.rename_file_input.len() as u16 + 1, area.y + 1));
}

fn ui_query_file_delete_confirm(f: &mut Frame, app: &App) {
    let area = centered_rect(60, 20, f.area());
    f.render_widget(Clear, area);

    let file_name = app
        .file_delete_target
        .as_ref()
        .map(|p| display_query_path(p, &app.query_root_dir))
        .unwrap_or_else(|| "(unknown)".to_string());

    let text = format!(
        "Delete query file '{}' ?\n\nPress 'y' to confirm, 'n' or Esc to cancel",
        file_name
    );
    let para = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Confirm File Delete")
                .border_style(Style::default().fg(Color::Red)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn ui_tracked_folder_select(f: &mut Frame, app: &mut App) {
    let area = centered_rect(70, 70, f.area());
    f.render_widget(Clear, area);

    let items: Vec<ListItem> = app
        .tracked_folder_items
        .iter()
        .map(|item| match item {
            TrackedFolderMenuItem::Header(title) => ListItem::new(format!("  {}", title)).style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            TrackedFolderMenuItem::Folder(entry) => {
                let path = Path::new(&entry.path);
                let active_marker = if path == app.query_root_dir {
                    " (active)"
                } else {
                    ""
                };
                let favorite_marker = if entry.favorite { " [fav]" } else { "" };
                let last_accessed = entry
                    .last_accessed
                    .map(|secs| format_relative_time(UNIX_EPOCH + Duration::from_secs(secs)))
                    .unwrap_or_else(|| "never".to_string());
                ListItem::new(format!(
                    "  {}{}{} · {}",
                    display_folder_path(path),
                    favorite_marker,
                    active_marker,
                    last_accessed
                ))
            }
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Folders [Enter=switch, a=add, f=favorite, d=remove, c=clear cache]"),
        )
        .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut app.tracked_folder_list_state);
}

fn ui_tracked_folder_add(f: &mut Frame, app: &App) {
    let area = centered_rect(70, 20, f.area());
    f.render_widget(Clear, area);

    let input = Paragraph::new(app.tracked_folder_input.as_str()).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Add Folder (~, absolute, or relative path)"),
    );
    f.render_widget(input, area);
    f.set_cursor_position((
        area.x + app.tracked_folder_input.len() as u16 + 1,
        area.y + 1,
    ));
}

fn ui_tracked_folder_delete_confirm(f: &mut Frame, app: &App) {
    let area = centered_rect(70, 20, f.area());
    f.render_widget(Clear, area);

    let folder = app
        .tracked_folder_delete_target
        .as_deref()
        .map(display_folder_path)
        .unwrap_or_else(|| "(unknown)".to_string());
    let text = format!(
        "Remove tracked folder '{}' ?\n\nThis does not delete files on disk.\n\nPress 'y' to confirm, 'n' or Esc to cancel",
        folder
    );
    let para = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Confirm Folder Remove")
                .border_style(Style::default().fg(Color::Red)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn ui_tracked_folder_cache_clear_confirm(f: &mut Frame, app: &App) {
    let area = centered_rect(70, 22, f.area());
    f.render_widget(Clear, area);

    let folder = app
        .tracked_folder_cache_clear_target
        .as_deref()
        .map(display_folder_path)
        .unwrap_or_else(|| "(unknown)".to_string());
    let text = format!(
        "Clear cached query results for '{}' ?\n\nThis deletes that folder's .marklogic-tui cache directory.\n\nPress 'y' to confirm, 'n' or Esc to cancel",
        folder
    );
    let para = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Confirm Cache Clear")
                .border_style(Style::default().fg(Color::Red)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
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
        .block(Block::default().borders(Borders::ALL).title("Servers"))
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
            Style::default().fg(Color::DarkGray)
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

fn help_text_for_focus(focus: &Focus, edit_mode: &EditMode, mode: &AppMode) -> Vec<&'static str> {
    if *mode == AppMode::FullScreenView {
        return vec![
            "",
            "  Document View",
            "    Esc / q    Close document view",
            "    e          Edit in external editor",
            "    j / k      Scroll down/up",
            "    d / u      Page down/up",
            "    g / G      Top/bottom",
            "    Space      Page down",
            "    PageUp     Page up",
            "",
            "  Press ? or Esc to close this help",
        ];
    }

    let mut lines = vec![
        "",
        "  Global",
        "    ?          Show/hide this help",
        "    :          Open command input",
        "    1          Focus query panel",
        "    2          Focus results panel",
        "    3          Focus command panel",
        "    \\          Toggle file list",
        "    Tab        Cycle focus forward",
        "    Shift+Tab  Cycle focus backward",
        "",
    ];

    match focus {
        Focus::Command => {
            lines.extend_from_slice(&[
                "  Command Panel",
                "    Enter      Execute command",
                "    Tab        Accept autocomplete / cycle panels",
                "    Up/Down    Navigate autocomplete",
                "    Esc        Close command line",
                "",
            ]);
        }
        Focus::Query => {
            if *edit_mode == EditMode::Insert {
                lines.extend_from_slice(&[
                    "  Query Panel (Insert Mode)",
                    "    Esc        Return to normal mode",
                    "    Ctrl+T     Cycle focus",
                    "    Ctrl+R     Run query",
                    "    Ctrl+S     Save query file",
                    "    Ctrl+O     Switch query file",
                    "",
                ]);
            } else {
                lines.extend_from_slice(&[
                    "  Query Panel (Normal Mode)",
                    "    i          Enter insert mode (editor)",
                    "    j/k        Navigate + auto-load file",
                    "    Enter      Load selected file",
                    "    f          Open tracked folders",
                    "    n          New query file",
                    "    m          Rename selected file",
                    "    Delete     Delete selected file",
                    "    r / F5     Run query",
                    "    e          Open in external editor",
                    "    \\          Toggle file list",
                    "    g/G        Go to top/bottom of file list",
                    "",
                ]);
            }
        }
        Focus::Results => {
            lines.extend_from_slice(&[
                "  Results Panel",
                "    Enter      Open selected document",
                "    j/k        Navigate up/down",
                "    d          Page down",
                "    u          Page up",
                "    n / p      Next/previous page",
                "    Space      Toggle selection",
                "    /          Filter results",
                "    Ctrl+D     Delete selected",
                "    g          Go to top (double-tap)",
                "    G          Go to bottom",
                "",
            ]);
        }
        Focus::Filter => {
            lines.extend_from_slice(&[
                "  Filter Panel",
                "    Enter      Apply filter",
                "    Esc        Cancel filter",
                "",
            ]);
        }
    }

    lines.push("  Press ? or Esc to close this help");
    lines.into()
}

fn ui_help(f: &mut Frame, app: &App) {
    let area = centered_rect(60, 70, f.area());
    f.render_widget(Clear, area);

    let help_lines = help_text_for_focus(&app.focus, &app.edit_mode, &app.mode);
    let text = help_lines.join("\n");

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Keybindings")
        .border_style(Style::default().fg(Color::Cyan));

    let para = Paragraph::new(text)
        .block(block)
        .style(Style::default().fg(Color::Gray));

    f.render_widget(para, area);
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
                AppMode::StartPage => {}
                AppMode::FullScreenView => return handle_fullscreen_key(app, key),
                AppMode::ServerAdd => return handle_server_add_key(app, key),
                AppMode::ServerSelect => return handle_server_select_key(app, key),
                AppMode::DatabaseSelect => return handle_database_select_key(app, key),
                AppMode::CollectionSelect => return handle_collection_select_key(app, key),
                AppMode::DeleteConfirm => return handle_delete_confirm_key(app, key),
                AppMode::QueryFileSelect => return handle_query_file_select_key(app, key),
                AppMode::QueryFileCreate => return handle_query_file_create_key(app, key),
                AppMode::QueryFileRename => return handle_query_file_rename_key(app, key),
                AppMode::QueryFileDeleteConfirm => {
                    return handle_query_file_delete_confirm_key(app, key);
                }
                AppMode::TrackedFolderSelect => return handle_tracked_folder_select_key(app, key),
                AppMode::TrackedFolderAdd => return handle_tracked_folder_add_key(app, key),
                AppMode::TrackedFolderDeleteConfirm => {
                    return handle_tracked_folder_delete_confirm_key(app, key);
                }
                AppMode::TrackedFolderCacheClearConfirm => {
                    return handle_tracked_folder_cache_clear_confirm_key(app, key);
                }
                AppMode::HelpOverlay => {
                    if key.code == KeyCode::Char('?') || key.code == KeyCode::Esc {
                        if let Some(prev) = app.previous_mode.take() {
                            app.mode = prev;
                        } else {
                            app.mode = AppMode::Normal;
                        }
                    }
                    return Ok(false);
                }
                AppMode::Normal => {}
            }

            // Global help overlay
            if key.code == KeyCode::Char('?') && key.modifiers.is_empty() {
                app.previous_mode = Some(app.mode.clone());
                app.mode = AppMode::HelpOverlay;
                return Ok(false);
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

            // Double-Esc in navigation mode: return to the centered start page
            if key.code == KeyCode::Esc {
                if app.edit_mode == EditMode::Navigate {
                    if app.mode != AppMode::StartPage {
                        if let Some(last) = app.last_esc {
                            if last.elapsed().as_millis() < 500 {
                                if let Err(e) = app.save_query_file_if_dirty() {
                                    app.status_message = format!("Save error: {}", e);
                                    return Ok(false);
                                }
                                app.last_esc = None;
                                app.enter_start_page();
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

            if app.should_quit {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn handle_navigation_mode_key(app: &mut App, key: KeyEvent) -> bool {
    // Allow Shift through for BackTab; reject all other modifier combos
    let is_shift_only = key.modifiers == KeyModifiers::SHIFT;
    if !key.modifiers.is_empty() && !(key.code == KeyCode::BackTab && is_shift_only) {
        return false;
    }

    let is_navigation_surface = matches!(app.focus, Focus::Query | Focus::Results);

    match key.code {
        KeyCode::Char('1') if is_navigation_surface => {
            app.show_query_editor();
            app.edit_mode = EditMode::Navigate;
            true
        }
        KeyCode::Char('2') if is_navigation_surface => {
            app.focus_results_panel();
            true
        }
        KeyCode::Char('3') if is_navigation_surface => {
            app.focus_command_panel();
            app.command_input = ":".to_string();
            app.update_autocomplete();
            true
        }
        KeyCode::Char('\\') if is_navigation_surface => {
            app.toggle_file_list();
            true
        }
        KeyCode::Tab if is_navigation_surface && app.mode != AppMode::StartPage => {
            app.cycle_panel_focus();
            true
        }
        KeyCode::BackTab if is_navigation_surface && app.mode != AppMode::StartPage => {
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
    if key.code == KeyCode::F(5) {
        app.execute_query();
    } else if app.edit_mode == EditMode::Navigate && key.modifiers.is_empty() {
        if app.file_list_visible {
            // File list navigation mode
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    let new_sel = app
                        .query_file_list_state
                        .selected()
                        .and_then(|sel| if sel > 0 { Some(sel - 1) } else { None });
                    if let Some(sel) = new_sel {
                        app.query_file_list_state.select(Some(sel));
                        if let Some(path) = app.query_files.get(sel).cloned() {
                            app.select_query_file(path);
                        }
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let new_sel = app.query_file_list_state.selected().and_then(|sel| {
                        if sel < app.query_files.len().saturating_sub(1) {
                            Some(sel + 1)
                        } else {
                            None
                        }
                    });
                    if let Some(sel) = new_sel {
                        app.query_file_list_state.select(Some(sel));
                        if let Some(path) = app.query_files.get(sel).cloned() {
                            app.select_query_file(path);
                        }
                    }
                }
                KeyCode::Enter => {
                    if let Some(sel) = app.query_file_list_state.selected() {
                        if let Some(path) = app.query_files.get(sel).cloned() {
                            app.select_query_file(path);
                        }
                    }
                }
                KeyCode::Char('e') => app.open_query_in_external_editor(),
                KeyCode::Char('f') => app.open_tracked_folder_picker(),
                KeyCode::Char('n') => app.open_query_file_create(),
                KeyCode::Char('m') => app.start_rename_query_file(),
                KeyCode::Delete => {
                    if let Some(sel) = app.query_file_list_state.selected() {
                        if let Some(path) = app.query_files.get(sel).cloned() {
                            app.file_delete_target = Some(path);
                            app.open_modal(AppMode::QueryFileDeleteConfirm);
                        }
                    }
                }
                KeyCode::Char('G') => {
                    if !app.query_files.is_empty() {
                        let last = app.query_files.len() - 1;
                        app.query_file_list_state.select(Some(last));
                        if let Some(path) = app.query_files.get(last).cloned() {
                            app.select_query_file(path);
                        }
                    }
                }
                KeyCode::Char('g') => {
                    if !app.query_files.is_empty() {
                        app.query_file_list_state.select(Some(0));
                        if let Some(path) = app.query_files.get(0).cloned() {
                            app.select_query_file(path);
                        }
                    }
                }
                KeyCode::Char('r') => app.execute_query(),
                KeyCode::Char('\\') => app.toggle_file_list(),
                _ => {
                    if let Some(input) = map_query_navigation_key(key) {
                        app.query_editor.input(input);
                    }
                }
            }
        } else {
            // Editor navigation mode (no file list visible)
            match key.code {
                KeyCode::Char('e') => app.open_query_in_external_editor(),
                KeyCode::Char('o') => app.open_query_file_picker(),
                KeyCode::Char('n') => app.open_query_file_create(),
                KeyCode::Char('r') => app.execute_query(),
                KeyCode::Char('\\') => app.toggle_file_list(),
                _ => {
                    if let Some(input) = map_query_navigation_key(key) {
                        app.query_editor.input(input);
                    }
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
        App, AppMode, EditMode, Focus, TrackedFolderEntry, TrackedFolderMenuItem,
        display_folder_path, handle_command_key, handle_filter_key, handle_fullscreen_key,
        handle_navigation_mode_key, handle_query_insert_transition_key, handle_query_key,
        handle_results_key, handle_server_select_key, map_query_navigation_key, resolve_folder_input,
        temp_editor_path,
    };
    use crate::config::AppConfig;
    use crate::tracked_folder::TrackedFolderStore;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
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
            fullscreen_area_height: 0,
            last_fullscreen_g: None,
            rt: Runtime::new().unwrap(),
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
        app.open_modal(AppMode::ServerSelect);

        handle_server_select_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();

        assert_eq!(app.mode, AppMode::Normal);
        assert_eq!(app.focus, Focus::Query);
    }

    #[test]
    fn server_switch_closes_modal_into_results_focus() {
        let mut app = test_app();
        app.focus_command_panel();
        app.config.servers = vec![crate::client::ServerConfig {
            name: "local".to_string(),
            uri: "http://localhost".to_string(),
            username: "admin".to_string(),
            password: "admin".to_string(),
            port: 8003,
        }];
        app.server_list = vec!["local - http://localhost".to_string()];
        app.server_list_state.select(Some(0));
        app.open_modal(AppMode::ServerSelect);

        handle_server_select_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        assert_eq!(app.mode, AppMode::Normal);
        assert_eq!(app.focus, Focus::Results);
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
        let text: String = app.query_editor.lines().join("\n");
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

fn handle_command_key(app: &mut App, key: KeyEvent) {
    let is_start_page = app.mode == AppMode::StartPage;

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
                if !is_start_page {
                    app.cycle_panel_focus();
                }
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
                if !is_start_page {
                    app.cycle_panel_focus_backward();
                }
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
            app.command_input.clear();
            app.autocomplete_suggestions.clear();
            app.autocomplete_selected = 0;
            if !is_start_page {
                app.cycle_panel_focus();
            }
        }
        KeyCode::Backspace => {
            app.command_input.pop();
            app.update_autocomplete();
        }
        KeyCode::Char(c) => {
            // '/' as first character switches to filter mode
            if c == '/' && app.command_input.is_empty() && !is_start_page {
                app.open_filter_input();
                return;
            }

            if c == ':' {
                if app.command_input == ":" {
                    return;
                }
                if app.command_input.is_empty() {
                    app.command_input.push(':');
                    app.update_autocomplete();
                    return;
                }
            } else if app.command_input.is_empty() {
                app.command_input.push(':');
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
            app.close_modal_restore_focus();
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
                    app.close_modal_with_focus(Focus::Results);
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
            app.close_modal_restore_focus();
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
                    app.close_modal_with_focus(Focus::Results);
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
            app.close_modal_restore_focus();
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
                    app.close_modal_with_focus(Focus::Query);
                    app.select_query_file(path);
                }
            }
        }
        _ => {}
    }
    Ok(false)
}

fn handle_tracked_folder_select_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.close_modal_restore_focus();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.move_tracked_folder_selection(-1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.move_tracked_folder_selection(1);
        }
        KeyCode::Char('g') => {
            app.tracked_folder_list_state
                .select(app.first_tracked_folder_item_index());
        }
        KeyCode::Char('G') => {
            app.tracked_folder_list_state
                .select(app.last_tracked_folder_item_index());
        }
        KeyCode::Enter => {
            app.switch_to_selected_tracked_folder();
        }
        KeyCode::Char('a') => {
            app.open_tracked_folder_add();
        }
        KeyCode::Char('f') => {
            app.toggle_selected_tracked_folder_favorite();
        }
        KeyCode::Char('d') => {
            app.start_delete_selected_tracked_folder();
        }
        KeyCode::Char('c') => {
            app.start_clear_selected_tracked_folder_cache();
        }
        _ => {}
    }
    Ok(false)
}

fn handle_tracked_folder_add_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.open_modal(AppMode::TrackedFolderSelect);
        }
        KeyCode::Enter => {
            app.add_tracked_folder_from_input();
        }
        KeyCode::Backspace => {
            app.tracked_folder_input.pop();
        }
        KeyCode::Char(c) => {
            app.tracked_folder_input.push(c);
        }
        _ => {}
    }
    Ok(false)
}

fn handle_tracked_folder_delete_confirm_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            app.confirm_delete_selected_tracked_folder();
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            app.tracked_folder_delete_target = None;
            app.mode = AppMode::TrackedFolderSelect;
        }
        _ => {}
    }
    Ok(false)
}

fn handle_tracked_folder_cache_clear_confirm_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            app.confirm_clear_selected_tracked_folder_cache();
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            app.tracked_folder_cache_clear_target = None;
            app.mode = AppMode::TrackedFolderSelect;
        }
        _ => {}
    }
    Ok(false)
}

fn handle_query_file_create_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.close_modal_restore_focus();
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
            app.close_modal_restore_focus();
        }
        _ => {}
    }
    Ok(false)
}

fn handle_fullscreen_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    let half_page = app.fullscreen_area_height.saturating_sub(2) / 2;
    let total_lines = app.full_view_content.lines().count() as u16;
    let visible_lines = app.fullscreen_area_height.saturating_sub(2);
    let max_scroll = total_lines.saturating_sub(visible_lines);

    if key.code != KeyCode::Char('g') {
        app.last_fullscreen_g = None;
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.mode = AppMode::Normal;
        }
        KeyCode::Char('?') if key.modifiers.is_empty() => {
            app.previous_mode = Some(app.mode.clone());
            app.mode = AppMode::HelpOverlay;
        }
        KeyCode::Char('e') if key.modifiers.is_empty() => {
            app.open_document_in_external_editor();
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.full_view_scroll = (app.full_view_scroll + 1).min(max_scroll);
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.full_view_scroll = app.full_view_scroll.saturating_sub(1);
        }
        KeyCode::PageDown | KeyCode::Char(' ') => {
            app.full_view_scroll = (app.full_view_scroll + 20).min(max_scroll);
        }
        KeyCode::PageUp => {
            app.full_view_scroll = app.full_view_scroll.saturating_sub(20);
        }
        KeyCode::Char('d') => {
            app.full_view_scroll = (app.full_view_scroll + half_page.max(1)).min(max_scroll);
        }
        KeyCode::Char('u') => {
            app.full_view_scroll = app.full_view_scroll.saturating_sub(half_page.max(1));
        }
        KeyCode::Char('G') => {
            app.full_view_scroll = max_scroll;
        }
        KeyCode::Char('g') => {
            if let Some(last_g) = app.last_fullscreen_g {
                if last_g.elapsed().as_millis() < 500 {
                    app.full_view_scroll = 0;
                    app.last_fullscreen_g = None;
                    return Ok(false);
                }
            }
            app.last_fullscreen_g = Some(Instant::now());
        }
        _ => {}
    }
    Ok(false)
}

fn handle_server_select_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.close_modal_restore_focus();
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
                    app.close_modal_with_focus(Focus::Results);
                    app.status_message = format!("Switched to server: {}", name);
                    app.current_collection = None;
                    app.current_page = 0;
                    app.fetch_list();
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
                        app.close_modal_with_focus(Focus::Results);
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
            app.open_modal(AppMode::ServerSelect);
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

fn handle_query_file_rename_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.close_modal_restore_focus();
            app.rename_file_target = None;
        }
        KeyCode::Enter => {
            app.rename_query_file();
        }
        KeyCode::Backspace => {
            app.rename_file_input.pop();
        }
        KeyCode::Char(c) => {
            app.rename_file_input.push(c);
        }
        _ => {}
    }
    Ok(false)
}

fn handle_query_file_delete_confirm_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            if let Some(path) = app.file_delete_target.take() {
                app.delete_query_file(path);
            }
            app.close_modal_with_focus(Focus::Query);
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            app.file_delete_target = None;
            app.close_modal_restore_focus();
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
