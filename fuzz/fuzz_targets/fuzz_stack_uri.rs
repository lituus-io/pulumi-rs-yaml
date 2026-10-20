// Author: spicyzhug@gmail.com
// Copyright: lituus-io, all rights reserved

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

use pulumi_rs_yaml_core::resource_key::{
    bq_to_resource_key, resource_key, resource_key_from_properties, resource_key_with_properties,
    TEMPLATES,
};
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
    // Every segment after the host is a collection or an id: never empty,
    // never a dot segment -- nothing escapes its place in the name.
    let path = key[2..].split_once('/').map_or("", |(_, p)| p);
    assert!(
        path.split('/').all(|s| !s.is_empty() && s != "." && s != ".."),
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

    // Resource keys: well-formed or absent, for every template, from a
    // provider id, from properties, and from both.
    let props = [
        ("project", project),
        ("location", org),
        ("region", org),
        ("name", stack),
        ("datasetId", stack),
        ("tableId", urn),
        ("routineId", urn),
        ("batchId", urn),
        ("jobId", urn),
        ("accountId", stack),
    ];
    for t in TEMPLATES {
        let ty = format!("gcp:{}:{}", t.module, t.name);
        if let Some(k) = resource_key(&ty, url) {
            check_key(&k);
        }
        if let Some(k) = resource_key_from_properties(&ty, props) {
            check_key(&k);
        }
        if let Some(k) = resource_key_with_properties(&ty, url, props) {
            check_key(&k);
        }
    }
    // A Cloud Run v1 id keys exactly as the v2 relative name it spells.
    if let Some(k) = resource_key("gcp:cloudrun:Service", url) {
        let mut p = url.split('/');
        let (_, l, _, ns, _, s) = (p.next(), p.next(), p.next(), p.next(), p.next(), p.next());
        let v2 = format!(
            "projects/{}/locations/{}/services/{}",
            ns.unwrap_or(""),
            l.unwrap_or(""),
            s.unwrap_or("")
        );
        assert_eq!(Some(k), resource_key("gcp:cloudrunv2:Service", &v2), "{url}");
    }
    // A BigQuery job's key never exists without a location.
    assert_eq!(
        resource_key_with_properties("gcp:bigquery:Job", url, [("project", project)]),
        resource_key("gcp:bigquery:Job", url),
        "{url}"
    );
    if let Some(k) = bq_to_resource_key(url) {
        check_key(&k);
    }
});
