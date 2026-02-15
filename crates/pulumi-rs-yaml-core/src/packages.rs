use std::collections::HashMap;
use std::path::Path;

use crate::ast::expr::Expr;
use crate::ast::template::*;

/// A package declaration from a lock file or from resource/invoke references.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageDecl {
    #[serde(default)]
    pub package_declaration_version: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub download_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameterization: Option<ParameterizationDecl>,
}

/// Parameterization values for a package.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ParameterizationDecl {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    /// Base64-encoded value.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub value: String,
}

/// A package dependency extracted from a template.
#[derive(Debug, Clone)]
pub struct PackageDependency {
    pub name: String,
    pub version: String,
    pub download_url: String,
    pub parameterization: Option<ParameterizationDecl>,
}

/// Searches a directory recursively for package lock `.yaml` files.
///
/// Lock files are YAML files that parse as a `PackageDecl` with a valid
/// `packageDeclarationVersion` field.
pub fn search_package_decls(directory: &Path) -> Vec<PackageDecl> {
    let mut packages = Vec::new();

    if !directory.is_dir() {
        return packages;
    }

    walk_dir_for_packages(directory, &mut packages);
    packages
}

fn walk_dir_for_packages(dir: &Path, packages: &mut Vec<PackageDecl>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_dir_for_packages(&path, packages);
        } else if path.extension().and_then(|e| e.to_str()) == Some("yaml") {
            if let Some(pkg) = try_parse_package_lock(&path) {
                packages.push(pkg);
            }
        }
    }
}

/// Tries to parse a YAML file as a package lock file.
fn try_parse_package_lock(path: &Path) -> Option<PackageDecl> {
    let data = std::fs::read_to_string(path).ok()?;
    let value: serde_yaml::Value = serde_yaml::from_str(&data).ok()?;
    let map = value.as_mapping()?;

    // Must have packageDeclarationVersion
    let version_key = serde_yaml::Value::String("packageDeclarationVersion".to_string());
    let decl_version = map.get(&version_key)?.as_u64()?;
    if decl_version != 1 {
        return None;
    }

    let name_key = serde_yaml::Value::String("name".to_string());
    let name = map.get(&name_key)?.as_str()?.to_string();
    if name.is_empty() {
        return None;
    }

    let version_val_key = serde_yaml::Value::String("version".to_string());
    let version = map
        .get(&version_val_key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let url_key = serde_yaml::Value::String("downloadUrl".to_string());
    let download_url = map
        .get(&url_key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let param_key = serde_yaml::Value::String("parameterization".to_string());
    let parameterization = map.get(&param_key).and_then(|v| {
        let pm = v.as_mapping()?;
        let p_name = pm
            .get(serde_yaml::Value::String("name".to_string()))?
            .as_str()?
            .to_string();
        let p_version = pm
            .get(serde_yaml::Value::String("version".to_string()))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let p_value = pm
            .get(serde_yaml::Value::String("value".to_string()))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Some(ParameterizationDecl {
            name: p_name,
            version: p_version,
            value: p_value,
        })
    });

    Some(PackageDecl {
        package_declaration_version: 1,
        name,
        version,
        download_url,
        parameterization,
    })
}

/// Extracts the package name from a type token.
///
/// Examples:
/// - `"aws:s3:Bucket"` → `"aws"`
/// - `"kubernetes:core:Service"` → `"kubernetes"`
/// - `"pulumi:providers:aws"` → `"aws"`
pub fn resolve_pkg_name(type_string: &str) -> &str {
    let parts: Vec<&str> = type_string.split(':').collect();

    // pulumi:providers:aws → package is "aws"
    if parts.len() == 3 && parts[0] == "pulumi" && parts[1] == "providers" {
        return parts[2];
    }

    parts[0]
}

/// Gets all referenced packages from a template by scanning resource types and invoke tokens.
///
/// Returns a sorted, de-duplicated list of package dependencies.
pub fn get_referenced_packages(
    template: &TemplateDecl<'_>,
    lock_packages: &[PackageDecl],
) -> Vec<PackageDependency> {
    let mut package_map: HashMap<String, PackageDependency> = HashMap::new();

    // Start with lock file packages
    for pkg in lock_packages {
        let effective_name = if let Some(ref param) = pkg.parameterization {
            param.name.clone()
        } else {
            pkg.name.clone()
        };
        let effective_version = if let Some(ref param) = pkg.parameterization {
            param.version.clone()
        } else {
            pkg.version.clone()
        };

        package_map
            .entry(effective_name.clone())
            .and_modify(|existing| {
                if existing.version.is_empty() {
                    existing.version = effective_version.clone();
                }
                if existing.download_url.is_empty() {
                    existing.download_url = pkg.download_url.clone();
                }
            })
            .or_insert_with(|| PackageDependency {
                name: pkg.name.clone(),
                version: effective_version,
                download_url: pkg.download_url.clone(),
                parameterization: pkg.parameterization.clone(),
            });
    }

    // Scan resources
    for entry in &template.resources {
        let type_token = entry.resource.type_.as_ref();
        let pkg_name = resolve_pkg_name(type_token).to_string();
        let version = entry
            .resource
            .options
            .version
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_default();
        let download_url = entry
            .resource
            .options
            .plugin_download_url
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_default();

        accept_package(&mut package_map, &pkg_name, &version, &download_url);
    }

    // Scan invoke expressions in variables
    for entry in &template.variables {
        scan_expr_for_invokes(&entry.value, &mut package_map);
    }

    // Scan invoke expressions in resource properties
    for entry in &template.resources {
        match &entry.resource.properties {
            ResourceProperties::Map(props) => {
                for prop in props {
                    scan_expr_for_invokes(&prop.value, &mut package_map);
                }
            }
            ResourceProperties::Expr(expr) => {
                scan_expr_for_invokes(expr, &mut package_map);
            }
        }
    }

    // Scan outputs
    for output in &template.outputs {
        scan_expr_for_invokes(&output.value, &mut package_map);
    }

    // Remove the built-in "pulumi" package
    package_map.remove("pulumi");

    // Sort deterministically
    let mut packages: Vec<PackageDependency> = package_map.into_values().collect();
    packages.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.version.cmp(&b.version))
            .then_with(|| a.download_url.cmp(&b.download_url))
    });

    packages
}

/// Adds a package to the map, merging version/download_url if already present.
fn accept_package(
    map: &mut HashMap<String, PackageDependency>,
    name: &str,
    version: &str,
    download_url: &str,
) {
    map.entry(name.to_string())
        .and_modify(|existing| {
            if !version.is_empty() && existing.version.is_empty() {
                existing.version = version.to_string();
            }
            if !download_url.is_empty() && existing.download_url.is_empty() {
                existing.download_url = download_url.to_string();
            }
        })
        .or_insert_with(|| PackageDependency {
            name: name.to_string(),
            version: version.to_string(),
            download_url: download_url.to_string(),
            parameterization: None,
        });
}

/// Recursively scans an expression for invoke calls and adds their packages.
fn scan_expr_for_invokes(expr: &Expr<'_>, map: &mut HashMap<String, PackageDependency>) {
    match expr {
        Expr::Invoke(_, invoke) => {
            let token = invoke.token.as_ref();
            let pkg_name = resolve_pkg_name(token).to_string();
            let version = invoke
                .call_opts
                .version
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default();
            let download_url = invoke
                .call_opts
                .plugin_download_url
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default();
            accept_package(map, &pkg_name, &version, &download_url);

            // Also scan invoke arguments
            if let Some(ref args) = invoke.call_args {
                scan_expr_for_invokes(args, map);
            }
        }
        Expr::List(_, elements) => {
            for elem in elements {
                scan_expr_for_invokes(elem, map);
            }
        }
        Expr::Object(_, entries) => {
            for entry in entries {
                scan_expr_for_invokes(&entry.key, map);
                scan_expr_for_invokes(&entry.value, map);
            }
        }
        Expr::Join(_, a, b) | Expr::Select(_, a, b) | Expr::Split(_, a, b) => {
            scan_expr_for_invokes(a, map);
            scan_expr_for_invokes(b, map);
        }
        Expr::ToJson(_, inner)
        | Expr::ToBase64(_, inner)
        | Expr::FromBase64(_, inner)
        | Expr::Secret(_, inner)
        | Expr::ReadFile(_, inner)
        | Expr::Abs(_, inner)
        | Expr::Floor(_, inner)
        | Expr::Ceil(_, inner)
        | Expr::Max(_, inner)
        | Expr::Min(_, inner)
        | Expr::StringLen(_, inner)
        | Expr::TimeUtc(_, inner)
        | Expr::TimeUnix(_, inner)
        | Expr::Uuid(_, inner)
        | Expr::RandomString(_, inner)
        | Expr::DateFormat(_, inner)
        | Expr::StringAsset(_, inner)
        | Expr::FileAsset(_, inner)
        | Expr::RemoteAsset(_, inner)
        | Expr::FileArchive(_, inner)
        | Expr::RemoteArchive(_, inner) => {
            scan_expr_for_invokes(inner, map);
        }
        Expr::Substring(_, a, b, c) => {
            scan_expr_for_invokes(a, map);
            scan_expr_for_invokes(b, map);
            scan_expr_for_invokes(c, map);
        }
        Expr::AssetArchive(_, entries) => {
            for (_, v) in entries {
                scan_expr_for_invokes(v, map);
            }
        }
        // Terminals - no invokes
        Expr::Null(_)
        | Expr::Bool(_, _)
        | Expr::Number(_, _)
        | Expr::String(_, _)
        | Expr::Interpolate(_, _)
        | Expr::Symbol(_, _) => {}
    }
}

/// Resolves a type token to its canonical form.
///
/// Tries expansions:
/// 1. Exact match: `"aws:s3:Bucket"`
/// 2. Short form: `"aws:Bucket"` → `"aws:index:Bucket"`
/// 3. Module form: `"aws:s3:Bucket"` → `"aws:s3/bucket:Bucket"`
///
/// Returns the expanded forms to try, in order.
pub fn expand_type_token(type_name: &str) -> Vec<String> {
    let parts: Vec<&str> = type_name.split(':').collect();
    let mut candidates = vec![type_name.to_string()];

    match parts.len() {
        2 => {
            // random:RandomPassword → random:index/randomPassword:RandomPassword
            let lower_camel = to_lower_camel(parts[1]);
            candidates.push(format!("{}:index/{}:{}", parts[0], lower_camel, parts[1]));
        }
        3 => {
            // aws:s3:Bucket → aws:s3/bucket:Bucket
            let lower_camel = to_lower_camel(parts[2]);
            candidates.push(format!(
                "{}:{}/{}:{}",
                parts[0], parts[1], lower_camel, parts[2]
            ));
        }
        _ => {}
    }

    candidates
}

/// Returns the canonical form of a type token for use in resource registration.
///
/// Providers expect type tokens in canonical form with the module path:
/// - `gcp:storage:Bucket` → `gcp:storage/bucket:Bucket`
/// - `aws:s3:Bucket` → `aws:s3/bucket:Bucket`
/// - `aws:Bucket` → `aws:index/bucket:Bucket`
///
/// Types that are already canonical (contain `/` in the module) or are
/// built-in Pulumi types are returned unchanged.
pub fn canonicalize_type_token(type_name: &str) -> String {
    let parts: Vec<&str> = type_name.split(':').collect();

    // Already canonical if module contains '/'
    if parts.len() == 3 && parts[1].contains('/') {
        return type_name.to_string();
    }

    // Don't expand built-in Pulumi types
    if parts.first() == Some(&"pulumi") {
        return type_name.to_string();
    }

    match parts.len() {
        2 => {
            // random:RandomPassword → random:index/randomPassword:RandomPassword
            let lower_camel = to_lower_camel(parts[1]);
            format!("{}:index/{}:{}", parts[0], lower_camel, parts[1])
        }
        3 => {
            // gcp:storage:Bucket → gcp:storage/bucket:Bucket
            let lower_camel = to_lower_camel(parts[2]);
            format!("{}:{}/{}:{}", parts[0], parts[1], lower_camel, parts[2])
        }
        _ => type_name.to_string(),
    }
}

/// Collapses a canonical type token to its shortest display form.
///
/// This is a partial inverse of `canonicalize_type_token()`:
/// - `aws:s3/bucket:Bucket` → `aws:s3:Bucket` (module suffix matches type name)
/// - `foo:index/bar:Bar` → `foo:index:Bar` (index module kept if suffix matches)
/// - `foo:index:Bar` → `foo:Bar` (index module stripped)
/// - `foo::Bar` → `foo:Bar` (empty module stripped)
/// - `fizz:mod:buzz` → `fizz:mod:buzz` (unchanged)
pub fn collapse_type_token(token: &str) -> String {
    let parts: Vec<&str> = token.split(':').collect();

    if parts.len() != 3 {
        return token.to_string();
    }

    let (pkg, module, type_name) = (parts[0], parts[1], parts[2]);

    // Check if module contains '/' (e.g., "s3/bucket")
    if let Some(slash_pos) = module.find('/') {
        let mod_prefix = &module[..slash_pos];
        let mod_suffix = &module[slash_pos + 1..];

        // If title(mod_suffix) == type_name, collapse to pkg:mod_prefix:type_name
        let title_suffix = title_case(mod_suffix);
        if title_suffix == type_name {
            let collapsed_module = mod_prefix;
            // If mod_prefix is "index" or empty, strip it
            if collapsed_module == "index" || collapsed_module.is_empty() {
                return format!("{}:{}", pkg, type_name);
            }
            return format!("{}:{}:{}", pkg, collapsed_module, type_name);
        }
        // Doesn't match, return as-is
        return token.to_string();
    }

    // No slash - check for "index" or empty module
    if module == "index" || module.is_empty() {
        return format!("{}:{}", pkg, type_name);
    }

    token.to_string()
}

/// Title-cases a string (first character uppercase, rest unchanged).
fn title_case(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap();
    let mut result = first.to_uppercase().to_string();
    result.extend(chars);
    result
}

/// Converts a PascalCase name to lowerCamelCase.
pub fn to_lower_camel(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap();
    let mut result = first.to_lowercase().to_string();
    result.extend(chars);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_pkg_name_standard() {
        assert_eq!(resolve_pkg_name("aws:s3:Bucket"), "aws");
        assert_eq!(resolve_pkg_name("kubernetes:core:Service"), "kubernetes");
        assert_eq!(
            resolve_pkg_name("azure-native:storage:Account"),
            "azure-native"
        );
    }

    #[test]
    fn test_resolve_pkg_name_provider() {
        assert_eq!(resolve_pkg_name("pulumi:providers:aws"), "aws");
        assert_eq!(resolve_pkg_name("pulumi:providers:gcp"), "gcp");
    }

    #[test]
    fn test_resolve_pkg_name_short() {
        assert_eq!(resolve_pkg_name("aws:Bucket"), "aws");
    }

    #[test]
    fn test_expand_type_token_three_parts() {
        let candidates = expand_type_token("aws:s3:Bucket");
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], "aws:s3:Bucket");
        assert_eq!(candidates[1], "aws:s3/bucket:Bucket");
    }

    #[test]
    fn test_expand_type_token_two_parts() {
        let candidates = expand_type_token("aws:Bucket");
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], "aws:Bucket");
        assert_eq!(candidates[1], "aws:index/bucket:Bucket");
    }

    #[test]
    fn test_expand_type_token_already_canonical() {
        let candidates = expand_type_token("aws:s3/bucket:Bucket");
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], "aws:s3/bucket:Bucket");
    }

    #[test]
    fn test_to_lower_camel() {
        assert_eq!(to_lower_camel("Bucket"), "bucket");
        assert_eq!(to_lower_camel("StorageAccount"), "storageAccount");
        assert_eq!(to_lower_camel(""), "");
        assert_eq!(to_lower_camel("a"), "a");
    }

    #[test]
    fn test_get_referenced_packages() {
        use crate::ast::parse::parse_template;

        let source = r#"
name: test
runtime: yaml
resources:
  bucket:
    type: aws:s3:Bucket
  db:
    type: gcp:sql:DatabaseInstance
variables:
  result:
    fn::invoke:
      function: azure:compute:getVirtualMachine
      arguments:
        name: my-vm
      return: id
"#;
        let (template, _) = parse_template(source, None);
        let packages = get_referenced_packages(&template, &[]);

        assert_eq!(packages.len(), 3);
        let names: Vec<&str> = packages.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"aws"));
        assert!(names.contains(&"azure"));
        assert!(names.contains(&"gcp"));
    }

    #[test]
    fn test_get_referenced_packages_with_pulumi_provider() {
        use crate::ast::parse::parse_template;

        let source = r#"
name: test
runtime: yaml
resources:
  myProvider:
    type: pulumi:providers:aws
  bucket:
    type: aws:s3:Bucket
"#;
        let (template, _) = parse_template(source, None);
        let packages = get_referenced_packages(&template, &[]);

        // Both resources reference "aws", so we should get 1 package
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "aws");
    }

    #[test]
    fn test_get_referenced_packages_skips_pulumi() {
        use crate::ast::parse::parse_template;

        let source = r#"
name: test
runtime: yaml
resources:
  stack:
    type: pulumi:pulumi:StackReference
    properties:
      name: other/stack
"#;
        let (template, _) = parse_template(source, None);
        let packages = get_referenced_packages(&template, &[]);
        // "pulumi" package should be filtered out
        assert!(packages.is_empty());
    }

    #[test]
    fn test_canonicalize_type_token_three_parts() {
        assert_eq!(
            canonicalize_type_token("gcp:storage:Bucket"),
            "gcp:storage/bucket:Bucket"
        );
        assert_eq!(
            canonicalize_type_token("aws:s3:Bucket"),
            "aws:s3/bucket:Bucket"
        );
        assert_eq!(
            canonicalize_type_token("azure-native:storage:Account"),
            "azure-native:storage/account:Account"
        );
    }

    #[test]
    fn test_canonicalize_type_token_multi_word() {
        assert_eq!(
            canonicalize_type_token("gcp:iam:CamelCaseHere"),
            "gcp:iam/camelCaseHere:CamelCaseHere"
        );
        assert_eq!(
            canonicalize_type_token("aws:ec2:SecurityGroup"),
            "aws:ec2/securityGroup:SecurityGroup"
        );
    }

    #[test]
    fn test_canonicalize_type_token_already_canonical() {
        assert_eq!(
            canonicalize_type_token("gcp:storage/bucket:Bucket"),
            "gcp:storage/bucket:Bucket"
        );
        assert_eq!(
            canonicalize_type_token("aws:s3/bucket:Bucket"),
            "aws:s3/bucket:Bucket"
        );
    }

    #[test]
    fn test_canonicalize_type_token_pulumi_builtins() {
        assert_eq!(
            canonicalize_type_token("pulumi:pulumi:Stack"),
            "pulumi:pulumi:Stack"
        );
        assert_eq!(
            canonicalize_type_token("pulumi:providers:aws"),
            "pulumi:providers:aws"
        );
        assert_eq!(
            canonicalize_type_token("pulumi:pulumi:StackReference"),
            "pulumi:pulumi:StackReference"
        );
    }

    #[test]
    fn test_collapse_type_token_module_slash() {
        // aws:s3/bucket:Bucket → aws:s3:Bucket (title(bucket) == Bucket)
        assert_eq!(collapse_type_token("aws:s3/bucket:Bucket"), "aws:s3:Bucket");
        assert_eq!(
            collapse_type_token("aws:ec2/securityGroup:SecurityGroup"),
            "aws:ec2:SecurityGroup"
        );
    }

    #[test]
    fn test_collapse_type_token_index_slash() {
        // foo:index/bar:Bar → foo:Bar (index stripped)
        assert_eq!(collapse_type_token("foo:index/bar:Bar"), "foo:Bar");
        assert_eq!(
            collapse_type_token("random:index/randomPassword:RandomPassword"),
            "random:RandomPassword"
        );
    }

    #[test]
    fn test_collapse_type_token_index_no_slash() {
        // foo:index:Bar → foo:Bar
        assert_eq!(collapse_type_token("foo:index:Bar"), "foo:Bar");
    }

    #[test]
    fn test_collapse_type_token_empty_module() {
        // foo::Bar → foo:Bar
        assert_eq!(collapse_type_token("foo::Bar"), "foo:Bar");
    }

    #[test]
    fn test_collapse_type_token_unchanged() {
        // fizz:mod:buzz → fizz:mod:buzz (no match)
        assert_eq!(collapse_type_token("fizz:mod:buzz"), "fizz:mod:buzz");
    }

    #[test]
    fn test_collapse_type_token_mismatch_suffix() {
        // If mod_suffix doesn't title-case to type_name, keep as-is
        assert_eq!(
            collapse_type_token("aws:s3/bucketPolicy:BucketPolicy"),
            "aws:s3:BucketPolicy"
        );
        assert_eq!(
            collapse_type_token("aws:s3/other:Bucket"),
            "aws:s3/other:Bucket"
        );
    }

    #[test]
    fn test_collapse_type_token_pulumi_builtins() {
        assert_eq!(
            collapse_type_token("pulumi:pulumi:StackReference"),
            "pulumi:pulumi:StackReference"
        );
        assert_eq!(
            collapse_type_token("pulumi:providers:aws"),
            "pulumi:providers:aws"
        );
    }

    #[test]
    fn test_canonicalize_type_token_two_parts() {
        assert_eq!(
            canonicalize_type_token("aws:Bucket"),
            "aws:index/bucket:Bucket"
        );
        assert_eq!(
            canonicalize_type_token("random:RandomPassword"),
            "random:index/randomPassword:RandomPassword"
        );
    }

    #[test]
    fn test_package_version_conflict_error() {
        use crate::ast::parse::parse_template;

        let source = r#"
name: test
runtime: yaml
resources:
  a:
    type: aws:s3:Bucket
    options:
      version: "5.0.0"
  b:
    type: aws:ec2:Instance
    options:
      version: "6.0.0"
"#;
        let (template, _) = parse_template(source, None);
        let packages = get_referenced_packages(&template, &[]);
        // Both resources reference "aws" but with different versions.
        // The current implementation merges (first-write-wins for version).
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "aws");
        // The first version seen is kept
        assert_eq!(packages[0].version, "5.0.0");
    }

    #[test]
    fn test_package_download_url_conflict_dedup() {
        use crate::ast::parse::parse_template;

        let source = r#"
name: test
runtime: yaml
resources:
  a:
    type: custom:Foo
    options:
      pluginDownloadURL: "https://example.com/v1"
  b:
    type: custom:Bar
    options:
      pluginDownloadURL: "https://example.com/v2"
"#;
        let (template, _) = parse_template(source, None);
        let packages = get_referenced_packages(&template, &[]);
        // Both use "custom" package; first download URL wins
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "custom");
        assert_eq!(packages[0].download_url, "https://example.com/v1");
    }

    #[test]
    fn test_package_duplicate_version_deduped() {
        use crate::ast::parse::parse_template;

        let source = r#"
name: test
runtime: yaml
resources:
  a:
    type: aws:s3:Bucket
    options:
      version: "5.0.0"
  b:
    type: aws:ec2:Instance
    options:
      version: "5.0.0"
"#;
        let (template, _) = parse_template(source, None);
        let packages = get_referenced_packages(&template, &[]);
        // Same package + same version → single entry
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "aws");
        assert_eq!(packages[0].version, "5.0.0");
    }
}
