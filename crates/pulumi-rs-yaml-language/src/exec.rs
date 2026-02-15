//! The `exec` subcommand: wraps a child process (e.g. `pulumi up`) so that
//! full Jinja `{% %}` blocks can appear in Pulumi.*.yaml files.
//!
//! Flow (multi-file):
//! 1. Discover all Pulumi.yaml + Pulumi.*.yaml files
//! 2. For each file with `{% %}` blocks:
//!    a. Validate Jinja syntax (early, rich errors)
//!    b. Strip `{% %}` lines → write valid YAML
//!    c. Save original to temp directory
//! 3. Set `PULUMI_YAML_JINJA_SOURCE` to temp directory (not a file)
//! 4. Spawn child process (inherits env + the new env var)
//! 5. Restore ALL original files on exit (including SIGINT/SIGTERM)

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use pulumi_rs_yaml_core::jinja::{
    has_jinja_block_syntax, strip_jinja_blocks, validate_jinja_syntax,
};
use pulumi_rs_yaml_core::multi_file::discover_project_files;

/// Environment variable pointing to the original Jinja source temp directory.
pub const JINJA_SOURCE_ENV: &str = "PULUMI_YAML_JINJA_SOURCE";

/// Runs the exec subcommand. Returns the process exit code.
pub fn run_exec(command_args: &[String]) -> i32 {
    // 1. Discover all project files
    let cwd = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("error: failed to get current directory: {}", e);
            return 1;
        }
    };

    let project_files = match discover_project_files(&cwd) {
        Ok(files) => files,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };

    // 2. Read all files and check for Jinja blocks
    let mut files_with_blocks: Vec<(PathBuf, String)> = Vec::new();
    let mut any_has_blocks = false;

    for path in project_files.all_files() {
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: failed to read {}: {}", path.display(), e);
                return 1;
            }
        };

        // Validate Jinja syntax in every file (even if no blocks, catches {{ }} errors)
        let filename = path.file_name().unwrap_or_default().to_string_lossy();
        if let Err(diag) = validate_jinja_syntax(&content, &filename) {
            eprintln!("{}", diag.format_rich(&filename));
            return 1;
        }

        if has_jinja_block_syntax(&content) {
            any_has_blocks = true;
            files_with_blocks.push((path.clone(), content));
        }
    }

    // 3. If no files have {% %} blocks, just run the command directly
    if !any_has_blocks {
        return spawn_command(command_args, None);
    }

    // 4. Create temp directory for original sources
    let temp_dir = match create_temp_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("error: failed to create temp directory: {}", e);
            return 1;
        }
    };

    // 5. For each file with blocks: strip, validate, write
    let mut modified_files: Vec<(PathBuf, String)> = Vec::new();

    for (path, original) in &files_with_blocks {
        let filename = path.file_name().unwrap_or_default().to_string_lossy();

        // Strip {% %} lines
        let stripped = strip_jinja_blocks(original);

        // Validate stripped YAML is parseable
        if let Err(e) = serde_yaml::from_str::<serde_yaml::Value>(&stripped) {
            eprintln!("error: stripped YAML in {} is not valid: {}", filename, e);
            eprintln!("hint: ensure {{{{ }}}} expressions are inside quoted strings");
            cleanup_all(&modified_files, &temp_dir);
            return 1;
        }

        // Save original to temp directory
        let original_path = temp_dir.join(format!("{}.original", filename));
        if let Err(e) = fs::write(&original_path, original) {
            eprintln!(
                "error: failed to write temp file {}: {}",
                original_path.display(),
                e
            );
            cleanup_all(&modified_files, &temp_dir);
            return 1;
        }

        // Write stripped YAML to the actual file
        if let Err(e) = fs::write(path, &stripped) {
            eprintln!(
                "error: failed to write stripped YAML to {}: {}",
                path.display(),
                e
            );
            cleanup_all(&modified_files, &temp_dir);
            return 1;
        }

        modified_files.push((path.clone(), original.clone()));
    }

    // 6. Install signal handler to restore on SIGINT/SIGTERM
    let restore = Arc::new(Mutex::new(Some(RestoreState {
        modified_files: modified_files.clone(),
        temp_dir: temp_dir.clone(),
    })));
    let restore_handler = Arc::clone(&restore);
    if let Err(e) = ctrlc::set_handler(move || {
        if let Ok(mut guard) = restore_handler.lock() {
            if let Some(state) = guard.take() {
                cleanup_all(&state.modified_files, &state.temp_dir);
            }
        }
        // Exit code 130 follows Unix convention (128 + SIGINT=2).
        // On Windows, ctrlc handles CTRL_C_EVENT; 130 still signals "interrupted"
        // to the parent Pulumi process (which only checks != 0).
        std::process::exit(130);
    }) {
        eprintln!("warning: failed to install signal handler: {}", e);
    }

    // 7. Spawn child process with env var pointing to temp directory
    let exit_code = spawn_command(
        command_args,
        Some((JINJA_SOURCE_ENV, temp_dir.to_string_lossy().as_ref())),
    );

    // 8. Restore all originals + cleanup
    if let Ok(mut guard) = restore.lock() {
        guard.take(); // Prevent signal handler from double-restoring
    }
    cleanup_all(&modified_files, &temp_dir);

    exit_code
}

/// State needed to restore files on signal.
struct RestoreState {
    modified_files: Vec<(PathBuf, String)>,
    temp_dir: PathBuf,
}

/// Creates a unique temp directory for storing original sources.
fn create_temp_dir() -> Result<PathBuf, std::io::Error> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let dir = std::env::temp_dir().join(format!(
        "pulumi-yaml-jinja-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

/// Spawns a command with optional extra env var. Returns exit code.
fn spawn_command(args: &[String], env: Option<(&str, &str)>) -> i32 {
    let mut cmd = Command::new(&args[0]);
    cmd.args(&args[1..]);
    if let Some((key, value)) = env {
        cmd.env(key, value);
    }
    match cmd.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("error: failed to execute '{}': {}", args[0], e);
            1
        }
    }
}

/// Restores all modified files and removes the temp directory.
fn cleanup_all(modified_files: &[(PathBuf, String)], temp_dir: &Path) {
    for (path, original) in modified_files {
        if let Err(e) = fs::write(path, original) {
            eprintln!("warning: failed to restore {}: {}", path.display(), e);
        }
    }
    let _ = fs::remove_dir_all(temp_dir);
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests that change the working directory must run serially (not in parallel)
    // because CWD is process-global state.
    use std::sync::Mutex;
    static CWD_LOCK: Mutex<()> = Mutex::new(());

    /// RAII guard that changes to a temp directory and restores on drop.
    struct TempCwd {
        original: PathBuf,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl TempCwd {
        fn new(target: &Path) -> Self {
            // Recover from poisoned mutex (previous test may have panicked while
            // holding the lock). The data behind the lock is just `()`, so it's
            // always safe to continue.
            let guard = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let original = std::env::current_dir().unwrap();
            std::env::set_current_dir(target).unwrap();
            Self {
                original,
                _guard: guard,
            }
        }
    }

    impl Drop for TempCwd {
        fn drop(&mut self) {
            // Never panic in Drop — a prior assertion failure already captures the
            // real test failure; panicking here would mask it and poison the mutex.
            let _ = std::env::set_current_dir(&self.original);
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_spawn_returns_exit_code() {
        let code = spawn_command(&["true".to_string()], None);
        assert_eq!(code, 0);
        let code = spawn_command(&["false".to_string()], None);
        assert_eq!(code, 1);
    }

    #[cfg(windows)]
    #[test]
    fn test_spawn_returns_exit_code() {
        let code = spawn_command(
            &["cmd".to_string(), "/c".to_string(), "exit 0".to_string()],
            None,
        );
        assert_eq!(code, 0);
        let code = spawn_command(
            &["cmd".to_string(), "/c".to_string(), "exit 1".to_string()],
            None,
        );
        assert_eq!(code, 1);
    }

    #[cfg(unix)]
    #[test]
    fn test_spawn_with_env() {
        let code = spawn_command(
            &[
                "sh".to_string(),
                "-c".to_string(),
                format!("test \"${}\" = \"hello\"", "TEST_VAR"),
            ],
            Some(("TEST_VAR", "hello")),
        );
        assert_eq!(code, 0);
    }

    #[cfg(windows)]
    #[test]
    fn test_spawn_with_env() {
        let code = spawn_command(
            &[
                "cmd".to_string(),
                "/c".to_string(),
                "if \"%TEST_VAR%\"==\"hello\" (exit 0) else (exit 1)".to_string(),
            ],
            Some(("TEST_VAR", "hello")),
        );
        assert_eq!(code, 0);
    }

    #[test]
    fn test_spawn_nonexistent_command() {
        let code = spawn_command(&["nonexistent-command-12345".to_string()], None);
        assert_eq!(code, 1);
    }

    #[test]
    fn test_cleanup_all_restores_files() {
        let dir = tempfile::tempdir().unwrap();
        let file1 = dir.path().join("file1.yaml");
        let file2 = dir.path().join("file2.yaml");
        fs::write(&file1, "modified1").unwrap();
        fs::write(&file2, "modified2").unwrap();

        let temp_dir = dir.path().join("temp");
        fs::create_dir_all(&temp_dir).unwrap();
        fs::write(temp_dir.join("dummy"), "data").unwrap();

        let modified = vec![
            (file1.clone(), "original1".to_string()),
            (file2.clone(), "original2".to_string()),
        ];
        cleanup_all(&modified, &temp_dir);

        assert_eq!(fs::read_to_string(&file1).unwrap(), "original1");
        assert_eq!(fs::read_to_string(&file2).unwrap(), "original2");
        assert!(!temp_dir.exists(), "temp directory should be removed");
    }

    #[cfg(unix)]
    #[test]
    fn test_run_exec_no_blocks_passthrough() {
        let dir = tempfile::tempdir().unwrap();
        let pulumi = dir.path().join("Pulumi.yaml");
        let content = "name: test\nruntime: yaml\nresources:\n  bucket:\n    type: aws:s3:Bucket\n";
        fs::write(&pulumi, content).unwrap();

        let _cwd = TempCwd::new(dir.path());
        let code = run_exec(&["true".to_string()]);
        assert_eq!(code, 0);
        assert_eq!(fs::read_to_string(&pulumi).unwrap(), content);
    }

    #[cfg(windows)]
    #[test]
    fn test_run_exec_no_blocks_passthrough() {
        let dir = tempfile::tempdir().unwrap();
        let pulumi = dir.path().join("Pulumi.yaml");
        let content = "name: test\nruntime: yaml\nresources:\n  bucket:\n    type: aws:s3:Bucket\n";
        fs::write(&pulumi, content).unwrap();

        let _cwd = TempCwd::new(dir.path());
        let code = run_exec(&["cmd".to_string(), "/c".to_string(), "exit 0".to_string()]);
        assert_eq!(code, 0);
        assert_eq!(fs::read_to_string(&pulumi).unwrap(), content);
    }

    #[cfg(unix)]
    #[test]
    fn test_run_exec_single_file_with_blocks_restores() {
        let dir = tempfile::tempdir().unwrap();
        let pulumi = dir.path().join("Pulumi.yaml");
        let content = r#"name: test
runtime: yaml
resources:
{% for i in range(2) %}
  "bucket{{ i }}":
    type: aws:s3:Bucket
{% endfor %}
"#;
        fs::write(&pulumi, content).unwrap();

        let _cwd = TempCwd::new(dir.path());
        let code = run_exec(&[
            "sh".to_string(),
            "-c".to_string(),
            format!(
                "! grep '{{% for' '{}' && test -d \"${}\"",
                pulumi.display(),
                JINJA_SOURCE_ENV
            ),
        ]);
        assert_eq!(
            code, 0,
            "stripped YAML should not contain blocks, JINJA_SOURCE_ENV should be a directory"
        );
        assert_eq!(fs::read_to_string(&pulumi).unwrap(), content);
    }

    #[cfg(windows)]
    #[test]
    fn test_run_exec_single_file_with_blocks_restores() {
        let dir = tempfile::tempdir().unwrap();
        let pulumi = dir.path().join("Pulumi.yaml");
        let content = r#"name: test
runtime: yaml
resources:
{% for i in range(2) %}
  "bucket{{ i }}":
    type: aws:s3:Bucket
{% endfor %}
"#;
        fs::write(&pulumi, content).unwrap();

        let _cwd = TempCwd::new(dir.path());
        // On Windows, just verify the exec completes successfully (blocks are stripped)
        let code = run_exec(&["cmd".to_string(), "/c".to_string(), "exit 0".to_string()]);
        assert_eq!(code, 0, "exec with blocks should succeed");
        assert_eq!(
            fs::read_to_string(&pulumi).unwrap(),
            content,
            "file must be restored"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_run_exec_multi_file_with_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let main_content = "name: test\nruntime: yaml\noutputs:\n  x: hello\n";
        let extra_content = r#"resources:
{% for i in range(2) %}
  "bucket{{ i }}":
    type: aws:s3:Bucket
{% endfor %}
"#;
        fs::write(dir.path().join("Pulumi.yaml"), main_content).unwrap();
        fs::write(dir.path().join("Pulumi.buckets.yaml"), extra_content).unwrap();

        let _cwd = TempCwd::new(dir.path());
        let code = run_exec(&[
            "sh".to_string(),
            "-c".to_string(),
            // Verify: blocks stripped from extra file, JINJA_SOURCE_ENV is a dir with originals
            format!(
                "! grep '{{% for' '{}' && test -d \"${}\" && test -f \"${}/Pulumi.buckets.yaml.original\"",
                dir.path().join("Pulumi.buckets.yaml").display(),
                JINJA_SOURCE_ENV,
                JINJA_SOURCE_ENV,
            ),
        ]);
        assert_eq!(
            code, 0,
            "multi-file exec should strip blocks and create temp dir with originals"
        );
        // All files should be restored
        assert_eq!(
            fs::read_to_string(dir.path().join("Pulumi.yaml")).unwrap(),
            main_content
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("Pulumi.buckets.yaml")).unwrap(),
            extra_content
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_run_exec_multi_file_with_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let main_content = "name: test\nruntime: yaml\noutputs:\n  x: hello\n";
        let extra_content = "resources:\n{% for i in range(2) %}\n  \"bucket{{ i }}\":\n    type: aws:s3:Bucket\n{% endfor %}\n";
        fs::write(dir.path().join("Pulumi.yaml"), main_content).unwrap();
        fs::write(dir.path().join("Pulumi.buckets.yaml"), extra_content).unwrap();

        let _cwd = TempCwd::new(dir.path());
        let code = run_exec(&["cmd".to_string(), "/c".to_string(), "exit 0".to_string()]);
        assert_eq!(code, 0, "multi-file exec should succeed");
        // All files should be restored
        assert_eq!(
            fs::read_to_string(dir.path().join("Pulumi.yaml")).unwrap(),
            main_content
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("Pulumi.buckets.yaml")).unwrap(),
            extra_content
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_run_exec_env_var_is_directory() {
        let dir = tempfile::tempdir().unwrap();
        let content = "name: test\nruntime: yaml\n{% if true %}\nresources: {}\n{% endif %}\n";
        fs::write(dir.path().join("Pulumi.yaml"), content).unwrap();

        let _cwd = TempCwd::new(dir.path());
        let code = run_exec(&[
            "sh".to_string(),
            "-c".to_string(),
            format!("test -d \"${}\"", JINJA_SOURCE_ENV),
        ]);
        assert_eq!(
            code, 0,
            "PULUMI_YAML_JINJA_SOURCE should point to a directory"
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_run_exec_env_var_is_directory() {
        let dir = tempfile::tempdir().unwrap();
        let content = "name: test\nruntime: yaml\n{% if true %}\nresources: {}\n{% endif %}\n";
        fs::write(dir.path().join("Pulumi.yaml"), content).unwrap();

        let _cwd = TempCwd::new(dir.path());
        // On Windows, just verify exec with jinja blocks completes successfully.
        // The Unix variant verifies the env var points to a directory via `test -d`;
        // cmd.exe env-var expansion with `if exist` is unreliable across arg passing.
        let code = run_exec(&["cmd".to_string(), "/c".to_string(), "exit 0".to_string()]);
        assert_eq!(code, 0, "exec with jinja blocks should succeed");
    }

    #[cfg(unix)]
    #[test]
    fn test_run_exec_invalid_jinja_syntax() {
        let dir = tempfile::tempdir().unwrap();
        let content = "name: test\nruntime: yaml\n{% for i in range(3) %}\n  item: {{ i }}\n";
        fs::write(dir.path().join("Pulumi.yaml"), content).unwrap();

        let _cwd = TempCwd::new(dir.path());
        let code = run_exec(&["true".to_string()]);
        assert_eq!(code, 1, "invalid Jinja syntax should cause exec to fail");
    }

    #[cfg(windows)]
    #[test]
    fn test_run_exec_invalid_jinja_syntax() {
        let dir = tempfile::tempdir().unwrap();
        let content = "name: test\nruntime: yaml\n{% for i in range(3) %}\n  item: {{ i }}\n";
        fs::write(dir.path().join("Pulumi.yaml"), content).unwrap();

        let _cwd = TempCwd::new(dir.path());
        let code = run_exec(&["cmd".to_string(), "/c".to_string(), "exit 0".to_string()]);
        assert_eq!(code, 1, "invalid Jinja syntax should cause exec to fail");
    }

    #[cfg(unix)]
    #[test]
    fn test_run_exec_child_failure_still_restores() {
        let dir = tempfile::tempdir().unwrap();
        let content = "name: test\nruntime: yaml\n{% if true %}\nresources: {}\n{% endif %}\n";
        fs::write(dir.path().join("Pulumi.yaml"), content).unwrap();

        let _cwd = TempCwd::new(dir.path());
        let code = run_exec(&["false".to_string()]);
        assert_eq!(code, 1);
        assert_eq!(
            fs::read_to_string(dir.path().join("Pulumi.yaml")).unwrap(),
            content,
            "Pulumi.yaml must be restored even when child process fails"
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_run_exec_child_failure_still_restores() {
        let dir = tempfile::tempdir().unwrap();
        let content = "name: test\nruntime: yaml\n{% if true %}\nresources: {}\n{% endif %}\n";
        fs::write(dir.path().join("Pulumi.yaml"), content).unwrap();

        let _cwd = TempCwd::new(dir.path());
        let code = run_exec(&["cmd".to_string(), "/c".to_string(), "exit 1".to_string()]);
        assert_eq!(code, 1);
        assert_eq!(
            fs::read_to_string(dir.path().join("Pulumi.yaml")).unwrap(),
            content,
            "Pulumi.yaml must be restored even when child process fails"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_run_exec_no_pulumi_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let _cwd = TempCwd::new(dir.path());
        let code = run_exec(&["true".to_string()]);
        assert_eq!(code, 1, "should fail when no Pulumi.yaml exists");
    }

    #[cfg(windows)]
    #[test]
    fn test_run_exec_no_pulumi_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let _cwd = TempCwd::new(dir.path());
        let code = run_exec(&["cmd".to_string(), "/c".to_string(), "exit 0".to_string()]);
        assert_eq!(code, 1, "should fail when no Pulumi.yaml exists");
    }

    #[test]
    fn test_strip_and_validate_roundtrip() {
        let source = r#"name: test
runtime: yaml
resources:
{% for i in range(3) %}
  "bucket{{ i }}":
    type: aws:s3:Bucket
    properties:
      name: "test-{{ i }}"
{% endfor %}
outputs:
  result: done
"#;
        assert!(validate_jinja_syntax(source, "test.yaml").is_ok());
        let stripped = strip_jinja_blocks(source);
        assert!(!stripped.contains("{% for"));
        assert!(!stripped.contains("{% endfor"));
        let parsed: Result<serde_yaml::Value, _> = serde_yaml::from_str(&stripped);
        assert!(
            parsed.is_ok(),
            "stripped YAML should be parseable: {:?}",
            parsed.err()
        );
    }

    #[test]
    fn test_strip_nested_blocks_roundtrip() {
        let source = r#"name: test
runtime: yaml
resources:
{% for i in range(2) %}
{% if i > 0 %}
  "bucket{{ i }}":
    type: aws:s3:Bucket
{% endif %}
{% endfor %}
"#;
        assert!(validate_jinja_syntax(source, "test.yaml").is_ok());
        let stripped = strip_jinja_blocks(source);
        assert!(!stripped.contains("{%"));
        let parsed: Result<serde_yaml::Value, _> = serde_yaml::from_str(&stripped);
        assert!(
            parsed.is_ok(),
            "nested-block stripped YAML should parse: {:?}",
            parsed.err()
        );
    }

    #[test]
    fn test_create_temp_dir() {
        let dir = create_temp_dir().unwrap();
        assert!(dir.exists());
        assert!(dir.is_dir());
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_run_exec_multi_file_only_some_have_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let main_content = "name: test\nruntime: yaml\n";
        let blocks_content = "resources:\n{% for i in range(2) %}\n  \"r{{ i }}\":\n    type: test:T\n{% endfor %}\n";
        let plain_content = "variables:\n  x: hello\n";

        fs::write(dir.path().join("Pulumi.yaml"), main_content).unwrap();
        fs::write(dir.path().join("Pulumi.blocks.yaml"), blocks_content).unwrap();
        fs::write(dir.path().join("Pulumi.plain.yaml"), plain_content).unwrap();

        let _cwd = TempCwd::new(dir.path());
        let code = run_exec(&["true".to_string()]);
        assert_eq!(code, 0);

        // All files should be restored to originals
        assert_eq!(
            fs::read_to_string(dir.path().join("Pulumi.yaml")).unwrap(),
            main_content
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("Pulumi.blocks.yaml")).unwrap(),
            blocks_content
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("Pulumi.plain.yaml")).unwrap(),
            plain_content
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_run_exec_multi_file_only_some_have_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let main_content = "name: test\nruntime: yaml\n";
        let blocks_content = "resources:\n{% for i in range(2) %}\n  \"r{{ i }}\":\n    type: test:T\n{% endfor %}\n";
        let plain_content = "variables:\n  x: hello\n";

        fs::write(dir.path().join("Pulumi.yaml"), main_content).unwrap();
        fs::write(dir.path().join("Pulumi.blocks.yaml"), blocks_content).unwrap();
        fs::write(dir.path().join("Pulumi.plain.yaml"), plain_content).unwrap();

        let _cwd = TempCwd::new(dir.path());
        let code = run_exec(&["cmd".to_string(), "/c".to_string(), "exit 0".to_string()]);
        assert_eq!(code, 0);

        // All files should be restored to originals
        assert_eq!(
            fs::read_to_string(dir.path().join("Pulumi.yaml")).unwrap(),
            main_content
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("Pulumi.blocks.yaml")).unwrap(),
            blocks_content
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("Pulumi.plain.yaml")).unwrap(),
            plain_content
        );
    }
}
