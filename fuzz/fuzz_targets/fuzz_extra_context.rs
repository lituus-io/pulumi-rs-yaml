//! Fuzz target: Extra context variables and readFile path validation
//!
//! Tests v0.4.0 new functionality:
//! - JinjaContext extra vars rendering (conditionals, expressions)
//! - readFile path traversal rejection (absolute paths, directory escape)
//! - has_any_jinja_block_syntax() inline detection
//! - build_minijinja_context collision safety (extras must not override builtins)
//!
//! Security targets:
//! - Template injection via crafted extra var names that collide with builtins
//! - Path traversal via readFile (../../../etc/passwd)
//! - Stack overflow from deeply nested Jinja conditionals with extra vars
//! - Panics from malformed extra var names or values

#![no_main]
use libfuzzer_sys::fuzz_target;
use std::collections::HashMap;

fuzz_target!(|data: &[u8]| {
    let Ok(input) = std::str::from_utf8(data) else {
        return;
    };

    if input.len() > 32 * 1024 {
        return;
    }

    // has_any_jinja_block_syntax must never panic
    let _ = pulumi_rs_yaml_core::jinja::has_any_jinja_block_syntax(input);

    // Consistency: has_any_jinja_block_syntax must be a superset of has_jinja_block_syntax
    if pulumi_rs_yaml_core::jinja::has_jinja_block_syntax(input) {
        assert!(
            pulumi_rs_yaml_core::jinja::has_any_jinja_block_syntax(input),
            "has_any should be superset of has_jinja_block_syntax"
        );
    }

    // Split input: first half = template, second half = extra var key/value pairs
    // Find a char boundary near the midpoint to avoid panicking on multi-byte UTF-8
    let mid = {
        let m = input.len() / 2;
        // Walk forward from approximate midpoint to find a char boundary
        (m..input.len())
            .find(|&i| input.is_char_boundary(i))
            .unwrap_or(input.len())
    };
    let template_part = &input[..mid];
    let extra_part = &input[mid..];

    // Build extra vars from fuzzed input (key=value pairs split by newlines)
    let mut extra = HashMap::new();
    for line in extra_part.lines().take(50) {
        if let Some((k, v)) = line.split_once('=') {
            let key = k.trim();
            let val = v.trim();
            if !key.is_empty() && key.len() < 256 && val.len() < 1024 {
                extra.insert(key.to_string(), val.to_string());
            }
        }
    }

    // Render with extra context — must never panic
    let config = HashMap::new();
    let ctx = pulumi_rs_yaml_core::jinja::JinjaContext {
        project_name: "fuzz-project",
        stack_name: "dev",
        cwd: "/tmp/fuzz",
        organization: "fuzz-org",
        root_directory: "/tmp/fuzz",
        config: &config,
        project_dir: "/tmp/fuzz",
        undefined: pulumi_rs_yaml_core::jinja::UndefinedMode::Strict,
        extra: &extra,
    };
    let preprocessor = pulumi_rs_yaml_core::jinja::JinjaPreprocessor::new(&ctx);
    use pulumi_rs_yaml_core::jinja::TemplatePreprocessor;
    let _ = preprocessor.preprocess(template_part, "fuzz.yaml");

    // Also test passthrough mode
    let ctx_pt = pulumi_rs_yaml_core::jinja::JinjaContext {
        project_name: "fuzz-project",
        stack_name: "dev",
        cwd: "/tmp/fuzz",
        organization: "fuzz-org",
        root_directory: "/tmp/fuzz",
        config: &config,
        project_dir: "/tmp/fuzz",
        undefined: pulumi_rs_yaml_core::jinja::UndefinedMode::Passthrough,
        extra: &extra,
    };
    let preprocessor_pt = pulumi_rs_yaml_core::jinja::JinjaPreprocessor::new(&ctx_pt);
    let _ = preprocessor_pt.preprocess(template_part, "fuzz.yaml");

    // Security: extra var named "pulumi_project" must NOT override the real value
    if extra.contains_key("pulumi_project") {
        let safe_ctx = pulumi_rs_yaml_core::jinja::JinjaContext {
            project_name: "REAL_PROJECT",
            stack_name: "dev",
            cwd: "/tmp",
            organization: "",
            root_directory: "/tmp",
            config: &config,
            project_dir: "/tmp",
            undefined: pulumi_rs_yaml_core::jinja::UndefinedMode::Strict,
            extra: &extra,
        };
        let safe_pp = pulumi_rs_yaml_core::jinja::JinjaPreprocessor::new(&safe_ctx);
        if let Ok(rendered) = safe_pp.preprocess("{{ pulumi_project }}", "test.yaml") {
            assert!(
                rendered.as_ref().contains("REAL_PROJECT"),
                "extra vars must NOT override builtins: got {}",
                rendered.as_ref()
            );
        }
    }
});
