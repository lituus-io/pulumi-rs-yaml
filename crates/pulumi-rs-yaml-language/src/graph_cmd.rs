// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! `pulumi-language-yaml graph` — export the project's static resource
//! dependency graph as JSON or as BigQuery-ingestable NDJSON.
//!
//! ```text
//! pulumi-language-yaml graph --stack <s> [--organization <o>] [--dir <d>]
//!                            [--format json|ndjson] [--out <dir>]
//!                            [--lineage] [--default-bq-project <p>]
//!                            [--sql-statements] [--compress gzip|none]
//!                            [--backend-url <url>] [--script-column-lineage]
//! ```
//!
//! With `--lineage`, the JSON document gains a `sql_lineage` root key
//! (output without the flag is byte-identical to before), and ndjson
//! mode additionally writes `lineage_nodes.ndjson` /
//! `lineage_edges.ndjson`. `--sql-statements` (with `--lineage`) adds the
//! statement layer -- every SQL statement's text, kind and position, and
//! statement-level `reads`/`writes` edges -- as `statements` and
//! `statement_occurrences` in JSON, and as `lineage_statements.ndjson` and
//! `lineage_statement_occurrences.ndjson`.
//!
//! `--script-column-lineage` (with `--lineage`) adds column lineage through
//! scripts: `INSERT ... (cols) SELECT`, `CREATE TABLE ... AS SELECT` and
//! `CREATE VIEW` in routines, jobs and scheduled queries.
//!
//! `--backend-url` names where the stack's state lives (Pulumi Cloud or a
//! `gs://`, `s3://`, `azblob://` or `file://` backend); without it the
//! `PULUMI_BACKEND_URL` environment variable, then `backend.url` in
//! `Pulumi.yaml`, then the Pulumi workspace's current backend are used --
//! the one resolver the Python binding uses too -- and `backend_source`
//! names which. A known backend adds `stack_uri`, `referenced_stack_uri` and
//! `resource_uri` to the graph.
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
use pulumi_rs_yaml_core::sql_lineage::{export_sql_lineage, SqlLineageOptions, SqlSourceStore};

const USAGE: &str = "usage: pulumi-language-yaml graph --stack <stack> \
    [--organization <org>] [--dir <project-dir>] [--format json|ndjson] [--out <dir>] \
    [--lineage] [--default-bq-project <project>] [--sql-statements] [--compress gzip|none] \
    [--backend-url <url>] [--script-column-lineage]";

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
    #[cfg_attr(not(feature = "sql-lineage"), allow(dead_code))]
    sql_statements: bool,
    compress: Compression,
    backend_url: Option<String>,
    #[cfg_attr(not(feature = "sql-lineage"), allow(dead_code))]
    script_column_lineage: bool,
}

/// How NDJSON tables are written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Compression {
    None,
    /// `*.ndjson.gz`; needs the `gzip` feature.
    Gzip,
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
    let mut sql_statements = false;
    let mut compress = Compression::None;
    let mut backend_url: Option<String> = None;
    let mut script_column_lineage = false;

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
            "--compress" => {
                compress = match take_value(i)?.as_str() {
                    "none" => Compression::None,
                    "gzip" if cfg!(feature = "gzip") => Compression::Gzip,
                    "gzip" => {
                        return Err(format!(
                            "--compress gzip requires the gzip feature (not built in)\n{}",
                            USAGE
                        ))
                    }
                    other => return Err(format!("unknown compression '{}'\n{}", other, USAGE)),
                };
                i += 2;
            }
            "--script-column-lineage" => {
                script_column_lineage = true;
                i += 1;
            }
            "--backend-url" => {
                backend_url = Some(take_value(i)?.clone());
                i += 2;
            }
            "--sql-statements" => {
                sql_statements = true;
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
    if sql_statements && !lineage {
        return Err(format!("--sql-statements requires --lineage\n{}", USAGE));
    }
    if script_column_lineage && !lineage {
        return Err(format!(
            "--script-column-lineage requires --lineage\n{}",
            USAGE
        ));
    }
    if compress != Compression::None && format != OutputFormat::Ndjson {
        return Err(format!("--compress applies to --format ndjson\n{}", USAGE));
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
        sql_statements,
        compress,
        backend_url,
        script_column_lineage,
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
    let prepass = match multi_file::project_prepass(&parsed.dir) {
        Ok(p) => p,
        Err(multi_file::PrepassError::Discover(e)) => {
            eprintln!("error: {}", e);
            return 1;
        }
        Err(multi_file::PrepassError::ReadMain(e)) => {
            eprintln!("error: failed to read project file: {}", e);
            return 1;
        }
    };
    let project_name = prepass.name;
    let (backend_url, backend_source) = pulumi_rs_yaml_core::stack_uri::discover_backend(
        parsed.backend_url.as_deref(),
        prepass.backend_url.as_deref(),
    );

    let dir_str = parsed.dir.to_string_lossy().into_owned();
    let scoped = prepass.provider_templated_packages;
    let scoped: Vec<&str> = scoped.iter().map(String::as_str).collect();
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
        provider_templated_packages: &scoped,
        extra: &extra,
    };

    let render = match pulumi_rs_yaml_core::jinja::jinja_enabled() {
        Ok(render) => render,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };
    let (merged, load_diags) = multi_file::load_project(&parsed.dir, render.then_some(&jinja_ctx));
    if load_diags.has_errors() {
        eprintln!("error: failed to load project: {}", load_diags);
        return 1;
    }
    for diag in load_diags.iter() {
        eprintln!("{}", diag);
    }

    let effective_project = merged.name().unwrap_or("unknown").to_string();
    let (template, source_map) = merged.into_parts();
    let opts = GraphExportOptions {
        organization: &parsed.organization,
        project: &effective_project,
        stack: &parsed.stack,
        source_map: Some(&source_map),
        schema_store: None,
        backend_url: backend_url.as_deref(),
        backend_source: backend_url.is_some().then_some(backend_source),
        ..Default::default()
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
    // Every file the program reads SQL from, read once and held for as long
    // as the lineage graph borrows from it.
    #[cfg(feature = "sql-lineage")]
    let sql_sources = parsed
        .lineage
        .then(|| SqlSourceStore::collect(&template, &parsed.dir));
    #[cfg(feature = "sql-lineage")]
    let lineage = if parsed.lineage {
        let lineage_opts = SqlLineageOptions {
            organization: &parsed.organization,
            project: &effective_project,
            stack: &parsed.stack,
            project_dir: Some(&parsed.dir),
            default_bq_project: parsed.default_bq_project.as_deref(),
            source_map: Some(&source_map),
            extra_sql_sources: &[],
            sql_sources: sql_sources.as_ref(),
            include_statements: parsed.sql_statements,
            script_column_lineage: parsed.script_column_lineage,
            config: None,
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
            if let Err(e) = write_ndjson(out_dir, &graph, parsed.compress) {
                eprintln!("error: {}", e);
                return 1;
            }
            #[cfg(feature = "sql-lineage")]
            if let Some(lineage) = &lineage {
                if let Err(e) =
                    write_lineage_ndjson(out_dir, lineage, parsed.sql_statements, parsed.compress)
                {
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
    statements: bool,
    compress: Compression,
) -> Result<(), String> {
    write_ndjson_rows(out_dir, "lineage_nodes", &lineage.nodes, compress)?;
    write_ndjson_rows(out_dir, "lineage_edges", &lineage.edges, compress)?;
    if statements {
        write_ndjson_rows(out_dir, "lineage_statements", &lineage.statements, compress)?;
        write_ndjson_rows(
            out_dir,
            "lineage_statement_occurrences",
            &lineage.statement_occurrences,
            compress,
        )?;
    }
    Ok(())
}

fn write_ndjson(
    out_dir: &std::path::Path,
    graph: &pulumi_rs_yaml_core::resource_graph::ResourceGraph<'_>,
    compress: Compression,
) -> Result<(), String> {
    write_ndjson_rows(out_dir, "nodes", &graph.nodes, compress)?;
    write_ndjson_rows(out_dir, "edges", &graph.edges, compress)?;
    Ok(())
}

/// Streams one JSON row per line into `{out_dir}/{table}.ndjson`, or
/// `.ndjson.gz` through a gzip encoder.
///
/// Rows used to be serialized into a `Vec<String>` -- the whole table held in
/// memory twice over -- and then written to an unbuffered `File`, two write
/// calls per row. Serializing straight into a buffered writer produces the
/// same bytes (`to_string` is `to_writer` into a `Vec`) with one allocation
/// for the buffer. Finishing is explicit: `BufWriter`'s drop would swallow a
/// failed final write, and an encoder dropped unfinished leaves a truncated
/// stream, and either must be an error rather than a short table.
fn write_ndjson_rows<T: serde::Serialize>(
    out_dir: &std::path::Path,
    table: &str,
    rows: &[T],
    compress: Compression,
) -> Result<(), String> {
    let name = match compress {
        Compression::None => format!("{table}.ndjson"),
        Compression::Gzip => format!("{table}.ndjson.gz"),
    };
    let path = out_dir.join(name);
    let file = std::fs::File::create(&path)
        .map_err(|e| format!("cannot create {}: {}", path.display(), e))?;
    let mut sink = Sink::new(std::io::BufWriter::new(file), compress);
    for row in rows {
        serde_json::to_writer(&mut sink, row)
            .map_err(|e| format!("serialize row for {}: {}", path.display(), e))?;
        sink.write_all(b"\n")
            .map_err(|e| format!("write {}: {}", path.display(), e))?;
    }
    sink.finish()
        .map_err(|e| format!("write {}: {}", path.display(), e))
}

/// Where rows go: straight to the buffered file, or through a gzip encoder
/// in front of it. An enum rather than a boxed writer: the set is closed.
enum Sink {
    Plain(std::io::BufWriter<std::fs::File>),
    #[cfg(feature = "gzip")]
    Gzip(flate2::write::GzEncoder<std::io::BufWriter<std::fs::File>>),
}

impl Sink {
    fn new(file: std::io::BufWriter<std::fs::File>, compress: Compression) -> Self {
        match compress {
            Compression::None => Sink::Plain(file),
            #[cfg(feature = "gzip")]
            Compression::Gzip => Sink::Gzip(flate2::write::GzEncoder::new(
                file,
                flate2::Compression::default(),
            )),
            // Refused at argument parsing when the feature is not built in.
            #[cfg(not(feature = "gzip"))]
            Compression::Gzip => Sink::Plain(file),
        }
    }

    /// Flushes everything -- the gzip trailer included -- and reports any
    /// failure, which a drop would hide.
    // Without the `gzip` feature the enum has one variant.
    #[allow(clippy::infallible_destructuring_match)]
    fn finish(self) -> std::io::Result<()> {
        let buffered = match self {
            Sink::Plain(w) => w,
            #[cfg(feature = "gzip")]
            Sink::Gzip(encoder) => encoder.finish()?,
        };
        buffered
            .into_inner()
            .map(drop)
            .map_err(std::io::IntoInnerError::into_error)
    }
}

impl std::io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Sink::Plain(w) => w.write(buf),
            #[cfg(feature = "gzip")]
            Sink::Gzip(w) => w.write(buf),
        }
    }

    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        match self {
            Sink::Plain(w) => w.write_all(buf),
            #[cfg(feature = "gzip")]
            Sink::Gzip(w) => w.write_all(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Sink::Plain(w) => w.flush(),
            #[cfg(feature = "gzip")]
            Sink::Gzip(w) => w.flush(),
        }
    }
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
    fn compress_applies_to_ndjson_only() {
        assert!(parse_args(&args(&["--stack", "dev", "--compress", "gzip"])).is_err());
        assert!(parse_args(&args(&["--stack", "dev", "--compress", "zstd"])).is_err());
        let ok = parse_args(&args(&[
            "--stack",
            "dev",
            "--format",
            "ndjson",
            "--out",
            "x",
            "--compress",
            "none",
        ]));
        assert!(ok.is_ok());
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn gzip_tables_decompress_to_the_plain_bytes() {
        use std::io::Read;
        let dir = tempfile::tempdir().unwrap();
        write_project(
            dir.path(),
            "name: proj\nruntime: yaml\nresources:\n  a:\n    type: t:m:A\n  b:\n    type: t:m:B\n    properties:\n      x: ${a.id}\n",
        );
        let run = |out: &std::path::Path, extra: &[&str]| {
            let mut a = vec![
                "--stack",
                "dev",
                "--dir",
                dir.path().to_str().unwrap(),
                "--format",
                "ndjson",
                "--out",
                out.to_str().unwrap(),
            ];
            a.extend_from_slice(extra);
            assert_eq!(run_graph(&args(&a)), 0);
        };
        let plain = dir.path().join("plain");
        let gz = dir.path().join("gz");
        run(&plain, &[]);
        run(&gz, &["--compress", "gzip"]);
        for table in ["nodes", "edges"] {
            let expected = fs::read(plain.join(format!("{table}.ndjson"))).unwrap();
            let mut decoded = Vec::new();
            flate2::read::GzDecoder::new(
                fs::File::open(gz.join(format!("{table}.ndjson.gz"))).unwrap(),
            )
            .read_to_end(&mut decoded)
            .unwrap();
            assert_eq!(decoded, expected, "{table}");
            assert!(!gz.join(format!("{table}.ndjson")).exists());
        }
    }

    #[test]
    fn backend_url_from_the_flag_and_from_pulumi_yaml() {
        let dir = tempfile::tempdir().unwrap();
        write_project(
            dir.path(),
            "name: proj\nruntime: yaml\nbackend:\n  url: gs://from-project\nresources:\n  a:\n    type: t:m:A\n",
        );
        let export = |extra: &[&str]| -> serde_json::Value {
            let out = dir.path().join("out");
            let mut a = vec![
                "--stack",
                "dev",
                "--organization",
                "acme",
                "--dir",
                dir.path().to_str().unwrap(),
                "--format",
                "ndjson",
                "--out",
                out.to_str().unwrap(),
            ];
            a.extend_from_slice(extra);
            assert_eq!(run_graph(&args(&a)), 0);
            let nodes = fs::read_to_string(out.join("nodes.ndjson")).unwrap();
            nodes
                .lines()
                .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
                .find(|n| n["kind"] == "stack")
                .unwrap()
        };
        // The flag wins over the project file. (The environment variable sits
        // between them; it is not set here, so the test does not race others.)
        if std::env::var_os("PULUMI_BACKEND_URL").is_none() {
            assert_eq!(
                export(&[])["stack_uri"],
                "gs://from-project/.pulumi/stacks/proj/dev.json"
            );
        }
        assert_eq!(
            export(&["--backend-url", "https://api.pulumi.com"])["stack_uri"],
            "https://app.pulumi.com/acme/proj/dev"
        );
    }

    #[test]
    fn script_column_lineage_requires_lineage() {
        assert!(parse_args(&args(&["--stack", "dev", "--script-column-lineage"])).is_err());
        assert!(parse_args(&args(&[
            "--stack",
            "dev",
            "--lineage",
            "--script-column-lineage"
        ]))
        .is_ok());
    }

    #[test]
    fn sql_statements_requires_lineage() {
        assert!(parse_args(&args(&["--stack", "dev", "--sql-statements"])).is_err());
        assert!(parse_args(&args(&["--stack", "dev", "--lineage", "--sql-statements"])).is_ok());
    }

    #[cfg(feature = "sql-lineage")]
    #[test]
    fn sql_statements_writes_the_statement_files() {
        let dir = tempfile::tempdir().unwrap();
        write_project(
            dir.path(),
            "name: proj\nruntime: yaml\nresources:\n  v:\n    type: gcp:bigquery:Table\n    properties:\n      project: data-proj\n      datasetId: marts\n      tableId: v1\n      view:\n        query: \"SELECT id FROM `data-proj.raw.events`\"\n",
        );
        let out = dir.path().join("export");
        let run = |extra: &[&str]| {
            let mut a = vec![
                "--stack",
                "dev",
                "--dir",
                dir.path().to_str().unwrap(),
                "--lineage",
                "--format",
                "ndjson",
                "--out",
                out.to_str().unwrap(),
            ];
            a.extend_from_slice(extra);
            run_graph(&args(&a))
        };
        assert_eq!(run(&[]), 0);
        assert!(!out.join("lineage_statements.ndjson").exists());
        assert_eq!(run(&["--sql-statements"]), 0);
        let statements = fs::read_to_string(out.join("lineage_statements.ndjson")).unwrap();
        let row: serde_json::Value =
            serde_json::from_str(statements.lines().next().unwrap()).unwrap();
        assert_eq!(row["statement_kind"], "select");
        assert_eq!(row["text"], "SELECT id FROM `data-proj.raw.events`");
        assert!(row["id"].as_str().unwrap().starts_with("sql:"));
        let occurrences =
            fs::read_to_string(out.join("lineage_statement_occurrences.ndjson")).unwrap();
        let occ: serde_json::Value =
            serde_json::from_str(occurrences.lines().next().unwrap()).unwrap();
        assert_eq!(occ["carrier_id"], "bq://data-proj/marts/v1");
        assert_eq!(occ["text_origin"], "literal");
        let edges = fs::read_to_string(out.join("lineage_edges.ndjson")).unwrap();
        assert!(edges.contains("\"has_statement\""));
        assert!(edges.contains("\"reads\""));
    }

    /// The streaming writer must produce exactly what the buffered one did:
    /// each row as `serde_json::to_string`, then one newline.
    #[test]
    fn streamed_rows_are_byte_identical_to_per_row_serialization() {
        let (template, diags) = pulumi_rs_yaml_core::ast::parse::parse_template(
            "name: proj\nruntime: yaml\nresources:\n  a:\n    type: t:m:A\n    properties:\n      x: \"q\\\"uote\\u00e9\"\n  b:\n    type: t:m:B\n    properties:\n      y: ${a.id}\noutputs:\n  o: ${b.id}\n",
            None,
        );
        assert!(!diags.has_errors(), "{diags}");
        let opts = GraphExportOptions {
            organization: "org",
            project: "proj",
            stack: "dev",
            source_map: None,
            schema_store: None,
            ..Default::default()
        };
        let (graph, _) = export_resource_graph(&template, &opts);
        let dir = tempfile::tempdir().unwrap();
        write_ndjson(dir.path(), &graph, Compression::None).unwrap();
        let expected = |rows: Vec<String>| rows.into_iter().map(|r| r + "\n").collect::<String>();
        let nodes = expected(
            graph
                .nodes
                .iter()
                .map(|n| serde_json::to_string(n).unwrap())
                .collect(),
        );
        let edges = expected(
            graph
                .edges
                .iter()
                .map(|e| serde_json::to_string(e).unwrap())
                .collect(),
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("nodes.ndjson")).unwrap(),
            nodes
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("edges.ndjson")).unwrap(),
            edges
        );
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
