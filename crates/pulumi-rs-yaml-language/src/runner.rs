//! The Run RPC implementation — the core integration between
//! the evaluator and the Pulumi engine.

use std::collections::HashMap;
use std::path::Path;

use pulumi_rs_yaml_core::ast::parse::parse_template;
use pulumi_rs_yaml_core::eval::callback::ResourceCallback;
use pulumi_rs_yaml_core::eval::evaluator::Evaluator;
use pulumi_rs_yaml_core::eval::value::Value;
use pulumi_rs_yaml_core::jinja::{
    validate_rendered_yaml, JinjaContext, JinjaPreprocessor, TemplatePreprocessor, UndefinedMode,
};
use pulumi_rs_yaml_core::multi_file;
use pulumi_rs_yaml_core::packages;

use crate::clients::GrpcCallback;
use crate::schema_loader::SchemaLoader;

/// Result of running a YAML program.
pub struct RunResult {
    pub error: String,
    pub bail: bool,
}

/// Runs a YAML program by connecting to the monitor/engine and evaluating the template.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    project: &str,
    stack: &str,
    pwd: &str,
    monitor_address: &str,
    engine_address: &str,
    config: &HashMap<String, String>,
    config_secret_keys: &[String],
    dry_run: bool,
    program_directory: &str,
    organization: &str,
    loader_target: Option<&str>,
    parallel: i32,
) -> RunResult {
    // 1. Change working directory to program directory (matching Go behavior)
    if !program_directory.is_empty() {
        if let Err(e) = std::env::set_current_dir(program_directory) {
            eprintln!(
                "warning: could not change to program directory {}: {}",
                program_directory, e
            );
        }
    }

    // Set environment variables for organization context
    if !organization.is_empty() {
        std::env::set_var("PULUMI_ORGANIZATION", organization);
    }

    // 2. Build Jinja context for preprocessing
    let coerced_config_pre = coerce_config_values(config, config_secret_keys);
    let undefined_mode = match std::env::var("PULUMI_YAML_JINJA_UNDEFINED").as_deref() {
        Ok("passthrough") => UndefinedMode::Passthrough,
        _ => UndefinedMode::Strict,
    };
    let jinja_ctx = JinjaContext {
        project_name: project,
        stack_name: stack,
        cwd: pwd,
        organization,
        root_directory: program_directory,
        config: &coerced_config_pre,
        project_dir: program_directory,
        undefined: undefined_mode,
    };

    // 3. Load template(s) — multi-file or single-file with Jinja source override
    let (template, source_map) =
        if let Ok(jinja_source_dir) = std::env::var(crate::exec::JINJA_SOURCE_ENV) {
            // Exec wrapper is active: read original Jinja sources from temp directory
            // and load/preprocess/merge them
            match load_from_jinja_source(&jinja_source_dir, program_directory, &jinja_ctx) {
                Ok((t, sm)) => (t, sm),
                Err(e) => {
                    return RunResult {
                        error: format!("failed to load template: {}", e),
                        bail: true,
                    };
                }
            }
        } else {
            // Normal mode: discover and load all Pulumi.*.yaml files
            let dir = Path::new(program_directory);
            let (merged, load_diags) = multi_file::load_project(dir, Some(&jinja_ctx));
            if load_diags.has_errors() {
                for diag in load_diags.iter() {
                    if diag.is_error() {
                        eprintln!("error: {}", diag.summary);
                    }
                }
                return RunResult {
                    error: "failed to load template".to_string(),
                    bail: true,
                };
            }
            let sm = merged.source_map().clone();
            (merged.as_template_decl(), sm)
        };

    // Leak the template to give it 'static lifetime
    // This is acceptable since the process runs once per evaluation
    let template: &'static _ = Box::leak(Box::new(template));

    // 4. Connect gRPC clients
    let callback = match GrpcCallback::connect(monitor_address, engine_address).await {
        Ok(cb) => cb,
        Err(e) => {
            return RunResult {
                error: format!("failed to connect: {}", e),
                bail: false,
            };
        }
    };

    // 5. Discover referenced packages (shared between schema loading and package registration)
    let lock_packages = packages::search_package_decls(Path::new(program_directory));
    let referenced_pkgs = packages::get_referenced_packages(template, &lock_packages);

    // 6. Load schemas from provider packages (if loader_target is available)
    let schema_store = if let Some(addr) = loader_target {
        match SchemaLoader::connect(addr).await {
            Ok(loader) => Some(loader.fetch_and_build_store(&referenced_pkgs)),
            Err(e) => {
                eprintln!("warning: schema loader: {}", e);
                None
            }
        }
    } else {
        None
    };

    // 7. Register packages and collect package refs
    let mut package_refs = HashMap::new();
    let mut callback = callback;
    for pkg_decl in &referenced_pkgs {
        match callback.register_package(
            &pkg_decl.name,
            &pkg_decl.version,
            &pkg_decl.download_url,
            pkg_decl.parameterization.as_ref().map(|p| {
                use base64::Engine as _;
                let value_bytes = base64::engine::general_purpose::STANDARD
                    .decode(&p.value)
                    .unwrap_or_default();
                (p.name.clone(), p.version.clone(), value_bytes)
            }),
        ) {
            Ok(pkg_ref) => {
                package_refs.insert(pkg_decl.name.clone(), pkg_ref);
            }
            Err(e) => {
                eprintln!("warning: register package {}: {}", pkg_decl.name, e);
            }
        }
    }

    // 7. Apply smart config coercion (matching Go behavior)
    let coerced_config = coerce_config_values(config, config_secret_keys);

    // 8. Create evaluator
    let mut eval = Evaluator::with_callback(
        project.to_string(),
        stack.to_string(),
        pwd.to_string(),
        dry_run,
        callback,
    );
    eval.organization = organization.to_string();
    eval.root_directory = program_directory.to_string();
    eval.schema_store = schema_store;
    eval.package_refs = package_refs;
    eval.parallel = parallel;
    if !source_map.is_empty() {
        eval.source_map = Some(source_map.clone());
    }

    // 8b. Type-check template against schemas (warnings only, non-blocking)
    if let Some(ref store) = eval.schema_store {
        let tc_result =
            pulumi_rs_yaml_core::type_check::type_check(template, store, eval.source_map.as_ref());
        for d in tc_result.diagnostics.iter() {
            eprintln!(
                "type-check: {}: {}",
                if d.is_error() { "error" } else { "warning" },
                d.summary
            );
        }
    }

    // 9. Register root stack resource
    let stack_name_full = format!("{}-{}", project, stack);
    let stack_type = "pulumi:pulumi:Stack";

    match eval.callback_mut().register_resource(
        stack_type,
        &stack_name_full,
        false, // custom=false for stack
        false, // remote
        HashMap::new(),
        Default::default(),
    ) {
        Ok(resp) => {
            eval.stack_urn = Some(resp.urn);
        }
        Err(e) => {
            return RunResult {
                error: format!("failed to register stack: {}", e),
                bail: false,
            };
        }
    }

    // 10. Evaluate the template
    eval.evaluate_template(template, &coerced_config, config_secret_keys);

    // 11. Check for errors
    if eval.diags.has_errors() {
        // Collect error messages to avoid borrow conflict
        let errors: Vec<String> = eval
            .diags
            .iter()
            .filter(|d| d.is_error())
            .map(|d| d.summary.clone())
            .collect();

        // Write errors to stderr and log to engine
        for msg in &errors {
            eprintln!("error: {}", msg);
            eval.callback_mut().log(3, msg);
        }

        // Register empty outputs for the stack
        let stack_urn = eval.stack_urn.clone();
        if let Some(urn) = stack_urn {
            let _ = eval.callback_mut().register_outputs(&urn, HashMap::new());
        }

        // Return with bail=true to signal program abort (matching Go)
        return RunResult {
            error: String::new(),
            bail: true,
        };
    }

    // 12. Log warnings to stderr and engine
    let warnings: Vec<String> = eval
        .diags
        .iter()
        .filter(|d| !d.is_error())
        .map(|d| d.summary.clone())
        .collect();
    for msg in &warnings {
        eprintln!("warning: {}", msg);
        eval.callback_mut().log(2, msg);
    }

    // 13. Register stack outputs
    let stack_urn = eval.stack_urn.clone();
    if let Some(urn) = stack_urn {
        let outputs: HashMap<String, Value<'static>> = eval
            .outputs
            .drain()
            .map(|(k, v)| (k, v.into_owned()))
            .collect();

        if let Err(e) = eval.callback_mut().register_outputs(&urn, outputs) {
            return RunResult {
                error: format!("failed to register stack outputs: {}", e),
                bail: false,
            };
        }
    }

    RunResult {
        error: String::new(),
        bail: false,
    }
}

/// Loads templates from the Jinja source temp directory (exec wrapper mode).
///
/// When the exec wrapper is active, original Jinja sources are stored in a temp
/// directory. This function reads them, preprocesses with Jinja, parses, and
/// merges into a single template.
fn load_from_jinja_source(
    jinja_source: &str,
    program_directory: &str,
    jinja_ctx: &JinjaContext<'_>,
) -> Result<
    (
        pulumi_rs_yaml_core::ast::template::TemplateDecl<'static>,
        std::collections::HashMap<String, String>,
    ),
    String,
> {
    let jinja_source_path = Path::new(jinja_source);

    if jinja_source_path.is_dir() {
        // Multi-file mode: temp directory contains *.original files
        let dir = Path::new(program_directory);
        let project_files = multi_file::discover_project_files(dir)?;
        let preprocessor = JinjaPreprocessor::new(jinja_ctx);

        // Read originals from temp dir, preprocess, parse
        let main_filename = project_files
            .main_file
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let main_original_path = jinja_source_path.join(format!("{}.original", main_filename));
        let main_source = if main_original_path.exists() {
            std::fs::read_to_string(&main_original_path)
                .map_err(|e| format!("failed to read {}: {}", main_original_path.display(), e))?
        } else {
            // Main file didn't have Jinja blocks, read from program directory
            std::fs::read_to_string(&project_files.main_file).map_err(|e| {
                format!(
                    "failed to read {}: {}",
                    project_files.main_file.display(),
                    e
                )
            })?
        };

        let main_rendered = preprocessor
            .preprocess(&main_source, &main_filename)
            .map_err(|e| format!("Jinja error in {}: {}", main_filename, e))?;

        if let Err(diag) =
            validate_rendered_yaml(main_rendered.as_ref(), &main_source, &main_filename)
        {
            return Err(diag.format_rich(&main_filename));
        }

        let (main_template, main_diags) = parse_template(main_rendered.as_ref(), None);
        if main_diags.has_errors() {
            return Err("failed to parse main template".to_string());
        }

        let mut additional = Vec::new();
        for path in &project_files.additional_files {
            let filename = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let original_path = jinja_source_path.join(format!("{}.original", filename));
            let source = if original_path.exists() {
                std::fs::read_to_string(&original_path)
                    .map_err(|e| format!("failed to read {}: {}", original_path.display(), e))?
            } else {
                // File didn't have Jinja blocks, read from disk directly
                std::fs::read_to_string(path)
                    .map_err(|e| format!("failed to read {}: {}", path.display(), e))?
            };

            let rendered = preprocessor
                .preprocess(&source, &filename)
                .map_err(|e| format!("Jinja error in {}: {}", filename, e))?;

            if let Err(diag) = validate_rendered_yaml(rendered.as_ref(), &source, &filename) {
                return Err(diag.format_rich(&filename));
            }

            let (template, parse_diags) = parse_template(rendered.as_ref(), None);
            if parse_diags.has_errors() {
                return Err(format!("failed to parse {}", filename));
            }
            additional.push((filename, template));
        }

        let (merged, merge_diags) =
            multi_file::merge_templates(main_template, &main_filename, additional);
        if merge_diags.has_errors() {
            let errors: Vec<String> = merge_diags
                .iter()
                .filter(|d| d.is_error())
                .map(|d| d.summary.clone())
                .collect();
            return Err(errors.join("; "));
        }

        let sm = merged.source_map().clone();
        Ok((merged.as_template_decl(), sm))
    } else {
        // Single-file mode (backward compat): temp file is the original source
        let source = std::fs::read_to_string(jinja_source)
            .map_err(|e| format!("failed to read Jinja source from {}: {}", jinja_source, e))?;

        let preprocessor = JinjaPreprocessor::new(jinja_ctx);
        let rendered = preprocessor
            .preprocess(&source, "Pulumi.yaml")
            .map_err(|e| format!("Jinja error: {}", e))?;

        if let Err(diag) = validate_rendered_yaml(rendered.as_ref(), &source, "Pulumi.yaml") {
            return Err(diag.format_rich("Pulumi.yaml"));
        }

        let (template, parse_diags) = parse_template(rendered.as_ref(), None);
        if parse_diags.has_errors() {
            return Err("failed to parse template".to_string());
        }

        Ok((template, std::collections::HashMap::new()))
    }
}

/// Applies smart config type coercion matching Go's behavior.
///
/// The Go implementation coerces raw string config values into typed values:
/// 1. Strings starting with "0" (but not "0." or just "0") stay as strings
/// 2. Try parse as integer
/// 3. Try parse as bool
/// 4. Try parse as float
/// 5. Try parse as JSON (for arrays/objects)
/// 6. Fallback to string
///
/// This function returns the coerced config map suitable for the evaluator.
/// The evaluator's own config resolution will handle the final type checking.
fn coerce_config_values(
    config: &HashMap<String, String>,
    _secret_keys: &[String],
) -> HashMap<String, String> {
    // The evaluator already handles string→typed coercion via config::resolve_config_entry.
    // We pass through the raw config as-is since the evaluator does its own parsing.
    // The Go implementation's coercion is done because Go's SDK expects typed PropertyValues,
    // but our Rust evaluator works directly with raw strings and applies types from schema declarations.
    config.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_coerce_config_values_passthrough() {
        let mut config = HashMap::new();
        config.insert("key1".to_string(), "hello".to_string());
        config.insert("key2".to_string(), "42".to_string());
        config.insert("key3".to_string(), "true".to_string());

        let result = coerce_config_values(&config, &[]);
        assert_eq!(result.get("key1").unwrap(), "hello");
        assert_eq!(result.get("key2").unwrap(), "42");
        assert_eq!(result.get("key3").unwrap(), "true");
    }
}
