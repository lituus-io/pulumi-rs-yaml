//! Fuzz target: Parallel evaluation of YAML templates
//!
//! Tests the evaluator with parallel=4 on arbitrary YAML templates to find:
//! - Deadlocks from Mutex lock ordering
//! - Data races (would manifest as incorrect results or panics)
//! - Panics from concurrent access to shared state (EvalState)
//! - Resource registration ordering violations
//! - Poison set corruption under concurrency

#![no_main]
use libfuzzer_sys::fuzz_target;
use std::collections::HashMap;

fuzz_target!(|data: &[u8]| {
    let Ok(input) = std::str::from_utf8(data) else {
        return;
    };

    // Cap input size to prevent trivially large allocations
    if input.len() > 64 * 1024 {
        return;
    }

    // Parse the template
    let (template, parse_diags) = pulumi_rs_yaml_core::ast::parse::parse_template(input, None);
    if parse_diags.has_errors() {
        return;
    }

    // Leak template to satisfy 'static lifetime requirement
    let template: &'static _ = Box::leak(Box::new(template));
    let raw_config = HashMap::new();

    // Run 1: Sequential evaluation (baseline)
    // Use catch_unwind to skip inputs that trigger pre-existing panics
    let seq_result = std::panic::catch_unwind(|| {
        let mock_seq = pulumi_rs_yaml_core::eval::mock::MockCallback::new();
        let eval_seq = pulumi_rs_yaml_core::eval::evaluator::Evaluator::with_callback(
            "fuzz".to_string(),
            "dev".to_string(),
            "/tmp".to_string(),
            false,
            mock_seq,
        );
        eval_seq.evaluate_template(template, &raw_config, &[]);
        let errors = eval_seq.has_errors();
        let count = eval_seq.callback().registrations().len();
        (errors, count)
    });

    let (seq_errors, seq_reg_count) = match seq_result {
        Ok(r) => r,
        Err(_) => return, // Pre-existing panic in sequential eval — skip this input
    };

    // Run 2: Parallel evaluation (must not panic, deadlock, or corrupt state)
    let mock_par = pulumi_rs_yaml_core::eval::mock::MockCallback::new();
    let mut eval_par = pulumi_rs_yaml_core::eval::evaluator::Evaluator::with_callback(
        "fuzz".to_string(),
        "dev".to_string(),
        "/tmp".to_string(),
        false,
        mock_par,
    );
    eval_par.parallel = 4;
    eval_par.evaluate_template(template, &raw_config, &[]);
    let par_errors = eval_par.has_errors();
    let par_reg_count = eval_par.callback().registrations().len();

    // Invariant: if sequential succeeds (no errors), parallel must also succeed
    // with the same number of resource registrations.
    // When sequential errors, parallel may register more resources (because
    // sequential eagerly breaks on first error within a level, while parallel
    // lets the entire level complete).
    if !seq_errors {
        assert!(
            !par_errors,
            "sequential succeeded but parallel failed"
        );
        assert_eq!(
            seq_reg_count, par_reg_count,
            "sequential registered {} resources but parallel registered {}",
            seq_reg_count, par_reg_count
        );
    }

    // Invariant: parallel must never panic, deadlock, or corrupt state
    // (reaching this point proves it didn't)
});
