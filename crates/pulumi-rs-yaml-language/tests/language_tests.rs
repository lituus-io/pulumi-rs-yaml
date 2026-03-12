//! Integration tests for the language host server and related components.
//!
//! These tests validate Pack, GeneratePackage, template loading, and
//! other language host functionality without requiring a running Pulumi engine.

// ========== Template Loader Tests ==========

#[test]
fn test_load_template_main_yaml() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Main.yaml"),
        "name: test\nruntime: yaml\nresources:\n  r1:\n    type: aws:s3:Bucket\n",
    )
    .unwrap();

    let result = load_template_from_dir(dir.path());
    assert!(result.is_ok());
    assert!(result.unwrap().contains("name: test"));
}

#[test]
fn test_load_template_main_json() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Main.json"),
        r#"{"name":"test","runtime":"yaml"}"#,
    )
    .unwrap();

    let result = load_template_from_dir(dir.path());
    assert!(result.is_ok());
}

#[test]
fn test_load_template_pulumi_yaml_fallback() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Pulumi.yaml"),
        "name: fallback\nruntime: yaml\n",
    )
    .unwrap();

    let result = load_template_from_dir(dir.path());
    assert!(result.is_ok());
    assert!(result.unwrap().contains("fallback"));
}

#[test]
fn test_load_template_missing() {
    let dir = tempfile::tempdir().unwrap();
    let result = load_template_from_dir(dir.path());
    assert!(result.is_err());
}

#[test]
fn test_load_template_priority() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Main.yaml"), "main_yaml").unwrap();
    std::fs::write(dir.path().join("Main.json"), "main_json").unwrap();
    std::fs::write(dir.path().join("Pulumi.yaml"), "pulumi_yaml").unwrap();

    let result = load_template_from_dir(dir.path()).unwrap();
    assert_eq!(result, "main_yaml");
}

// ========== Pack Tests ==========

#[test]
fn test_pack_single_file() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    std::fs::write(src.path().join("aws-1.0.0.yaml"), "test content").unwrap();

    let result = pack_directory(src.path(), dst.path());
    assert!(result.is_ok());

    let artifact = result.unwrap();
    assert!(artifact.ends_with("aws-1.0.0.yaml"));
    assert_eq!(
        std::fs::read_to_string(dst.path().join("aws-1.0.0.yaml")).unwrap(),
        "test content"
    );
}

#[test]
fn test_pack_empty_directory() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    let result = pack_directory(src.path(), dst.path());
    assert!(result.is_err());
}

#[test]
fn test_pack_multiple_files_error() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    std::fs::write(src.path().join("file1.yaml"), "a").unwrap();
    std::fs::write(src.path().join("file2.yaml"), "b").unwrap();

    let result = pack_directory(src.path(), dst.path());
    assert!(result.is_err());
}

// ========== GeneratePackage Tests ==========

#[test]
fn test_generate_package_simple() {
    let dir = tempfile::tempdir().unwrap();
    let schema = serde_json::json!({
        "name": "aws",
        "version": "6.0.0",
        "pluginDownloadURL": ""
    });

    let result = generate_package_lock(dir.path(), &schema.to_string());
    assert!(result.is_ok());

    let lock_path = dir.path().join("aws-6.0.0.yaml");
    assert!(lock_path.exists());

    let content = std::fs::read_to_string(lock_path).unwrap();
    assert!(content.contains("aws"));
    assert!(content.contains("6.0.0"));
}

#[test]
fn test_generate_package_no_version() {
    let dir = tempfile::tempdir().unwrap();
    let schema = serde_json::json!({
        "name": "custom",
        "pluginDownloadURL": "https://example.com"
    });

    let result = generate_package_lock(dir.path(), &schema.to_string());
    assert!(result.is_ok());

    let lock_path = dir.path().join("custom.yaml");
    assert!(lock_path.exists());
}

#[test]
fn test_generate_package_with_parameterization() {
    let dir = tempfile::tempdir().unwrap();
    let schema = serde_json::json!({
        "name": "my-pkg",
        "version": "1.0.0",
        "pluginDownloadURL": "",
        "parameterization": {
            "baseProvider": {
                "name": "base-provider",
                "version": "2.0.0"
            },
            "parameter": "dGVzdA=="
        }
    });

    let result = generate_package_lock(dir.path(), &schema.to_string());
    assert!(result.is_ok());

    let lock_path = dir.path().join("my-pkg-1.0.0.yaml");
    assert!(lock_path.exists());

    let content = std::fs::read_to_string(lock_path).unwrap();
    assert!(content.contains("base-provider"));
    assert!(content.contains("my-pkg"));
}

// ========== Package Scanning Tests ==========

#[test]
fn test_package_decl_round_trip() {
    let decl = pulumi_rs_yaml_core::packages::PackageDecl {
        package_declaration_version: 1,
        name: "aws".to_string(),
        version: "6.0.0".to_string(),
        download_url: String::new(),
        parameterization: None,
    };

    let yaml = serde_yaml::to_string(&decl).unwrap();
    assert!(yaml.contains("packageDeclarationVersion: 1"));
    assert!(yaml.contains("name: aws"));
    assert!(yaml.contains("version: 6.0.0"));

    let parsed: pulumi_rs_yaml_core::packages::PackageDecl = serde_yaml::from_str(&yaml).unwrap();
    assert_eq!(parsed.name, "aws");
    assert_eq!(parsed.version, "6.0.0");
    assert_eq!(parsed.package_declaration_version, 1);
}

#[test]
fn test_package_decl_with_parameterization_round_trip() {
    let decl = pulumi_rs_yaml_core::packages::PackageDecl {
        package_declaration_version: 1,
        name: "base".to_string(),
        version: "1.0.0".to_string(),
        download_url: "https://example.com".to_string(),
        parameterization: Some(pulumi_rs_yaml_core::packages::ParameterizationDecl {
            name: "derived".to_string(),
            version: "2.0.0".to_string(),
            value: "dGVzdA==".to_string(),
        }),
    };

    let yaml = serde_yaml::to_string(&decl).unwrap();
    let parsed: pulumi_rs_yaml_core::packages::PackageDecl = serde_yaml::from_str(&yaml).unwrap();

    assert_eq!(parsed.parameterization.as_ref().unwrap().name, "derived");
    assert_eq!(parsed.parameterization.as_ref().unwrap().version, "2.0.0");
}

#[test]
fn test_search_package_decls_in_directory() {
    let dir = tempfile::tempdir().unwrap();

    // Write a valid package lock file
    let lock_content = r#"
packageDeclarationVersion: 1
name: aws
version: "6.0.0"
"#;
    std::fs::write(dir.path().join("aws-6.0.0.yaml"), lock_content).unwrap();

    // Write a non-lock YAML file
    std::fs::write(
        dir.path().join("Pulumi.yaml"),
        "name: test\nruntime: yaml\n",
    )
    .unwrap();

    let packages = pulumi_rs_yaml_core::packages::search_package_decls(dir.path());
    assert_eq!(packages.len(), 1);
    assert_eq!(packages[0].name, "aws");
    assert_eq!(packages[0].version, "6.0.0");
}

// ========== Helper Functions ==========
// These simulate the server RPCs without needing a gRPC connection.

fn load_template_from_dir(dir: &std::path::Path) -> Result<String, String> {
    let dir_str = dir.to_str().unwrap();
    for name in &["Main.yaml", "Main.json", "Pulumi.yaml"] {
        let path = dir.join(name);
        if path.exists() {
            return std::fs::read_to_string(&path)
                .map_err(|e| format!("failed to read {}: {}", path.display(), e));
        }
    }
    Err(format!("no template found in {}", dir_str))
}

fn pack_directory(src: &std::path::Path, dst: &std::path::Path) -> Result<String, String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("mkdir: {}", e))?;

    let entries: Vec<_> = std::fs::read_dir(src)
        .map_err(|e| format!("readdir: {}", e))?
        .filter_map(|e| e.ok())
        .collect();

    if entries.is_empty() {
        return Err("no files in package directory".to_string());
    }
    if entries.len() > 1 {
        return Err(format!(
            "multiple files: {} and {}",
            entries[0].file_name().to_string_lossy(),
            entries[1].file_name().to_string_lossy()
        ));
    }

    let name = entries[0].file_name();
    let src_file = src.join(&name);
    let dst_file = dst.join(&name);
    std::fs::copy(&src_file, &dst_file).map_err(|e| format!("copy: {}", e))?;
    Ok(dst_file.to_string_lossy().to_string())
}

fn generate_package_lock(dir: &std::path::Path, schema_json: &str) -> Result<(), String> {
    let spec: serde_json::Value =
        serde_json::from_str(schema_json).map_err(|e| format!("parse: {}", e))?;

    let pkg_name = spec
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let pkg_version = spec
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let download_url = spec
        .get("pluginDownloadURL")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let parameterization = spec.get("parameterization");

    let mut lock = pulumi_rs_yaml_core::packages::PackageDecl {
        package_declaration_version: 1,
        name: String::new(),
        version: String::new(),
        download_url: String::new(),
        parameterization: None,
    };

    if let Some(param) = parameterization {
        let base_name = param
            .get("baseProvider")
            .and_then(|bp| bp.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let base_version = param
            .get("baseProvider")
            .and_then(|bp| bp.get("version"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        lock.name = base_name;
        lock.version = base_version;
        lock.download_url = download_url;

        let param_value = param
            .get("parameter")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        lock.parameterization = Some(pulumi_rs_yaml_core::packages::ParameterizationDecl {
            name: pkg_name.clone(),
            version: pkg_version.clone(),
            value: param_value,
        });
    } else {
        lock.name = pkg_name.clone();
        lock.version = pkg_version.clone();
        lock.download_url = download_url;
    }

    let version_suffix = if pkg_version.is_empty() {
        String::new()
    } else {
        format!("-{}", pkg_version)
    };
    let dest = dir.join(format!("{}{}.yaml", pkg_name, version_suffix));

    std::fs::create_dir_all(dir).map_err(|e| format!("mkdir: {}", e))?;

    let yaml = serde_yaml::to_string(&lock).map_err(|e| format!("serialize: {}", e))?;
    std::fs::write(&dest, yaml).map_err(|e| format!("write: {}", e))?;

    Ok(())
}
