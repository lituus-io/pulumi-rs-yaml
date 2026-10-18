// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Fuzz target: backend parsing, stack/resource URIs, backend resolution and
//! resource keys.
//!
//! Security targets:
//! - A backend URL carrying userinfo (credentials) must never yield a URI
//! - No name -- organization, project, stack, URN -- can change a URI's
//!   shape: no whitespace, control character, query, extra fragment or
//!   dot-segment escapes into it
//! - The workspace credentials reader never panics and returns only `current`
//! - Resource keys are well-formed or absent, never an escaped guess

#![no_main]
use libfuzzer_sys::fuzz_target;

use pulumi_rs_yaml_core::resource_key::{bq_to_resource_key, resource_key, TEMPLATES};
use pulumi_rs_yaml_core::stack_uri::{
    current_backend_from_credentials, parse_backend, resolve_backend, Backend, BackendSource,
    StackUri,
};

fn check_uri(uri: &str, fragments: usize) {
    assert!(
        !uri.chars().any(|c| c.is_whitespace() || c.is_control()),
        "{uri:?}"
    );
    assert!(!uri.contains('?'), "{uri}");
    assert_eq!(uri.matches('#').count(), fragments, "{uri}");
    let path = uri.split('#').next().unwrap_or(uri);
    assert!(!path.contains("/../") && !path.contains("/./"), "{uri}");
}

fn check_key(key: &str) {
    assert!(key.starts_with("//"), "{key}");
    assert!(
        !key.chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '#' || c == '?'),
        "{key:?}"
    );
    assert!(
        !key.contains("//",) || key.matches("//").count() == 1,
        "{key}"
    );
}

fuzz_target!(|data: &[u8]| {
    if data.len() > 16 * 1024 {
        return;
    }
    let _ = current_backend_from_credentials(data);
    let Ok(input) = std::str::from_utf8(data) else {
        return;
    };
    let mut parts = input.split('\u{1}');
    let url = parts.next().unwrap_or("");
    let org = parts.next().unwrap_or("o");
    let project = parts.next().unwrap_or("p");
    let stack = parts.next().unwrap_or("s");
    let urn = parts.next().unwrap_or("urn");

    if let Some(backend) = parse_backend(url) {
        // Userinfo never survives into a recognised backend.
        let scheme_end = url.find("://").map_or(0, |i| i + 3);
        let authority = &url[scheme_end..];
        let authority = &authority[..authority.find(['/', '?', '#']).unwrap_or(authority.len())];
        assert!(!authority.contains('@'), "{url}");
        if let Backend::SelfManaged { base } = backend {
            assert!(url.starts_with(base));
        }
        for known in [true, false] {
            if let Some(s) = StackUri::new(backend, org, known, project, stack) {
                let stack_uri = s.to_string();
                check_uri(&stack_uri, 0);
                let r = s.resource(urn).to_string();
                check_uri(&r, 1);
                assert!(r.starts_with(&format!("{stack_uri}#")));
            }
        }
    }

    // The resolution order: the first non-empty wins, and says where from.
    fn opt(s: &str) -> Option<&str> {
        (!s.is_empty()).then_some(s)
    }
    let (got, source) = resolve_backend(opt(url), Some(org), opt(project), Some(stack));
    match source {
        BackendSource::Argument => assert_eq!(got, Some(url)),
        BackendSource::Env => assert_eq!(got, Some(org)),
        BackendSource::Project => assert_eq!(got, Some(project)),
        BackendSource::Workspace => assert_eq!(got, Some(stack)),
        BackendSource::None => assert_eq!(got, None),
    }

    // Resource keys: well-formed or absent, for every template.
    for t in TEMPLATES {
        let ty = format!("gcp:{}:{}", t.module, t.name);
        if let Some(k) = resource_key(&ty, url) {
            check_key(&k);
        }
    }
    if let Some(k) = bq_to_resource_key(url) {
        check_key(&k);
    }
});
