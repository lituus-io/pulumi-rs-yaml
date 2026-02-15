mod convert;

use std::collections::HashMap;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyDict;

use pulumi_rs_yaml_core::diag::Diagnostics;
use pulumi_rs_yaml_core::eval::builtins;
use pulumi_rs_yaml_core::eval::value::Value;

use convert::{
    expr_to_py, py_dict_to_string_map, py_to_value, resource_options_to_py,
    resource_properties_to_py, value_to_py,
};

/// Parse a YAML template string and return its structure as a Python dict.
#[pyfunction]
fn parse_template(py: Python<'_>, source: &str) -> PyResult<PyObject> {
    let (template, diags) = pulumi_rs_yaml_core::ast::parse::parse_template(source, None);

    let dict = PyDict::new(py);
    dict.set_item("name", template.name.as_deref())?;
    dict.set_item("description", template.description.as_deref())?;
    dict.set_item("resource_count", template.resources.len())?;
    dict.set_item("variable_count", template.variables.len())?;
    dict.set_item("output_count", template.outputs.len())?;
    dict.set_item("config_count", template.config.len())?;
    dict.set_item("component_count", template.components.len())?;

    let resource_names: Vec<&str> = template
        .resources
        .iter()
        .map(|r| r.logical_name.as_ref())
        .collect();
    dict.set_item("resource_names", resource_names)?;

    let variable_names: Vec<&str> = template.variables.iter().map(|v| v.key.as_ref()).collect();
    dict.set_item("variable_names", variable_names)?;

    let output_names: Vec<&str> = template.outputs.iter().map(|o| o.key.as_ref()).collect();
    dict.set_item("output_names", output_names)?;

    let diag_list = diags_to_py(py, &diags)?;
    dict.set_item("diagnostics", diag_list)?;
    dict.set_item("has_errors", diags.has_errors())?;

    Ok(dict.into_any().unbind())
}

/// Load a multi-file project from a directory.
#[pyfunction]
fn load_project(py: Python<'_>, dir: &str) -> PyResult<PyObject> {
    let path = std::path::Path::new(dir);

    let discovery = pulumi_rs_yaml_core::multi_file::discover_project_files(path)
        .map_err(|e| PyValueError::new_err(format!("Failed to discover project files: {}", e)))?;

    let (merged, diags) = pulumi_rs_yaml_core::multi_file::load_project(path, None);

    let dict = PyDict::new(py);
    dict.set_item("resource_count", merged.resource_count())?;
    dict.set_item("variable_count", merged.variable_count())?;
    dict.set_item("output_count", merged.output_count())?;
    dict.set_item("component_count", merged.component_count())?;

    let resource_names: Vec<&str> = merged.resource_names();
    dict.set_item("resource_names", resource_names.clone())?;

    let source_map = PyDict::new(py);
    for name in &resource_names {
        if let Some(file) = merged.source_file(name) {
            source_map.set_item(*name, file)?;
        }
    }
    dict.set_item("source_map", source_map)?;

    let diag_list = diags_to_py(py, &diags)?;
    dict.set_item("diagnostics", diag_list)?;
    dict.set_item("has_errors", diags.has_errors())?;
    dict.set_item("file_count", discovery.file_count())?;

    Ok(dict.into_any().unbind())
}

/// Discover all Pulumi.*.yaml files in a project directory.
#[pyfunction]
fn discover_project_files(py: Python<'_>, dir: &str) -> PyResult<PyObject> {
    let path = std::path::Path::new(dir);
    let discovery = pulumi_rs_yaml_core::multi_file::discover_project_files(path)
        .map_err(|e| PyValueError::new_err(format!("Failed to discover project files: {}", e)))?;

    let dict = PyDict::new(py);
    dict.set_item("main_file", discovery.main_file.to_string_lossy().as_ref())?;

    let additional: Vec<String> = discovery
        .additional_files
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    dict.set_item("additional_files", additional)?;
    dict.set_item("file_count", discovery.file_count())?;

    Ok(dict.into_any().unbind())
}

/// Check if a YAML source contains Jinja block syntax ({% %}).
#[pyfunction]
fn has_jinja_blocks(source: &str) -> bool {
    pulumi_rs_yaml_core::jinja::has_jinja_block_syntax(source)
}

/// Strip Jinja block lines from YAML source.
#[pyfunction]
fn strip_jinja_blocks(source: &str) -> String {
    pulumi_rs_yaml_core::jinja::strip_jinja_blocks(source)
}

/// Validate Jinja syntax in a source string.
#[pyfunction]
fn validate_jinja(source: &str, filename: &str) -> PyResult<()> {
    match pulumi_rs_yaml_core::jinja::validate_jinja_syntax(source, filename) {
        Ok(()) => Ok(()),
        Err(e) => Err(PyValueError::new_err(format!("Jinja syntax error: {}", e))),
    }
}

/// Preprocess a YAML source with Jinja rendering.
#[pyfunction]
fn preprocess_jinja(source: &str, filename: &str, context: &Bound<'_, PyDict>) -> PyResult<String> {
    let ctx_map = py_dict_to_string_map(context)?;

    // Build owned strings, then borrow for JinjaContext
    let project_name = ctx_map.get("project_name").cloned().unwrap_or_default();
    let stack_name = ctx_map.get("stack_name").cloned().unwrap_or_default();
    let cwd = ctx_map.get("cwd").cloned().unwrap_or_default();
    let organization = ctx_map.get("organization").cloned().unwrap_or_default();
    let root_directory = ctx_map.get("root_directory").cloned().unwrap_or_default();
    let project_dir = ctx_map.get("project_dir").cloned().unwrap_or_default();
    let config: HashMap<String, String> = ctx_map
        .iter()
        .filter(|(k, _)| k.starts_with("config."))
        .map(|(k, v)| (k.trim_start_matches("config.").to_string(), v.clone()))
        .collect();

    let jinja_ctx = pulumi_rs_yaml_core::jinja::JinjaContext {
        project_name: &project_name,
        stack_name: &stack_name,
        cwd: &cwd,
        organization: &organization,
        root_directory: &root_directory,
        config: &config,
        project_dir: &project_dir,
        undefined: pulumi_rs_yaml_core::jinja::UndefinedMode::Strict,
    };

    let preprocessor = pulumi_rs_yaml_core::jinja::JinjaPreprocessor::new(&jinja_ctx);
    use pulumi_rs_yaml_core::jinja::TemplatePreprocessor;
    match preprocessor.preprocess(source, filename) {
        Ok(result) => Ok(result.into_owned()),
        Err(e) => Err(PyValueError::new_err(format!(
            "Jinja preprocessing error: {}",
            e.format_rich(filename)
        ))),
    }
}

/// Evaluate a single builtin function by name.
#[pyfunction]
fn evaluate_builtin(py: Python<'_>, name: &str, args: PyObject) -> PyResult<PyObject> {
    let mut diags = Diagnostics::new();
    let arg_val = py_to_value(args.bind(py))?;

    let result = match name {
        // Math
        "abs" => builtins::eval_abs(&arg_val, &mut diags),
        "floor" => builtins::eval_floor(&arg_val, &mut diags),
        "ceil" => builtins::eval_ceil(&arg_val, &mut diags),
        "max" => builtins::eval_max(&arg_val, &mut diags),
        "min" => builtins::eval_min(&arg_val, &mut diags),
        // String
        "stringLen" => builtins::eval_string_len(&arg_val, &mut diags),
        "substring" => match &arg_val {
            Value::List(items) if items.len() == 3 => {
                builtins::eval_substring(&items[0], &items[1], &items[2], &mut diags)
            }
            _ => {
                return Err(PyValueError::new_err(
                    "substring expects a list of [source, start, length]",
                ));
            }
        },
        // Existing string builtins
        "join" => match &arg_val {
            Value::List(items) if items.len() == 2 => {
                builtins::eval_join(&items[0], &items[1], &mut diags)
            }
            _ => {
                return Err(PyValueError::new_err(
                    "join expects a list of [delimiter, list]",
                ));
            }
        },
        "split" => match &arg_val {
            Value::List(items) if items.len() == 2 => {
                builtins::eval_split(&items[0], &items[1], &mut diags)
            }
            _ => {
                return Err(PyValueError::new_err(
                    "split expects a list of [delimiter, source]",
                ));
            }
        },
        "select" => match &arg_val {
            Value::List(items) if items.len() == 2 => {
                builtins::eval_select(&items[0], &items[1], &mut diags)
            }
            _ => {
                return Err(PyValueError::new_err(
                    "select expects a list of [index, list]",
                ));
            }
        },
        "toJSON" => builtins::eval_to_json(&arg_val, &mut diags),
        "toBase64" => builtins::eval_to_base64(&arg_val, &mut diags),
        "fromBase64" => builtins::eval_from_base64(&arg_val, &mut diags),
        "secret" => Some(builtins::eval_secret(arg_val.clone())),
        // Time
        "timeUtc" => builtins::eval_time_utc(&arg_val, &mut diags),
        "timeUnix" => builtins::eval_time_unix(&arg_val, &mut diags),
        // UUID/Random
        "uuid" => builtins::eval_uuid(&arg_val, &mut diags),
        "randomString" => builtins::eval_random_string(&arg_val, &mut diags),
        // Date
        "dateFormat" => builtins::eval_date_format(&arg_val, &mut diags),
        _ => {
            return Err(PyValueError::new_err(format!(
                "unknown builtin function: {}",
                name
            )));
        }
    };

    if diags.has_errors() {
        return Err(PyValueError::new_err(format!("builtin error: {}", diags)));
    }

    match result {
        Some(val) => value_to_py(py, &val),
        None => Err(PyValueError::new_err("builtin returned no result")),
    }
}

/// Create an execution plan from a YAML project directory.
///
/// Pipeline: discover files → Jinja preprocess → parse → merge → validate DAG →
/// canonicalize types → serialize expression trees as Python dicts.
///
/// Returns a dict: { project_name, nodes: [...], outputs: [...], source_map, diagnostics }
#[pyfunction]
#[pyo3(signature = (project_dir, jinja_context=None))]
fn create_execution_plan(
    py: Python<'_>,
    project_dir: &str,
    jinja_context: Option<&Bound<'_, PyDict>>,
) -> PyResult<PyObject> {
    let path = std::path::Path::new(project_dir);

    // Build JinjaContext from optional dict
    let ctx_map: HashMap<String, String> = match jinja_context {
        Some(d) => py_dict_to_string_map(d)?,
        None => HashMap::new(),
    };
    let project_dir_str = project_dir.to_string();
    let stack_name = ctx_map.get("stack_name").cloned().unwrap_or_default();
    let cwd = ctx_map
        .get("cwd")
        .cloned()
        .unwrap_or(project_dir_str.clone());
    let organization = ctx_map.get("organization").cloned().unwrap_or_default();
    let root_directory = ctx_map
        .get("root_directory")
        .cloned()
        .unwrap_or(project_dir_str.clone());
    let config_map: HashMap<String, String> = ctx_map
        .iter()
        .filter(|(k, _)| k.starts_with("config."))
        .map(|(k, v)| (k.trim_start_matches("config.").to_string(), v.clone()))
        .collect();

    // Extract project name from main file (strip {% %} blocks first so it parses as valid YAML).
    let project_name_owned = {
        let files = pulumi_rs_yaml_core::multi_file::discover_project_files(path).map_err(|e| {
            PyValueError::new_err(format!("Failed to discover project files: {}", e))
        })?;
        let raw = std::fs::read_to_string(&files.main_file)
            .map_err(|e| PyValueError::new_err(format!("Failed to read main file: {}", e)))?;
        let stripped = pulumi_rs_yaml_core::jinja::strip_jinja_blocks(&raw);
        let (tmpl, _) = pulumi_rs_yaml_core::ast::parse::parse_template(&stripped, None);
        tmpl.name
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    };

    let jinja_ctx = pulumi_rs_yaml_core::jinja::JinjaContext {
        project_name: &project_name_owned,
        stack_name: &stack_name,
        cwd: &cwd,
        organization: &organization,
        root_directory: &root_directory,
        config: &config_map,
        project_dir,
        undefined: pulumi_rs_yaml_core::jinja::UndefinedMode::Strict,
    };

    // Reload with Jinja preprocessing (handles both {{ }} and {% %} via full rendering)
    let jinja_opt = if jinja_context.is_some() {
        Some(&jinja_ctx)
    } else {
        None
    };
    let (merged, load_diags) = pulumi_rs_yaml_core::multi_file::load_project(path, jinja_opt);
    if load_diags.has_errors() {
        return Err(PyValueError::new_err(format!(
            "Failed to load project: {}",
            load_diags
        )));
    }

    let project_name = merged.name().unwrap_or("unknown").to_string();

    // Validate DAG (topological sort with dep graph for level computation)
    let template = merged.as_template_decl();
    let (sort_result, sort_diags) = pulumi_rs_yaml_core::eval::graph::topological_sort_with_deps(
        &template,
        Some(merged.source_map()),
    );
    if sort_diags.has_errors() {
        return Err(PyValueError::new_err(format!(
            "DAG validation failed: {}",
            sort_diags
        )));
    }
    let order = &sort_result.order;

    // Compute topological levels for parallel evaluation
    let levels = pulumi_rs_yaml_core::eval::graph::topological_levels(order, &sort_result.deps);
    let mut node_level_map: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for (level_idx, level_nodes) in levels.iter().enumerate() {
        for name in level_nodes {
            node_level_map.insert(name.clone(), level_idx);
        }
    }

    // Build lookup maps: name → kind + data
    let mut config_map_by_name: HashMap<
        &str,
        &pulumi_rs_yaml_core::ast::template::ConfigEntry<'_>,
    > = HashMap::new();
    for entry in &template.config {
        config_map_by_name.insert(entry.key.as_ref(), entry);
    }
    let mut var_map: HashMap<&str, &pulumi_rs_yaml_core::ast::template::VariableEntry<'_>> =
        HashMap::new();
    for entry in &template.variables {
        var_map.insert(entry.key.as_ref(), entry);
    }
    let mut res_map: HashMap<&str, &pulumi_rs_yaml_core::ast::template::ResourceEntry<'_>> =
        HashMap::new();
    for entry in &template.resources {
        res_map.insert(entry.logical_name.as_ref(), entry);
    }

    // Walk order and serialize each node
    let mut nodes: Vec<PyObject> = Vec::new();
    for name in order {
        let name_str = name.as_str();

        if name_str == "pulumi" {
            // Skip the pulumi settings node
            continue;
        }

        let level = node_level_map.get(name_str).copied().unwrap_or(0);

        if let Some(cfg) = config_map_by_name.get(name_str) {
            let node = PyDict::new(py);
            node.set_item("kind", "config")?;
            node.set_item("name", cfg.key.as_ref())?;
            node.set_item("type", cfg.param.type_.as_deref())?;
            node.set_item("secret", cfg.param.secret)?;
            node.set_item("level", level)?;
            if let Some(ref def) = cfg.param.default {
                node.set_item("default", expr_to_py(py, def)?)?;
            } else {
                node.set_item("default", py.None())?;
            }
            if let Some(ref val) = cfg.param.value {
                node.set_item("value", expr_to_py(py, val)?)?;
            } else {
                node.set_item("value", py.None())?;
            }
            nodes.push(node.into_any().unbind());
        } else if let Some(var) = var_map.get(name_str) {
            let node = PyDict::new(py);
            node.set_item("kind", "variable")?;
            node.set_item("name", var.key.as_ref())?;
            node.set_item("value", expr_to_py(py, &var.value)?)?;
            node.set_item("level", level)?;
            nodes.push(node.into_any().unbind());
        } else if let Some(res) = res_map.get(name_str) {
            let node = PyDict::new(py);
            node.set_item("kind", "resource")?;
            node.set_item("name", res.logical_name.as_ref())?;
            let canonical =
                pulumi_rs_yaml_core::packages::canonicalize_type_token(res.resource.type_.as_ref());
            node.set_item("type_token", &canonical)?;
            node.set_item("level", level)?;

            // Include explicit resource name if set (for physical name override)
            if let Some(ref physical_name) = res.resource.name {
                node.set_item("resource_name", physical_name.as_ref())?;
            } else {
                node.set_item("resource_name", py.None())?;
            }

            // Include component detection hint (will be refined when schema is available)
            let is_component = false; // Schema not available at plan time; set by Python SDK
            node.set_item("is_component", is_component)?;

            node.set_item(
                "properties",
                resource_properties_to_py(py, &res.resource.properties)?,
            )?;
            node.set_item(
                "options",
                resource_options_to_py(py, &res.resource.options)?,
            )?;
            // Add empty output_properties and property_types (populated when schema available)
            let empty_list: Vec<String> = Vec::new();
            node.set_item("output_properties", empty_list)?;
            node.set_item("property_types", PyDict::new(py))?;

            if let Some(ref get) = res.resource.get {
                let get_dict = PyDict::new(py);
                get_dict.set_item("id", expr_to_py(py, &get.id)?)?;
                let state_entries: Vec<PyObject> = get
                    .state
                    .iter()
                    .map(|e| {
                        let d = PyDict::new(py);
                        d.set_item("k", e.key.as_ref())?;
                        d.set_item("v", expr_to_py(py, &e.value)?)?;
                        Ok(d.into_any().unbind())
                    })
                    .collect::<PyResult<_>>()?;
                get_dict.set_item("state", pyo3::types::PyList::new(py, &state_entries)?)?;
                node.set_item("get", get_dict)?;
            } else {
                node.set_item("get", py.None())?;
            }
            nodes.push(node.into_any().unbind());
        }
        // else: skip unknown nodes
    }

    // Serialize outputs
    let py_outputs: Vec<PyObject> = template
        .outputs
        .iter()
        .map(|o| {
            let d = PyDict::new(py);
            d.set_item("name", o.key.as_ref())?;
            d.set_item("value", expr_to_py(py, &o.value)?)?;
            Ok(d.into_any().unbind())
        })
        .collect::<PyResult<_>>()?;

    // Build source_map dict
    let py_source_map = PyDict::new(py);
    for (name, file) in merged.source_map() {
        py_source_map.set_item(name.as_str(), file.as_str())?;
    }

    // Build diagnostics
    let mut all_diags = Diagnostics::new();
    all_diags.extend(load_diags);
    all_diags.extend(sort_diags);
    let py_diags = diags_to_py(py, &all_diags)?;

    // Build levels list (list of list of node names per level)
    let py_levels: Vec<PyObject> = levels
        .iter()
        .map(|level_names| {
            let py_names: Vec<&str> = level_names.iter().map(|s| s.as_str()).collect();
            Ok(pyo3::types::PyList::new(py, &py_names)?.into_any().unbind())
        })
        .collect::<PyResult<_>>()?;

    // Return the plan dict
    let plan = PyDict::new(py);
    plan.set_item("project_name", &project_name)?;
    plan.set_item("nodes", pyo3::types::PyList::new(py, &nodes)?)?;
    plan.set_item("outputs", pyo3::types::PyList::new(py, &py_outputs)?)?;
    plan.set_item("source_map", py_source_map)?;
    plan.set_item("diagnostics", py_diags)?;
    plan.set_item("levels", pyo3::types::PyList::new(py, &py_levels)?)?;

    Ok(plan.into_any().unbind())
}

/// Convert diagnostics to a Python list of dicts.
fn diags_to_py(py: Python<'_>, diags: &Diagnostics) -> PyResult<PyObject> {
    let list: Vec<PyObject> = diags
        .iter()
        .map(|entry| {
            let dict = PyDict::new(py);
            dict.set_item("message", entry.summary.as_str()).ok();
            dict.set_item("is_error", entry.is_error()).ok();
            dict.into_any().unbind()
        })
        .collect();
    let py_list = pyo3::types::PyList::new(py, &list)?;
    Ok(py_list.into_any().unbind())
}

/// The native Python module.
#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(parse_template, m)?)?;
    m.add_function(wrap_pyfunction!(load_project, m)?)?;
    m.add_function(wrap_pyfunction!(discover_project_files, m)?)?;
    m.add_function(wrap_pyfunction!(has_jinja_blocks, m)?)?;
    m.add_function(wrap_pyfunction!(strip_jinja_blocks, m)?)?;
    m.add_function(wrap_pyfunction!(validate_jinja, m)?)?;
    m.add_function(wrap_pyfunction!(preprocess_jinja, m)?)?;
    m.add_function(wrap_pyfunction!(evaluate_builtin, m)?)?;
    m.add_function(wrap_pyfunction!(create_execution_plan, m)?)?;
    Ok(())
}
