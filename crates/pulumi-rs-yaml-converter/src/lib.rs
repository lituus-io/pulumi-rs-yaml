pub mod importer;
pub mod names;
pub mod schema_loader;
pub mod server;

use pulumi_rs_yaml_core::ast::parse::parse_template;
use pulumi_rs_yaml_core::diag::Diagnostics;
use pulumi_rs_yaml_core::schema::SchemaStore;

use importer::Importer;

/// Result of converting YAML to PCL.
pub struct ConvertResult {
    pub pcl_text: String,
    pub diagnostics: Diagnostics,
}

/// Converts YAML source to PCL text.
pub fn yaml_to_pcl(yaml_source: &str) -> ConvertResult {
    let (template, mut diags) = parse_template(yaml_source, None);

    if diags.has_errors() {
        return ConvertResult {
            pcl_text: String::new(),
            diagnostics: diags,
        };
    }

    let mut importer = Importer::new();
    let pcl_text = importer.import_template(&template);
    diags.extend(importer.diagnostics());

    ConvertResult {
        pcl_text,
        diagnostics: diags,
    }
}

/// Converts YAML source to PCL text with schema-based token resolution.
pub fn yaml_to_pcl_with_schema(yaml_source: &str, schema_store: SchemaStore) -> ConvertResult {
    let (template, mut diags) = parse_template(yaml_source, None);

    if diags.has_errors() {
        return ConvertResult {
            pcl_text: String::new(),
            diagnostics: diags,
        };
    }

    let mut importer = Importer::with_schema(schema_store);
    let pcl_text = importer.import_template(&template);
    diags.extend(importer.diagnostics());

    ConvertResult {
        pcl_text,
        diagnostics: diags,
    }
}
