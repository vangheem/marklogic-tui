use anyhow::{bail, Context, Result};
use digest_auth::AuthContext;
use reqwest::{Client, Method, RequestBuilder, Response};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub name: String,
    pub uri: String,
    pub username: String,
    pub password: String,
    pub port: u16,
}

impl ServerConfig {
    pub fn base_url(&self) -> String {
        format!("{}:{}", self.uri.trim_end_matches('/'), self.port)
    }
}

#[derive(Debug, Clone)]
pub struct MarkLogicClient {
    pub server: ServerConfig,
    pub database: Option<String>,
    client: Client,
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub uri: String,
    pub collections: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct DocumentDetail {
    pub uri: String,
    pub collections: Vec<String>,
    pub quality: Option<i64>,
    pub permissions: Vec<String>,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct PagedResults {
    pub results: Vec<SearchResult>,
    pub start: usize,
    pub page_size: usize,
    pub total: Option<usize>,
}

impl MarkLogicClient {
    pub fn new(server: ServerConfig) -> Self {
        let client = Client::new();
        Self {
            server,
            database: None,
            client,
        }
    }

    pub fn set_database(&mut self, db: String) {
        self.database = Some(db);
    }

    fn system_url(&self, path: &str) -> String {
        format!("{}{}", self.server.base_url(), path)
    }

    /// Perform a request with digest authentication.
    /// First sends without auth to get the WWW-Authenticate challenge,
    /// then retries with the computed digest header.
    async fn request_with_digest(
        &self,
        method: Method,
        url: &str,
        configure: impl Fn(RequestBuilder) -> RequestBuilder,
    ) -> Result<Response> {
        // First request to get the digest challenge
        let req = configure(self.client.request(method.clone(), url));
        let resp = req.send().await?;

        if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Ok(resp);
        }

        // Extract WWW-Authenticate header
        let www_auth = resp
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .map(String::from);

        let Some(www_auth) = www_auth else {
            bail!("Server returned 401 but no WWW-Authenticate header");
        };

        // Parse the URI path for digest auth
        let uri_path = url
            .strip_prefix(&format!("{}://", url.split("://").next().unwrap_or("http")))
            .and_then(|s| s.find('/').map(|i| &s[i..]))
            .unwrap_or("/");
        // Strip query params for the path used in digest, but keep full URI in request
        let digest_uri = uri_path.split('?').next().unwrap_or(uri_path);

        let method_enum = match method.as_str() {
            "POST" => digest_auth::HttpMethod::POST,
            "PUT" => digest_auth::HttpMethod::PUT,
            "PATCH" => digest_auth::HttpMethod::PATCH,
            "DELETE" => digest_auth::HttpMethod::DELETE,
            _ => digest_auth::HttpMethod::GET,
        };

        let context = AuthContext::new_with_method(
            &self.server.username,
            &self.server.password,
            digest_uri,
            Option::<&[u8]>::None,
            method_enum,
        );

        let mut prompt = digest_auth::parse(&www_auth)
            .map_err(|e| anyhow::anyhow!("Failed to parse digest challenge: {:?}", e))?;
        let auth_header = prompt
            .respond(&context)
            .map_err(|e| anyhow::anyhow!("Failed to compute digest response: {:?}", e))?
            .to_header_string();

        // Retry with digest auth
        let req = configure(self.client.request(method, url))
            .header("Authorization", auth_header);
        let resp = req.send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("Request failed with status {}: {}", status, body);
        }

        Ok(resp)
    }

    /// List databases via the manage API (port 8002)
    pub async fn list_databases(&self) -> Result<Vec<String>> {
        let url = format!(
            "{}:{}/manage/v2/databases?format=json",
            self.server.uri.trim_end_matches('/'),
            8002
        );
        let resp = self
            .request_with_digest(Method::GET, &url, |r| r)
            .await
            .context("Failed to list databases")?;
        let body: Value = resp.json().await?;
        let dbs = body["database-default-list"]["list-items"]["list-item"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| item["nameref"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        Ok(dbs)
    }

    /// List collections in current database
    pub async fn list_collections(&self) -> Result<Vec<String>> {
        let db = self.database.as_deref().unwrap_or("Documents").to_string();
        let script = "JSON.stringify(Array.from(cts.collections()))";
        let url = self.system_url("/v1/eval");
        let body_str = format!("javascript={}", script);
        let db_clone = db.clone();
        let resp = self
            .request_with_digest(Method::POST, &url, |r| {
                r.header("Content-Type", "application/x-www-form-urlencoded")
                    .query(&[("database", &db_clone)])
                    .body(body_str.clone())
            })
            .await
            .context("Failed to list collections")?;
        let text = resp.text().await?;
        let collections = parse_eval_json_array(&text);
        Ok(collections)
    }

    /// Query using Optic API (rows endpoint)
    pub async fn optic_query(&self, dsl: &str) -> Result<Value> {
        let db = self.database.as_deref().unwrap_or("Documents").to_string();
        let url = self.system_url("/v1/rows");
        let dsl_owned = dsl.to_string();
        let db_clone = db.clone();
        let resp = self
            .request_with_digest(Method::POST, &url, |r| {
                r.header("Content-Type", "application/vnd.marklogic.querydsl+javascript")
                    .header("Accept", "application/json")
                    .query(&[("database", &db_clone)])
                    .body(dsl_owned.clone())
            })
            .await
            .context("Failed to execute optic query")?;
        let body: Value = resp.json().await?;
        Ok(body)
    }

    /// Execute a JavaScript query via eval endpoint, returning individual result parts
    pub async fn js_query(&self, script: &str) -> Result<Vec<String>> {
        let db = self.database.as_deref().unwrap_or("Documents").to_string();
        let url = self.system_url("/v1/eval");
        let body_str = format!("javascript={}", script);
        let db_clone = db.clone();
        let resp = self
            .request_with_digest(Method::POST, &url, |r| {
                r.header("Content-Type", "application/x-www-form-urlencoded")
                    .query(&[("database", db_clone.as_str())])
                    .body(body_str.clone())
            })
            .await
            .context("Failed to execute JavaScript query")?;
        let text = resp.text().await?;
        parse_eval_response_parts(&text)
    }

    /// Search/list documents in a collection with paging (lightweight, no full content)
    pub async fn search_documents(
        &self,
        collection: Option<&str>,
        query: Option<&str>,
        uri_filter: Option<&str>,
        start: usize,
        page_size: usize,
    ) -> Result<PagedResults> {
        let db = self.database.as_deref().unwrap_or("Documents").to_string();

        // If URI filter is set, use eval with cts.uriMatch for flexible matching
        if let Some(filter) = uri_filter {
            return self.search_documents_with_uri_filter(
                collection, filter, start, page_size, &db,
            ).await;
        }

        let url = self.system_url("/v1/search");
        let start_str = start.to_string();
        let page_str = page_size.to_string();
        let collection_owned = collection.map(String::from);
        let query_owned = query.map(String::from);
        let db_clone = db.clone();

        let resp = self
            .request_with_digest(Method::GET, &url, |mut r| {
                r = r.query(&[
                    ("database", db_clone.as_str()),
                    ("format", "json"),
                    ("start", start_str.as_str()),
                    ("pageLength", page_str.as_str()),
                ]);
                if let Some(ref col) = collection_owned {
                    r = r.query(&[("collection", col.as_str())]);
                }
                if let Some(ref q) = query_owned {
                    r = r.query(&[("q", q.as_str())]);
                }
                r
            })
            .await
            .context("Failed to search documents")?;
        let body: Value = resp.json().await?;

        let total = body["total"].as_u64().map(|t| t as usize);
        let mut results: Vec<SearchResult> = body["results"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|item| {
                        let uri = item["uri"].as_str().unwrap_or("").to_string();
                        // Collections aren't returned in default search; will be fetched on detail view
                        SearchResult { uri, collections: Vec::new() }
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Fetch collections for the returned URIs
        if !results.is_empty() {
            let uris_json = serde_json::to_string(
                &results.iter().map(|r| r.uri.as_str()).collect::<Vec<_>>()
            ).unwrap();
            let script = format!(
                r#"
                const uris = {};
                JSON.stringify(uris.map(uri => ({{
                    uri: uri,
                    collections: Array.from(xdmp.documentGetCollections(uri))
                }})))
                "#,
                uris_json
            );
            let eval_url = self.system_url("/v1/eval");
            let body_str = format!("javascript={}", script);
            let db_clone2 = db.clone();
            if let Ok(resp) = self
                .request_with_digest(Method::POST, &eval_url, |r| {
                    r.header("Content-Type", "application/x-www-form-urlencoded")
                        .query(&[("database", db_clone2.as_str())])
                        .body(body_str.clone())
                })
                .await
            {
                if let Ok(text) = resp.text().await {
                    // Parse the multipart eval response
                    for line in text.lines() {
                        let trimmed = line.trim();
                        if trimmed.starts_with('[') {
                            if let Ok(arr) = serde_json::from_str::<Vec<Value>>(trimmed) {
                                for item in arr {
                                    let uri = item["uri"].as_str().unwrap_or("");
                                    let cols: Vec<String> = item["collections"]
                                        .as_array()
                                        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                                        .unwrap_or_default();
                                    if let Some(r) = results.iter_mut().find(|r| r.uri == uri) {
                                        r.collections = cols;
                                    }
                                }
                            }
                            break;
                        }
                    }
                }
            }
        }

        Ok(PagedResults {
            results,
            start,
            page_size,
            total,
        })
    }

    /// Search documents with URI pattern filter using cts.uriMatch via eval
    async fn search_documents_with_uri_filter(
        &self,
        collection: Option<&str>,
        uri_filter: &str,
        start: usize,
        page_size: usize,
        db: &str,
    ) -> Result<PagedResults> {
        let col_filter = if let Some(col) = collection {
            format!(r#", cts.collectionQuery("{}")"#, col.replace('"', r#"\""#))
        } else {
            String::new()
        };
        let pattern = format!("*{}*", uri_filter.replace('"', r#"\""#));
        let script = format!(
            r#"'use strict';
            const uriArray = Array.from(cts.uriMatch("{pattern}"));
            const query = cts.andQuery([cts.documentQuery(uriArray){col_filter}]);
            const total = uriArray.length;
            const results = fn.subsequence(cts.search(query), {start}, {page_size});
            const output = [];
            for (const doc of results) {{
                const uri = xdmp.nodeUri(doc);
                const cols = Array.from(xdmp.documentGetCollections(uri));
                output.push({{uri: uri, collections: cols}});
            }}
            JSON.stringify({{total: total, results: output}});"#,
            pattern = pattern,
            col_filter = col_filter,
            start = start,
            page_size = page_size,
        );

        let eval_url = self.system_url("/v1/eval");
        let body_str = format!("javascript={}", urlencoding::encode(&script));
        let db_clone = db.to_string();

        let resp = self
            .request_with_digest(Method::POST, &eval_url, |r| {
                r.header("Content-Type", "application/x-www-form-urlencoded")
                    .query(&[("database", db_clone.as_str())])
                    .body(body_str.clone())
            })
            .await
            .context("Failed to search documents with URI filter")?;

        let text = resp.text().await?;
        let mut results = Vec::new();
        let mut total = None;

        let parts = parse_eval_response_parts(&text).unwrap_or_default();
        for part in &parts {
            let trimmed = part.trim();
            if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
                total = val["total"].as_u64().map(|t| t as usize);
                if let Some(arr) = val["results"].as_array() {
                    for item in arr {
                        let uri = item["uri"].as_str().unwrap_or("").to_string();
                        let collections: Vec<String> = item["collections"]
                            .as_array()
                            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                            .unwrap_or_default();
                        results.push(SearchResult { uri, collections });
                    }
                }
                break;
            }
        }

        Ok(PagedResults {
            results,
            start,
            page_size,
            total,
        })
    }

    /// Get a single document by URI with full content and metadata
    pub async fn get_document(&self, uri: &str) -> Result<DocumentDetail> {
        let db = self.database.as_deref().unwrap_or("Documents").to_string();
        let url = self.system_url("/v1/documents");
        let uri_owned = uri.to_string();
        let db_clone = db.clone();

        // Get content
        let resp = self
            .request_with_digest(Method::GET, &url, |r| {
                r.query(&[
                    ("database", db_clone.as_str()),
                    ("uri", uri_owned.as_str()),
                ])
            })
            .await
            .context("Failed to get document")?;
        let content = resp.text().await?;

        // Get metadata
        let db_clone2 = db.clone();
        let uri_owned2 = uri.to_string();
        let meta_resp = self
            .request_with_digest(Method::GET, &url, |r| {
                r.query(&[
                    ("database", db_clone2.as_str()),
                    ("uri", uri_owned2.as_str()),
                    ("category", "metadata"),
                    ("format", "json"),
                ])
            })
            .await
            .context("Failed to get document metadata")?;
        let meta_body: Value = meta_resp.json().await.unwrap_or(Value::Null);

        let collections = meta_body["collections"]
            .as_array()
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let quality = meta_body["quality"].as_i64();
        let permissions = meta_body["permissions"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|p| {
                        let role = p["role-name"].as_str().unwrap_or("");
                        let caps = p["capabilities"]
                            .as_array()
                            .map(|c| {
                                c.iter()
                                    .filter_map(|v| v.as_str())
                                    .collect::<Vec<_>>()
                                    .join(",")
                            })
                            .unwrap_or_default();
                        if role.is_empty() {
                            None
                        } else {
                            Some(format!("{}:[{}]", role, caps))
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(DocumentDetail {
            uri: uri.to_string(),
            collections,
            quality,
            permissions,
            content,
        })
    }

    /// Delete documents by URIs
    pub async fn delete_documents(&self, uris: &[String]) -> Result<()> {
        if uris.is_empty() {
            return Ok(());
        }
        let db = self.database.as_deref().unwrap_or("Documents").to_string();
        let url = self.system_url("/v1/documents");

        // Delete each URI individually via DELETE /v1/documents?uri=...
        for uri in uris {
            let db_clone = db.clone();
            let uri_clone = uri.clone();
            self.request_with_digest(Method::DELETE, &url, |r| {
                r.query(&[
                    ("database", db_clone.as_str()),
                    ("uri", uri_clone.as_str()),
                ])
            })
            .await
            .with_context(|| format!("Failed to delete document: {}", uri))?;
        }
        Ok(())
    }
}

/// Parse eval multipart response to extract JSON array
fn parse_eval_json_array(text: &str) -> Vec<String> {
    let mut results = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') || trimmed.starts_with('"') {
            if let Ok(arr) = serde_json::from_str::<Vec<String>>(trimmed) {
                return arr;
            }
            if let Ok(s) = serde_json::from_str::<String>(trimmed) {
                results.push(s);
            }
        }
    }
    results
}

/// Parse multipart/mixed eval response, extracting content parts
fn parse_eval_response(text: &str) -> Result<String> {
    let mut parts = Vec::new();
    let mut in_content = false;
    let mut current_content = String::new();

    for line in text.lines() {
        if line.starts_with("--") && !line.starts_with("---") {
            if in_content && !current_content.trim().is_empty() {
                parts.push(current_content.trim().to_string());
            }
            current_content.clear();
            in_content = false;
        } else if line.is_empty() && !in_content {
            // Empty line after headers signals start of content
            in_content = true;
        } else if in_content {
            if !current_content.is_empty() {
                current_content.push('\n');
            }
            current_content.push_str(line);
        }
    }
    if in_content && !current_content.trim().is_empty() {
        parts.push(current_content.trim().to_string());
    }

    // Try to pretty-print each part if it's JSON
    let formatted: Vec<String> = parts
        .iter()
        .map(|p| {
            if let Ok(val) = serde_json::from_str::<Value>(p) {
                serde_json::to_string_pretty(&val).unwrap_or_else(|_| p.clone())
            } else {
                p.clone()
            }
        })
        .collect();

    Ok(formatted.join("\n\n"))
}

/// Parse multipart/mixed eval response into individual result parts
fn parse_eval_response_parts(text: &str) -> Result<Vec<String>> {
    let mut parts = Vec::new();
    let mut in_content = false;
    let mut current_content = String::new();

    for line in text.lines() {
        if line.starts_with("--") && !line.starts_with("---") {
            if in_content && !current_content.trim().is_empty() {
                parts.push(current_content.trim().to_string());
            }
            current_content.clear();
            in_content = false;
        } else if line.is_empty() && !in_content {
            in_content = true;
        } else if in_content {
            if !current_content.is_empty() {
                current_content.push('\n');
            }
            current_content.push_str(line);
        }
    }
    if in_content && !current_content.trim().is_empty() {
        parts.push(current_content.trim().to_string());
    }

    // Pretty-print JSON parts
    let formatted: Vec<String> = parts
        .iter()
        .map(|p| {
            if let Ok(val) = serde_json::from_str::<Value>(p) {
                serde_json::to_string_pretty(&val).unwrap_or_else(|_| p.clone())
            } else {
                p.clone()
            }
        })
        .collect();

    Ok(formatted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_client() -> MarkLogicClient {
        let server = ServerConfig {
            name: "local".to_string(),
            uri: "http://localhost".to_string(),
            username: "admin".to_string(),
            password: "admin".to_string(),
            port: 8003,
        };
        MarkLogicClient::new(server)
    }

    #[tokio::test]
    async fn test_list_databases() {
        let client = test_client();
        let result = client.list_databases().await;
        match result {
            Ok(dbs) => {
                println!("Databases: {:?}", dbs);
                assert!(!dbs.is_empty(), "Should have at least one database");
                // MarkLogic always has these system databases
                assert!(
                    dbs.iter().any(|d| d == "Documents" || d == "Security" || d == "Schemas"),
                    "Should contain system databases, got: {:?}",
                    dbs
                );
            }
            Err(e) => {
                panic!("Failed to list databases: {}", e);
            }
        }
    }

    #[tokio::test]
    async fn test_list_collections() {
        let mut client = test_client();
        client.set_database("data-platform-content".to_string());
        let result = client.list_collections().await;
        match result {
            Ok(cols) => {
                println!("Collections: {:?}", cols);
                assert!(!cols.is_empty(), "Should have collections in data-platform-content");
            }
            Err(e) => {
                panic!("Failed to list collections: {}", e);
            }
        }
    }

    #[tokio::test]
    async fn test_search_documents() {
        let mut client = test_client();
        client.set_database("data-platform-content".to_string());
        let result = client.search_documents(None, None, None, 1, 10).await;
        match result {
            Ok(paged) => {
                println!(
                    "Search results: {} docs, total: {:?}",
                    paged.results.len(),
                    paged.total
                );
                for r in &paged.results {
                    println!("  URI: {} | Collections: {:?}", r.uri, r.collections);
                }
                // At least some should have collections
                let has_collections = paged.results.iter().any(|r| !r.collections.is_empty());
                println!("Has collections: {}", has_collections);
            }
            Err(e) => {
                panic!("Failed to search documents: {}", e);
            }
        }
    }

    #[tokio::test]
    async fn test_get_collections_for_uri() {
        let mut client = test_client();
        client.set_database("data-platform-content".to_string());
        // First get a URI
        let paged = client.search_documents(None, None, None, 1, 1).await.unwrap();
        let uri = &paged.results[0].uri;
        println!("Testing collections for URI: {}", uri);

        let db = "data-platform-content";
        let script = format!(
            r#"JSON.stringify(Array.from(xdmp.documentGetCollections("{}")))"#,
            uri
        );
        let eval_url = client.system_url("/v1/eval");
        let body_str = format!("javascript={}", script);
        let resp = client
            .request_with_digest(Method::POST, &eval_url, |r| {
                r.header("Content-Type", "application/x-www-form-urlencoded")
                    .query(&[("database", db)])
                    .body(body_str.clone())
            })
            .await
            .unwrap();
        let text = resp.text().await.unwrap();
        println!("Eval response for collections:\n{}", text);
    }

    #[tokio::test]
    async fn test_search_raw_response() {
        let mut client = test_client();
        client.set_database("data-platform-content".to_string());
        let db = "data-platform-content";
        let url = client.system_url("/v1/search");
        let resp = client
            .request_with_digest(Method::GET, &url, |r| {
                r.query(&[
                    ("database", db),
                    ("format", "json"),
                    ("start", "1"),
                    ("pageLength", "5"),
                ])
            })
            .await
            .unwrap();
        let text = resp.text().await.unwrap();
        println!("Raw search response:\n{}", &text[..text.len().min(2000)]);
    }

    #[tokio::test]
    async fn test_js_query() {
        let mut client = test_client();
        client.set_database("data-platform-content".to_string());
        let script = r#"
            'use strict';
            fn.subsequence(fn.doc(), 1, 5);
        "#;
        let result = client.js_query(script).await;
        match result {
            Ok(output) => {
                println!("JS query result:\n{:?}", &output[..output.len().min(1000)]);
                assert!(!output.is_empty(), "Should return some output");
            }
            Err(e) => {
                panic!("Failed to execute JS query: {}", e);
            }
        }
    }

    #[tokio::test]
    async fn test_search_with_uri_filter() {
        let mut client = test_client();
        client.set_database("data-platform-content".to_string());
        let result = client
            .search_documents(None, None, Some("/activity/"), 1, 5)
            .await;
        match result {
            Ok(paged) => {
                println!(
                    "Filtered results: {} docs, total: {:?}",
                    paged.results.len(),
                    paged.total
                );
                for r in &paged.results {
                    println!("  URI: {} | Collections: {:?}", r.uri, r.collections);
                    assert!(r.uri.contains("/activity/"), "URI should contain /activity/");
                }
                assert!(
                    !paged.results.is_empty(),
                    "Should find documents matching /activity/"
                );
            }
            Err(e) => {
                panic!("Failed to search with URI filter: {}", e);
            }
        }
    }
}
