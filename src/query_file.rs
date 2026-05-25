use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const SUPPORTED_EXTENSIONS: &[&str] = &["js", "mjs", "sparql", "sql", "sjs", "xqy"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryExecutionKind {
    JavaScript,
    XQuery,
    Deferred,
    Unsupported,
}

pub fn discover_query_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = fs::read_dir(dir)
        .with_context(|| format!("Failed to read query directory: {}", dir.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_file() && is_supported_query_file(path))
        .collect::<Vec<_>>();

    files.sort_by(|left, right| {
        left.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase()
            .cmp(
                &right
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_lowercase(),
            )
    });

    Ok(files)
}

pub fn is_supported_query_file(path: &Path) -> bool {
    normalized_extension(path)
        .map(|ext| SUPPORTED_EXTENSIONS.contains(&ext.as_str()))
        .unwrap_or(false)
}

pub fn query_execution_kind(path: &Path) -> QueryExecutionKind {
    match normalized_extension(path).as_deref() {
        Some("js" | "mjs" | "sjs") => QueryExecutionKind::JavaScript,
        Some("xqy") => QueryExecutionKind::XQuery,
        Some("sql" | "sparql") => QueryExecutionKind::Deferred,
        Some(_) | None => QueryExecutionKind::Unsupported,
    }
}

pub fn load_query_file(path: &Path) -> Result<String> {
    fs::read_to_string(path)
        .with_context(|| format!("Failed to read query file: {}", path.display()))
}

pub fn save_query_file(path: &Path, contents: &str) -> Result<()> {
    fs::write(path, contents)
        .with_context(|| format!("Failed to save query file: {}", path.display()))
}

pub fn display_query_path(path: &Path, root_dir: &Path) -> String {
    path.strip_prefix(root_dir)
        .unwrap_or(path)
        .display()
        .to_string()
}

pub fn editor_lines(text: &str) -> Vec<String> {
    text.split('\n').map(String::from).collect()
}

pub fn should_autosave(
    is_dirty: bool,
    last_edit_at: Option<Instant>,
    now: Instant,
    interval: Duration,
) -> bool {
    is_dirty
        && last_edit_at
            .map(|last_edit_at| now.duration_since(last_edit_at) >= interval)
            .unwrap_or(false)
}

fn normalized_extension(path: &Path) -> Option<String> {
    path.extension()
        .map(|ext| ext.to_string_lossy().to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_test_dir(name: &str) -> PathBuf {
        let unique = format!(
            "marklogic-tui-{}-{}-{}",
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
    fn discover_query_files_filters_and_sorts_supported_extensions() {
        let dir = temp_test_dir("discover");
        fs::create_dir_all(&dir).unwrap();

        fs::write(dir.join("z-last.sql"), "SELECT 1").unwrap();
        fs::write(dir.join("a-first.XQY"), "xquery version \"1.0-ml\";").unwrap();
        fs::write(dir.join("middle.txt"), "ignore me").unwrap();
        fs::create_dir_all(dir.join("nested.js")).unwrap();
        fs::write(dir.join("b-second.sjs"), "'use strict';").unwrap();

        let files = discover_query_files(&dir).unwrap();
        let names = files
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().to_string())
            .collect::<Vec<_>>();

        assert_eq!(names, vec!["a-first.XQY", "b-second.sjs", "z-last.sql"]);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn query_execution_kind_is_case_insensitive() {
        assert_eq!(
            query_execution_kind(Path::new("query.SJS")),
            QueryExecutionKind::JavaScript
        );
        assert_eq!(
            query_execution_kind(Path::new("query.xqy")),
            QueryExecutionKind::XQuery
        );
        assert_eq!(
            query_execution_kind(Path::new("query.SPARQL")),
            QueryExecutionKind::Deferred
        );
        assert_eq!(
            query_execution_kind(Path::new("query.txt")),
            QueryExecutionKind::Unsupported
        );
    }

    #[test]
    fn should_autosave_uses_idle_cooldown() {
        let now = Instant::now();

        assert!(!should_autosave(
            false,
            Some(now - Duration::from_secs(3)),
            now,
            Duration::from_secs(2)
        ));
        assert!(!should_autosave(
            true,
            Some(now - Duration::from_secs(1)),
            now,
            Duration::from_secs(2)
        ));
        assert!(should_autosave(
            true,
            Some(now - Duration::from_secs(3)),
            now,
            Duration::from_secs(2)
        ));
    }
}
