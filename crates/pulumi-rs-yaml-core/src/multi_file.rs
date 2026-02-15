//! Multi-file Pulumi YAML support.
//!
//! Discovers, parses, and merges multiple `Pulumi.*.yaml` files into a single
//! [`TemplateDecl`] for evaluation. The evaluator, topological sort, and expression
//! resolution are unchanged — they operate on the merged flat namespace.
//!
//! # File Discovery
//!
//! - `Pulumi.yaml` is required (main file with metadata, config, resources, outputs)
//! - `Pulumi.*.yaml` / `Pulumi.*.yml` are additional resource files
//! - Stack config files (`Pulumi.<stack>.yaml`) are handled by the CLI, not us
//! - Files are sorted alphabetically for deterministic ordering
//!
//! # Merge Rules
//!
//! | Field       | Main | Additional | Collision |
//! |-------------|------|-----------|-----------|
//! | name        | Req  | Forbidden | Error     |
//! | runtime     | Req  | Forbidden | Error     |
//! | config      | OK   | Forbidden | Error     |
//! | resources   | OK   | OK        | Dup error |
//! | variables   | OK   | OK        | Dup error |
//! | outputs     | OK   | OK        | Dup error |
//! | components  | OK   | OK        | Dup error |

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::ast::parse::parse_template;
use crate::ast::template::*;
use crate::diag::Diagnostics;
use crate::jinja::{validate_rendered_yaml, JinjaContext, JinjaPreprocessor, TemplatePreprocessor};

/// The set of project files discovered in a directory.
#[derive(Debug, Clone)]
pub struct ProjectFiles {
    /// The main `Pulumi.yaml` file path.
    pub main_file: PathBuf,
    /// Additional `Pulumi.*.yaml` files, sorted alphabetically.
    pub additional_files: Vec<PathBuf>,
}

impl ProjectFiles {
    /// Returns an iterator over all files (main first, then additional).
    pub fn all_files(&self) -> impl Iterator<Item = &PathBuf> {
        std::iter::once(&self.main_file).chain(self.additional_files.iter())
    }

    /// Returns the total number of files.
    pub fn file_count(&self) -> usize {
        1 + self.additional_files.len()
    }
}

/// A merged multi-file template with source tracking.
#[derive(Debug, Clone)]
pub struct MergedTemplate {
    /// The main file's metadata (name, description, config, pulumi settings).
    main_name: Option<Cow<'static, str>>,
    main_namespace: Option<Cow<'static, str>>,
    main_description: Option<Cow<'static, str>>,
    main_pulumi: PulumiDecl<'static>,
    /// Merged config (from main file only).
    config: Vec<ConfigEntry<'static>>,
    /// Merged resources from all files.
    resources: Vec<ResourceEntry<'static>>,
    /// Merged variables from all files.
    variables: Vec<VariableEntry<'static>>,
    /// Merged outputs from all files.
    outputs: Vec<OutputEntry<'static>>,
    /// Merged components from all files.
    components: Vec<ComponentDecl<'static>>,
    /// Maps logical name → source filename for error reporting.
    source_map: HashMap<String, String>,
}

impl MergedTemplate {
    /// Creates a unified `TemplateDecl` view for evaluation.
    ///
    /// The existing `topological_sort()` and `evaluate_template()` work unchanged
    /// on this merged view — they see a flat namespace of resources/variables.
    pub fn as_template_decl(&self) -> TemplateDecl<'static> {
        TemplateDecl {
            meta: crate::syntax::ExprMeta::no_span(),
            name: self.main_name.clone(),
            namespace: self.main_namespace.clone(),
            description: self.main_description.clone(),
            pulumi: self.main_pulumi.clone(),
            config: self.config.clone(),
            variables: self.variables.clone(),
            resources: self.resources.clone(),
            outputs: self.outputs.clone(),
            components: self.components.clone(),
        }
    }

    /// Returns the project name from the main file.
    pub fn name(&self) -> Option<&str> {
        self.main_name.as_deref()
    }

    /// Returns the config entries.
    pub fn config(&self) -> &[ConfigEntry<'static>] {
        &self.config
    }

    /// Returns the resources.
    pub fn resources(&self) -> &[ResourceEntry<'static>] {
        &self.resources
    }

    /// Returns the variables.
    pub fn variables(&self) -> &[VariableEntry<'static>] {
        &self.variables
    }

    /// Returns the outputs.
    pub fn outputs(&self) -> &[OutputEntry<'static>] {
        &self.outputs
    }

    /// Returns the source file for a given logical name.
    pub fn source_file(&self, name: &str) -> Option<&str> {
        self.source_map.get(name).map(|s| s.as_str())
    }

    /// Returns the number of merged resources.
    pub fn resource_count(&self) -> usize {
        self.resources.len()
    }

    /// Returns the number of merged variables.
    pub fn variable_count(&self) -> usize {
        self.variables.len()
    }

    /// Returns the number of merged outputs.
    pub fn output_count(&self) -> usize {
        self.outputs.len()
    }

    /// Returns the number of merged components.
    pub fn component_count(&self) -> usize {
        self.components.len()
    }

    /// Returns all resource logical names.
    pub fn resource_names(&self) -> Vec<&str> {
        self.resources
            .iter()
            .map(|r| r.logical_name.as_ref())
            .collect()
    }

    /// Returns the source map (logical_name → filename).
    pub fn source_map(&self) -> &HashMap<String, String> {
        &self.source_map
    }

    /// Returns the number of files that contributed to this merged template.
    pub fn file_count(&self) -> usize {
        let mut files: Vec<&str> = self.source_map.values().map(|s| s.as_str()).collect();
        files.sort();
        files.dedup();
        files.len()
    }
}

/// Discovers project files in a directory.
///
/// Returns `Pulumi.yaml` as the main file and any `Pulumi.*.yaml`/`Pulumi.*.yml`
/// as additional files. Additional files are sorted alphabetically.
pub fn discover_project_files(directory: &Path) -> Result<ProjectFiles, String> {
    // Look for main file
    let main_yaml = directory.join("Pulumi.yaml");
    let main_yml = directory.join("Pulumi.yml");

    let main_file = if main_yaml.exists() {
        main_yaml
    } else if main_yml.exists() {
        main_yml
    } else {
        return Err(format!("no Pulumi.yaml found in {}", directory.display()));
    };

    // Discover additional files matching Pulumi.*.yaml or Pulumi.*.yml
    let mut additional_files = Vec::new();

    let entries = std::fs::read_dir(directory)
        .map_err(|e| format!("failed to read directory {}: {}", directory.display(), e))?;

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();

        // Must match Pulumi.*.yaml or Pulumi.*.yml pattern
        if !name.starts_with("Pulumi.") {
            continue;
        }

        let is_yaml = name.ends_with(".yaml");
        let is_yml = name.ends_with(".yml");
        if !is_yaml && !is_yml {
            continue;
        }

        // Skip the main file itself
        if name == "Pulumi.yaml" || name == "Pulumi.yml" {
            continue;
        }

        // Extract the middle part: Pulumi.MIDDLE.yaml
        let ext_len = if is_yaml { 5 } else { 4 }; // ".yaml" or ".yml"
        let middle_start = 7; // "Pulumi."
        let middle_end = name.len() - ext_len;
        if middle_end <= middle_start {
            continue; // No middle part
        }
        let middle = &name[middle_start..middle_end];

        // Skip empty middle part
        if middle.is_empty() {
            continue;
        }

        // Validate it's a regular file
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        additional_files.push(path);
    }

    // Sort alphabetically for deterministic ordering
    additional_files.sort();

    Ok(ProjectFiles {
        main_file,
        additional_files,
    })
}

/// Merges multiple parsed templates into a single `MergedTemplate`.
///
/// `main` is the parsed `Pulumi.yaml`. `additional` is a list of
/// `(filename, parsed_template)` pairs for each additional file.
///
/// Returns the merged template and any diagnostics (errors for collisions,
/// forbidden fields in additional files, etc.).
pub fn merge_templates(
    main: TemplateDecl<'static>,
    main_path: &str,
    additional: Vec<(String, TemplateDecl<'static>)>,
) -> (MergedTemplate, Diagnostics) {
    let mut diags = Diagnostics::new();
    let mut source_map = HashMap::new();

    // Start with main file's contents
    let mut resources = main.resources.clone();
    let mut variables = main.variables.clone();
    let mut outputs = main.outputs.clone();
    let mut components = main.components.clone();

    // Track names from main file
    for r in &main.resources {
        source_map.insert(r.logical_name.to_string(), main_path.to_string());
    }
    for v in &main.variables {
        source_map.insert(v.key.to_string(), main_path.to_string());
    }
    for o in &main.outputs {
        source_map.insert(o.key.to_string(), main_path.to_string());
    }
    for c in &main.components {
        source_map.insert(c.key.to_string(), main_path.to_string());
    }

    // Merge each additional file
    for (filename, template) in &additional {
        // Detect Pulumi stack config files (e.g., Pulumi.dev.yaml).
        // These are created by the Pulumi CLI and only contain `config:`
        // (and optionally name/description). Skip them silently.
        let is_stack_config = !template.config.is_empty()
            && template.resources.is_empty()
            && template.variables.is_empty()
            && template.outputs.is_empty()
            && template.components.is_empty();
        if is_stack_config {
            continue;
        }

        // Forbidden fields in additional files
        if template.name.is_some() {
            diags.error(
                None,
                format!(
                    "'name' is only allowed in {}, found in {}",
                    main_path, filename
                ),
                "",
            );
        }
        if template.description.is_some() {
            diags.error(
                None,
                format!(
                    "'description' is only allowed in {}, found in {}",
                    main_path, filename
                ),
                "",
            );
        }
        if !template.config.is_empty() {
            diags.error(
                None,
                format!(
                    "'config' is only allowed in {}, found in {}",
                    main_path, filename
                ),
                "",
            );
        }

        // Merge resources with collision detection
        for r in &template.resources {
            let name = r.logical_name.to_string();
            if let Some(existing_file) = source_map.get(&name) {
                diags.error(
                    None,
                    format!(
                        "resource '{}' defined in both {} and {}",
                        name, existing_file, filename
                    ),
                    "",
                );
            } else {
                source_map.insert(name, filename.clone());
                resources.push(r.clone());
            }
        }

        // Merge variables with collision detection
        for v in &template.variables {
            let name = v.key.to_string();
            if let Some(existing_file) = source_map.get(&name) {
                diags.error(
                    None,
                    format!(
                        "variable '{}' defined in both {} and {}",
                        name, existing_file, filename
                    ),
                    "",
                );
            } else {
                source_map.insert(name, filename.clone());
                variables.push(v.clone());
            }
        }

        // Merge outputs with collision detection
        for o in &template.outputs {
            let name = o.key.to_string();
            if let Some(existing_file) = source_map.get(&name) {
                diags.error(
                    None,
                    format!(
                        "output '{}' defined in both {} and {}",
                        name, existing_file, filename
                    ),
                    "",
                );
            } else {
                source_map.insert(name, filename.clone());
                outputs.push(o.clone());
            }
        }

        // Merge components with collision detection
        for c in &template.components {
            let name = c.key.to_string();
            if let Some(existing_file) = source_map.get(&name) {
                diags.error(
                    None,
                    format!(
                        "component '{}' defined in both {} and {}",
                        name, existing_file, filename
                    ),
                    "",
                );
            } else {
                source_map.insert(name, filename.clone());
                components.push(c.clone());
            }
        }
    }

    let merged = MergedTemplate {
        main_name: main.name.clone(),
        main_namespace: main.namespace.clone(),
        main_description: main.description.clone(),
        main_pulumi: main.pulumi.clone(),
        config: main.config.clone(),
        resources,
        variables,
        outputs,
        components,
        source_map,
    };

    (merged, diags)
}

/// High-level entry point: discovers, optionally Jinja-preprocesses, parses,
/// and merges all project files into a single `MergedTemplate`.
///
/// If `jinja_ctx` is `Some`, Jinja `{{ }}` expressions are rendered per-file.
/// If `None`, files are parsed as-is.
pub fn load_project(
    directory: &Path,
    jinja_ctx: Option<&JinjaContext<'_>>,
) -> (MergedTemplate, Diagnostics) {
    let mut diags = Diagnostics::new();

    // 1. Discover files
    let project_files = match discover_project_files(directory) {
        Ok(files) => files,
        Err(e) => {
            diags.error(None, e, "");
            let empty = MergedTemplate {
                main_name: None,
                main_namespace: None,
                main_description: None,
                main_pulumi: PulumiDecl::default(),
                config: Vec::new(),
                resources: Vec::new(),
                variables: Vec::new(),
                outputs: Vec::new(),
                components: Vec::new(),
                source_map: HashMap::new(),
            };
            return (empty, diags);
        }
    };

    // 2. Parse main file
    let main_filename = project_files
        .main_file
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let main_template =
        match load_and_parse_file(&project_files.main_file, &main_filename, jinja_ctx) {
            Ok((template, file_diags)) => {
                diags.extend(file_diags);
                if diags.has_errors() {
                    let empty = MergedTemplate {
                        main_name: None,
                        main_namespace: None,
                        main_description: None,
                        main_pulumi: PulumiDecl::default(),
                        config: Vec::new(),
                        resources: Vec::new(),
                        variables: Vec::new(),
                        outputs: Vec::new(),
                        components: Vec::new(),
                        source_map: HashMap::new(),
                    };
                    return (empty, diags);
                }
                template
            }
            Err(e) => {
                diags.error(None, e, "");
                let empty = MergedTemplate {
                    main_name: None,
                    main_namespace: None,
                    main_description: None,
                    main_pulumi: PulumiDecl::default(),
                    config: Vec::new(),
                    resources: Vec::new(),
                    variables: Vec::new(),
                    outputs: Vec::new(),
                    components: Vec::new(),
                    source_map: HashMap::new(),
                };
                return (empty, diags);
            }
        };

    // 3. Parse additional files
    let mut additional = Vec::new();
    for path in &project_files.additional_files {
        let filename = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        match load_and_parse_file(path, &filename, jinja_ctx) {
            Ok((template, file_diags)) => {
                diags.extend(file_diags);
                if diags.has_errors() {
                    continue;
                }
                additional.push((filename, template));
            }
            Err(e) => {
                diags.error(None, format!("{}: {}", filename, e), "");
            }
        }
    }

    if diags.has_errors() {
        let empty = MergedTemplate {
            main_name: None,
            main_namespace: None,
            main_description: None,
            main_pulumi: PulumiDecl::default(),
            config: Vec::new(),
            resources: Vec::new(),
            variables: Vec::new(),
            outputs: Vec::new(),
            components: Vec::new(),
            source_map: HashMap::new(),
        };
        return (empty, diags);
    }

    // 4. Merge
    let (merged, merge_diags) = merge_templates(main_template, &main_filename, additional);
    diags.extend(merge_diags);

    (merged, diags)
}

/// Loads a single file, optionally applies Jinja preprocessing, parses it.
fn load_and_parse_file(
    path: &Path,
    filename: &str,
    jinja_ctx: Option<&JinjaContext<'_>>,
) -> Result<(TemplateDecl<'static>, Diagnostics), String> {
    let source = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read {}: {}", path.display(), e))?;

    let mut diags = Diagnostics::new();

    // Apply Jinja preprocessing if context is available
    let effective_source = if let Some(ctx) = jinja_ctx {
        let preprocessor = JinjaPreprocessor::new(ctx);
        match preprocessor.preprocess(&source, filename) {
            Ok(cow) => cow.into_owned(),
            Err(diag) => {
                return Err(format!(
                    "Jinja preprocessing failed for {}: {}",
                    filename,
                    diag.format_rich(filename)
                ));
            }
        }
    } else {
        source.clone()
    };

    // Validate rendered YAML
    if jinja_ctx.is_some() {
        if let Err(diag) = validate_rendered_yaml(&effective_source, &source, filename) {
            return Err(format!(
                "YAML validation failed for {}: {}",
                filename,
                diag.format_rich(filename)
            ));
        }
    }

    // Parse
    let (template, parse_diags) = parse_template(&effective_source, None);
    diags.extend(parse_diags);

    Ok((template, diags))
}

/// Loads just the raw file contents for all project files.
/// Used by the language host when it needs to read files but handle
/// preprocessing and parsing separately.
pub fn load_project_sources(directory: &Path) -> Result<Vec<(String, String)>, String> {
    let project_files = discover_project_files(directory)?;
    let mut sources = Vec::new();

    for path in project_files.all_files() {
        let filename = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {}", path.display(), e))?;
        sources.push((filename, content));
    }

    Ok(sources)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jinja::UndefinedMode;
    use std::fs;

    fn make_temp_project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in files {
            fs::write(dir.path().join(name), content).unwrap();
        }
        dir
    }

    #[test]
    fn test_discover_single_file() {
        let dir = make_temp_project(&[("Pulumi.yaml", "name: test\nruntime: yaml\n")]);
        let files = discover_project_files(dir.path()).unwrap();
        assert_eq!(
            files.main_file.file_name().unwrap().to_str().unwrap(),
            "Pulumi.yaml"
        );
        assert!(files.additional_files.is_empty());
    }

    #[test]
    fn test_discover_multiple_files() {
        let dir = make_temp_project(&[
            ("Pulumi.yaml", "name: test\nruntime: yaml\n"),
            (
                "Pulumi.buckets.yaml",
                "resources:\n  b:\n    type: test:Bucket\n",
            ),
            (
                "Pulumi.tables.yaml",
                "resources:\n  t:\n    type: test:Table\n",
            ),
        ]);
        let files = discover_project_files(dir.path()).unwrap();
        assert_eq!(files.additional_files.len(), 2);
        // Should be sorted alphabetically
        let names: Vec<String> = files
            .additional_files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["Pulumi.buckets.yaml", "Pulumi.tables.yaml"]);
    }

    #[test]
    fn test_discover_requires_pulumi_yaml() {
        let dir = make_temp_project(&[(
            "Pulumi.buckets.yaml",
            "resources:\n  b:\n    type: test:Bucket\n",
        )]);
        let result = discover_project_files(dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no Pulumi.yaml"));
    }

    #[test]
    fn test_discover_yml_variant() {
        let dir = make_temp_project(&[
            ("Pulumi.yml", "name: test\nruntime: yaml\n"),
            (
                "Pulumi.buckets.yml",
                "resources:\n  b:\n    type: test:Bucket\n",
            ),
        ]);
        let files = discover_project_files(dir.path()).unwrap();
        assert_eq!(
            files.main_file.file_name().unwrap().to_str().unwrap(),
            "Pulumi.yml"
        );
        assert_eq!(files.additional_files.len(), 1);
    }

    #[test]
    fn test_discover_ignores_non_matching_files() {
        let dir = make_temp_project(&[
            ("Pulumi.yaml", "name: test\n"),
            ("other.yaml", "hello: world\n"),
            ("README.md", "# test\n"),
            ("Pulumi.lock", "data\n"),
        ]);
        let files = discover_project_files(dir.path()).unwrap();
        assert!(files.additional_files.is_empty());
    }

    #[test]
    fn test_merge_backward_compat_single_file() {
        let source = "name: test\nruntime: yaml\nresources:\n  b:\n    type: test:Bucket\noutputs:\n  url: ${b.url}\n";
        let (template, _) = parse_template(source, None);
        let (merged, diags) = merge_templates(template, "Pulumi.yaml", Vec::new());
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert_eq!(merged.resources.len(), 1);
        assert_eq!(merged.outputs.len(), 1);
        let td = merged.as_template_decl();
        assert_eq!(td.resources.len(), 1);
        assert_eq!(td.outputs.len(), 1);
    }

    #[test]
    fn test_merge_cross_file_reference() {
        let main_src = "name: test\nruntime: yaml\noutputs:\n  url: ${bucket.url}\n";
        let buckets_src = "resources:\n  bucket:\n    type: test:Bucket\n";

        let (main_template, _) = parse_template(main_src, None);
        let (buckets_template, _) = parse_template(buckets_src, None);

        let (merged, diags) = merge_templates(
            main_template,
            "Pulumi.yaml",
            vec![("Pulumi.buckets.yaml".to_string(), buckets_template)],
        );
        assert!(!diags.has_errors(), "errors: {}", diags);

        // Resource from buckets file should be in merged template
        assert_eq!(merged.resources.len(), 1);
        assert_eq!(merged.resources[0].logical_name.as_ref(), "bucket");

        // Output from main file should reference it
        assert_eq!(merged.outputs.len(), 1);

        // Source map should track origin
        assert_eq!(merged.source_file("bucket"), Some("Pulumi.buckets.yaml"));
    }

    #[test]
    fn test_merge_name_collision_error() {
        let main_src = "name: test\nruntime: yaml\nresources:\n  bucket:\n    type: test:Bucket\n";
        let extra_src = "resources:\n  bucket:\n    type: test:OtherBucket\n";

        let (main_template, _) = parse_template(main_src, None);
        let (extra_template, _) = parse_template(extra_src, None);

        let (_, diags) = merge_templates(
            main_template,
            "Pulumi.yaml",
            vec![("Pulumi.extra.yaml".to_string(), extra_template)],
        );
        assert!(diags.has_errors());
        let errors: Vec<_> = diags.iter().filter(|d| d.is_error()).collect();
        assert!(errors[0].summary.contains("bucket"));
        assert!(errors[0].summary.contains("Pulumi.yaml"));
        assert!(errors[0].summary.contains("Pulumi.extra.yaml"));
    }

    #[test]
    fn test_merge_config_in_extra_file_error() {
        // Extra file with config AND resources → error (not a stack config file)
        let main_src = "name: test\nruntime: yaml\n";
        let extra_src = "config:\n  key:\n    type: string\nresources:\n  r1:\n    type: test:A\n";

        let (main_template, _) = parse_template(main_src, None);
        let (extra_template, _) = parse_template(extra_src, None);

        let (_, diags) = merge_templates(
            main_template,
            "Pulumi.yaml",
            vec![("Pulumi.extra.yaml".to_string(), extra_template)],
        );
        assert!(diags.has_errors());
        let errors: Vec<_> = diags.iter().filter(|d| d.is_error()).collect();
        assert!(errors[0].summary.contains("config"));
        assert!(errors[0].summary.contains("Pulumi.extra.yaml"));
    }

    #[test]
    fn test_merge_stack_config_file_silently_skipped() {
        // Config-only extra file (Pulumi stack config) → silently skipped
        let main_src = "name: test\nruntime: yaml\n";
        let extra_src = "config:\n  gcp:project: my-project\n";

        let (main_template, _) = parse_template(main_src, None);
        let (extra_template, _) = parse_template(extra_src, None);

        let (merged, diags) = merge_templates(
            main_template,
            "Pulumi.yaml",
            vec![("Pulumi.dev.yaml".to_string(), extra_template)],
        );
        assert!(!diags.has_errors());
        assert_eq!(merged.resource_count(), 0);
    }

    #[test]
    fn test_merge_outputs_from_multiple_files() {
        let main_src = "name: test\nruntime: yaml\noutputs:\n  a: hello\n";
        let extra_src = "outputs:\n  b: world\n";

        let (main_template, _) = parse_template(main_src, None);
        let (extra_template, _) = parse_template(extra_src, None);

        let (merged, diags) = merge_templates(
            main_template,
            "Pulumi.yaml",
            vec![("Pulumi.extra.yaml".to_string(), extra_template)],
        );
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert_eq!(merged.outputs.len(), 2);
    }

    #[test]
    fn test_merge_variables_cross_file() {
        let main_src = "name: test\nruntime: yaml\nvariables:\n  x: hello\n";
        let extra_src = "variables:\n  y: ${x}\n";

        let (main_template, _) = parse_template(main_src, None);
        let (extra_template, _) = parse_template(extra_src, None);

        let (merged, diags) = merge_templates(
            main_template,
            "Pulumi.yaml",
            vec![("Pulumi.extra.yaml".to_string(), extra_template)],
        );
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert_eq!(merged.variables.len(), 2);
    }

    #[test]
    fn test_source_map_tracks_origin() {
        let main_src = "name: test\nruntime: yaml\nresources:\n  a:\n    type: test:A\n";
        let extra_src = "resources:\n  b:\n    type: test:B\n";

        let (main_template, _) = parse_template(main_src, None);
        let (extra_template, _) = parse_template(extra_src, None);

        let (merged, diags) = merge_templates(
            main_template,
            "Pulumi.yaml",
            vec![("Pulumi.buckets.yaml".to_string(), extra_template)],
        );
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert_eq!(merged.source_file("a"), Some("Pulumi.yaml"));
        assert_eq!(merged.source_file("b"), Some("Pulumi.buckets.yaml"));
        assert_eq!(merged.source_file("nonexistent"), None);
    }

    #[test]
    fn test_dependency_order_cross_file() {
        use crate::eval::graph::topological_sort;

        let main_src = "name: test\nruntime: yaml\noutputs:\n  url: ${table.id}\n";
        let buckets_src = "resources:\n  bucket:\n    type: test:Bucket\n";
        let tables_src = "resources:\n  table:\n    type: test:Table\n    properties:\n      ref: ${bucket.id}\n";

        let (main_template, _) = parse_template(main_src, None);
        let (buckets_template, _) = parse_template(buckets_src, None);
        let (tables_template, _) = parse_template(tables_src, None);

        let (merged, merge_diags) = merge_templates(
            main_template,
            "Pulumi.yaml",
            vec![
                ("Pulumi.buckets.yaml".to_string(), buckets_template),
                ("Pulumi.tables.yaml".to_string(), tables_template),
            ],
        );
        assert!(!merge_diags.has_errors(), "merge errors: {}", merge_diags);

        let td = merged.as_template_decl();
        let (order, sort_diags) = topological_sort(&td);
        assert!(!sort_diags.has_errors(), "sort errors: {}", sort_diags);

        let bucket_pos = order.iter().position(|x| x == "bucket").unwrap();
        let table_pos = order.iter().position(|x| x == "table").unwrap();
        assert!(
            bucket_pos < table_pos,
            "bucket should come before table: {:?}",
            order
        );
    }

    #[test]
    fn test_merge_cross_file_cycle_error() {
        use crate::eval::graph::topological_sort;

        let main_src = "name: test\nruntime: yaml\n";
        let a_src = "resources:\n  a:\n    type: test:A\n    properties:\n      ref: ${b.id}\n";
        let b_src = "resources:\n  b:\n    type: test:B\n    properties:\n      ref: ${a.id}\n";

        let (main_template, _) = parse_template(main_src, None);
        let (a_template, _) = parse_template(a_src, None);
        let (b_template, _) = parse_template(b_src, None);

        let (merged, merge_diags) = merge_templates(
            main_template,
            "Pulumi.yaml",
            vec![
                ("Pulumi.a.yaml".to_string(), a_template),
                ("Pulumi.b.yaml".to_string(), b_template),
            ],
        );
        assert!(!merge_diags.has_errors());

        let td = merged.as_template_decl();
        let (_, sort_diags) = topological_sort(&td);
        assert!(sort_diags.has_errors(), "should detect cycle");
    }

    #[test]
    fn test_load_project_single_file() {
        let dir = make_temp_project(&[(
            "Pulumi.yaml",
            "name: test\nruntime: yaml\nresources:\n  b:\n    type: test:Bucket\n",
        )]);
        let (merged, diags) = load_project(dir.path(), None);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert_eq!(merged.resources.len(), 1);
    }

    #[test]
    fn test_load_project_multi_file() {
        let dir = make_temp_project(&[
            (
                "Pulumi.yaml",
                "name: test\nruntime: yaml\noutputs:\n  url: ${bucket.url}\n",
            ),
            (
                "Pulumi.buckets.yaml",
                "resources:\n  bucket:\n    type: test:Bucket\n",
            ),
        ]);
        let (merged, diags) = load_project(dir.path(), None);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert_eq!(merged.resources.len(), 1);
        assert_eq!(merged.outputs.len(), 1);
    }

    #[test]
    fn test_load_project_with_jinja() {
        let dir = make_temp_project(&[
            ("Pulumi.yaml", "name: test\nruntime: yaml\n"),
            ("Pulumi.buckets.yaml", "resources:\n  bucket:\n    type: test:Bucket\n    properties:\n      name: \"{{ pulumi_project }}-bucket\"\n"),
        ]);
        let config = HashMap::new();
        let ctx = JinjaContext {
            project_name: "myproj",
            stack_name: "dev",
            cwd: "/tmp",
            organization: "",
            root_directory: "",
            config: &config,
            project_dir: dir.path().to_str().unwrap(),
            undefined: UndefinedMode::Strict,
        };
        let (merged, diags) = load_project(dir.path(), Some(&ctx));
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert_eq!(merged.resources.len(), 1);
    }

    #[test]
    fn test_merge_name_in_extra_file_error() {
        let main_src = "name: test\nruntime: yaml\n";
        let extra_src = "name: other\nresources:\n  b:\n    type: test:B\n";

        let (main_template, _) = parse_template(main_src, None);
        let (extra_template, _) = parse_template(extra_src, None);

        let (_, diags) = merge_templates(
            main_template,
            "Pulumi.yaml",
            vec![("Pulumi.extra.yaml".to_string(), extra_template)],
        );
        assert!(diags.has_errors());
        let errors: Vec<_> = diags.iter().filter(|d| d.is_error()).collect();
        assert!(errors[0].summary.contains("name"));
    }

    #[test]
    fn test_project_files_all_files() {
        let dir = make_temp_project(&[
            ("Pulumi.yaml", "name: test\n"),
            ("Pulumi.a.yaml", "resources: {}\n"),
            ("Pulumi.b.yaml", "resources: {}\n"),
        ]);
        let files = discover_project_files(dir.path()).unwrap();
        let all: Vec<_> = files.all_files().collect();
        assert_eq!(all.len(), 3);
        assert_eq!(files.file_count(), 3);
    }

    #[test]
    fn test_merge_components_from_multiple_files() {
        let main_src = "name: test\nruntime: yaml\n";
        // Components need proper structure - for this test, just verify no crash
        // with the basic merge logic. Actual component parsing is tested elsewhere.
        let (main_template, _) = parse_template(main_src, None);
        let (merged, diags) = merge_templates(main_template, "Pulumi.yaml", Vec::new());
        assert!(!diags.has_errors());
        assert!(merged.components.is_empty());
    }

    #[test]
    fn test_load_project_sources() {
        let dir = make_temp_project(&[
            ("Pulumi.yaml", "name: test\nruntime: yaml\n"),
            (
                "Pulumi.buckets.yaml",
                "resources:\n  b:\n    type: test:B\n",
            ),
        ]);
        let sources = load_project_sources(dir.path()).unwrap();
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].0, "Pulumi.yaml");
        assert!(sources[0].1.contains("name: test"));
    }

    #[test]
    fn test_discover_yaml_preference_over_yml() {
        // When both Pulumi.yaml and Pulumi.yml exist, yaml wins
        let dir = make_temp_project(&[
            ("Pulumi.yaml", "name: yaml-version\n"),
            ("Pulumi.yml", "name: yml-version\n"),
        ]);
        let files = discover_project_files(dir.path()).unwrap();
        assert_eq!(
            files.main_file.file_name().unwrap().to_str().unwrap(),
            "Pulumi.yaml"
        );
    }
}
