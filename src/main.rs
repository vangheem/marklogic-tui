mod client;
mod config;

use anyhow::Result;
use client::{MarkLogicClient, SearchResult, ServerConfig};
use config::AppConfig;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},

    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap},
    Frame, Terminal,
};
use std::io;
use std::time::Instant;
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
enum AppMode {
    Normal,
    FullScreenView,
    ServerAdd,
    DatabaseSelect,
    ServerSelect,
    CollectionSelect,
    DeleteConfirm,
}

struct App {
    config: AppConfig,
    client: Option<MarkLogicClient>,
    focus: Focus,
    mode: AppMode,
    command_input: String,
    query_editor: TextArea<'static>,
    query_visible: bool,
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
    // Full screen view
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
            mode: AppMode::Normal,
            command_input: String::new(),
            query_editor: {
                let mut ta = TextArea::new(vec![
                    "'use strict';".to_string(),
                    "".to_string(),
                    "// Fetches all documents".to_string(),
                    "fn.subsequence(fn.doc(), 1, 50);".to_string(),
                ]);
                ta.set_block(Block::default().borders(Borders::ALL).title("Query (JavaScript) [Alt+Enter or F5 to run]"));
                ta
            },
            query_visible: false,
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
        (":databases", "List databases"),
        (":list", "List all documents (paged)"),
        (":collections", "Show collections"),
        (":list:<collection>", "List documents in collection"),
        (":clear", "Clear collection filter and reset page"),
        (":tdes", "List Template Driven Extraction templates"),
        (":query", "Toggle query editor"),
    ];

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

    fn status_line(&self) -> String {
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
        format!("Server: {} | Database: {}{} | {}", server, db, collection, self.status_message)
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
            "databases" => self.cmd_databases(),
            "list" => {
                self.current_collection = None;
                self.uri_filter = None;
                self.filter_input.clear();
                self.current_page = 0;
                self.query_visible = false;
                self.focus = Focus::Results;
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
            "query" => {
                self.query_visible = true;
                self.focus = Focus::Query;
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
            self.status_message = "No servers configured. Use :server-add to add one.".to_string();
        } else {
            self.server_list = list;
            self.server_list_state.select(Some(0));
            self.mode = AppMode::ServerSelect;
        }
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
                    self.collection_list_state.select(if self.collection_list.is_empty() {
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
                        self.records.clear();
                        self.results_text.clear();
                        self.query_results = uris;
                        self.query_results_state.select(Some(0));
                        self.status_message = format!("{} TDE(s) found", self.query_results.len());
                        self.focus = Focus::Results;
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
            match self.rt.block_on(client.search_documents(
                col,
                None,
                dir,
                start,
                self.page_size,
            )) {
                Ok(paged) => {
                    self.total_results = paged.total;
                    self.records = paged.results;
                    self.list_state.select(if self.records.is_empty() {
                        None
                    } else {
                        Some(0)
                    });
                    self.selected_indices.clear();
                    self.focus = Focus::Results;
                    let total_str = paged
                        .total
                        .map(|t| t.to_string())
                        .unwrap_or("?".to_string());
                    self.status_message = format!(
                        "Page {} | Total: {}",
                        self.current_page + 1,
                        total_str
                    );
                }
                Err(e) => {
                    self.results_text = format!("Error: {}", e);
                }
            }
        } else {
            self.results_text = "No server connected.".to_string();
        }
    }

    fn execute_query(&mut self) {
        if let Some(client) = &self.client {
            let client = client.clone();
            let query = self.query_editor.lines().join("\n");
            match self.rt.block_on(client.js_query(&query)) {
                Ok(parts) => {
                    self.records.clear();
                    self.query_results = parts;
                    self.query_results_state.select(if self.query_results.is_empty() {
                        None
                    } else {
                        Some(0)
                    });
                    self.results_text.clear();
                    self.focus = Focus::Results;
                    self.status_message = format!("{} result(s)", self.query_results.len());
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
                            self.full_view_content = format_document_detail(&detail);
                        }
                        Err(e) => {
                            self.full_view_content = format!("Error loading document: {}", e);
                        }
                    }
                } else {
                    self.full_view_content = format!("URI: {}", uri);
                }
                self.full_view_scroll = 0;
                self.mode = AppMode::FullScreenView;
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
        output.push_str(&format!("Permissions:  {}\n", detail.permissions.join(", ")));
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
        AppMode::Normal => ui_normal(f, app),
    }
}

fn ui_normal(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // status
            Constraint::Length(3), // command
            Constraint::Min(0),   // main area
        ])
        .split(f.area());

    // Status row: split into status (left) and keybinds (right)
    let status_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[0]);

    let status = Paragraph::new(app.status_line())
        .block(Block::default().borders(Borders::ALL).title("Status").border_style(Style::default().fg(Color::Blue)));
    f.render_widget(status, status_chunks[0]);

    let help_text = ":servers :databases :list :collections :list:<col> :tdes :query :clear";
    let keys = Paragraph::new(help_text)
        .block(Block::default().borders(Borders::ALL).title("Help").border_style(Style::default().fg(Color::Yellow)));
    f.render_widget(keys, status_chunks[1]);

    // Command / Filter input
    if app.focus == Focus::Filter {
        let filter_display = format!("/{}", app.filter_input);
        let filter_widget = Paragraph::new(filter_display.as_str())
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Filter URI (Enter=apply, Esc=cancel)")
                    .border_style(Style::default().fg(Color::Green)),
            );
        f.render_widget(filter_widget, chunks[1]);
        let cursor_x = chunks[1].x + app.filter_input.len() as u16 + 2; // +1 border +1 for '/'
        let cursor_y = chunks[1].y + 1;
        f.set_cursor_position((cursor_x, cursor_y));
    } else {
        let cmd_style = Style::default().fg(Color::Magenta);
        let title = if let Some(ref filter) = app.uri_filter {
            format!("Command (: prefix) [Tab=complete] | Filter: {}", filter)
        } else {
            "Command (: prefix) [Tab=complete, /=filter URI]".to_string()
        };
        let command = Paragraph::new(app.command_input.as_str())
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(cmd_style),
            );
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

    let results_area = if app.query_visible { main_chunks[1] } else { main_chunks[0] };

    if app.query_visible {
    // Query input
    let border_style = if app.focus == Focus::Query {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };
    app.query_editor.set_block(
        Block::default()
            .borders(Borders::ALL)
            .title("Query (JavaScript) [Alt+Enter or F5 to run]")
            .border_style(border_style),
    );
    app.query_editor.set_cursor_line_style(Style::default());
    if app.focus == Focus::Query {
        app.query_editor.set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
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
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                Row::new(vec![check.to_string(), r.uri.clone(), cols]).style(style)
            })
            .collect();

        let title = "Results [Space=select, Enter=view, Ctrl+D=delete, n/p=page]";
        let header = Row::new(vec!["", "URI", "Collections"])
            .style(Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan));
        let widths = [
            Constraint::Length(3),
            Constraint::Percentage(45),
            Constraint::Percentage(50),
        ];
        let table = Table::new(rows, widths)
            .header(header)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(results_style),
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
                Row::new(vec![num, snippet])
            })
            .collect();

        let title = "Query Results [Enter=view full, j/k=navigate]";
        let header = Row::new(vec!["#", "Result"])
            .style(Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan));
        let widths = [
            Constraint::Length(4),
            Constraint::Min(0),
        ];
        let table = Table::new(rows, widths)
            .header(header)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(results_style),
            )
            .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
            .row_highlight_style(Style::default().bg(Color::Blue).fg(Color::White));
        f.render_stateful_widget(table, results_area, &mut app.query_results_state);
    } else {
        let results = Paragraph::new(app.results_text.as_str())
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Results")
                    .border_style(results_style),
            )
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
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title("Suggestions"));
        f.render_widget(Clear, popup_area);
        f.render_widget(list, popup_area);
    }
}

fn ui_fullscreen(f: &mut Frame, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Document View [Esc to close, j/k or arrows to scroll]");
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

fn ui_delete_confirm(f: &mut Frame, app: &App) {
    let uris = app.delete_uris();
    let area = centered_rect(70, 60, f.area());
    f.render_widget(Clear, area);

    let mut lines = vec![
        format!("Delete {} document(s)?", uris.len()),
        String::new(),
    ];
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
    let labels = ["Name", "URI (e.g. http://localhost)", "Username", "Password", "Port (default 8003)"];
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
        let p = Paragraph::new(display)
            .block(Block::default().borders(Borders::ALL).title(*label).border_style(style));
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
                return Ok(true);
            }

            match app.mode {
                AppMode::FullScreenView => return handle_fullscreen_key(app, key),
                AppMode::ServerAdd => return handle_server_add_key(app, key),
                AppMode::ServerSelect => return handle_server_select_key(app, key),
                AppMode::DatabaseSelect => return handle_database_select_key(app, key),
                AppMode::CollectionSelect => return handle_collection_select_key(app, key),
                AppMode::DeleteConfirm => return handle_delete_confirm_key(app, key),
                AppMode::Normal => {}
            }

            // Double-Esc: clear all filters and re-fetch
            if key.code == KeyCode::Esc {
                if let Some(last) = app.last_esc {
                    if last.elapsed().as_millis() < 500 {
                        app.last_esc = None;
                        app.current_collection = None;
                        app.uri_filter = None;
                        app.filter_input.clear();
                        app.current_page = 0;
                        app.query_visible = false;
                        app.focus = Focus::Results;
                        app.status_message = "Cleared all filters".to_string();
                        app.fetch_list();
                        return Ok(false);
                    }
                }
                app.last_esc = Some(Instant::now());
            } else {
                app.last_esc = None;
            }

            // ':' typed in query or results mode opens command input
            if app.focus != Focus::Command && app.focus != Focus::Filter && key.code == KeyCode::Char(':') {
                app.focus = Focus::Command;
                app.command_input = ":".to_string();
                app.update_autocomplete();
                return Ok(false);
            }

            // '/' typed in results mode opens URI filter input
            if app.focus == Focus::Results && key.code == KeyCode::Char('/') {
                app.focus = Focus::Filter;
                app.filter_input = app.uri_filter.clone().unwrap_or_default();
                return Ok(false);
            }

            match app.focus {
                Focus::Command => handle_command_key(app, key),
                Focus::Query => {
                    if key.code == KeyCode::Esc {
                        app.focus = Focus::Command;
                    } else if (key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::ALT))
                        || (key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::CONTROL))
                        || key.code == KeyCode::F(5)
                    {
                        app.execute_query();
                    } else {
                        app.query_editor.input(Input::from(key));
                    }
                }
                Focus::Results => handle_results_key(app, key),
                Focus::Filter => handle_filter_key(app, key),
            }
        }
    }
    Ok(false)
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
        KeyCode::BackTab | KeyCode::Up => {
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
                app.focus = Focus::Filter;
                app.filter_input = app.uri_filter.clone().unwrap_or_default();
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
            app.focus = Focus::Results;
            app.fetch_list();
        }
        KeyCode::Esc => {
            // Cancel filter editing
            app.focus = Focus::Results;
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
                        app.full_view_content = format_document_content(result);
                        app.full_view_scroll = 0;
                        app.mode = AppMode::FullScreenView;
                    }
                }
            } else {
                app.open_record();
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
            app.mode = AppMode::ServerAdd;
            app.server_add_step = 0;
            app.server_add_fields = vec![String::new(); 5];
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
            let port: u16 = app.server_add_fields[4]
                .parse()
                .unwrap_or(8003);
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
            app.server_add_step = if app.server_add_step == 0 { 4 } else { app.server_add_step - 1 };
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
        terminal.draw(|f| ui(f, &mut app))?;
        if handle_event(&mut app)? {
            break;
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(())
}
