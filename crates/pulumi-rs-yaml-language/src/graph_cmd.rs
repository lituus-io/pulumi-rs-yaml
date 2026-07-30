//! `pulumi-language-yaml graph` — export the project's static resource
//! dependency graph as JSON or as BigQuery-ingestable NDJSON.
//!
//! ```text
//! pulumi-language-yaml graph --stack <s> [--organization <o>] [--dir <d>]
//!                            [--format json|ndjson] [--out <dir>]
//!                            [--lineage] [--default-bq-project <p>]
//! ```
//!
//! With `--lineage`, the JSON document gains a `sql_lineage` root key
//! (output without the flag is byte-identical to before), and ndjson
//! mode additionally writes `lineage_nodes.ndjson` /
//! `lineage_edges.ndjson`.
//!
//! `json` (default) prints one pretty document to stdout. `ndjson`
//! writes `nodes.ndjson` and `edges.ndjson` (one flat row per line) to
//! the `--out` directory, ready for
//! `bq load --source_format=NEWLINE_DELIMITED_JSON`. Diagnostics go to
//! stderr; the exit code is non-zero on errors.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

use pulumi_rs_yaml_core::multi_file;
use pulumi_rs_yaml_core::resource_graph::{export_resource_graph, GraphExportOptions};
#[cfg(feature = "sql-lineage")]
use pulumi_rs_yaml_core::sql_lineage::{export_sql_lineage, SqlLineageOptions};

const USAGE: &str = "usage: pulumi-language-yaml graph --stack <stack> \
    [--organization <org>] [--dir <project-dir>] [--format json|ndjson] [--out <dir>] \
    [--lineage] [--default-bq-project <project>]";

struct GraphArgs {
    stack: String,
    organization: String,
    dir: PathBuf,
    format: OutputFormat,
    out: Option<PathBuf>,
    lineage: bool,
    /// Only read by the lineage exporter; parsed regardless so the flag is
    /// still validated in builds without the `sql-lineage` feature.
    #[cfg_attr(not(feature = "sql-lineage"), allow(dead_code))]
    default_bq_project: Option<String>,
}

#[derive(PartialEq)]
enum OutputFormat {
    Json,
    Ndjson,
}

fn parse_args(args: &[String]) -> Result<GraphArgs, String> {
    let mut stack = String::new();
    let mut organization = String::new();
    let mut dir: Option<PathBuf> = None;
    let mut format = OutputFormat::Json;
    let mut out: Option<PathBuf> = None;
    let mut lineage = false;
    let mut default_bq_project: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let take_value = |i: usize| -> Result<&String, String> {
            args.get(i + 1)
                .ok_or_else(|| format!("missing value for {}\n{}", args[i], USAGE))
        };
        match args[i].as_str() {
            "--stack" => {
                stack = take_value(i)?.clone();
                i += 2;
            }
            "--organization" => {
                organization = take_value(i)?.clone();
                i += 2;
            }
            "--dir" => {
                dir = Some(PathBuf::from(take_value(i)?));
                i += 2;
            }
            "--format" => {
                format = match take_value(i)?.as_str() {
                    "json" => OutputFormat::Json,
                    "ndjson" => OutputFormat::Ndjson,
                    other => return Err(format!("unknown format '{}'\n{}", other, USAGE)),
                };
                i += 2;
            }
            "--out" => {
                out = Some(PathBuf::from(take_value(i)?));
                i += 2;
            }
            "--lineage" => {
                lineage = true;
                i += 1;
            }
            "--default-bq-project" => {
                default_bq_project = Some(take_value(i)?.clone());
                i += 2;
            }
            other => return Err(format!("unknown argument '{}'\n{}", other, USAGE)),
        }
    }

    if stack.is_empty() {
        return Err(format!(
            "--stack is required: exported node ids embed the stack name\n{}",
            USAGE
        ));
    }
    if format == OutputFormat::Ndjson && out.is_none() {
        return Err(format!("--format ndjson requires --out <dir>\n{}", USAGE));
    }
    let dir = match dir {
        Some(d) => d,
        None => std::env::current_dir().map_err(|e| format!("cannot resolve cwd: {}", e))?,
    };
    Ok(GraphArgs {
        stack,
        organization,
        dir,
        format,
        out,
        lineage,
        default_bq_project,
    })
}

/// Runs the graph subcommand; returns the process exit code.
pub fn run_graph(args: &[String]) -> i32 {
    let parsed = match parse_args(args) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("error: {}", msg);
            return 1;
        }
    };

    // Project name pre-pass for the Jinja context (strip {% %} so the
    // main file parses as YAML even when it carries block syntax).
    let project_name = match multi_file::discover_project_files(&parsed.dir) {
        Ok(files) => match std::fs::read_to_string(&files.main_file) {
            Ok(raw) => {
                let stripped = pulumi_rs_yaml_core::jinja::strip_jinja_blocks(&raw);
                let (tmpl, _) = pulumi_rs_yaml_core::ast::parse::parse_template(&stripped, None);
                tmpl.name
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            }
            Err(e) => {
                eprintln!("error: failed to read project file: {}", e);
                return 1;
            }
        },
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };

    let dir_str = parsed.dir.to_string_lossy().into_owned();
    let config: HashMap<String, String> = HashMap::new();
    let extra: HashMap<String, String> = HashMap::new();
    let jinja_ctx = pulumi_rs_yaml_core::jinja::JinjaContext {
        project_name: &project_name,
        stack_name: &parsed.stack,
        cwd: &dir_str,
        organization: &parsed.organization,
        root_directory: &dir_str,
        config: &config,
        project_dir: &dir_str,
        undefined: pulumi_rs_yaml_core::jinja::UndefinedMode::Strict,
        extra: &extra,
    };

    let (merged, load_diags) = multi_file::load_project(&parsed.dir, Some(&jinja_ctx));
    if load_diags.has_errors() {
        eprintln!("error: failed to load project: {}", load_diags);
        return 1;
    }
    for diag in load_diags.iter() {
        eprintln!("{}", diag);
    }

    let effective_project = merged.name().unwrap_or("unknown").to_string();
    let template = merged.as_template_decl();
    let opts = GraphExportOptions {
        organization: &parsed.organization,
        project: &effective_project,
        stack: &parsed.stack,
        source_map: Some(merged.source_map()),
        schema_store: None,
    };
    let (graph, diags) = export_resource_graph(&template, &opts);
    for diag in diags.iter() {
        eprintln!("{}", diag);
    }
    if diags.has_errors() {
        return 1;
    }

    #[cfg(not(feature = "sql-lineage"))]
    if parsed.lineage {
        eprintln!("error: --lineage requires the sql-lineage feature (not built in)");
        return 1;
    }
    #[cfg(feature = "sql-lineage")]
    let lineage = if parsed.lineage {
        let lineage_opts = SqlLineageOptions {
            organization: &parsed.organization,
            project: &effective_project,
            stack: &parsed.stack,
            project_dir: Some(&parsed.dir),
            default_bq_project: parsed.default_bq_project.as_deref(),
            source_map: Some(merged.source_map()),
            extra_sql_sources: &[],
        };
        let (lineage, lineage_diags) = export_sql_lineage(&template, &graph, &lineage_opts);
        for diag in lineage_diags.iter() {
            eprintln!("{}", diag);
        }
        if lineage_diags.has_errors() {
            return 1;
        }
        Some(lineage)
    } else {
        None
    };

    match parsed.format {
        OutputFormat::Json => {
            // Without --lineage the output is byte-identical to prior
            // releases; with it, one `sql_lineage` root key is added.
            #[cfg(not(feature = "sql-lineage"))]
            let rendered = graph.to_json();
            #[cfg(feature = "sql-lineage")]
            let rendered = match &lineage {
                None => graph.to_json(),
                Some(lineage) => serde_json::to_value(&graph)
                    .and_then(|mut doc| {
                        doc["sql_lineage"] = serde_json::to_value(lineage)?;
                        serde_json::to_string_pretty(&doc)
                    })
                    .map(|mut s| {
                        s.push('\n');
                        s
                    })
                    .map_err(|e| format!("failed to serialize graph document: {}", e)),
            };
            match rendered {
                Ok(json) => {
                    print!("{}", json);
                    0
                }
                Err(e) => {
                    eprintln!("error: {}", e);
                    1
                }
            }
        }
        OutputFormat::Ndjson => {
            let out_dir = match &parsed.out {
                Some(d) => d,
                None => {
                    eprintln!("error: --format ndjson requires --out <dir>");
                    return 1;
                }
            };
            if let Err(e) = std::fs::create_dir_all(out_dir) {
                eprintln!("error: cannot create {}: {}", out_dir.display(), e);
                return 1;
            }
            if let Err(e) = write_ndjson(out_dir, &graph) {
                eprintln!("error: {}", e);
                return 1;
            }
            #[cfg(feature = "sql-lineage")]
            if let Some(lineage) = &lineage {
                if let Err(e) = write_lineage_ndjson(out_dir, lineage) {
                    eprintln!("error: {}", e);
                    return 1;
                }
            }
            0
        }
    }
}

#[cfg(feature = "sql-lineage")]
fn write_lineage_ndjson(
    out_dir: &std::path::Path,
    lineage: &pulumi_rs_yaml_core::sql_lineage::SqlLineageGraph<'_>,
) -> Result<(), String> {
    let write_rows = |name: &str, rows: Vec<String>| -> Result<(), String> {
        let path = out_dir.join(name);
        let mut file = std::fs::File::create(&path)
            .map_err(|e| format!("cannot create {}: {}", path.display(), e))?;
        for row in rows {
            writeln!(file, "{}", row).map_err(|e| format!("write {}: {}", path.display(), e))?;
        }
        Ok(())
    };
    let nodes: Vec<String> = lineage
        .nodes
        .iter()
        .map(|n| serde_json::to_string(n).map_err(|e| format!("serialize lineage node: {}", e)))
        .collect::<Result<_, _>>()?;
    let edges: Vec<String> = lineage
        .edges
        .iter()
        .map(|e| serde_json::to_string(e).map_err(|err| format!("serialize lineage edge: {}", err)))
        .collect::<Result<_, _>>()?;
    write_rows("lineage_nodes.ndjson", nodes)?;
    write_rows("lineage_edges.ndjson", edges)?;
    Ok(())
}

fn write_ndjson(
    out_dir: &std::path::Path,
    graph: &pulumi_rs_yaml_core::resource_graph::ResourceGraph<'_>,
) -> Result<(), String> {
    let write_rows = |name: &str, rows: Vec<String>| -> Result<(), String> {
        let path = out_dir.join(name);
        let mut file = std::fs::File::create(&path)
            .map_err(|e| format!("cannot create {}: {}", path.display(), e))?;
        for row in rows {
            writeln!(file, "{}", row).map_err(|e| format!("write {}: {}", path.display(), e))?;
        }
        Ok(())
    };

    let nodes: Vec<String> = graph
        .nodes
        .iter()
        .map(|n| serde_json::to_string(n).map_err(|e| format!("serialize node: {}", e)))
        .collect::<Result<_, _>>()?;
    let edges: Vec<String> = graph
        .edges
        .iter()
        .map(|e| serde_json::to_string(e).map_err(|err| format!("serialize edge: {}", err)))
        .collect::<Result<_, _>>()?;
    write_rows("nodes.ndjson", nodes)?;
    write_rows("edges.ndjson", edges)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn write_project(dir: &std::path::Path, content: &str) {
        fs::write(dir.join("Pulumi.yaml"), content).unwrap();
    }

    const BASIC: &str = "name: proj\nruntime: yaml\nresources:\n  bucket:\n    type: gcp:storage:Bucket\n    properties:\n      location: US\noutputs:\n  id: ${bucket.id}\n";

    #[test]
    fn missing_stack_fails() {
        assert!(parse_args(&args(&["--dir", "/tmp"])).is_err());
    }

    #[test]
    fn ndjson_requires_out() {
        assert!(parse_args(&args(&["--stack", "dev", "--format", "ndjson"])).is_err());
    }

    #[test]
    fn unknown_arg_fails() {
        assert!(parse_args(&args(&["--stack", "dev", "--bogus"])).is_err());
    }

    #[test]
    fn json_mode_exit_zero() {
        let dir = tempfile::tempdir().unwrap();
        write_project(dir.path(), BASIC);
        let code = run_graph(&args(&[
            "--stack",
            "dev",
            "--organization",
            "org",
            "--dir",
            &dir.path().to_string_lossy(),
        ]));
        assert_eq!(code, 0);
    }

    #[test]
    fn ndjson_mode_writes_valid_rows() {
        let dir = tempfile::tempdir().unwrap();
        write_project(dir.path(), BASIC);
        let out = dir.path().join("export");
        let code = run_graph(&args(&[
            "--stack",
            "dev",
            "--organization",
            "org",
            "--dir",
            &dir.path().to_string_lossy(),
            "--format",
            "ndjson",
            "--out",
            &out.to_string_lossy(),
        ]));
        assert_eq!(code, 0);
        for file in ["nodes.ndjson", "edges.ndjson"] {
            let content = fs::read_to_string(out.join(file)).unwrap();
            assert!(!content.is_empty(), "{} should not be empty", file);
            for line in content.lines() {
                let row: serde_json::Value = serde_json::from_str(line).unwrap();
                assert!(row.is_object(), "each NDJSON line is one flat object");
            }
        }
        let nodes = fs::read_to_string(out.join("nodes.ndjson")).unwrap();
        assert!(nodes.contains("urn:pulumi:dev::proj::gcp:storage/bucket:Bucket::bucket"));
    }

    #[test]
    fn lineage_flag_parses() {
        let parsed = parse_args(&args(&[
            "--stack",
            "dev",
            "--lineage",
            "--default-bq-project",
            "proj-x",
        ]))
        .expect("parses");
        assert!(parsed.lineage);
        assert_eq!(parsed.default_bq_project.as_deref(), Some("proj-x"));
        let plain = parse_args(&args(&["--stack", "dev"])).expect("parses");
        assert!(!plain.lineage);
    }

    #[test]
    fn ndjson_without_lineage_writes_two_files() {
        let dir = tempfile::tempdir().unwrap();
        write_project(dir.path(), BASIC);
        let out = dir.path().join("export");
        let code = run_graph(&args(&[
            "--stack",
            "dev",
            "--dir",
            &dir.path().to_string_lossy(),
            "--format",
            "ndjson",
            "--out",
            &out.to_string_lossy(),
        ]));
        assert_eq!(code, 0);
        assert!(out.join("nodes.ndjson").exists());
        assert!(out.join("edges.ndjson").exists());
        assert!(!out.join("lineage_nodes.ndjson").exists());
    }

    #[test]
    fn ndjson_with_lineage_writes_four_files() {
        let dir = tempfile::tempdir().unwrap();
        write_project(
            dir.path(),
            "name: proj\nruntime: yaml\nresources:\n  v:\n    type: gcp:bigquery:Table\n    properties:\n      project: data-proj\n      datasetId: marts\n      tableId: v1\n      view:\n        query: \"SELECT id FROM `data-proj.raw.events`\"\n",
        );
        let out = dir.path().join("export");
        let code = run_graph(&args(&[
            "--stack",
            "dev",
            "--dir",
            &dir.path().to_string_lossy(),
            "--lineage",
            "--format",
            "ndjson",
            "--out",
            &out.to_string_lossy(),
        ]));
        assert_eq!(code, 0);
        for file in [
            "nodes.ndjson",
            "edges.ndjson",
            "lineage_nodes.ndjson",
            "lineage_edges.ndjson",
        ] {
            let content = fs::read_to_string(out.join(file)).unwrap();
            assert!(!content.is_empty(), "{} should not be empty", file);
            for line in content.lines() {
                let row: serde_json::Value = serde_json::from_str(line).unwrap();
                assert!(row.is_object());
            }
        }
        let lineage_nodes = fs::read_to_string(out.join("lineage_nodes.ndjson")).unwrap();
        assert!(lineage_nodes.contains("bq://data-proj/marts/v1"));
        let lineage_edges = fs::read_to_string(out.join("lineage_edges.ndjson")).unwrap();
        assert!(lineage_edges.contains("derives_from"));
    }

    #[test]
    fn dag_error_exit_one() {
        let dir = tempfile::tempdir().unwrap();
        write_project(
            dir.path(),
            "name: proj\nruntime: yaml\nresources:\n  a:\n    type: t:m:A\n    properties:\n      x: ${missing}\n",
        );
        let code = run_graph(&args(&[
            "--stack",
            "dev",
            "--dir",
            &dir.path().to_string_lossy(),
        ]));
        assert_eq!(code, 1);
    }
}
