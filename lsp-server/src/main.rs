mod tools;

use anyhow::Result;
use lz4_flex::{compress_prepend_size, decompress_size_prepended};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use tokio::io::{stdin, stdout};
use tokio::process::Command as ProcessCommand;
use tokio::sync::Semaphore;
use tokio::time::{timeout, Duration};
use tower_lsp::jsonrpc::Result as LspResult;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use url::Url;

use tools::PhpTool;

#[derive(Debug, Deserialize, Serialize, Clone)]
struct InitializationOptions {
    standard: Option<String>,
    phpcs_path: Option<String>,
    phpcbf_path: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct PhpcsSettings {
    standard: Option<String>,
    phpcs_path: Option<String>,
    phpcbf_path: Option<String>,
}

#[derive(Debug, Clone)]
struct CompressedDocument {
    compressed_data: Vec<u8>,
    checksum: String,
}

#[derive(Debug, Clone)]
struct CachedResults {
    diagnostics: Vec<Diagnostic>,
    result_id: String,
}

#[derive(Debug, Clone)]
struct PhpcsLanguageServer {
    client: Client,
    // Compressed document storage to reduce memory usage
    open_docs: std::sync::Arc<std::sync::RwLock<HashMap<Url, CompressedDocument>>>,
    // Cache PHPCS results to avoid redundant linting
    results_cache: std::sync::Arc<std::sync::RwLock<HashMap<Url, CachedResults>>>,
    // Memory tracking
    total_memory_usage: std::sync::Arc<AtomicUsize>,
    standard: std::sync::Arc<std::sync::RwLock<Option<String>>>, // None means use PHPCS defaults
    // Cached auto-detected paths
    phpcs_path: std::sync::Arc<std::sync::RwLock<Option<String>>>,
    phpcbf_path: std::sync::Arc<std::sync::RwLock<Option<String>>>,
    // User-configured custom paths
    user_phpcs_path: std::sync::Arc<std::sync::RwLock<Option<String>>>,
    user_phpcbf_path: std::sync::Arc<std::sync::RwLock<Option<String>>>,
    workspace_root: std::sync::Arc<std::sync::RwLock<Option<std::path::PathBuf>>>,
    // Limit concurrent PHPCS processes to prevent system overload
    process_semaphore: std::sync::Arc<Semaphore>,
    // Track when we last returned a fixAll edit per URI to prevent duplicate edits
    // After returning an edit, skip all fixAll requests for that URI for a cooldown period
    fix_all_cooldown: std::sync::Arc<std::sync::RwLock<HashMap<Url, Instant>>>,
}

/// Generate a documentation URL for a PHPCS sniff based on its source identifier.
///
/// Routes to appropriate documentation based on the standard:
/// - PSR1/PSR2/PSR12 → PHPCS Wiki customisable sniff properties
/// - Generic/Squiz/PEAR/Zend → PHPCS Wiki customisable sniff properties
/// - Unknown standards → GitHub search fallback
fn generate_phpcs_doc_url(source: &str) -> Option<Url> {
    if source.is_empty() {
        return None;
    }

    // Parse the source: e.g., "PSR12.Files.OpenTag" -> standard is "PSR12"
    let parts: Vec<&str> = source.split('.').collect();
    if parts.is_empty() {
        return None;
    }

    let standard = parts[0];
    let url_str = match standard {
        // Standard PHPCS sniffs - link to the wiki with anchor
        "PSR1" | "PSR2" | "PSR12" | "Generic" | "Squiz" | "PEAR" | "Zend" | "MySource" => {
            format!(
                "https://github.com/PHPCSStandards/PHP_CodeSniffer/wiki/Customisable-Sniff-Properties#{}",
                source.to_lowercase().replace('.', "-")
            )
        }
        // WordPress coding standards
        "WordPress" | "WordPress-Core" | "WordPress-Docs" | "WordPress-Extra" => {
            format!(
                "https://developer.wordpress.org/coding-standards/wordpress-coding-standards/php/#{}",
                parts.get(1).unwrap_or(&"").to_lowercase()
            )
        }
        // Slevomat coding standard
        "SlevomatCodingStandard" => {
            format!(
                "https://github.com/slevomat/coding-standard#{}",
                source.to_lowercase().replace('.', "-")
            )
        }
        // Fallback: search on PHPCS GitHub
        _ => {
            format!(
                "https://github.com/PHPCSStandards/PHP_CodeSniffer/search?q={}",
                urlencoding::encode(source)
            )
        }
    };

    Url::parse(&url_str).ok()
}

/// Build a process command for a resolved PHP-tool path, applying the
/// platform-specific spawn strategy from [`tools::plan_spawn`]. On Windows this
/// routes the Composer `vendor/bin` proxy and `*.phar` files through `php`, and
/// `.bat`/`.cmd` wrappers through `cmd /C`, instead of spawning them directly
/// (which fails with `os error 193`). The caller appends the tool's own
/// arguments afterwards.
fn build_tool_command(tool_path: &str) -> ProcessCommand {
    let plan = tools::plan_spawn(tool_path, cfg!(windows));
    let mut cmd = ProcessCommand::new(&plan.program);
    cmd.args(&plan.prefix_args);
    cmd
}

impl PhpcsLanguageServer {
    fn new(client: Client) -> Self {
        Self {
            client,
            open_docs: std::sync::Arc::new(std::sync::RwLock::new(HashMap::with_capacity(100))),
            results_cache: std::sync::Arc::new(std::sync::RwLock::new(HashMap::with_capacity(100))),
            total_memory_usage: std::sync::Arc::new(AtomicUsize::new(0)),
            standard: std::sync::Arc::new(std::sync::RwLock::new(None)), // Let PHPCS use its defaults
            phpcs_path: std::sync::Arc::new(std::sync::RwLock::new(None)),
            phpcbf_path: std::sync::Arc::new(std::sync::RwLock::new(None)),
            user_phpcs_path: std::sync::Arc::new(std::sync::RwLock::new(None)),
            user_phpcbf_path: std::sync::Arc::new(std::sync::RwLock::new(None)),
            workspace_root: std::sync::Arc::new(std::sync::RwLock::new(None)),
            // Limit to 4 concurrent PHPCS processes to avoid overwhelming the system
            process_semaphore: std::sync::Arc::new(Semaphore::new(4)),
            fix_all_cooldown: std::sync::Arc::new(std::sync::RwLock::new(HashMap::new())),
        }
    }

    fn compress_document(&self, content: &str) -> CompressedDocument {
        // Use LZ4 for fast compression
        let compressed_data = compress_prepend_size(content.as_bytes());
        let compressed_size = compressed_data.len();

        // Compute checksum for cache invalidation
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        let checksum = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();

        // Update memory tracking
        self.total_memory_usage
            .fetch_add(compressed_size, Ordering::Relaxed);

        CompressedDocument {
            compressed_data,
            checksum,
        }
    }

    fn decompress_document(&self, doc: &CompressedDocument) -> Result<String> {
        let decompressed = decompress_size_prepended(&doc.compressed_data)
            .map_err(|e| anyhow::anyhow!("Decompression failed: {}", e))?;

        let content = String::from_utf8(decompressed)
            .map_err(|e| anyhow::anyhow!("UTF-8 conversion failed: {}", e))?;

        Ok(content)
    }

    fn get_tool_path(
        &self,
        tool: PhpTool,
        user_path: &std::sync::RwLock<Option<String>>,
        cache: &std::sync::RwLock<Option<String>>,
    ) -> String {
        let display = tool.display_name();

        // Check cache first
        if let Ok(guard) = cache.read() {
            if let Some(cached_path) = &*guard {
                return cached_path.clone();
            }
        }

        eprintln!("🔍 PHPCS LSP: Detecting {} path...", display);

        // Gather inputs for detection
        let user_path_val = user_path.read().ok().and_then(|guard| guard.clone());
        let workspace_root = self
            .workspace_root
            .read()
            .ok()
            .and_then(|guard| guard.clone());

        // Detect with full priority order:
        // vendor/bin → user config → env var → system PATH → bundled PHAR
        let path =
            tools::detect_tool_path(tool, workspace_root.as_deref(), user_path_val.as_deref());

        eprintln!("🎯 PHPCS LSP: Final {} path: {}", display, path);

        // Cache the result
        if let Ok(mut guard) = cache.write() {
            *guard = Some(path.clone());
        }

        path
    }

    fn get_phpcs_path(&self) -> String {
        self.get_tool_path(PhpTool::Phpcs, &self.user_phpcs_path, &self.phpcs_path)
    }

    fn get_phpcbf_path(&self) -> String {
        self.get_tool_path(PhpTool::Phpcbf, &self.user_phpcbf_path, &self.phpcbf_path)
    }

    /// Run phpcbf to fix code style issues and return the fixed content
    /// If `sniff_filter` is provided, only fixes for that specific sniff will be applied
    async fn run_phpcbf(
        &self,
        uri: &Url,
        content: &str,
        sniff_filter: Option<&str>,
    ) -> Result<String> {
        let file_name = uri
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .unwrap_or("unknown");

        eprintln!(
            "🔧 PHPCS LSP: Running phpcbf on {}{}",
            file_name,
            sniff_filter
                .map(|s| format!(" (sniff: {})", s))
                .unwrap_or_default()
        );

        let phpcbf_path = self.get_phpcbf_path();

        let mut cmd = build_tool_command(&phpcbf_path);
        cmd.arg("--no-colors").arg("-q");

        // Add standard if configured
        if let Ok(standard_guard) = self.standard.read() {
            if let Some(ref standard) = *standard_guard {
                if !((standard.starts_with('/')
                    || standard.starts_with("./")
                    || standard.ends_with(".xml"))
                    && !std::path::Path::new(standard).exists())
                {
                    eprintln!("📏 PHPCS LSP: phpcbf using standard: {}", standard);
                    cmd.arg(format!("--standard={}", standard));
                }
            } else {
                eprintln!("📏 PHPCS LSP: phpcbf using PHPCS defaults (no standard configured)");
            }
        }

        // Add sniff filter if provided
        if let Some(sniff) = sniff_filter {
            cmd.arg(format!("--sniffs={}", sniff));
        }

        // Use stdin with stdin-path for proper file context
        if let Ok(file_path) = uri.to_file_path() {
            cmd.arg(format!("--stdin-path={}", file_path.display()));
        }
        cmd.arg("-");

        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn phpcbf: {}", e))?;

        // Write content to stdin
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin
                .write_all(content.as_bytes())
                .await
                .map_err(|e| anyhow::anyhow!("Failed to write to phpcbf stdin: {}", e))?;
            drop(stdin);
        }

        // Wait for output with timeout
        let output = timeout(Duration::from_secs(30), child.wait_with_output())
            .await
            .map_err(|_| anyhow::anyhow!("phpcbf timeout"))?
            .map_err(|e| anyhow::anyhow!("phpcbf error: {}", e))?;

        // phpcbf exit codes:
        // 0 = No fixable errors found
        // 1 = All fixable errors were fixed
        // 2 = Some errors fixed, but unfixable errors remain (still success for our purposes)
        // 3+ = Processing error
        let exit_code = output.status.code().unwrap_or(-1);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if exit_code > 2 {
            eprintln!(
                "❌ PHPCS LSP: phpcbf error (exit {}): {}",
                exit_code, stderr
            );
            return Err(anyhow::anyhow!(
                "phpcbf failed with exit code {}: {}",
                exit_code,
                stderr
            ));
        }

        let fixed_content = String::from_utf8(output.stdout)
            .map_err(|e| anyhow::anyhow!("Invalid UTF-8 from phpcbf: {}", e))?;

        eprintln!(
            "✅ PHPCS LSP: phpcbf completed for {} (exit code {})",
            file_name, exit_code
        );
        Ok(fixed_content)
    }

    /// Compute TextEdits for changes affecting specific lines only
    /// Returns edits that modify lines within the given range (inclusive)
    fn compute_line_edits(
        original: &str,
        fixed: &str,
        target_start_line: u32,
        target_end_line: u32,
    ) -> Vec<TextEdit> {
        let original_lines: Vec<&str> = original.lines().collect();
        let fixed_lines: Vec<&str> = fixed.lines().collect();

        let mut edits = Vec::new();

        // Simple line-by-line comparison
        // Find contiguous regions of changes that overlap with target lines
        let max_lines = original_lines.len().max(fixed_lines.len());
        let mut i = 0;

        while i < max_lines {
            let orig_line = original_lines.get(i).copied();
            let fixed_line = fixed_lines.get(i).copied();

            if orig_line != fixed_line {
                // Found a difference - check if it's in our target range
                let line_num = i as u32;

                // Check if this change affects our target lines
                if line_num >= target_start_line && line_num <= target_end_line {
                    // Create an edit for this specific line
                    let start = Position {
                        line: line_num,
                        character: 0,
                    };
                    let end = Position {
                        line: line_num,
                        character: orig_line.map(|l| l.len() as u32).unwrap_or(0),
                    };

                    let new_text = fixed_line.unwrap_or("").to_string();

                    edits.push(TextEdit {
                        range: Range { start, end },
                        new_text,
                    });
                }
            }
            i += 1;
        }

        edits
    }

    fn discover_standard(&self, workspace_root: Option<&std::path::Path>) {
        eprintln!("🔍 PHPCS LSP: Discovering coding standard...");

        if let Some(root) = workspace_root {
            let config_files = [
                ".phpcs.xml",
                "phpcs.xml",
                ".phpcs.xml.dist",
                "phpcs.xml.dist",
            ];

            for config_file in &config_files {
                let config_path = root.join(config_file);

                if config_path.exists() {
                    if let Some(path_str) = config_path.to_str() {
                        eprintln!("✅ PHPCS LSP: Found config file: {}", path_str);
                        if let Ok(mut standard_guard) = self.standard.write() {
                            *standard_guard = Some(path_str.to_string());
                        }
                        return;
                    }
                }
            }
        }

        // No config file found - use PHPCS defaults
        eprintln!("🎯 PHPCS LSP: No config files found - will use PHPCS defaults");
        if let Ok(mut standard_guard) = self.standard.write() {
            *standard_guard = None;
        }
    }

    async fn run_phpcs(
        &self,
        uri: &Url,
        _file_path: &str,
        content: Option<&str>,
    ) -> Result<Vec<Diagnostic>> {
        let start_time = Instant::now();
        let file_name = uri
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .unwrap_or("unknown");

        eprintln!("🔍 PHPCS LSP: Starting lint for file: {}", file_name);

        // Acquire semaphore permit to limit concurrent PHPCS processes
        let _permit = self
            .process_semaphore
            .acquire()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to acquire process semaphore: {}", e))?;

        // Use cached PHPCS path
        let phpcs_path = self.get_phpcs_path();

        // Always use stdin for content to avoid file system reads
        if content.is_none() {
            eprintln!("❌ PHPCS LSP: No content provided for {}", file_name);
            return Ok(vec![]);
        }

        let text = content.unwrap();
        let mut cmd = build_tool_command(&phpcs_path);
        cmd.arg("--report=json").arg("--no-colors").arg("-q");

        // Only add standard if explicitly configured and file still exists
        let standard_info = if let Ok(standard_guard) = self.standard.read() {
            if let Some(ref standard) = *standard_guard {
                // Check if it's a file path and validate it exists
                if (standard.starts_with('/')
                    || standard.starts_with("./")
                    || standard.ends_with(".xml"))
                    && !std::path::Path::new(standard).exists()
                {
                    eprintln!("⚠️ PHPCS LSP: Config file no longer exists: {}", standard);
                    eprintln!("🔄 PHPCS LSP: Re-discovering standard...");

                    // Get workspace root from the file URI
                    let workspace_root = if let Ok(file_path) = uri.to_file_path() {
                        file_path.parent().map(|p| p.to_path_buf())
                    } else {
                        None
                    };

                    // Re-discover the standard
                    self.discover_standard(workspace_root.as_deref());

                    // Use default for this run
                    eprintln!("🎯 PHPCS LSP: Using PHPCS default standard for this run");
                    " with default standard (config file missing)".to_string()
                } else {
                    eprintln!("⚙️ PHPCS LSP: Using configured standard: {}", standard);
                    cmd.arg(format!("--standard={}", standard));
                    format!(" with standard '{}'", standard)
                }
            } else {
                eprintln!("🎯 PHPCS LSP: Using PHPCS default standard (no --standard flag)");
                " with default standard".to_string()
            }
        } else {
            " (failed to read standard)".to_string()
        };

        // Always use stdin to avoid file system reads
        // Add --stdin-path to provide the actual filename to PHPCS
        let stdin_path_info = if let Ok(file_path) = uri.to_file_path() {
            cmd.arg(format!("--stdin-path={}", file_path.display()));
            format!(" (stdin-path: {})", file_path.display())
        } else {
            " (stdin-path: not available)".to_string()
        };
        cmd.arg("-");

        eprintln!(
            "🚀 PHPCS LSP: Running PHPCS on {}{}{}",
            file_name, standard_info, stdin_path_info
        );
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true); // Ensure process is killed if dropped

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                eprintln!(
                    "❌ PHPCS LSP: Failed to spawn PHPCS for {}: {}",
                    file_name, e
                );
                return Err(anyhow::anyhow!("PHPCS error: {}", e));
            }
        };

        // Async write to stdin
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            match timeout(Duration::from_secs(5), stdin.write_all(text.as_bytes())).await {
                Ok(Ok(_)) => {
                    drop(stdin); // Close stdin to signal EOF
                }
                Ok(Err(e)) => {
                    eprintln!(
                        "⚠️ PHPCS LSP: Failed to write {} bytes to stdin for {}: {}",
                        text.len(),
                        file_name,
                        e
                    );
                    child.kill().await.ok();
                    return Err(anyhow::anyhow!(
                        "Failed to send content to PHPCS for {}: {}",
                        file_name,
                        e
                    ));
                }
                Err(_) => {
                    eprintln!(
                        "⏱️ PHPCS LSP: Timeout writing {} bytes to PHPCS stdin for {} (>5s)",
                        text.len(),
                        file_name
                    );
                    child.kill().await.ok();
                    return Err(anyhow::anyhow!(
                        "Timeout writing to PHPCS for {} after 5 seconds",
                        file_name
                    ));
                }
            }
        }

        // Wait for output with timeout (10 seconds for PHPCS execution)
        let output = match timeout(Duration::from_secs(10), child.wait_with_output()).await {
            Ok(Ok(output)) => {
                let elapsed = start_time.elapsed();
                eprintln!(
                    "⚡ PHPCS LSP: Process completed for {} in {:.2}s",
                    file_name,
                    elapsed.as_secs_f64()
                );
                output
            }
            Ok(Err(e)) => {
                let elapsed = start_time.elapsed();
                eprintln!(
                    "❌ PHPCS LSP: PHPCS process error for {} after {:.2}s: {}",
                    file_name,
                    elapsed.as_secs_f64(),
                    e
                );
                return Err(anyhow::anyhow!(
                    "PHPCS process error for {}: {}",
                    file_name,
                    e
                ));
            }
            Err(_) => {
                eprintln!(
                    "⏱️ PHPCS LSP: PHPCS timeout for {} (>10s) with {} bytes of content",
                    file_name,
                    text.len()
                );
                // Process will be killed automatically due to kill_on_drop(true)
                return Err(anyhow::anyhow!(
                    "PHPCS execution timeout for {} after 10 seconds",
                    file_name
                ));
            }
        };
        let raw_output = String::from_utf8_lossy(&output.stdout);

        // Permit is automatically released when it goes out of scope
        drop(_permit);

        let diagnostics = self.parse_phpcs_output(&raw_output, uri).await?;

        // Log results with timing
        let total_time = start_time.elapsed();
        let issue_count = diagnostics.len();
        if issue_count == 0 {
            eprintln!(
                "✅ PHPCS LSP: {} is clean! No issues found (took {:.2}s)",
                file_name,
                total_time.as_secs_f64()
            );
        } else {
            let errors = diagnostics
                .iter()
                .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
                .count();
            let warnings = diagnostics
                .iter()
                .filter(|d| d.severity == Some(DiagnosticSeverity::WARNING))
                .count();
            let infos = diagnostics
                .iter()
                .filter(|d| d.severity == Some(DiagnosticSeverity::INFORMATION))
                .count();

            eprintln!("📊 PHPCS LSP: {} issues found in {}: {} errors, {} warnings, {} info (took {:.2}s)",
                issue_count, file_name, errors, warnings, infos, total_time.as_secs_f64());
        }

        Ok(diagnostics)
    }

    async fn parse_phpcs_output(&self, json_output: &str, uri: &Url) -> Result<Vec<Diagnostic>> {
        // Early return if empty output
        if json_output.trim().is_empty() {
            return Ok(vec![]);
        }

        let mut diagnostics = Vec::with_capacity(10); // Pre-allocate for common case

        let phpcs_result: serde_json::Value = match serde_json::from_str(json_output) {
            Ok(result) => result,
            Err(_) => return Ok(vec![]),
        };

        if let Some(files) = phpcs_result.get("files").and_then(|f| f.as_object()) {
            for (_, file_data) in files {
                if let Some(messages) = file_data.get("messages").and_then(|m| m.as_array()) {
                    for message in messages {
                        if let Some(diagnostic) =
                            self.convert_message_to_diagnostic(message, uri).await
                        {
                            diagnostics.push(diagnostic);
                        }
                    }
                }
            }
        }

        Ok(diagnostics)
    }

    async fn convert_message_to_diagnostic(
        &self,
        message: &serde_json::Value,
        uri: &Url,
    ) -> Option<Diagnostic> {
        let line = message.get("line")?.as_u64()? as u32;
        let column = message.get("column")?.as_u64()? as u32;
        let msg = message.get("message")?.as_str()?;
        let severity_str = message.get("type")?.as_str()?;
        let source = message.get("source")?.as_str().unwrap_or("");
        let fixable = message.get("fixable")?.as_bool().unwrap_or(false);

        let severity = match severity_str {
            "ERROR" => DiagnosticSeverity::ERROR,
            "WARNING" => DiagnosticSeverity::WARNING,
            _ => DiagnosticSeverity::INFORMATION,
        };

        // Convert to 0-based indexing for LSP
        let line = if line > 0 { line - 1 } else { 0 };
        let column = if column > 0 { column - 1 } else { 0 };

        // Determine if this is a line-level or tag-level issue
        let is_line_level = msg.contains("Line exceeds")
            || msg.contains("line is too long")
            || msg.contains("Whitespace found at end of line")
            || msg.contains("Line indented incorrectly")
            || msg.contains("separated by a single blank line")
            || msg.contains("blocks must be separated")
            || source.contains("Generic.Files.LineLength")
            || source.contains("Generic.WhiteSpace.DisallowTabIndent")
            || source.contains("Squiz.WhiteSpace.SuperfluousWhitespace")
            || source.contains("PSR12.Files.FileHeader.SpacingAfterBlock");

        let is_tag_level = msg.contains("closing tag")
            || msg.contains("Opening PHP tag")
            || msg.contains("<?php")
            || msg.contains("?>")
            || source.contains("PSR2.Files.ClosingTag")
            || source.contains("PSR12.Files.OpenTag");

        // Get the line content from the stored document
        let range = if let Ok(docs) = self.open_docs.read() {
            if let Some(compressed_doc) = docs.get(uri) {
                // Decompress to get line content
                if let Ok(content) = self.decompress_document(compressed_doc) {
                    if let Some(line_content) = content.lines().nth(line as usize) {
                        if is_line_level {
                            // Underline from first non-whitespace character to end of line
                            let first_non_whitespace = line_content
                                .chars()
                                .position(|c| !c.is_whitespace())
                                .unwrap_or(0)
                                as u32;
                            Range {
                                start: Position {
                                    line,
                                    character: first_non_whitespace,
                                },
                                end: Position {
                                    line,
                                    character: line_content.len() as u32,
                                },
                            }
                        } else if is_tag_level {
                            // Find and underline the PHP tag
                            self.find_tag_range(line_content, line, column)
                        } else {
                            // Normal token-based underlining
                            self.find_token_range(line_content, line, column)
                        }
                    } else {
                        // Fallback if line not found
                        Range {
                            start: Position {
                                line,
                                character: column,
                            },
                            end: Position {
                                line,
                                character: column + 1,
                            },
                        }
                    }
                } else {
                    // Fallback if decompression fails
                    Range {
                        start: Position {
                            line,
                            character: column,
                        },
                        end: Position {
                            line,
                            character: column + 1,
                        },
                    }
                }
            } else {
                // Fallback if no document content
                Range {
                    start: Position {
                        line,
                        character: column,
                    },
                    end: Position {
                        line,
                        character: column + 1,
                    },
                }
            }
        } else {
            // Fallback if lock fails
            Range {
                start: Position {
                    line,
                    character: column,
                },
                end: Position {
                    line,
                    character: column + 1,
                },
            }
        };

        // Create enhanced source with standard information
        let enhanced_source = "phpcs".to_string();

        // Prefix fixable messages with wrench emoji to indicate quick fix available
        let display_message = if fixable {
            format!("🛠️ {}", msg)
        } else {
            msg.to_string()
        };

        // Generate documentation URL for the sniff
        let code_description = generate_phpcs_doc_url(source).map(|href| CodeDescription { href });

        // Extract related information for context
        let related_information = self.extract_related_information(msg, uri, line);

        // Store additional data for potential future features
        let data = serde_json::json!({
            "fixable": fixable,
            "phpcs_source": source,
            "phpcs_severity": message.get("severity")
        });

        Some(Diagnostic {
            range,
            severity: Some(severity),
            code: if !source.is_empty() {
                Some(NumberOrString::String(source.to_string()))
            } else {
                None
            },
            source: Some(enhanced_source),
            message: display_message,
            related_information,
            tags: None,
            code_description,
            data: Some(data),
        })
    }

    /// Extract related information from PHPCS messages that reference other locations.
    ///
    /// This provides additional context for certain error types:
    /// - Opening/closing brace mismatches
    /// - Expected vs found comparisons
    /// - Duplicate declarations
    fn extract_related_information(
        &self,
        msg: &str,
        uri: &Url,
        _current_line: u32,
    ) -> Option<Vec<DiagnosticRelatedInformation>> {
        let mut related = Vec::new();

        // Pattern: "Opening brace" / "Closing brace" errors - provide context
        if msg.contains("Opening brace") || msg.contains("Closing brace") {
            // Add a hint about brace placement rules
            related.push(DiagnosticRelatedInformation {
                location: Location {
                    uri: uri.clone(),
                    range: Range {
                        start: Position { line: 0, character: 0 },
                        end: Position { line: 0, character: 0 },
                    },
                },
                message: "PSR-12 requires specific brace placement for control structures and declarations".to_string(),
            });
        }

        // Pattern: Namespace/use statement ordering issues
        if msg.contains("use statement") || msg.contains("namespace") {
            related.push(DiagnosticRelatedInformation {
                location: Location {
                    uri: uri.clone(),
                    range: Range {
                        start: Position { line: 0, character: 0 },
                        end: Position { line: 0, character: 0 },
                    },
                },
                message: "PSR-12 requires specific ordering: namespace → use statements → class declaration".to_string(),
            });
        }

        if related.is_empty() {
            None
        } else {
            Some(related)
        }
    }

    fn find_tag_range(&self, line_content: &str, line: u32, column: u32) -> Range {
        let col = column as usize;

        // Look for all possible PHP tags and find the one closest to column position
        let mut best_match: Option<(usize, usize)> = None; // (start_pos, end_pos)

        // Check for opening tag "<?php"
        if let Some(pos) = line_content.find("<?php") {
            let distance = if col >= pos && col <= pos + 5 {
                0
            } else {
                col.abs_diff(pos)
            };
            if best_match.is_none() || distance <= col.abs_diff(best_match.unwrap().0) {
                best_match = Some((pos, pos + 5));
            }
        }

        // Check for closing tag "?>"
        if let Some(pos) = line_content.find("?>") {
            let distance = if col >= pos && col <= pos + 2 {
                0
            } else {
                col.abs_diff(pos)
            };
            if best_match.is_none() || distance < col.abs_diff(best_match.unwrap().0) {
                best_match = Some((pos, pos + 2));
            }
        }

        // Check for short opening tag "<?" (but only if not part of "<?php")
        let mut search_pos = 0;
        while let Some(pos) = line_content[search_pos..].find("<?") {
            let actual_pos = search_pos + pos;
            // Make sure it's not part of "<?php"
            if !line_content[actual_pos..].starts_with("<?php") {
                let distance = if col >= actual_pos && col <= actual_pos + 2 {
                    0
                } else {
                    col.abs_diff(actual_pos)
                };
                if best_match.is_none() || distance < col.abs_diff(best_match.unwrap().0) {
                    best_match = Some((actual_pos, actual_pos + 2));
                }
            }
            search_pos = actual_pos + 2;
            if search_pos >= line_content.len() {
                break;
            }
        }

        if let Some((start, end)) = best_match {
            Range {
                start: Position {
                    line,
                    character: start as u32,
                },
                end: Position {
                    line,
                    character: end as u32,
                },
            }
        } else {
            // If no tag found, underline from column position with a reasonable default
            Range {
                start: Position {
                    line,
                    character: column,
                },
                end: Position {
                    line,
                    character: column.saturating_add(2),
                },
            }
        }
    }

    fn find_token_range(&self, line_content: &str, line: u32, column: u32) -> Range {
        let chars: Vec<char> = line_content.chars().collect();
        let col = column as usize;

        // If column is beyond line length, use end of line
        if col >= chars.len() {
            return Range {
                start: Position {
                    line,
                    character: column.saturating_sub(1),
                },
                end: Position {
                    line,
                    character: column,
                },
            };
        }

        // Find token boundaries
        let mut start = col;
        let mut end = col;

        // Determine token type at column position
        let ch = chars[col];

        if ch.is_alphanumeric() || ch == '_' || ch == '$' {
            // Identifier or variable token
            while start > 0
                && (chars[start - 1].is_alphanumeric()
                    || chars[start - 1] == '_'
                    || chars[start - 1] == '$')
            {
                start -= 1;
            }
            while end < chars.len() && (chars[end].is_alphanumeric() || chars[end] == '_') {
                end += 1;
            }
        } else if ch.is_whitespace() {
            // For whitespace issues, highlight the space
            while end < chars.len() && chars[end].is_whitespace() {
                end += 1;
            }
        } else {
            // Operator or punctuation
            let operator_chars = [
                '=', '!', '<', '>', '+', '-', '*', '/', '%', '&', '|', '^', '~',
            ];
            if operator_chars.contains(&ch) {
                // Check for multi-character operators
                while end < chars.len() && operator_chars.contains(&chars[end]) {
                    end += 1;
                }
                // Also check backward for multi-char operators
                while start > 0 && operator_chars.contains(&chars[start - 1]) {
                    start -= 1;
                }
            } else {
                // Single character token (parenthesis, bracket, semicolon, etc.)
                end = col + 1;
            }
        }

        // Ensure we have at least one character highlighted
        if start == end {
            end = (col + 1).min(chars.len());
        }

        Range {
            start: Position {
                line,
                character: start as u32,
            },
            end: Position {
                line,
                character: end as u32,
            },
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for PhpcsLanguageServer {
    async fn initialize(&self, params: InitializeParams) -> LspResult<InitializeResult> {
        eprintln!("🚀 PHPCS LSP: Server initializing...");
        eprintln!("🔧 PHPCS LSP: Client info: {:?}", params.client_info);

        // Determine workspace root for config file lookup
        let workspace_root = params
            .root_uri
            .as_ref()
            .and_then(|uri| uri.to_file_path().ok());

        if let Some(ref root) = workspace_root {
            eprintln!("📁 PHPCS LSP: Workspace root: {}", root.display());
        } else {
            eprintln!("❌ PHPCS LSP: No workspace root detected");
        }

        // Store workspace root for PHPCS path detection
        if let Ok(mut workspace_guard) = self.workspace_root.write() {
            *workspace_guard = workspace_root.clone();
        }

        if let Some(options) = params.initialization_options {
            // Parse initialization options
            eprintln!("📦 PHPCS LSP: Processing initialization options from extension");
            match serde_json::from_value::<InitializationOptions>(options.clone()) {
                Ok(init_options) => {
                    if let Some(standard) = init_options.standard {
                        eprintln!("⚙️ PHPCS LSP: Extension provided standard: '{}'", standard);
                        if let Ok(mut standard_guard) = self.standard.write() {
                            *standard_guard = Some(standard.clone());
                        }
                    } else {
                        eprintln!("🎯 PHPCS LSP: No standard provided by extension - will use PHPCS defaults");
                    }

                    if let Some(phpcs_path) = init_options.phpcs_path {
                        eprintln!(
                            "🎯 PHPCS LSP: Extension provided phpcsPath: '{}'",
                            phpcs_path
                        );
                        if let Ok(mut path_guard) = self.user_phpcs_path.write() {
                            *path_guard = Some(phpcs_path.clone());
                        }
                    }

                    if let Some(phpcbf_path) = init_options.phpcbf_path {
                        eprintln!(
                            "🎯 PHPCS LSP: Extension provided phpcbfPath: '{}'",
                            phpcbf_path
                        );
                        if let Ok(mut path_guard) = self.user_phpcbf_path.write() {
                            *path_guard = Some(phpcbf_path.clone());
                        }
                    }
                }
                Err(e) => {
                    eprintln!(
                        "❌ PHPCS LSP: Failed to parse initialization options: {}",
                        e
                    );
                }
            }
        } else {
            // No initialization options provided, discover from workspace
            self.discover_standard(workspace_root.as_deref());
        }

        // Log final initialization state
        if let Ok(standard_guard) = self.standard.read() {
            match &*standard_guard {
                Some(standard) => {
                    eprintln!("🎯 PHPCS LSP: Initialized with standard: '{}'", standard)
                }
                None => eprintln!(
                    "🎯 PHPCS LSP: Initialized with no explicit standard (PHPCS defaults)"
                ),
            }
        }

        eprintln!("✅ PHPCS LSP: Server initialization complete!");

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                diagnostic_provider: Some(DiagnosticServerCapabilities::Options(
                    DiagnosticOptions {
                        identifier: Some("phpcs".to_string()),
                        inter_file_dependencies: false,
                        workspace_diagnostics: false,
                        ..Default::default()
                    },
                )),
                code_action_provider: Some(CodeActionProviderCapability::Options(
                    CodeActionOptions {
                        code_action_kinds: Some(vec![
                            CodeActionKind::QUICKFIX,
                            CodeActionKind::new("source.fixAll.phpcs"),
                        ]),
                        resolve_provider: Some(false),
                        ..Default::default()
                    },
                )),
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                        supported: Some(true),
                        change_notifications: Some(OneOf::Left(true)),
                    }),
                    file_operations: None,
                }),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        eprintln!("🎉 PHPCS LSP: Server is ready and operational!");
        // Pre-cache the PHPCS path on initialization
        let _ = self.get_phpcs_path();
        eprintln!("🚀 PHPCS LSP: Ready to lint PHP files!");
    }

    async fn shutdown(&self) -> LspResult<()> {
        eprintln!("🔄 PHPCS LSP: Shutting down, clearing caches...");

        // Clear all cached data on shutdown
        if let Ok(mut docs) = self.open_docs.write() {
            docs.clear();
        }
        if let Ok(mut cache) = self.results_cache.write() {
            cache.clear();
        }

        // Reset memory counter
        self.total_memory_usage.store(0, Ordering::Relaxed);

        eprintln!("✅ PHPCS LSP: Shutdown complete");
        Ok(())
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        // Clear document from memory to prevent memory leaks
        let uri = params.text_document.uri;

        // Remove compressed document and update memory tracking
        if let Ok(mut docs) = self.open_docs.write() {
            if let Some(doc) = docs.remove(&uri) {
                let freed_memory = doc.compressed_data.len();
                self.total_memory_usage
                    .fetch_sub(freed_memory, Ordering::Relaxed);
            }
        }

        // Clear cached results
        if let Ok(mut cache) = self.results_cache.write() {
            cache.remove(&uri);
        }

        // Clear diagnostics for closed file
        let _ = self.client.publish_diagnostics(uri, vec![], None).await;
    }

    async fn did_change_workspace_folders(&self, _params: DidChangeWorkspaceFoldersParams) {
        // Clear cached auto-detected PHPCS path when workspace changes
        // Note: We keep user-configured paths as they should work across workspaces
        if let Ok(mut guard) = self.phpcs_path.write() {
            *guard = None;
        }

        // Clear results cache as paths may have changed
        if let Ok(mut cache) = self.results_cache.write() {
            cache.clear();
        }

        eprintln!(
            "🔄 PHPCS LSP: Workspace changed, cleared auto-detection cache (keeping user paths)"
        );

        // Re-detect PHPCS configuration for new workspace
        // This will be done lazily on next PHPCS run
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        eprintln!("🔄 PHPCS LSP: Configuration change detected!");

        let mut path_changed = false;

        // Parse the settings
        if let Some(settings) = params.settings.as_object() {
            // Look for phpcs settings
            if let Some(phpcs_settings) = settings.get("phpcs") {
                // Try to parse as PhpcsSettings
                if let Ok(parsed_settings) =
                    serde_json::from_value::<PhpcsSettings>(phpcs_settings.clone())
                {
                    // Update the standard if provided
                    if let Some(new_standard) = parsed_settings.standard {
                        eprintln!(
                            "⚙️ PHPCS LSP: Runtime config change - standard: '{}'",
                            new_standard
                        );
                        if let Ok(mut standard_guard) = self.standard.write() {
                            *standard_guard = Some(new_standard);
                        }
                    }

                    // Update custom PHPCS path if provided
                    if let Some(new_phpcs_path) = parsed_settings.phpcs_path {
                        eprintln!(
                            "📂 PHPCS LSP: Runtime config change - phpcs_path: '{}'",
                            new_phpcs_path
                        );
                        if let Ok(mut path_guard) = self.user_phpcs_path.write() {
                            let old_path = path_guard.clone();
                            *path_guard = Some(new_phpcs_path.clone());
                            if old_path.as_deref() != Some(&new_phpcs_path) {
                                path_changed = true;
                            }
                        }
                    }

                    // Update custom PHPCBF path if provided
                    if let Some(new_phpcbf_path) = parsed_settings.phpcbf_path {
                        eprintln!(
                            "📂 PHPCS LSP: Runtime config change - phpcbf_path: '{}'",
                            new_phpcbf_path
                        );
                        if let Ok(mut path_guard) = self.user_phpcbf_path.write() {
                            *path_guard = Some(new_phpcbf_path);
                            path_changed = true; // Always clear cache when phpcbf path changes
                        }
                    }
                }
            }

            // Also check for standard directly in settings (for compatibility)
            if let Some(standard_value) = settings.get("standard") {
                if let Some(new_standard) = standard_value.as_str() {
                    if let Ok(mut standard_guard) = self.standard.write() {
                        *standard_guard = Some(new_standard.to_string());
                    }
                }
            }

            if let Some(phpcs_path_value) = settings.get("phpcs_path") {
                if let Some(new_phpcs_path) = phpcs_path_value.as_str() {
                    eprintln!(
                        "📂 PHPCS LSP: Runtime config change (compat) - phpcs_path: '{}'",
                        new_phpcs_path
                    );
                    if let Ok(mut path_guard) = self.user_phpcs_path.write() {
                        let old_path = path_guard.clone();
                        *path_guard = Some(new_phpcs_path.to_string());
                        if old_path.as_deref() != Some(new_phpcs_path) {
                            path_changed = true;
                        }
                    }
                }
            }

            if let Some(phpcbf_path_value) = settings.get("phpcbf_path") {
                if let Some(new_phpcbf_path) = phpcbf_path_value.as_str() {
                    eprintln!(
                        "📂 PHPCS LSP: Runtime config change (compat) - phpcbf_path: '{}'",
                        new_phpcbf_path
                    );
                    if let Ok(mut path_guard) = self.user_phpcbf_path.write() {
                        *path_guard = Some(new_phpcbf_path.to_string());
                        path_changed = true;
                    }
                }
            }
        }

        // Clear auto-detection cache if paths changed
        if path_changed {
            if let Ok(mut guard) = self.phpcs_path.write() {
                *guard = None;
                eprintln!("🗑️ PHPCS LSP: Cleared auto-detection cache due to path changes");
            }
        }

        // Clear results cache to force re-linting with new config
        if let Ok(mut cache) = self.results_cache.write() {
            cache.clear();
            eprintln!("🗑️ PHPCS LSP: Cleared results cache after config change");
        }

        // Note: Documents will be re-linted on next diagnostic() call
        // No need to proactively re-run PHPCS on all files
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let text = params.text_document.text;

        // Compress and store the document
        let compressed_doc = self.compress_document(&text);

        {
            let mut docs = self.open_docs.write().unwrap();
            docs.insert(uri.clone(), compressed_doc);
        }

        // Invalidate any cached results for this file
        if let Ok(mut cache) = self.results_cache.write() {
            cache.remove(&uri);
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.clone();

        // With FULL sync, we always get the complete document content
        if let Some(change) = params.content_changes.first() {
            // Remove old compressed document to update memory tracking
            let old_size = if let Ok(docs) = self.open_docs.read() {
                docs.get(&uri).map(|doc| doc.compressed_data.len())
            } else {
                None
            };

            if let Some(size) = old_size {
                self.total_memory_usage.fetch_sub(size, Ordering::Relaxed);
            }

            // Compress and store new content
            let compressed_doc = self.compress_document(&change.text);

            let mut docs = self.open_docs.write().unwrap();
            docs.insert(uri.clone(), compressed_doc);

            // Invalidate cached results since content changed
            if let Ok(mut cache) = self.results_cache.write() {
                cache.remove(&uri);
            }
        }

        // Diagnostics will be provided via diagnostic() method
        // This reduces unnecessary PHPCS runs during rapid typing
    }

    async fn did_save(&self, _params: DidSaveTextDocumentParams) {
        // Diagnostics will be provided via diagnostic() method calls from Zed
        // We don't need to proactively run PHPCS here to avoid duplicate linting
    }

    async fn diagnostic(
        &self,
        params: DocumentDiagnosticParams,
    ) -> LspResult<DocumentDiagnosticReportResult> {
        let uri = params.text_document.uri;
        let file_name = uri
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .unwrap_or("unknown");

        if let Ok(file_path) = uri.to_file_path() {
            if let Some(path_str) = file_path.to_str() {
                // First check if we have cached results
                if let Ok(cache) = self.results_cache.read() {
                    if let Some(cached) = cache.get(&uri) {
                        // Check if client has the same version
                        if let Some(previous_result_id) = params.previous_result_id {
                            if previous_result_id == cached.result_id {
                                return Ok(DocumentDiagnosticReportResult::Report(
                                    DocumentDiagnosticReport::Unchanged(
                                        RelatedUnchangedDocumentDiagnosticReport {
                                            unchanged_document_diagnostic_report:
                                                UnchangedDocumentDiagnosticReport {
                                                    result_id: cached.result_id.clone(),
                                                },
                                            related_documents: None,
                                        },
                                    ),
                                ));
                            }
                        }

                        // Return cached diagnostics
                        return Ok(DocumentDiagnosticReportResult::Report(
                            DocumentDiagnosticReport::Full(RelatedFullDocumentDiagnosticReport {
                                full_document_diagnostic_report: FullDocumentDiagnosticReport {
                                    result_id: Some(cached.result_id.clone()),
                                    items: cached.diagnostics.clone(),
                                },
                                related_documents: None,
                            }),
                        ));
                    }
                }

                // No cached results, need to get content and run PHPCS
                let compressed_doc = {
                    let docs = self.open_docs.read().unwrap();
                    docs.get(&uri).cloned()
                };

                // Handle missing document (rare edge case)
                let compressed_doc = if compressed_doc.is_none() {
                    // Try to read from disk as fallback
                    match fs::read_to_string(path_str) {
                        Ok(file_content) => {
                            eprintln!(
                                "⚠️ PHPCS LSP: Document not in memory, reading from disk: {}",
                                file_name
                            );
                            let compressed = self.compress_document(&file_content);
                            let mut docs = self.open_docs.write().unwrap();
                            docs.insert(uri.clone(), compressed.clone());
                            Some(compressed)
                        }
                        Err(e) => {
                            eprintln!("❌ PHPCS LSP: Failed to read file {}: {}", file_name, e);
                            None
                        }
                    }
                } else {
                    compressed_doc
                };

                if let Some(compressed_doc) = compressed_doc {
                    // Decompress content
                    let content = match self.decompress_document(&compressed_doc) {
                        Ok(content) => content,
                        Err(e) => {
                            eprintln!("❌ PHPCS LSP: Failed to decompress {}: {}", file_name, e);
                            return Ok(DocumentDiagnosticReportResult::Report(
                                DocumentDiagnosticReport::Full(
                                    RelatedFullDocumentDiagnosticReport {
                                        full_document_diagnostic_report:
                                            FullDocumentDiagnosticReport {
                                                result_id: None,
                                                items: vec![],
                                            },
                                        related_documents: None,
                                    },
                                ),
                            ));
                        }
                    };

                    let version_id = compressed_doc.checksum.clone();
                    eprintln!(
                        "📋 PHPCS LSP: Running PHPCS for {} with version: {}",
                        file_name,
                        &version_id[..16]
                    );

                    // Run PHPCS
                    if let Ok(diagnostics) = self.run_phpcs(&uri, path_str, Some(&content)).await {
                        eprintln!(
                            "📊 PHPCS LSP: Generated {} diagnostics for {}",
                            diagnostics.len(),
                            file_name
                        );

                        // Cache the results
                        let cached_results = CachedResults {
                            diagnostics: diagnostics.clone(),
                            result_id: version_id.clone(),
                        };

                        if let Ok(mut cache) = self.results_cache.write() {
                            cache.insert(uri.clone(), cached_results);
                        }

                        return Ok(DocumentDiagnosticReportResult::Report(
                            DocumentDiagnosticReport::Full(RelatedFullDocumentDiagnosticReport {
                                full_document_diagnostic_report: FullDocumentDiagnosticReport {
                                    result_id: Some(version_id),
                                    items: diagnostics,
                                },
                                related_documents: None,
                            }),
                        ));
                    }
                }
            }
        }

        // Fallback: return empty diagnostics with no version
        eprintln!(
            "⚠️ PHPCS LSP: Unable to generate diagnostics for {}",
            file_name
        );
        Ok(DocumentDiagnosticReportResult::Report(
            DocumentDiagnosticReport::Full(RelatedFullDocumentDiagnosticReport {
                full_document_diagnostic_report: FullDocumentDiagnosticReport {
                    result_id: None,
                    items: vec![],
                },
                related_documents: None,
            }),
        ))
    }

    async fn code_action(&self, params: CodeActionParams) -> LspResult<Option<CodeActionResponse>> {
        let uri = params.text_document.uri;
        let file_name = uri
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .unwrap_or("unknown");

        // Check if this is a source.fixAll request (e.g., from code_actions_on_format)
        // Only match exact "source.fixAll" or "source.fixAll.phpcs", not broad kinds like "" or "source"
        let is_fix_all_request = params.context.only.as_ref().is_some_and(|kinds| {
            kinds.iter().any(|k| {
                let kind_str = k.as_str();
                kind_str == "source.fixAll" || kind_str == "source.fixAll.phpcs"
            })
        });

        // Get the document content first
        let content = {
            let docs = self
                .open_docs
                .read()
                .map_err(|_| tower_lsp::jsonrpc::Error::internal_error())?;

            if let Some(compressed_doc) = docs.get(&uri) {
                self.decompress_document(compressed_doc).ok()
            } else {
                None
            }
        };

        let Some(content) = content else {
            eprintln!("❌ PHPCS LSP: No document content for {}", file_name);
            return Ok(Some(vec![]));
        };

        // Handle source.fixAll.phpcs requests (from code_actions_on_format)
        if is_fix_all_request {
            eprintln!(
                "🔧 PHPCS LSP: source.fixAll.phpcs requested for {}",
                file_name
            );

            // Deduplicate: after returning a fixAll edit, skip all requests for this URI
            // for a cooldown period to prevent multiple conflicting edits
            if let Ok(cooldown) = self.fix_all_cooldown.read() {
                if let Some(sent_at) = cooldown.get(&uri) {
                    if sent_at.elapsed() < Duration::from_secs(5) {
                        eprintln!("⏭️ PHPCS LSP: Skipping source.fixAll.phpcs for {} (cooldown, {:.1}s since last edit)", file_name, sent_at.elapsed().as_secs_f64());
                        return Ok(Some(vec![]));
                    }
                }
            }

            match self.run_phpcbf(&uri, &content, None).await {
                Ok(fixed_content) => {
                    if fixed_content == content {
                        eprintln!("✅ PHPCS LSP: No fixable issues for {}", file_name);
                        return Ok(Some(vec![]));
                    }

                    // Start cooldown to prevent duplicate edits
                    if let Ok(mut cooldown) = self.fix_all_cooldown.write() {
                        cooldown.insert(uri.clone(), Instant::now());
                    }

                    let line_count = content.lines().count() as u32;
                    let last_line_len = content.lines().last().map(|l| l.len() as u32).unwrap_or(0);

                    let edit = TextEdit {
                        range: Range {
                            start: Position {
                                line: 0,
                                character: 0,
                            },
                            end: Position {
                                line: line_count,
                                character: last_line_len,
                            },
                        },
                        new_text: fixed_content,
                    };

                    let mut changes = HashMap::new();
                    changes.insert(uri.clone(), vec![edit]);

                    let workspace_edit = WorkspaceEdit {
                        changes: Some(changes),
                        document_changes: None,
                        change_annotations: None,
                    };

                    let action = CodeActionOrCommand::CodeAction(CodeAction {
                        title: "Fix all PHPCS issues".to_string(),
                        kind: Some(CodeActionKind::new("source.fixAll.phpcs")),
                        diagnostics: None,
                        edit: Some(workspace_edit),
                        command: None,
                        is_preferred: Some(true),
                        disabled: None,
                        data: None,
                    });

                    eprintln!(
                        "✅ PHPCS LSP: Returning source.fixAll.phpcs action for {}",
                        file_name
                    );
                    return Ok(Some(vec![action]));
                }
                Err(e) => {
                    eprintln!(
                        "❌ PHPCS LSP: source.fixAll.phpcs failed for {}: {}",
                        file_name, e
                    );
                    return Ok(Some(vec![]));
                }
            }
        }

        // Check if any diagnostics in the ENTIRE file are fixable
        let file_has_fixable = {
            let cache = self
                .results_cache
                .read()
                .map_err(|_| tower_lsp::jsonrpc::Error::internal_error())?;

            if let Some(cached) = cache.get(&uri) {
                cached.diagnostics.iter().any(|diag| {
                    diag.data
                        .as_ref()
                        .and_then(|d| d.get("fixable"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                })
            } else {
                false
            }
        };

        if !file_has_fixable {
            return Ok(Some(vec![]));
        }

        let mut code_actions: Vec<CodeActionOrCommand> = Vec::new();

        // Collect fixable diagnostics at cursor position
        let fixable_at_cursor: Vec<_> = params
            .context
            .diagnostics
            .iter()
            .filter(|diag| {
                diag.source.as_deref() == Some("phpcs")
                    && diag
                        .data
                        .as_ref()
                        .and_then(|d| d.get("fixable"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
            })
            .collect();

        // Track unique sniff types for "Fix all X issues" actions
        let mut seen_sniffs: std::collections::HashSet<String> = std::collections::HashSet::new();

        // 1. Add single-line fix actions for each fixable diagnostic at cursor
        for diag in &fixable_at_cursor {
            // Get the full message code (e.g., "Squiz.ControlStructures.ControlSignature.SpaceAfterCloseBrace")
            let full_code = diag.code.as_ref().map(|c| match c {
                tower_lsp::lsp_types::NumberOrString::String(s) => s.clone(),
                tower_lsp::lsp_types::NumberOrString::Number(n) => n.to_string(),
            });

            // Extract sniff code (first 3 parts) from message code (4 parts)
            // "Squiz.ControlStructures.ControlSignature.SpaceAfterCloseBrace" -> "Squiz.ControlStructures.ControlSignature"
            let sniff_code = full_code.as_ref().and_then(|code| {
                let parts: Vec<&str> = code.split('.').collect();
                if parts.len() >= 3 {
                    Some(parts[..3].join("."))
                } else {
                    None
                }
            });

            if let Some(ref sniff) = sniff_code {
                // Run phpcbf with sniff filter for single-line fix
                if let Ok(fixed_content) = self.run_phpcbf(&uri, &content, Some(sniff)).await {
                    let target_line = diag.range.start.line;
                    let line_edits = Self::compute_line_edits(
                        &content,
                        &fixed_content,
                        target_line,
                        diag.range.end.line,
                    );

                    if !line_edits.is_empty() {
                        let mut changes = HashMap::new();
                        changes.insert(uri.clone(), line_edits);

                        let workspace_edit = WorkspaceEdit {
                            changes: Some(changes),
                            document_changes: None,
                            change_annotations: None,
                        };

                        code_actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                            title: format!("🎯 Fix this {} issue", sniff),
                            kind: Some(CodeActionKind::QUICKFIX),
                            diagnostics: Some(vec![(*diag).clone()]),
                            edit: Some(workspace_edit),
                            command: None,
                            is_preferred: Some(false),
                            disabled: None,
                            data: None,
                        }));
                    }
                }

                // Track sniff for "Fix all X issues" action
                seen_sniffs.insert(sniff.clone());
            }
        }

        // 2. Add "Fix all [sniff] issues" for each unique sniff type at cursor
        for sniff in &seen_sniffs {
            if let Ok(fixed_content) = self.run_phpcbf(&uri, &content, Some(sniff)).await {
                if fixed_content != content {
                    let line_count = content.lines().count() as u32;
                    let last_line_len = content.lines().last().map(|l| l.len() as u32).unwrap_or(0);

                    let edit = TextEdit {
                        range: Range {
                            start: Position {
                                line: 0,
                                character: 0,
                            },
                            end: Position {
                                line: line_count,
                                character: last_line_len,
                            },
                        },
                        new_text: fixed_content,
                    };

                    let mut changes = HashMap::new();
                    changes.insert(uri.clone(), vec![edit]);

                    let workspace_edit = WorkspaceEdit {
                        changes: Some(changes),
                        document_changes: None,
                        change_annotations: None,
                    };

                    // Get short sniff name (last part after last dot)
                    let short_sniff = sniff.rsplit('.').next().unwrap_or(sniff);

                    code_actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                        title: format!("🛠️ Fix all {} issues", short_sniff),
                        kind: Some(CodeActionKind::QUICKFIX),
                        diagnostics: None,
                        edit: Some(workspace_edit),
                        command: None,
                        is_preferred: Some(false),
                        disabled: None,
                        data: None,
                    }));
                }
            }
        }

        // 3. Add "Fix all PHPCS issues" action (always available if file has fixable)
        if let Ok(fixed_content) = self.run_phpcbf(&uri, &content, None).await {
            if fixed_content != content {
                let line_count = content.lines().count() as u32;
                let last_line_len = content.lines().last().map(|l| l.len() as u32).unwrap_or(0);

                let edit = TextEdit {
                    range: Range {
                        start: Position {
                            line: 0,
                            character: 0,
                        },
                        end: Position {
                            line: line_count,
                            character: last_line_len,
                        },
                    },
                    new_text: fixed_content,
                };

                let mut changes = HashMap::new();
                changes.insert(uri.clone(), vec![edit]);

                let workspace_edit = WorkspaceEdit {
                    changes: Some(changes),
                    document_changes: None,
                    change_annotations: None,
                };

                code_actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: "🛠️ Fix all PHPCS issues (phpcbf)".to_string(),
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: None,
                    edit: Some(workspace_edit),
                    command: None,
                    is_preferred: Some(true),
                    disabled: None,
                    data: None,
                }));
            }
        }

        eprintln!(
            "✅ PHPCS LSP: Returning {} code actions for {}",
            code_actions.len(),
            file_name
        );
        Ok(Some(code_actions))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let stdin = stdin();
    let stdout = stdout();

    let (service, socket) = LspService::new(PhpcsLanguageServer::new);
    Server::new(stdin, stdout, socket).serve(service).await;

    Ok(())
}
