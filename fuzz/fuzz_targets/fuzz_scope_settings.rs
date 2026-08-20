// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

// The switch itself. `providerTemplatedPackages` decides whether any of this
// runs, and it has to be read textually — before the file can be parsed —
// which makes it another small parser with its own way of being wrong.
//
// Reading too little is harmless: no protection is the behaviour every stack
// already has, and unprotected dbt syntax fails loudly. Reading too *much* is
// the dangerous direction, because it silently stops rendering block scalars in
// a stack that never opted in. So the property asserted here is containment: a
// document that does not contain the key yields nothing, and a package can only
// come back if its name is actually in the file.
#![no_main]

use libfuzzer_sys::fuzz_target;
use pulumi_rs_yaml_core::provider_scope::packages_from_source;

fuzz_target!(|data: &[u8]| {
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };
    let found = packages_from_source(source);

    if !source.contains("providerTemplatedPackages") {
        assert!(
            found.is_empty(),
            "read {found:?} from a document that never names the option:\n{source:?}"
        );
    }
    for pkg in &found {
        assert!(
            source.contains(pkg.as_str()),
            "returned package {pkg:?} that does not appear in the source:\n{source:?}"
        );
        assert!(!pkg.is_empty(), "returned an empty package name");
    }
});
