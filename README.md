# PHPCS LSP for Zed Editor

> A Language Server Protocol implementation that brings PHP_CodeSniffer integration to Zed Editor

[![MIT License](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![PHP](https://img.shields.io/badge/PHP-8.0%2B-777BB4?logo=php&logoColor=white)](https://php.net)
[![Zed](https://img.shields.io/badge/Zed-Editor-blue?logo=zed&logoColor=white)](https://zed.dev)
[![PHPCS](https://img.shields.io/badge/PHPCS-3.13.2%2B-green)](https://github.com/PHPCSStandards/PHP_CodeSniffer)

This extension integrates PHP_CodeSniffer with Zed Editor to provide real-time code style checking and automatic formatting. It highlights violations as you code, auto-fixes them on save via PHPCBF, and supports various PHP coding standards including PSR-12, custom rulesets, and project-specific configurations.

## Features

- **Real-time diagnostics** - See code style violations as you type
- **Auto-fix on save** - Automatically fix all PHPCS issues on save via `source.fixAll.phpcs`
- **Zero configuration** - Works out of the box using PHPCS native defaults
- **Live configuration** - Settings changes apply immediately without restart
- **Auto-recovery** - Automatically handles deleted or invalid config files
- **Multiple standards** - PSR-12, PSR-2, Squiz, Slevomat, custom rulesets  
- **Project awareness** - Automatically discovers phpcs.xml configuration
- **Smart PHPCS detection** - Prefers project-local installations with dependencies
- **Cross-platform** - Static binaries for Linux (musl), macOS, and Windows
- **Flexible configuration** - Via Zed settings, environment variables, or project files

### Performance & Reliability

- **Async process handling** - Non-blocking PHPCS execution keeps editor responsive
- **Concurrent processing** - Lint up to 4 files simultaneously for faster results
- **Timeout protection** - Automatic 10-second timeout prevents hanging on large files
- **Memory optimization** - LZ4 compression reduces memory usage by ~85%
- **Smart caching** - Results cached to avoid redundant linting on unchanged files
- **Process management** - Automatic cleanup of zombie processes

## Quick Start

### Installation

```bash
# Via Zed Extensions (coming soon)
# For now: manual installation for development
```

### Basic Usage

1. **Ensure the PHPCS extension is installed** in Zed from the Extensions panel

2. **Enable the language server** in your Zed settings.json:

```json
{
  "languages": {
    "PHP": {
      "language_servers": ["intelephense", "phpcs", "!phpactor"]
    }
  }
}
```

3. **Open any PHP project** and the extension will start analyzing your code:

```php
<?php
// This will show underlines for style violations
if($x==1){echo "test";}

// This follows PSR-12 and won't show any issues
if ($x === 1) {
    echo "test";
}
```

4. **Enable auto-fix on save** (optional) to fix all PHPCS issues when you save:

```json
{
  "languages": {
    "PHP": {
      "code_actions_on_format": {
        "source.fixAll.phpcs": true
      },
      "formatter": [],
      "format_on_save": "on"
    }
  }
}
```

> **Note:** This runs PHPCBF on every save, automatically fixing all fixable code style issues. The `"formatter": []` is required to prevent Zed's default formatter from interfering. See the [Auto-Fix on Save](#auto-fix-on-save) section for more configuration options.

## Configuration

> **Note:** The extension works without any configuration, using PHPCS's natural defaults and bundled binaries.

### Coding Standards

<details>
<summary><strong>Automatic Discovery</strong> (recommended)</summary>

The extension follows **PHP_CodeSniffer's native discovery behavior** with this priority order:

1. **Project config files** (discovered automatically, same as PHPCS):
   - `.phpcs.xml` (highest priority)
   - `phpcs.xml`
   - `.phpcs.xml.dist`
   - `phpcs.xml.dist` (lowest config file priority)
2. **Zed settings** - Custom configuration in settings.json  
3. **Environment variables** - `PHPCS_STANDARD`
4. **PHPCS native defaults** - User config (`~/.phpcs.xml`), global config, or PEAR standard

> **💡 Global Defaults:** Set system-wide standards with `phpcs --config-set default_standard PSR12` or create `~/.phpcs.xml` for user-specific defaults that work across all projects.

</details>

<details>
<summary><strong>Zed Settings Configuration</strong></summary>

Configure standards in your **Zed settings.json** file (open with `Cmd+,` or `Ctrl+,`):

**Single standard:**
```json
{
  "lsp": {
    "phpcs": {
      "settings": {
        "standard": "PSR12"
      }
    }
  }
}
```

**Multiple standards (comma-separated):**
```json
{
  "lsp": {
    "phpcs": {
      "settings": {
        "standard": ["PSR12", "Squiz.Commenting", "Generic.Files.LineLength"]
      }
    }
  }
}
```

**Path to custom ruleset:**
```json
{
  "lsp": {
    "phpcs": {
      "settings": {
        "standard": "/path/to/custom-phpcs.xml"
      }
    }
  }
}
```

**Relative path to project ruleset:**
```json
{
  "lsp": {
    "phpcs": {
      "settings": {
        "standard": "./ruleset.xml"
      }
    }
  }
}
```

> **💡 Tip:** You can also set these in **local project settings** by creating `.zed/settings.json` in your project root.

</details>

<details>
<summary><strong>Environment Variables</strong></summary>

```bash
export PHPCS_STANDARD="PSR12"
export PHPCS_PATH="/custom/path/to/phpcs"
export PHPCBF_PATH="/custom/path/to/phpcbf"
```

</details>

### PHPCS Executable

<details>
<summary><strong>Automatic Discovery</strong> (recommended)</summary>

The extension finds PHPCS and PHPCBF in this priority order:

1. **Project composer** - `vendor/bin/phpcs` (includes project dependencies like Slevomat)
2. **User-configured path** - Custom path from Zed LSP settings
3. **Environment variable** - `PHPCS_PATH` / `PHPCBF_PATH`
4. **System PATH** - Global phpcs installation (respects your `phpcs --config-set` settings)
5. **Bundled PHAR** - Modern PHPCS v3.13.2+ (fallback, included with extension)

> **💡 Global Config Support:** The extension now respects your system PHPCS configuration. Set global defaults with `phpcs --config-set default_standard PSR12` or `phpcs --config-set installed_paths /path/to/sniffs` and they'll work automatically without any Zed configuration.

</details>

<details>
<summary><strong>Custom Paths</strong></summary>

Specify custom PHPCS/PHPCBF paths in settings.json:

```json
{
  "lsp": {
    "phpcs": {
      "settings": {
        "phpcs_path": "/custom/path/to/phpcs",
        "phpcbf_path": "/custom/path/to/phpcbf"
      }
    }
  }
}
```

</details>

## Out-of-the-box Standards

| Standard | Description |
|----------|-------------|
| **PSR-12** | Modern PHP coding style (recommended) |
| **PSR-2** | Legacy coding style guide |
| **PSR-1** | Basic coding standard |
| **PEAR** | PEAR coding standard (PHPCS default) |
| **Zend** | Zend framework standard |
| **Multiple** | `"PSR12,Generic.Files.LineLength"` |
| **Custom** | Your phpcs.xml ruleset |

## Project Configuration

Create a `phpcs.xml` in your project root for team consistency. The extension will automatically discover and use any of these files (in priority order):

- `.phpcs.xml` (typically for local overrides, often gitignored)
- `phpcs.xml` (main project configuration) 
- `.phpcs.xml.dist` (distributable version, lower priority)
- `phpcs.xml.dist` (template version, lowest priority)

```xml
<?xml version="1.0"?>
<ruleset name="Project Standards">
    <description>Custom coding standard for our project</description>

    <rule ref="PSR12"/>

    <!-- Customize line length -->
    <rule ref="Generic.Files.LineLength">
        <properties>
            <property name="lineLimit" value="120"/>
        </properties>
    </rule>

    <!-- Exclude directories -->
    <exclude-pattern>*/vendor/*</exclude-pattern>
    <exclude-pattern>*/storage/*</exclude-pattern>
</ruleset>
```

## Auto-Recovery

The extension automatically handles configuration changes and edge cases:

### **Deleted Config Files**
If you delete a `phpcs.xml` file after the LSP is running:
- **Proactive detection** - Checks file exists before each lint operation
- Automatically re-scans for other config files (`.phpcs.xml.dist`, etc.)
- Falls back to PHPCS defaults if no config found
- **No restart required** - recovery happens seamlessly

### **Invalid Config Files**
If a config file becomes corrupted or references missing standards:
- File existence validated before use, with immediate re-discovery if missing
- Standard discovery process re-runs automatically for any configuration issues
- Graceful fallback to working configuration
- Detailed logging shows the recovery process

### **Dynamic Updates**
- **Settings changes** - Applied immediately via `did_change_configuration`
- **Workspace changes** - Config re-discovered when switching projects
- **File system changes** - Config errors trigger automatic re-discovery

## Auto-Fix on Save

The extension supports automatic fixing of code style issues via PHPCBF (PHP Code Beautifier and Fixer), using the `source.fixAll.phpcs` code action. This follows the same convention used by ESLint, Biome, and Ruff.

### PHPCS Fixes Only

```json
{
  "languages": {
    "PHP": {
      "code_actions_on_format": {
        "source.fixAll.phpcs": true
      },
      "formatter": [],
      "format_on_save": "on"
    }
  }
}
```

### All Linter Fixes

Fix issues from PHPCS and any other linters that support `source.fixAll`:

```json
{
  "languages": {
    "PHP": {
      "code_actions_on_format": {
        "source.fixAll": true
      },
      "formatter": [],
      "format_on_save": "on"
    }
  }
}
```

### Combine with a Separate Formatter

Use PHPCS fixing alongside a separate formatter (e.g., Prettier for embedded HTML/JS):

```json
{
  "languages": {
    "PHP": {
      "code_actions_on_format": {
        "source.fixAll.phpcs": true
      },
      "formatter": {
        "external": {
          "command": "prettier",
          "arguments": ["--stdin-filepath", "{buffer_path}"]
        }
      },
      "format_on_save": "on"
    }
  }
}
```

> **Important:** When using `code_actions_on_format` without a separate formatter, you must set `"formatter": []` to prevent Zed's default `"auto"` formatter from interfering. See [zed-industries/zed#51490](https://github.com/zed-industries/zed/issues/51490) for details.

### How It Works

- Auto-fixing uses the same coding standard as linting (phpcs.xml discovery, Zed settings, etc.)
- PHPCBF is discovered automatically using the same priority as PHPCS: project `vendor/bin` → user-configured path → `PHPCBF_PATH` env var → system PATH → bundled PHAR
- Auto-fixing and linting run from the same LSP process — no extra configuration needed
- The `source.fixAll.phpcs` code action is also available in the lightbulb menu for manual use

## Troubleshooting

<details>
<summary><strong>Extension not working?</strong></summary>

1. Check Zed's debug console for error messages
2. Verify PHPCS is accessible (custom paths must exist)
3. **No restart needed** - configuration changes apply immediately

</details>

<details>
<summary><strong>No diagnostics showing?</strong></summary>

1. Ensure you're editing a `.php` file
2. Check that your configured standard exists
3. Test with a file containing obvious style violations

</details>

<details>
<summary><strong>Custom rules not working?</strong></summary>

1. Validate your `phpcs.xml` syntax
2. Ensure paths are relative to your project root
3. Test your configuration manually with `phpcs --config-show`

</details>

<details>
<summary><strong>Want to set global defaults?</strong></summary>

**Set PHPCS global configuration (affects all projects without local config):**
```bash
# Set global default standard
phpcs --config-set default_standard PSR12

# View current global config
phpcs --config-show

# Create user-specific config file
echo '<?xml version="1.0"?>
<ruleset name="My Default">
    <rule ref="PSR12"/>
</ruleset>' > ~/.phpcs.xml
```

> **💡 Pro Tip:** The extension respects all PHPCS configuration methods, so you can mix global defaults with project-specific overrides.

</details>

## Contributing

### Development Setup

1. **Clone the repository:**
   ```bash
   git clone https://github.com/GeneaLabs/zed-phpcs-lsp.git
   cd zed-phpcs-lsp
   ```

2. **Build the LSP server:**
   ```bash
   cd lsp-server
   cargo build --release
   ```

3. **Configure Zed to use local build:**

   Create `.zed/settings.json` in the project root:
   ```json
   {
     "lsp": {
       "phpcs": {
         "binary": {
           "path": "lsp-server/target/release/phpcs-lsp-server"
         }
       }
     }
   }
   ```

4. **Open the project in Zed** and edit PHP files to test your changes.

> **Note:** The `.zed/` folder is gitignored to avoid conflicts with user settings.

### Testing

Create PHP files with intentional PHPCS violations in `test-files/` to verify diagnostics and code actions are working correctly. This folder is gitignored.

## Resources & Credits

### Learn More
- [PHP_CodeSniffer Documentation](https://github.com/PHPCSStandards/PHP_CodeSniffer/wiki)
- [PSR Standards](https://www.php-fig.org/psr/)
- [Zed Editor Documentation](https://zed.dev/docs)

### Built With
- [PHP_CodeSniffer](https://github.com/PHPCSStandards/PHP_CodeSniffer) - The excellent tool that powers code analysis
- [Zed Editor](https://zed.dev) - The fast, collaborative editor
- [Tower LSP](https://github.com/ebkalderon/tower-lsp) - Rust LSP framework

## License

### Main License

This project is licensed under the [MIT License](LICENSE).

```
MIT License

Copyright (c) 2025 Mike Bronner

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

### Third-Party Licenses

This extension bundles and redistributes third-party software. For a complete list of third-party licenses and attributions, please see [THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md).

**Key third-party components:**

- **PHP_CodeSniffer** - BSD-3-Clause License  
  The core tool that powers code analysis. Bundled as PHAR binaries.
  
- **Rust Dependencies** - Various permissive licenses (Apache-2.0, MIT, etc.)  
  All dependencies are compatible with the MIT license. See the full list in the third-party licenses file.

-----
**Made with ❤️ and lots of ☕ for the PHP community.**
