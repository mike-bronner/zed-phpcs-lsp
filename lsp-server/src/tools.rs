use std::path::Path;

#[derive(Debug, Clone, Copy)]
pub enum PhpTool {
    Phpcs,
    Phpcbf,
}

impl PhpTool {
    pub fn name(&self) -> &'static str {
        match self {
            PhpTool::Phpcs => "phpcs",
            PhpTool::Phpcbf => "phpcbf",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            PhpTool::Phpcs => "PHPCS",
            PhpTool::Phpcbf => "PHPCBF",
        }
    }

    pub fn vendor_bin(&self) -> &'static str {
        match self {
            PhpTool::Phpcs => "vendor/bin/phpcs",
            PhpTool::Phpcbf => "vendor/bin/phpcbf",
        }
    }

    pub fn phar_name(&self) -> &'static str {
        match self {
            PhpTool::Phpcs => "phpcs.phar",
            PhpTool::Phpcbf => "phpcbf.phar",
        }
    }

    pub fn env_var_name(&self) -> &'static str {
        match self {
            PhpTool::Phpcs => "PHPCS_PATH",
            PhpTool::Phpcbf => "PHPCBF_PATH",
        }
    }
}

/// Check if a command exists in the system PATH
pub fn command_exists(cmd: &str) -> bool {
    #[cfg(unix)]
    {
        std::process::Command::new("which")
            .arg(cmd)
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        std::process::Command::new("where")
            .arg(cmd)
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }
}

/// Detect the path to a PHP tool using the following priority:
/// 1. User-configured path from LSP settings (explicit override wins)
/// 2. Project vendor/bin/{tool} (project-local Composer install)
/// 3. Environment variable (PHPCS_PATH / PHPCBF_PATH)
/// 4. System {tool} (in PATH)
/// 5. Bundled {tool}.phar
/// 6. Fallback to tool name (will fail at runtime if not found)
///
/// The user-configured path is checked first so that an explicit `phpcs_path`
/// setting always takes effect — e.g. on Windows, where the Composer-generated
/// `vendor/bin/phpcs` proxy cannot be spawned directly and a user must point at
/// a working binary. See `plan_spawn` for how the resolved path is executed.
pub fn detect_tool_path(tool: PhpTool, workspace_root: Option<&Path>, user_path: Option<&str>) -> String {
    let display = tool.display_name();
    let name = tool.name();

    // Priority 1: User-configured path (explicit override wins over auto-detection)
    if let Some(path) = user_path {
        if !path.trim().is_empty() {
            eprintln!("🎯 PHPCS LSP: Using user-configured {} path: {}", display, path);
            return path.to_string();
        }
    }

    // Priority 2: Project vendor/bin
    if let Some(workspace_root) = workspace_root {
        let vendor_path = workspace_root.join(tool.vendor_bin());
        eprintln!(
            "🔍 PHPCS LSP: Checking for project {} at: {}",
            display,
            vendor_path.display()
        );

        if vendor_path.exists() {
            eprintln!("✅ PHPCS LSP: Found project-local {}", display);
            return vendor_path.to_string_lossy().to_string();
        }
        eprintln!("❌ PHPCS LSP: No project-local {} found", display);
    }

    // Priority 3: Environment variable
    let env_var = tool.env_var_name();
    eprintln!("🔍 PHPCS LSP: Checking {} env var for {}...", env_var, display);
    if let Ok(path) = std::env::var(env_var) {
        if !path.trim().is_empty() {
            eprintln!("✅ PHPCS LSP: Found {} via {} env var", display, env_var);
            return path;
        }
    }
    eprintln!("❌ PHPCS LSP: No {} env var set", env_var);

    // Priority 4: System command
    eprintln!("🔍 PHPCS LSP: Checking for system {}...", name);
    if command_exists(name) {
        eprintln!("✅ PHPCS LSP: Found system {}", name);
        return name.to_string();
    }
    eprintln!("❌ PHPCS LSP: No system {} found", name);

    // Priority 5: Bundled PHAR
    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(exe_dir) = current_exe.parent() {
            let bundled = exe_dir.join(tool.phar_name());
            eprintln!(
                "🔍 PHPCS LSP: Checking for bundled {} at: {}",
                display,
                bundled.display()
            );

            if bundled.exists() {
                eprintln!("✅ PHPCS LSP: Found bundled {} PHAR", display);
                return bundled.to_string_lossy().to_string();
            }
            eprintln!("❌ PHPCS LSP: No bundled {} found", display);
        }
    }

    // Fallback
    eprintln!(
        "⚠️ PHPCS LSP: No {} found, using '{}' as fallback",
        display, name
    );
    name.to_string()
}

/// How to spawn a resolved tool path: the program to launch and any arguments
/// that must precede the tool's own arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnPlan {
    pub program: String,
    pub prefix_args: Vec<String>,
}

/// Decide how to spawn a resolved tool path on a given platform.
///
/// On Unix the path is launched directly — Composer's `vendor/bin` proxy and the
/// bundled `*.phar` both carry a `#!/usr/bin/env php` shebang and the executable
/// bit, so the kernel runs them via PHP.
///
/// Windows has no shebang mechanism, and `CreateProcess` only accepts real PE
/// executables, so launching the proxy or a `.phar` directly fails with
/// `os error 193` ("%1 is not a valid Win32 application"). Dispatch by kind:
/// - `.exe` → launch directly.
/// - `.bat` / `.cmd` → launch via `cmd /C` (batch files require the interpreter).
/// - a path to anything else (the extensionless Composer proxy, `*.phar`) → run
///   it through `php <path>`.
/// - a bare command name with no path separator (e.g. a system `phpcs` on PATH)
///   → `cmd /C <name>` so the shell can resolve a `.bat`/`.exe` on PATH.
pub fn plan_spawn(tool_path: &str, is_windows: bool) -> SpawnPlan {
    if !is_windows {
        return SpawnPlan {
            program: tool_path.to_string(),
            prefix_args: Vec::new(),
        };
    }

    let extension = Path::new(tool_path)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase());

    match extension.as_deref() {
        Some("exe") => SpawnPlan {
            program: tool_path.to_string(),
            prefix_args: Vec::new(),
        },
        Some("bat") | Some("cmd") => SpawnPlan {
            program: "cmd".to_string(),
            prefix_args: vec!["/C".to_string(), tool_path.to_string()],
        },
        _ => {
            let has_separator = tool_path.contains('/') || tool_path.contains('\\');
            if has_separator {
                // A path to a Composer proxy script or a `.phar` — run it via PHP.
                SpawnPlan {
                    program: "php".to_string(),
                    prefix_args: vec![tool_path.to_string()],
                }
            } else {
                // A bare command name — let cmd resolve it on PATH (it may be a `.bat`).
                SpawnPlan {
                    program: "cmd".to_string(),
                    prefix_args: vec!["/C".to_string(), tool_path.to_string()],
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn direct(path: &str) -> SpawnPlan {
        SpawnPlan {
            program: path.to_string(),
            prefix_args: Vec::new(),
        }
    }

    fn via_cmd(path: &str) -> SpawnPlan {
        SpawnPlan {
            program: "cmd".to_string(),
            prefix_args: vec!["/C".to_string(), path.to_string()],
        }
    }

    fn via_php(path: &str) -> SpawnPlan {
        SpawnPlan {
            program: "php".to_string(),
            prefix_args: vec![path.to_string()],
        }
    }

    #[test]
    fn unix_always_launches_directly() {
        // On Unix every kind of path is spawned as-is (shebang + exec bit do the work).
        for path in [
            "vendor/bin/phpcs",
            "/usr/local/bin/phpcs.phar",
            "phpcs",
            "/some/phpcs.bat",
        ] {
            assert_eq!(plan_spawn(path, false), direct(path), "path: {path}");
        }
    }

    #[test]
    fn windows_runs_exe_directly() {
        assert_eq!(
            plan_spawn(r"C:\tools\phpcs.exe", true),
            direct(r"C:\tools\phpcs.exe")
        );
    }

    #[test]
    fn windows_routes_bat_and_cmd_through_cmd() {
        assert_eq!(
            plan_spawn(r"C:\proj\vendor\bin\phpcs.bat", true),
            via_cmd(r"C:\proj\vendor\bin\phpcs.bat")
        );
        assert_eq!(
            plan_spawn(r"C:\proj\vendor\bin\phpcs.cmd", true),
            via_cmd(r"C:\proj\vendor\bin\phpcs.cmd")
        );
    }

    #[test]
    fn windows_runs_composer_proxy_through_php() {
        // The extensionless Composer proxy (issue #60's failing case) → `php <path>`.
        assert_eq!(
            plan_spawn(r"C:\proj\vendor/bin/phpcs", true),
            via_php(r"C:\proj\vendor/bin/phpcs")
        );
        assert_eq!(
            plan_spawn(r"C:\proj\vendor\bin\phpcs", true),
            via_php(r"C:\proj\vendor\bin\phpcs")
        );
    }

    #[test]
    fn windows_runs_phar_through_php() {
        assert_eq!(
            plan_spawn(r"C:\Users\me\.phpcs\phpcs.phar", true),
            via_php(r"C:\Users\me\.phpcs\phpcs.phar")
        );
    }

    #[test]
    fn windows_resolves_bare_command_through_cmd() {
        // A bare name (system PATH lookup) → cmd so a `.bat`/`.exe` on PATH resolves.
        assert_eq!(plan_spawn("phpcs", true), via_cmd("phpcs"));
    }

    #[test]
    fn windows_extension_matching_is_case_insensitive() {
        assert_eq!(plan_spawn(r"C:\t\phpcs.EXE", true), direct(r"C:\t\phpcs.EXE"));
        assert_eq!(plan_spawn(r"C:\t\phpcs.BAT", true), via_cmd(r"C:\t\phpcs.BAT"));
        assert_eq!(plan_spawn(r"C:\t\phpcs.Phar", true), via_php(r"C:\t\phpcs.Phar"));
    }

    #[test]
    fn user_path_takes_precedence_over_vendor_bin() {
        // Regression for issue #60: an explicit phpcs_path must win even when a
        // project-local vendor/bin/phpcs exists.
        let tmp = tempfile::tempdir().expect("create temp dir");
        let vendor_bin = tmp.path().join("vendor").join("bin");
        fs::create_dir_all(&vendor_bin).expect("create vendor/bin");
        fs::write(vendor_bin.join("phpcs"), "#!/usr/bin/env php\n").expect("write proxy");

        let resolved = detect_tool_path(
            PhpTool::Phpcs,
            Some(tmp.path()),
            Some(r"C:\custom\phpcs.bat"),
        );

        assert_eq!(resolved, r"C:\custom\phpcs.bat");
    }

    #[test]
    fn vendor_bin_is_used_when_no_user_path() {
        // Without a user path, the project-local vendor/bin install is preferred.
        let tmp = tempfile::tempdir().expect("create temp dir");
        let vendor_bin = tmp.path().join("vendor").join("bin");
        fs::create_dir_all(&vendor_bin).expect("create vendor/bin");
        let proxy = vendor_bin.join("phpcs");
        fs::write(&proxy, "#!/usr/bin/env php\n").expect("write proxy");

        let resolved = detect_tool_path(PhpTool::Phpcs, Some(tmp.path()), None);

        assert_eq!(resolved, proxy.to_string_lossy());
    }

    #[test]
    fn blank_user_path_is_ignored() {
        // A whitespace-only setting should fall through to vendor/bin, not win.
        let tmp = tempfile::tempdir().expect("create temp dir");
        let vendor_bin = tmp.path().join("vendor").join("bin");
        fs::create_dir_all(&vendor_bin).expect("create vendor/bin");
        let proxy = vendor_bin.join("phpcs");
        fs::write(&proxy, "#!/usr/bin/env php\n").expect("write proxy");

        let resolved = detect_tool_path(PhpTool::Phpcs, Some(tmp.path()), Some("   "));

        assert_eq!(resolved, proxy.to_string_lossy());
    }
}
