use std::collections::HashSet;

use heck::ToLowerCamelCase;

use pulumi_rs_yaml_core::ast::template::TemplateDecl;

/// PCL reserved words — identifiers that cannot be used as variable names.
pub const PCL_RESERVED: &[&str] = &[
    "cwd",
    "element",
    "entries",
    "fileArchive",
    "fileAsset",
    "filebase64",
    "filebase64sha256",
    "fromBase64",
    "invoke",
    "join",
    "length",
    "lookup",
    "mimeType",
    "organization",
    "project",
    "range",
    "readDir",
    "readFile",
    "rootDirectory",
    "secret",
    "sha1",
    "split",
    "stack",
    "toBase64",
    "toJSON",
];

/// Returns true if the character is a legal HCL2 identifier start character.
fn is_legal_identifier_start(c: char) -> bool {
    c == '$' || c == '_' || c.is_alphabetic()
}

/// Returns true if the character is a legal HCL2 identifier continuation character.
fn is_legal_identifier_part(c: char) -> bool {
    is_legal_identifier_start(c) || c.is_numeric() || c == '_'
}

/// Converts a string to a legal HCL2 identifier by replacing invalid characters
/// with underscores.
pub fn make_legal_identifier(name: &str) -> String {
    if name.is_empty() {
        return "x".to_string();
    }

    let mut result = String::with_capacity(name.len());

    for (i, c) in name.chars().enumerate() {
        if is_legal_identifier_part(c) {
            if i == 0 && !is_legal_identifier_start(c) {
                result.push('_');
            }
            result.push(c);
        } else {
            result.push('_');
        }
    }

    if result.is_empty() {
        return "x".to_string();
    }

    result
}

/// Converts a string to lowerCamelCase, matching Go's `strcase.ToLowerCamel`.
pub fn to_lower_camel(name: &str) -> String {
    // heck's to_lower_camel_case treats digits as word boundaries
    // Go's strcase.ToLowerCamel also does word-boundary splitting at _ and -
    // and at case transitions
    let result = name.to_lower_camel_case();
    if result.is_empty() {
        return name.to_string();
    }
    result
}

/// Assigned names for all entities in a template, grouped by category.
pub struct AssignedNames {
    pub configuration: Vec<(String, String)>, // (yaml_name, pcl_name)
    pub outputs: Vec<(String, String)>,
    pub variables: Vec<(String, String)>,
    pub resources: Vec<(String, String)>,
    pub components: Vec<(String, String)>,
}

/// Assigns PCL-legal names to all entities in a template, resolving collisions.
///
/// Matches the Go `assignNames()` algorithm exactly:
/// 1. Pre-fill assigned set with PCL_RESERVED
/// 2. For each category (config, outputs, variables, resources), sorted alphabetically:
///    - Convert name via `to_lower_camel(make_legal_identifier(yaml_name))`
///    - If not in assigned, use it
///    - Otherwise append category suffix ("", "Var", "Resource")
///    - If still conflicting, append counter: base0, base1, ...
pub fn assign_names(template: &TemplateDecl<'_>) -> AssignedNames {
    let mut assigned: HashSet<String> = PCL_RESERVED.iter().map(|s| s.to_string()).collect();

    let mut configuration = Vec::new();
    let mut outputs = Vec::new();
    let mut variables = Vec::new();
    let mut resources = Vec::new();
    let mut components = Vec::new();

    // Config entries — no suffix
    let mut config_keys: Vec<&str> = template.config.iter().map(|c| c.key.as_ref()).collect();
    config_keys.sort();
    for key in config_keys {
        let pcl_name = assign_name(key, "", &mut assigned);
        configuration.push((key.to_string(), pcl_name));
    }

    // Outputs — no suffix
    let mut output_keys: Vec<&str> = template.outputs.iter().map(|o| o.key.as_ref()).collect();
    output_keys.sort();
    for key in output_keys {
        let pcl_name = assign_name(key, "", &mut assigned);
        outputs.push((key.to_string(), pcl_name));
    }

    // Variables — "Var" suffix
    let mut var_keys: Vec<&str> = template.variables.iter().map(|v| v.key.as_ref()).collect();
    var_keys.sort();
    for key in var_keys {
        let pcl_name = assign_name(key, "Var", &mut assigned);
        variables.push((key.to_string(), pcl_name));
    }

    // Resources — "Resource" suffix
    let mut res_keys: Vec<&str> = template
        .resources
        .iter()
        .map(|r| r.logical_name.as_ref())
        .collect();
    res_keys.sort();
    for key in res_keys {
        let pcl_name = assign_name(key, "Resource", &mut assigned);
        resources.push((key.to_string(), pcl_name));
    }

    // Components — "Component" suffix
    let mut comp_keys: Vec<&str> = template.components.iter().map(|c| c.key.as_ref()).collect();
    comp_keys.sort();
    for key in comp_keys {
        let pcl_name = assign_name(key, "Component", &mut assigned);
        components.push((key.to_string(), pcl_name));
    }

    AssignedNames {
        configuration,
        outputs,
        variables,
        resources,
        components,
    }
}

/// Assigns a unique PCL name for a YAML name, applying suffix and counter as needed.
fn assign_name(yaml_name: &str, suffix: &str, assigned: &mut HashSet<String>) -> String {
    let base = to_lower_camel(&make_legal_identifier(yaml_name));
    let base = if base.is_empty() {
        "x".to_string()
    } else {
        base
    };

    // Try the base name first
    if !assigned.contains(&base) {
        assigned.insert(base.clone());
        return base;
    }

    // Try base + suffix
    if !suffix.is_empty() {
        let with_suffix = format!("{}{}", base, suffix);
        if !assigned.contains(&with_suffix) {
            assigned.insert(with_suffix.clone());
            return with_suffix;
        }
    }

    // Append counter
    let counter_base = if suffix.is_empty() {
        base.clone()
    } else {
        format!("{}{}", base, suffix)
    };
    for i in 0.. {
        let candidate = format!("{}{}", counter_base, i);
        if !assigned.contains(&candidate) {
            assigned.insert(candidate.clone());
            return candidate;
        }
    }

    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_make_legal_identifier_simple() {
        assert_eq!(make_legal_identifier("foo"), "foo");
        assert_eq!(make_legal_identifier("_bar"), "_bar");
        assert_eq!(make_legal_identifier("$baz"), "$baz");
    }

    #[test]
    fn test_make_legal_identifier_dashes() {
        assert_eq!(make_legal_identifier("my-bucket"), "my_bucket");
        assert_eq!(make_legal_identifier("foo-bar-baz"), "foo_bar_baz");
    }

    #[test]
    fn test_make_legal_identifier_leading_digit() {
        assert_eq!(make_legal_identifier("0foo"), "_0foo");
    }

    #[test]
    fn test_make_legal_identifier_empty() {
        assert_eq!(make_legal_identifier(""), "x");
    }

    #[test]
    fn test_make_legal_identifier_special_chars() {
        assert_eq!(make_legal_identifier("a.b"), "a_b");
        assert_eq!(make_legal_identifier("a:b"), "a_b");
    }

    #[test]
    fn test_to_lower_camel_basic() {
        assert_eq!(to_lower_camel("my_var"), "myVar");
        assert_eq!(to_lower_camel("MyVar"), "myVar");
        assert_eq!(to_lower_camel("my_bucket"), "myBucket");
    }

    #[test]
    fn test_to_lower_camel_single_word() {
        assert_eq!(to_lower_camel("foo"), "foo");
        assert_eq!(to_lower_camel("Foo"), "foo");
    }

    #[test]
    fn test_to_lower_camel_already_camel() {
        assert_eq!(to_lower_camel("myVar"), "myVar");
    }

    #[test]
    fn test_assign_name_no_conflict() {
        let mut assigned = HashSet::new();
        let name = assign_name("myBucket", "", &mut assigned);
        assert_eq!(name, "myBucket");
        assert!(assigned.contains("myBucket"));
    }

    #[test]
    fn test_assign_name_conflict_with_suffix() {
        let mut assigned: HashSet<String> = ["myBucket".to_string()].into();
        let name = assign_name("myBucket", "Resource", &mut assigned);
        assert_eq!(name, "myBucketResource");
    }

    #[test]
    fn test_assign_name_conflict_counter() {
        let mut assigned: HashSet<String> =
            ["myBucket".to_string(), "myBucketResource".to_string()].into();
        let name = assign_name("myBucket", "Resource", &mut assigned);
        assert_eq!(name, "myBucketResource0");
    }

    #[test]
    fn test_assign_name_reserved_word() {
        let mut assigned: HashSet<String> = PCL_RESERVED.iter().map(|s| s.to_string()).collect();
        let name = assign_name("stack", "", &mut assigned);
        // "stack" is reserved, so it gets a counter
        assert_eq!(name, "stack0");
    }

    #[test]
    fn test_pcl_reserved_contains_expected() {
        assert!(PCL_RESERVED.contains(&"cwd"));
        assert!(PCL_RESERVED.contains(&"stack"));
        assert!(PCL_RESERVED.contains(&"project"));
        assert!(PCL_RESERVED.contains(&"invoke"));
        assert!(PCL_RESERVED.contains(&"toJSON"));
    }
}
