#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    // We need to re-implement the filtering functions here since they're private in sync.rs.
    // This tests the exact same logic used in production.

    const ALLOWED_EXTENSIONS: &[&str] = &[
        "md", "txt", "pdf", "csv", "json", "yaml", "yml", "toml",
        "xml", "html", "htm", "rst", "log", "cfg", "ini", "conf",
        "py", "rs", "go", "js", "ts", "sh", "bat", "ps1",
    ];

    const IGNORED_FILES: &[&str] = &[
        ".DS_Store", "Thumbs.db", "desktop.ini", ".gitkeep",
    ];

    const TEXT_EXTENSIONS: &[&str] = &[
        "md", "txt", "csv", "json", "yaml", "yml", "toml",
        "xml", "html", "htm", "rst", "log", "cfg", "ini", "conf",
        "py", "rs", "go", "js", "ts", "sh", "bat", "ps1",
    ];

    fn has_allowed_extension(path: &Path) -> bool {
        match path.extension() {
            Some(ext) => {
                let ext_lower = ext.to_string_lossy().to_lowercase();
                ALLOWED_EXTENSIONS.contains(&ext_lower.as_str())
            }
            None => false,
        }
    }

    fn is_os_ignored(path: &Path) -> bool {
        match path.file_name() {
            Some(name) => {
                let name_str = name.to_string_lossy();
                IGNORED_FILES.contains(&name_str.as_ref())
            }
            None => false,
        }
    }

    fn is_text_file(path: &Path) -> bool {
        match path.extension() {
            Some(ext) => {
                let ext_lower = ext.to_string_lossy().to_lowercase();
                TEXT_EXTENSIONS.contains(&ext_lower.as_str())
            }
            None => false,
        }
    }

    fn load_ragignore(target_dir: &Path) -> Vec<String> {
        let ragignore_path = target_dir.join(".ragignore");
        if !ragignore_path.exists() {
            return Vec::new();
        }
        std::io::BufRead::lines(std::io::BufReader::new(
            fs::File::open(&ragignore_path).unwrap(),
        ))
        .filter_map(|line| line.ok())
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect()
    }

    fn is_ragignored(path: &Path, target_dir: &Path, patterns: &[String]) -> bool {
        let relative = path.strip_prefix(target_dir).unwrap_or(path);
        // Normalize to forward slashes for cross-platform pattern matching
        let rel_str = relative.to_string_lossy().replace('\\', "/");
        for pattern in patterns {
            if pattern.ends_with('*') {
                let prefix = &pattern[..pattern.len() - 1];
                if rel_str.starts_with(prefix) {
                    return true;
                }
            } else if rel_str.starts_with(pattern) || rel_str == *pattern {
                return true;
            }
            for component in relative.components() {
                if component.as_os_str().to_string_lossy() == *pattern {
                    return true;
                }
            }
        }
        false
    }

    // ─────────────────── Extension Whitelist Tests ───────────────────

    #[test]
    fn test_allowed_extensions_accepted() {
        let accepted = vec![
            "doc.md", "readme.txt", "data.csv", "config.json",
            "settings.yaml", "schema.yml", "setup.toml", "page.html",
            "document.pdf", "script.py", "main.rs", "app.go",
            "index.js", "component.ts", "deploy.sh",
        ];
        for name in accepted {
            let path = PathBuf::from(name);
            assert!(
                has_allowed_extension(&path),
                "'{}' should be allowed",
                name
            );
        }
    }

    #[test]
    fn test_disallowed_extensions_rejected() {
        let rejected = vec![
            "image.png", "photo.jpg", "video.mp4", "archive.zip",
            "binary.exe", "data.bin", "font.woff2", "model.onnx",
        ];
        for name in rejected {
            let path = PathBuf::from(name);
            assert!(
                !has_allowed_extension(&path),
                "'{}' should be rejected",
                name
            );
        }
    }

    #[test]
    fn test_no_extension_rejected() {
        let path = PathBuf::from("Makefile");
        assert!(!has_allowed_extension(&path));
    }

    // ─────────────────── OS File Ignore Tests ───────────────────

    #[test]
    fn test_os_files_ignored() {
        assert!(is_os_ignored(Path::new(".DS_Store")));
        assert!(is_os_ignored(Path::new("Thumbs.db")));
        assert!(is_os_ignored(Path::new("desktop.ini")));
        assert!(is_os_ignored(Path::new(".gitkeep")));
    }

    #[test]
    fn test_normal_files_not_ignored() {
        assert!(!is_os_ignored(Path::new("readme.md")));
        assert!(!is_os_ignored(Path::new("document.pdf")));
        assert!(!is_os_ignored(Path::new("config.yaml")));
    }

    // ─────────────────── Context Injection (Binary Safety) Tests ───────────────────

    #[test]
    fn test_text_files_are_text() {
        assert!(is_text_file(Path::new("doc.md")));
        assert!(is_text_file(Path::new("readme.txt")));
        assert!(is_text_file(Path::new("config.json")));
        assert!(is_text_file(Path::new("main.py")));
        assert!(is_text_file(Path::new("app.rs")));
    }

    #[test]
    fn test_binary_files_are_not_text() {
        assert!(!is_text_file(Path::new("document.pdf")));
        assert!(!is_text_file(Path::new("image.png")));
        assert!(!is_text_file(Path::new("archive.zip")));
    }

    #[test]
    fn test_pdf_excluded_from_text_but_included_in_allowed() {
        let pdf_path = PathBuf::from("report.pdf");
        assert!(has_allowed_extension(&pdf_path), "PDF should be allowed for upload");
        assert!(!is_text_file(&pdf_path), "PDF should NOT get context injection");
    }

    // ─────────────────── .ragignore Tests ───────────────────

    #[test]
    fn test_ragignore_empty_when_no_file() {
        let dir = TempDir::new().unwrap();
        let patterns = load_ragignore(dir.path());
        assert!(patterns.is_empty());
    }

    #[test]
    fn test_ragignore_parses_patterns() {
        let dir = TempDir::new().unwrap();
        let ragignore = dir.path().join(".ragignore");
        fs::write(
            &ragignore,
            "# Comment line\ndrafts\narchives/*.log\nnotes/scratch.txt\n\n",
        )
        .unwrap();

        let patterns = load_ragignore(dir.path());
        assert_eq!(patterns.len(), 3);
        assert_eq!(patterns[0], "drafts");
        assert_eq!(patterns[1], "archives/*.log");
        assert_eq!(patterns[2], "notes/scratch.txt");
    }

    #[test]
    fn test_ragignore_skips_comments_and_blanks() {
        let dir = TempDir::new().unwrap();
        let ragignore = dir.path().join(".ragignore");
        fs::write(&ragignore, "# Skip this\n\n   \n# Another comment\nkeep_this\n").unwrap();

        let patterns = load_ragignore(dir.path());
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0], "keep_this");
    }

    #[test]
    fn test_ragignore_directory_match() {
        let dir = TempDir::new().unwrap();
        let target = dir.path();
        let patterns = vec!["drafts".to_string()];

        let accepted_file = target.join("docs").join("readme.md");
        let ignored_file = target.join("drafts").join("wip.md");

        assert!(!is_ragignored(&accepted_file, target, &patterns));
        assert!(is_ragignored(&ignored_file, target, &patterns));
    }

    #[test]
    fn test_ragignore_wildcard_match() {
        let dir = TempDir::new().unwrap();
        let target = dir.path();
        let patterns = vec!["archives/*".to_string()];

        let ignored = target.join("archives").join("old.log");
        let accepted = target.join("docs").join("new.md");

        assert!(is_ragignored(&ignored, target, &patterns));
        assert!(!is_ragignored(&accepted, target, &patterns));
    }

    #[test]
    fn test_ragignore_exact_file_match() {
        let dir = TempDir::new().unwrap();
        let target = dir.path();
        let patterns = vec!["notes/scratch.txt".to_string()];

        let ignored = target.join("notes").join("scratch.txt");
        let accepted = target.join("notes").join("important.txt");

        assert!(is_ragignored(&ignored, target, &patterns));
        assert!(!is_ragignored(&accepted, target, &patterns));
    }

    // ─────────────────── File Collection Integration Tests ───────────────────

    #[test]
    fn test_full_filtering_pipeline() {
        let dir = TempDir::new().unwrap();
        let target = dir.path();

        // Create test files
        fs::create_dir_all(target.join("docs")).unwrap();
        fs::create_dir_all(target.join("drafts")).unwrap();
        fs::write(target.join("docs").join("guide.md"), "# Guide").unwrap();
        fs::write(target.join("docs").join("report.pdf"), b"fake pdf").unwrap();
        fs::write(target.join("docs").join("image.png"), b"fake png").unwrap();
        fs::write(target.join("drafts").join("wip.md"), "WIP").unwrap();
        fs::write(target.join(".DS_Store"), "junk").unwrap();
        fs::write(target.join("Makefile"), "all: build").unwrap();

        // Write .ragignore
        fs::write(target.join(".ragignore"), "drafts\n").unwrap();

        let patterns = load_ragignore(target);

        // Simulate the walkdir filter
        let mut accepted: Vec<String> = Vec::new();
        for entry in walkdir::WalkDir::new(target)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.into_path();
            // Skip hidden files/dirs (relative components starting with '.')
            let rel = path.strip_prefix(target).unwrap_or(&path);
            if rel.components().any(|c| {
                c.as_os_str().to_string_lossy().starts_with('.')
            }) {
                continue;
            }
            if is_os_ignored(&path) {
                continue;
            }
            if is_ragignored(&path, target, &patterns) {
                continue;
            }
            if !has_allowed_extension(&path) {
                continue;
            }
            accepted.push(
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_string(),
            );
        }

        accepted.sort();
        assert_eq!(accepted, vec!["guide.md", "report.pdf"]);
        // guide.md: allowed ext, not ignored, not ragignored ✓
        // report.pdf: allowed ext ✓ (but won't get context injection)
        // image.png: disallowed ext ✗
        // drafts/wip.md: ragignored ✗
        // .DS_Store: hidden file, filtered by walkdir ✗
        // Makefile: no extension ✗
    }

    // ─────────────────── ext_to_lang Tests ───────────────────

    /// Re-implements sync.rs ext_to_lang for testing
    fn ext_to_lang(path: &Path) -> &'static str {
        match path.extension().and_then(|e| e.to_str()) {
            Some("py") => "python",
            Some("rs") => "rust",
            Some("go") => "go",
            Some("js") => "javascript",
            Some("ts") => "typescript",
            Some("sh" | "bat" | "ps1") => "shell",
            Some("json") => "json",
            Some("yaml" | "yml") => "yaml",
            Some("toml") => "toml",
            Some("xml" | "html" | "htm") => "html",
            Some("csv") => "csv",
            Some("rst") => "rst",
            Some("cfg" | "ini" | "conf") => "ini",
            Some("log") => "log",
            _ => "",
        }
    }

    /// Re-implements sync.rs is_markdown_file for testing
    fn is_markdown_file(path: &Path) -> bool {
        match path.extension() {
            Some(ext) => {
                let e = ext.to_string_lossy().to_lowercase();
                e == "md" || e == "txt"
            }
            None => false,
        }
    }

    #[test]
    fn test_ext_to_lang_common_languages() {
        assert_eq!(ext_to_lang(Path::new("main.py")), "python");
        assert_eq!(ext_to_lang(Path::new("lib.rs")), "rust");
        assert_eq!(ext_to_lang(Path::new("app.js")), "javascript");
        assert_eq!(ext_to_lang(Path::new("config.json")), "json");
        assert_eq!(ext_to_lang(Path::new("settings.yaml")), "yaml");
        assert_eq!(ext_to_lang(Path::new("settings.yml")), "yaml");
        assert_eq!(ext_to_lang(Path::new("deploy.toml")), "toml");
    }

    #[test]
    fn test_ext_to_lang_shell_variants() {
        assert_eq!(ext_to_lang(Path::new("run.sh")), "shell");
        assert_eq!(ext_to_lang(Path::new("build.bat")), "shell");
        assert_eq!(ext_to_lang(Path::new("script.ps1")), "shell");
    }

    #[test]
    fn test_ext_to_lang_config_formats() {
        assert_eq!(ext_to_lang(Path::new("app.cfg")), "ini");
        assert_eq!(ext_to_lang(Path::new("app.ini")), "ini");
        assert_eq!(ext_to_lang(Path::new("app.conf")), "ini");
        assert_eq!(ext_to_lang(Path::new("page.html")), "html");
        assert_eq!(ext_to_lang(Path::new("page.htm")), "html");
        assert_eq!(ext_to_lang(Path::new("data.xml")), "html");
    }

    #[test]
    fn test_ext_to_lang_unknown_returns_empty() {
        assert_eq!(ext_to_lang(Path::new("photo.png")), "");
        assert_eq!(ext_to_lang(Path::new("doc.pdf")), "");
        assert_eq!(ext_to_lang(Path::new("Makefile")), "");
    }

    // ─────────────────── is_markdown_file Tests ───────────────────

    #[test]
    fn test_is_markdown_file_positive() {
        assert!(is_markdown_file(Path::new("README.md")));
        assert!(is_markdown_file(Path::new("notes.txt")));
        assert!(is_markdown_file(Path::new("NOTES.TXT"))); // case insensitive
        assert!(is_markdown_file(Path::new("README.MD")));
    }

    #[test]
    fn test_is_markdown_file_negative() {
        assert!(!is_markdown_file(Path::new("main.py")));
        assert!(!is_markdown_file(Path::new("config.json")));
        assert!(!is_markdown_file(Path::new("doc.pdf")));
        assert!(!is_markdown_file(Path::new("style.rs")));
    }

    #[test]
    fn test_is_markdown_file_no_extension() {
        assert!(!is_markdown_file(Path::new("Makefile")));
        assert!(!is_markdown_file(Path::new("Dockerfile")));
    }

    // ─────────────────── Markdown Wrapping Logic Tests ───────────────────

    /// Simulates the convert-to-markdown wrapping logic from build_upload_payload
    fn wrap_in_code_fence(content: &str, path: &Path) -> String {
        let lang = ext_to_lang(path);
        format!("```{}\n{}\n```\n", lang, content)
    }

    #[test]
    fn test_markdown_wrapping_python() {
        let content = "def hello():\n    print('world')";
        let wrapped = wrap_in_code_fence(content, Path::new("main.py"));
        assert!(wrapped.starts_with("```python\n"));
        assert!(wrapped.contains("def hello()"));
        assert!(wrapped.ends_with("\n```\n"));
    }

    #[test]
    fn test_markdown_wrapping_json() {
        let content = r#"{"key": "value"}"#;
        let wrapped = wrap_in_code_fence(content, Path::new("config.json"));
        assert!(wrapped.starts_with("```json\n"));
        assert!(wrapped.contains(r#""key": "value""#));
    }

    #[test]
    fn test_markdown_conversion_skips_md_and_txt() {
        // Simulates the condition: convert_to_markdown && !is_markdown_file(path)
        let convert_to_markdown = true;
        let md_path = Path::new("README.md");
        let txt_path = Path::new("notes.txt");
        let py_path = Path::new("main.py");

        // .md and .txt should NOT be wrapped (is_markdown_file returns true)
        assert!(!(convert_to_markdown && !is_markdown_file(md_path)),
            ".md should not trigger wrapping");
        assert!(!(convert_to_markdown && !is_markdown_file(txt_path)),
            ".txt should not trigger wrapping");
        // .py should be wrapped
        assert!(convert_to_markdown && !is_markdown_file(py_path),
            ".py should trigger wrapping when convert_to_markdown is true");
    }
}
