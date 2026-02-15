//! Shared template file discovery for the YAML language host.
//!
//! Used by both the runner (Run RPC) and the server (GetRequiredPackages, etc.).
//! Delegates to `pulumi_rs_yaml_core::multi_file` for multi-file discovery.

#![allow(dead_code)]

use std::path::Path;

use pulumi_rs_yaml_core::multi_file::{discover_project_files, ProjectFiles};

/// Discovers all project files in a program directory.
///
/// Returns `ProjectFiles` with the main `Pulumi.yaml` and any additional
/// `Pulumi.*.yaml` files sorted alphabetically.
pub fn load_project_files(directory: &str) -> Result<ProjectFiles, String> {
    let dir = Path::new(directory);
    discover_project_files(dir)
}

/// Loads the main YAML template source from a program directory.
///
/// Only loads `Pulumi.yaml` (the main file). For multi-file support,
/// use `load_project_files()` or `multi_file::load_project()` instead.
pub fn load_template(directory: &str) -> Result<String, String> {
    let dir = Path::new(directory);
    let main_path = dir.join("Pulumi.yaml");

    if main_path.exists() {
        return std::fs::read_to_string(&main_path)
            .map_err(|e| format!("failed to read {}: {}", main_path.display(), e));
    }

    // Try .yml variant
    let yml_path = dir.join("Pulumi.yml");
    if yml_path.exists() {
        return std::fs::read_to_string(&yml_path)
            .map_err(|e| format!("failed to read {}: {}", yml_path.display(), e));
    }

    Err(format!("no Pulumi.yaml found in {}", directory))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_template_missing_dir() {
        let result = load_template("/nonexistent/path");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no Pulumi.yaml"));
    }

    #[test]
    fn test_load_template_pulumi_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let pulumi_yaml = dir.path().join("Pulumi.yaml");
        std::fs::write(&pulumi_yaml, "name: test\nruntime: yaml\n").unwrap();

        let result = load_template(dir.path().to_str().unwrap());
        assert!(result.is_ok());
        assert!(result.unwrap().contains("name: test"));
    }

    #[test]
    fn test_load_template_pulumi_yml() {
        let dir = tempfile::tempdir().unwrap();
        let pulumi_yml = dir.path().join("Pulumi.yml");
        std::fs::write(&pulumi_yml, "name: yml-test\nruntime: yaml\n").unwrap();

        let result = load_template(dir.path().to_str().unwrap());
        assert!(result.is_ok());
        assert!(result.unwrap().contains("name: yml-test"));
    }

    #[test]
    fn test_load_project_files_single() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Pulumi.yaml"), "name: test\n").unwrap();

        let files = load_project_files(dir.path().to_str().unwrap()).unwrap();
        assert!(files.additional_files.is_empty());
    }

    #[test]
    fn test_load_project_files_multi() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Pulumi.yaml"), "name: test\n").unwrap();
        std::fs::write(dir.path().join("Pulumi.buckets.yaml"), "resources: {}\n").unwrap();
        std::fs::write(dir.path().join("Pulumi.tables.yaml"), "resources: {}\n").unwrap();

        let files = load_project_files(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(files.additional_files.len(), 2);
    }
}
