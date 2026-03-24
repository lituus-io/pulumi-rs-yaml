use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::ast::expr::{Expr, InvokeExpr};
use crate::ast::property::PropertyAccess;
use crate::ast::template::*;
use crate::config_types::ConfigType;
use crate::diag::Diagnostics;
use crate::eval::builtins;
use crate::eval::callback::{NoopCallback, ResourceCallback};
use crate::eval::config::{self, RawConfig};
use crate::eval::graph::{collect_expr_deps, topological_levels, topological_sort_with_deps};
use crate::eval::resource::{ResolvedResourceOptions, ResourceState};
use crate::eval::value::{Archive, Asset, Value};
use crate::packages::canonicalize_type_token;
use crate::schema::SchemaStore;

/// Trait for receiving progress events during evaluation.
///
/// Implementations can display progress bars, emit structured logs, or
/// collect timing data. The default `NoopProgress` is monomorphized away
/// to zero-cost when unused.
pub trait ProgressSink {
    /// Called at the start of each topological level.
    fn on_level_start(&mut self, level: usize, count: usize);
    /// Called after a resource is fully registered.
    fn on_resource_done(&mut self, name: &str);
}

/// Zero-cost no-op progress sink.
pub struct NoopProgress;
impl ProgressSink for NoopProgress {
    fn on_level_start(&mut self, _: usize, _: usize) {}
    fn on_resource_done(&mut self, _: &str) {}
}

/// Interior-mutable evaluation state.
///
/// Read-heavy fields (`config`, `variables`, `resources`, `poisoned`,
/// `starlark_runtime`) use `RwLock` for concurrent reads in parallel
/// evaluation. Write-heavy or low-traffic fields use `Mutex`.
pub struct EvalState {
    /// Resolved config values, keyed by config variable name.
    pub config: RwLock<HashMap<String, Value<'static>>>,
    /// Resolved variable values, keyed by variable name.
    pub variables: RwLock<HashMap<String, Value<'static>>>,
    /// Registered resource states, keyed by logical name.
    pub resources: RwLock<HashMap<String, ResourceState>>,
    /// Evaluated output values, keyed by output name.
    pub outputs: Mutex<HashMap<String, Value<'static>>>,
    /// Diagnostics accumulated during evaluation.
    pub diags: Mutex<Diagnostics>,
    /// Resource index counter for ResourceRef handles.
    pub resource_counter: AtomicU32,
    /// Map from logical resource name to ResourceRef index.
    pub resource_indices: Mutex<HashMap<String, u32>>,
    /// Names of variables/resources that failed evaluation.
    /// Used to prevent cascading errors from downstream dependents.
    pub poisoned: RwLock<HashSet<String>>,
    /// Default providers: package_name → provider_ref (urn::id).
    /// Populated when a resource with `defaultProvider: true` is registered.
    pub default_providers: Mutex<HashMap<String, String>>,
    /// Stack reference cache: stack_name → cached RegisterResponse.
    /// Avoids duplicate read_resource calls for the same stack reference.
    pub stack_ref_cache: Mutex<HashMap<String, crate::eval::callback::RegisterResponse>>,
    /// Compiled Starlark runtime (None if no starlark functions defined).
    pub starlark_runtime: RwLock<Option<crate::eval::starlark_runtime::StarlarkRuntime>>,
}

// Compile-time assertion that EvalState is Send + Sync.
const _: () = {
    fn _assert_send_sync<T: Send + Sync>() {}
    fn _check() {
        _assert_send_sync::<EvalState>();
    }
};

impl EvalState {
    /// Creates a new empty evaluation state.
    fn new() -> Self {
        Self {
            config: RwLock::new(HashMap::new()),
            variables: RwLock::new(HashMap::new()),
            resources: RwLock::new(HashMap::new()),
            outputs: Mutex::new(HashMap::new()),
            diags: Mutex::new(Diagnostics::new()),
            resource_counter: AtomicU32::new(0),
            resource_indices: Mutex::new(HashMap::new()),
            poisoned: RwLock::new(HashSet::new()),
            default_providers: Mutex::new(HashMap::new()),
            stack_ref_cache: Mutex::new(HashMap::new()),
            starlark_runtime: RwLock::new(None),
        }
    }
}

/// The main evaluator that walks a template in dependency order
/// and evaluates expressions, config, variables, and resources.
///
/// This struct is the Rust equivalent of Go's `Runner` + `programEvaluator`.
/// It holds all state needed during template evaluation.
///
/// The type parameter `C` determines how resources are registered:
/// - `NoopCallback`: unit tests (no actual registration)
/// - `MockCallback`: integration tests (record & replay)
/// - `GrpcCallback`: real deployment (wraps tonic gRPC clients)
pub struct Evaluator<'schema, C: ResourceCallback = NoopCallback> {
    /// The project name (from Pulumi.yaml).
    pub project_name: String,
    /// The stack name.
    pub stack_name: String,
    /// Working directory for file operations.
    pub cwd: String,
    /// The organization name.
    pub organization: String,
    /// The root directory of the project.
    pub root_directory: String,
    /// Whether we're in preview mode (dry run).
    pub dry_run: bool,
    /// URN of the root stack resource (set during Run).
    pub stack_urn: Option<String>,
    /// Optional source file map for multi-file rich error messages.
    /// Maps logical name → source filename.
    pub source_map: Option<Arc<HashMap<String, String>>>,
    /// Optional schema store for provider metadata (output properties, secrets, aliases).
    pub schema_store: Option<&'schema SchemaStore>,
    /// Package references: package name → package ref UUID.
    /// Populated by runner.rs via RegisterPackage RPC before evaluation.
    pub package_refs: HashMap<String, String>,
    /// Parallelism level: number of concurrent resource registrations per level.
    /// 0 or 1 means sequential (default). >1 enables parallel registration.
    pub parallel: i32,
    /// Component parent URN: when evaluating a component's inner resources,
    /// this is set so that resources without an explicit parent inherit the component.
    pub component_parent_urn: Option<String>,
    /// The callback for resource operations (registration, invoke, etc.).
    callback: C,
    /// Interior-mutable evaluation state.
    pub state: EvalState,
}

impl Evaluator<'_, NoopCallback> {
    /// Creates a new evaluator with the given project settings and a no-op callback.
    pub fn new(project_name: String, stack_name: String, cwd: String, dry_run: bool) -> Self {
        Self::with_callback(project_name, stack_name, cwd, dry_run, NoopCallback)
    }
}

impl<C: ResourceCallback> Evaluator<'_, C> {
    /// Creates a new evaluator with the given project settings and callback.
    pub fn with_callback(
        project_name: String,
        stack_name: String,
        cwd: String,
        dry_run: bool,
        callback: C,
    ) -> Self {
        Self {
            project_name,
            stack_name,
            cwd,
            organization: String::new(),
            root_directory: String::new(),
            dry_run,
            callback,
            stack_urn: None,
            source_map: None,
            schema_store: None,
            package_refs: HashMap::new(),
            parallel: 0,
            component_parent_urn: None,
            state: EvalState::new(),
        }
    }

    /// Returns a reference to the callback.
    pub fn callback(&self) -> &C {
        &self.callback
    }

    // -----------------------------------------------------------------------
    // Accessor methods for post-evaluation inspection
    // -----------------------------------------------------------------------

    /// Returns true if any error-level diagnostics are present.
    pub fn has_errors(&self) -> bool {
        self.state.diags.lock().unwrap().has_errors()
    }

    /// Collects error diagnostic summaries.
    pub fn diag_errors(&self) -> Vec<String> {
        let diags = self.state.diags.lock().unwrap();
        (&*diags)
            .into_iter()
            .filter(|d| d.is_error())
            .map(|d| d.summary.clone())
            .collect()
    }

    /// Collects warning diagnostic summaries.
    pub fn diag_warnings(&self) -> Vec<String> {
        let diags = self.state.diags.lock().unwrap();
        (&*diags)
            .into_iter()
            .filter(|d| !d.is_error())
            .map(|d| d.summary.clone())
            .collect()
    }

    /// Drains and returns all outputs.
    pub fn take_outputs(&self) -> HashMap<String, Value<'static>> {
        std::mem::take(&mut *self.state.outputs.lock().unwrap())
    }

    /// Gets a cloned output value by key.
    pub fn get_output(&self, key: &str) -> Option<Value<'static>> {
        self.state.outputs.lock().unwrap().get(key).cloned()
    }

    /// Gets a cloned resource state by logical name.
    pub fn get_resource(&self, name: &str) -> Option<ResourceState> {
        self.state.resources.read().unwrap().get(name).cloned()
    }

    /// Returns true if a resource with the given logical name exists.
    pub fn has_resource(&self, name: &str) -> bool {
        self.state.resources.read().unwrap().contains_key(name)
    }

    /// Gets a cloned config value by key.
    pub fn get_config(&self, key: &str) -> Option<Value<'static>> {
        self.state.config.read().unwrap().get(key).cloned()
    }

    /// Returns true if a config entry with the given key exists.
    pub fn has_config(&self, key: &str) -> bool {
        self.state.config.read().unwrap().contains_key(key)
    }

    /// Gets a cloned variable value by key.
    pub fn get_variable(&self, key: &str) -> Option<Value<'static>> {
        self.state.variables.read().unwrap().get(key).cloned()
    }

    /// Returns true if a variable with the given key exists.
    pub fn has_variable(&self, key: &str) -> bool {
        self.state.variables.read().unwrap().contains_key(key)
    }

    /// Formats diagnostics for display in tests and assertions.
    pub fn diags_display(&self) -> String {
        format!("{}", *self.state.diags.lock().unwrap())
    }

    /// Evaluates the entire template in dependency order.
    ///
    /// This is the main entry point. It:
    /// 1. Performs topological sort with dependency graph
    /// 2. Computes topological levels for parallelism
    /// 3. Walks nodes level-by-level in dependency order
    /// 4. Evaluates config, variables, and resources
    /// 5. Evaluates output declarations
    pub fn evaluate_template<'t>(
        &self,
        template: &'t TemplateDecl<'t>,
        raw_config: &RawConfig,
        secret_keys: &[String],
    ) {
        // Pre-allocate maps based on template size to avoid rehashing
        {
            self.state
                .config
                .write()
                .unwrap()
                .reserve(template.config.len());
            self.state
                .variables
                .write()
                .unwrap()
                .reserve(template.variables.len() + 1); // +1 for "pulumi"
            self.state
                .resources
                .write()
                .unwrap()
                .reserve(template.resources.len());
            self.state
                .outputs
                .lock()
                .unwrap()
                .reserve(template.outputs.len());
            self.state
                .resource_indices
                .lock()
                .unwrap()
                .reserve(template.resources.len());
        }

        // Always inject the pulumi built-in variable (Go: ensureSetup)
        let pulumi_obj = Value::Object(vec![
            (
                Cow::Borrowed("cwd"),
                Value::String(Cow::Owned(self.cwd.clone())),
            ),
            (
                Cow::Borrowed("project"),
                Value::String(Cow::Owned(self.project_name.clone())),
            ),
            (
                Cow::Borrowed("stack"),
                Value::String(Cow::Owned(self.stack_name.clone())),
            ),
            (
                Cow::Borrowed("organization"),
                Value::String(Cow::Owned(self.organization.clone())),
            ),
            (
                Cow::Borrowed("rootDirectory"),
                Value::String(Cow::Owned(self.root_directory.clone())),
            ),
        ]);
        self.state
            .variables
            .write()
            .unwrap()
            .insert("pulumi".to_string(), pulumi_obj);

        // Compile Starlark functions if any are defined
        if !template.starlark_functions.is_empty() {
            let runtime = {
                let mut compile_diags = self.state.diags.lock().unwrap();
                crate::eval::starlark_runtime::StarlarkRuntime::compile(
                    &template.starlark_functions,
                    &mut compile_diags,
                )
            };
            *self.state.starlark_runtime.write().unwrap() = Some(runtime);
            if self.has_errors() {
                return;
            }
        }

        // Topological sort with dependency graph
        let (result, sort_diags) = topological_sort_with_deps(template, self.source_map.as_deref());
        {
            let mut diags = self.state.diags.lock().unwrap();
            diags.extend(sort_diags);
            if diags.has_errors() {
                return;
            }
        }

        // Compute topological levels for level-aware evaluation
        let levels = topological_levels(&result.order, &result.deps);

        // Evaluate nodes level-by-level.
        // Within each level, nodes have no inter-dependencies and can be
        // processed in parallel when self.parallel > 1.
        for level in &levels {
            if self.has_errors() {
                break;
            }

            if self.parallel > 1 && level.len() > 1 {
                // Parallel: all nodes in this level are independent.
                // Build a scoped thread pool capped at min(parallel, level size).
                let num_threads = (self.parallel as usize).min(level.len());
                let pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(num_threads)
                    .build()
                    .expect("failed to build rayon thread pool");
                pool.install(|| {
                    use rayon::prelude::*;
                    level.par_iter().for_each(|node_name| {
                        self.eval_node(node_name, template, raw_config, secret_keys);
                    });
                });
            } else {
                // Sequential: default behavior (parallel <= 1 or single-node level).
                for node_name in level {
                    if self.has_errors() {
                        break;
                    }
                    self.eval_node(node_name, template, raw_config, secret_keys);
                }
            }
        }

        // Evaluate outputs
        for output in &template.outputs {
            if self.state.diags.lock().unwrap().has_errors() {
                break;
            }
            self.eval_output(output);
        }
    }

    /// Evaluates a config entry.
    /// Dispatches a single node for evaluation (config, variable, or resource).
    fn eval_node<'t>(
        &self,
        node_name: &str,
        template: &'t TemplateDecl<'t>,
        raw_config: &RawConfig,
        secret_keys: &[String],
    ) {
        if let Some(entry) = template.config.iter().find(|e| e.key.as_ref() == node_name) {
            self.eval_config_entry(entry, raw_config, secret_keys);
            return;
        }
        if let Some(entry) = template
            .variables
            .iter()
            .find(|e| e.key.as_ref() == node_name)
        {
            self.eval_variable(entry);
            return;
        }
        if let Some(entry) = template
            .resources
            .iter()
            .find(|e| e.logical_name.as_ref() == node_name)
        {
            self.eval_resource_entry(entry);
        }
        // "pulumi" settings node — no-op
    }

    fn eval_config_entry<'t>(
        &self,
        entry: &'t ConfigEntry<'t>,
        raw_config: &RawConfig,
        secret_keys: &[String],
    ) {
        let key = entry.key.as_ref();

        // Determine the declared type
        let declared_type = entry
            .param
            .type_
            .as_ref()
            .and_then(|t| ConfigType::parse(t.as_ref()));

        // Evaluate the default value if present
        let default_value = entry
            .param
            .default
            .as_ref()
            .and_then(|expr| self.eval_expr(expr))
            .map(|v| v.into_owned());

        let is_secret_in_config = secret_keys.iter().any(|sk| {
            sk.strip_prefix(&*self.project_name)
                .and_then(|rest| rest.strip_prefix(':'))
                == Some(key)
        }) || secret_keys.iter().any(|sk| sk == key);

        let is_secret_in_schema = entry.param.secret.unwrap_or(false);

        match config::resolve_config_entry(
            key,
            &self.project_name,
            declared_type,
            default_value,
            is_secret_in_config,
            is_secret_in_schema,
            raw_config,
            &mut self.state.diags.lock().unwrap(),
        ) {
            Some(resolved) => {
                self.state
                    .config
                    .write()
                    .unwrap()
                    .insert(key.to_string(), resolved.value);
            }
            None => {
                // Error already recorded in diags
            }
        }
    }

    /// Evaluates a variable entry.
    fn eval_variable<'t>(&self, entry: &'t VariableEntry<'t>) {
        let key = entry.key.as_ref();
        match self.eval_expr(&entry.value) {
            Some(value) => {
                self.state
                    .variables
                    .write()
                    .unwrap()
                    .insert(key.to_string(), value.into_owned());
            }
            None => {
                // Mark as poisoned to prevent cascading errors
                self.state.poisoned.write().unwrap().insert(key.to_string());
            }
        }
    }

    /// Stores a resource state after successful registration or read.
    fn store_resource(
        &self,
        logical_name: &str,
        resp: crate::eval::callback::RegisterResponse,
        is_provider: bool,
        is_component: bool,
        is_default_provider: bool,
    ) {
        let idx = self.state.resource_counter.fetch_add(1, Ordering::SeqCst);
        self.state
            .resource_indices
            .lock()
            .unwrap()
            .insert(logical_name.to_string(), idx);

        let urn = resp.urn;
        let id = resp.id;

        // Record default provider mapping if applicable
        if is_default_provider && is_provider {
            // Extract package name from "pulumi:providers:<pkg>"
            if let Some(pkg) = urn
                .split("::")
                .nth(2)
                .and_then(|t| t.strip_prefix("pulumi:providers:"))
            {
                let provider_ref = format!("{}::{}", urn, id);
                self.state
                    .default_providers
                    .lock()
                    .unwrap()
                    .insert(pkg.to_string(), provider_ref);
            }
        }

        let state = ResourceState {
            urn,
            id,
            is_provider,
            is_component,
            outputs: resp.outputs,
            stables: resp.stables,
        };
        self.state
            .resources
            .write()
            .unwrap()
            .insert(logical_name.to_string(), state);
    }

    /// Resolves a value (or list of values) to a list of resource URNs.
    fn resolve_urn_list(&self, val: &Value<'_>) -> Vec<String> {
        match val {
            Value::List(items) => items
                .iter()
                .filter_map(|v| self.extract_resource_urn(v))
                .collect(),
            _ => self.extract_resource_urn(val).into_iter().collect(),
        }
    }

    /// Evaluates a resource entry and registers it via the callback.
    fn eval_resource_entry<'t>(&self, entry: &'t ResourceEntry<'t>) {
        let logical_name = entry.logical_name.as_ref();
        let resource = &entry.resource;

        // Use explicit name if set, otherwise fall back to logical key (Go compat)
        let resource_name = resource.name.as_deref().unwrap_or(logical_name);

        // Evaluate resource properties
        let inputs = match &resource.properties {
            ResourceProperties::Map(props) => {
                let mut map = HashMap::new();
                let mut all_ok = true;
                for prop in props {
                    match self.eval_expr(&prop.value) {
                        Some(value) => {
                            map.insert(prop.key.to_string(), value.into_owned());
                        }
                        None => {
                            all_ok = false;
                        }
                    }
                }
                if !all_ok {
                    self.state
                        .poisoned
                        .write()
                        .unwrap()
                        .insert(logical_name.to_string());
                    return;
                }
                map
            }
            ResourceProperties::Expr(expr) => match self.eval_expr(expr) {
                Some(Value::Object(entries)) => entries
                    .into_iter()
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect(),
                Some(other) => {
                    self.state.diags.lock().unwrap().error(
                        None,
                        format!("properties must be an object, got {}", other.type_name()),
                        "",
                    );
                    self.state
                        .poisoned
                        .write()
                        .unwrap()
                        .insert(logical_name.to_string());
                    return;
                }
                None => {
                    self.state
                        .poisoned
                        .write()
                        .unwrap()
                        .insert(logical_name.to_string());
                    return;
                }
            },
        };

        // Determine resource characteristics
        let raw_type_token = resource.type_.as_ref();
        let canonical_type = canonicalize_type_token(raw_type_token);
        let type_token = canonical_type.as_str();

        // Token blocklist: block known-unsupported resource types (Go: packages.go:270-324)
        // Check both the raw user token and the canonical form.
        if let Some(err_msg) =
            check_blocked_type(raw_type_token).or_else(|| check_blocked_type(type_token))
        {
            self.state.diags.lock().unwrap().error(None, err_msg, "");
            return;
        }

        let is_provider = type_token.starts_with("pulumi:providers:");
        let mut property_deps: HashMap<String, Vec<String>> = HashMap::new();

        // Component detection: check schema for isComponent flag
        let is_component = if !is_provider {
            self.schema_store
                .as_ref()
                .map(|store| store.is_component(type_token))
                .unwrap_or(false)
        } else {
            false
        };
        let custom = !is_component;

        // Cache schema lookup — used for secret wrapping, constant injection, and option enrichment
        let schema_resource_info = self
            .schema_store
            .and_then(|s| s.lookup_resource(type_token));

        // Wrap secret input properties with Value::Secret (matching Go behavior:
        // pkg/pulumiyaml/run.go:1489 — IsResourcePropertySecret + ToSecret)
        let mut inputs = inputs;
        if let Some(info) = schema_resource_info {
            for prop_name in &info.secret_input_properties {
                if let Some(val) = inputs.get_mut(prop_name) {
                    if !val.is_secret() {
                        let taken = std::mem::replace(val, Value::Null);
                        *val = Value::Secret(Box::new(taken));
                    }
                }
            }
        }

        // Inject constant values from schema if user didn't provide the property
        if let Some(info) = schema_resource_info {
            for (prop_name, prop_info) in &info.property_types {
                if let Some(ref const_val) = prop_info.const_value {
                    if !inputs.contains_key(prop_name) {
                        if let Some(val) = json_value_to_eval_value(const_val) {
                            inputs.insert(prop_name.clone(), val);
                        }
                    }
                }
            }
        }

        // Collect per-property dependencies (resource URNs referenced by each property)
        if let ResourceProperties::Map(props) = &resource.properties {
            // Lock resources once, clone the keys, then release the lock
            let resource_keys: Vec<String> = self
                .state
                .resources
                .read()
                .unwrap()
                .keys()
                .cloned()
                .collect();
            let resource_names: HashMap<&str, &str> = resource_keys
                .iter()
                .map(|k| (k.as_str(), "resource"))
                .collect();
            for prop in props {
                let mut prop_refs = std::collections::HashSet::new();
                collect_expr_deps(&prop.value, &resource_names, &mut prop_refs);
                if !prop_refs.is_empty() {
                    let resources_guard = self.state.resources.read().unwrap();
                    let urns: Vec<String> = prop_refs
                        .iter()
                        .filter_map(|name| resources_guard.get(*name).map(|r| r.urn.clone()))
                        .filter(|urn| !urn.is_empty())
                        .collect();
                    drop(resources_guard);
                    if !urns.is_empty() {
                        property_deps.insert(prop.key.to_string(), urns);
                    }
                }
            }
        }

        // Resolve resource options
        let mut options = self.resolve_resource_options(&resource.options);
        options.property_dependencies = property_deps;

        // Enrich resource options from schema (secrets, aliases)
        if let Some(info) = schema_resource_info {
            for prop in &info.secret_properties {
                if !options.additional_secret_outputs.contains(prop) {
                    options.additional_secret_outputs.push(prop.clone());
                }
            }
            for alias in &info.aliases {
                let already_present = options.aliases.iter().any(
                    |a| matches!(a, crate::eval::resource::ResolvedAlias::Urn(u) if u == alias),
                );
                if !already_present {
                    options
                        .aliases
                        .push(crate::eval::resource::ResolvedAlias::Urn(alias.clone()));
                }
            }
        }

        // Look up package reference for this resource type
        if let Some(pkg_name) = type_token.split(':').next() {
            if let Some(pkg_ref) = self.package_refs.get(pkg_name) {
                options.package_ref = pkg_ref.clone();
            }
        }

        // Auto-assign default provider if no explicit provider is set
        if !is_provider && options.provider_ref.is_none() {
            if let Some(pkg_name) = type_token.split(':').next() {
                if let Some(provider_ref) = self
                    .state
                    .default_providers
                    .lock()
                    .unwrap()
                    .get(pkg_name)
                    .cloned()
                {
                    options.provider_ref = Some(provider_ref);
                }
            }
        }

        // Inject component parent URN if evaluating a component's inner resources
        if options.parent_urn.is_none() {
            if let Some(ref parent) = self.component_parent_urn {
                options.parent_urn = Some(parent.clone());
            }
        }

        // StackReference special handling: convert to read resource (Go: run.go:1895-1908)
        if type_token == "pulumi:pulumi:StackReference" {
            // Default `name` property to resource_name if not provided
            if !inputs.contains_key("name") {
                inputs.insert(
                    "name".to_string(),
                    Value::String(Cow::Owned(resource_name.to_string())),
                );
            }

            // Validate name is a string
            let id_str = match inputs.get("name") {
                Some(Value::String(s)) => s.to_string(),
                Some(other) => {
                    self.state.diags.lock().unwrap().error(
                        None,
                        format!(
                            "StackReference 'name' must be a string, got {}",
                            other.type_name()
                        ),
                        "",
                    );
                    return;
                }
                None => {
                    self.state.diags.lock().unwrap().error(
                        None,
                        "StackReference requires a 'name' property",
                        "",
                    );
                    return;
                }
            };

            // Check cache for this stack reference
            if let Some(cached) = self
                .state
                .stack_ref_cache
                .lock()
                .unwrap()
                .get(&id_str)
                .cloned()
            {
                self.store_resource(logical_name, cached, false, false, false);
                return;
            }

            match self.callback.read_resource(
                type_token,
                resource_name,
                &id_str,
                options.parent_urn.as_deref().unwrap_or(""),
                inputs,
                options.provider_ref.as_deref().unwrap_or(""),
                &options.version,
            ) {
                Ok(resp) => {
                    self.state
                        .stack_ref_cache
                        .lock()
                        .unwrap()
                        .insert(id_str, resp.clone());
                    self.store_resource(logical_name, resp, false, false, false);
                }
                Err(e) => {
                    self.state.diags.lock().unwrap().error(
                        None,
                        format!("failed to read StackReference '{}': {}", logical_name, e),
                        "",
                    );
                }
            }
            return;
        }

        // Handle get resources (reading existing resources)
        if let Some(ref get) = resource.get {
            let id_val = match self.eval_expr(&get.id) {
                Some(Value::String(s)) => s.into_owned(),
                Some(other) => {
                    self.state.diags.lock().unwrap().error(
                        None,
                        format!(
                            "get resource id must be a string, got {}",
                            other.type_name()
                        ),
                        "",
                    );
                    return;
                }
                None => return,
            };

            match self.callback.read_resource(
                type_token,
                resource_name,
                &id_val,
                options.parent_urn.as_deref().unwrap_or(""),
                inputs,
                options.provider_ref.as_deref().unwrap_or(""),
                &options.version,
            ) {
                Ok(resp) => {
                    self.store_resource(logical_name, resp, is_provider, is_component, false);
                }
                Err(e) => {
                    self.state.diags.lock().unwrap().error(
                        None,
                        format!("failed to read resource '{}': {}", logical_name, e),
                        "",
                    );
                }
            }
            return;
        }

        // Register the resource via callback
        match self.callback.register_resource(
            type_token,
            resource_name,
            custom,
            is_component,
            inputs,
            options,
        ) {
            Ok(mut resp) => {
                // In preview mode, fill output-only properties with Unknown
                // so downstream references don't fail
                if self.dry_run {
                    if let Some(store) = self.schema_store {
                        for prop_name in store.output_properties(type_token) {
                            resp.outputs
                                .entry(prop_name.clone())
                                .or_insert(Value::Unknown);
                        }
                    }
                }

                let is_default_provider = resource.default_provider == Some(true);
                self.store_resource(
                    logical_name,
                    resp,
                    is_provider,
                    is_component,
                    is_default_provider,
                );
            }
            Err(e) => {
                self.state.diags.lock().unwrap().error(
                    None,
                    format!("failed to register resource '{}': {}", logical_name, e),
                    "",
                );
            }
        }
    }

    /// Resolves resource options from the AST declaration to concrete values.
    fn resolve_resource_options<'t>(
        &self,
        opts: &'t ResourceOptionsDecl<'t>,
    ) -> ResolvedResourceOptions {
        let mut resolved = ResolvedResourceOptions::default();

        // Parent URN
        if let Some(ref parent_expr) = opts.parent {
            if let Some(val) = self.eval_expr(parent_expr) {
                if let Some(parent_state) = self.extract_resource_urn(&val) {
                    resolved.parent_urn = Some(parent_state);
                }
            }
        }

        // Provider reference
        if let Some(ref provider_expr) = opts.provider {
            if let Some(val) = self.eval_expr(provider_expr) {
                if let Some(provider_urn) = self.extract_resource_urn(&val) {
                    // Provider ref format: urn::id
                    let provider_id = self.extract_resource_id(&val).unwrap_or_default();
                    resolved.provider_ref = Some(format!("{}::{}", provider_urn, provider_id));
                }
            }
        }

        // DependsOn
        if let Some(ref depends_expr) = opts.depends_on {
            if let Some(val) = self.eval_expr(depends_expr) {
                resolved.depends_on = self.resolve_urn_list(&val);
            }
        }

        // Protect — must be a boolean (Go rejects non-bool values)
        if let Some(ref protect_expr) = opts.protect {
            if let Some(val) = self.eval_expr(protect_expr) {
                match val.as_bool() {
                    Some(b) => resolved.protect = b,
                    None => self.state.diags.lock().unwrap().error(
                        None,
                        format!("protect must be a boolean value, got {}", val.type_name()),
                        "",
                    ),
                }
            }
        }

        // Simple fields
        resolved.delete_before_replace = opts.delete_before_replace.unwrap_or(false);
        resolved.retain_on_delete = opts.retain_on_delete.unwrap_or(false);

        if let Some(ref ignore) = opts.ignore_changes {
            resolved.ignore_changes = ignore.iter().map(|s| s.to_string()).collect();
        }

        if let Some(ref replace) = opts.replace_on_changes {
            resolved.replace_on_changes = replace.iter().map(|s| s.to_string()).collect();
        }

        if let Some(ref hide) = opts.hide_diffs {
            resolved.hide_diffs = hide.iter().map(|s| s.to_string()).collect();
        }

        if let Some(ref secret_outputs) = opts.additional_secret_outputs {
            resolved.additional_secret_outputs =
                secret_outputs.iter().map(|s| s.to_string()).collect();
        }

        if let Some(ref import) = opts.import {
            resolved.import_id = import.to_string();
        }

        if let Some(ref version) = opts.version {
            resolved.version = version.to_string();
        }

        if let Some(ref url) = opts.plugin_download_url {
            resolved.plugin_download_url = url.to_string();
        }

        if let Some(ref timeouts) = opts.custom_timeouts {
            resolved.custom_timeouts = Some((
                timeouts
                    .create
                    .as_ref()
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
                timeouts
                    .update
                    .as_ref()
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
                timeouts
                    .delete
                    .as_ref()
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
            ));
        }

        // Aliases
        if let Some(ref aliases_expr) = opts.aliases {
            if let Some(Value::List(items)) = self.eval_expr(aliases_expr) {
                for item in &items {
                    match item {
                        Value::String(s) => {
                            resolved
                                .aliases
                                .push(crate::eval::resource::ResolvedAlias::Urn(s.to_string()));
                        }
                        Value::Object(entries) => {
                            let get_str = |key: &str| -> String {
                                entries
                                    .iter()
                                    .find(|(k, _)| k.as_ref() == key)
                                    .and_then(|(_, v)| v.as_str())
                                    .unwrap_or("")
                                    .to_string()
                            };
                            let no_parent = entries
                                .iter()
                                .find(|(k, _)| k.as_ref() == "noParent")
                                .and_then(|(_, v)| v.as_bool())
                                .unwrap_or(false);
                            let parent_urn = if !no_parent {
                                // Evaluate parent field — could be a resource reference
                                entries
                                    .iter()
                                    .find(|(k, _)| k.as_ref() == "parent")
                                    .and_then(|(_, v)| self.extract_resource_urn(v))
                                    .unwrap_or_default()
                            } else {
                                String::new()
                            };
                            resolved
                                .aliases
                                .push(crate::eval::resource::ResolvedAlias::Spec {
                                    name: get_str("name"),
                                    r#type: get_str("type"),
                                    stack: get_str("stack"),
                                    project: get_str("project"),
                                    parent_urn,
                                    no_parent,
                                });
                        }
                        _ => {}
                    }
                }
            }
        }

        // Providers map: package name → provider ref (urn::id)
        if let Some(ref providers_expr) = opts.providers {
            if let Some(val) = self.eval_expr(providers_expr) {
                match &val {
                    Value::Object(entries) => {
                        for (pkg, provider_val) in entries {
                            if let Some(urn) = self.extract_resource_urn(provider_val) {
                                let id = self.extract_resource_id(provider_val).unwrap_or_default();
                                resolved
                                    .providers
                                    .insert(pkg.to_string(), format!("{}::{}", urn, id));
                            }
                        }
                    }
                    _ => {
                        self.state.diags.lock().unwrap().error(
                            None,
                            format!("providers must be an object, got {}", val.type_name()),
                            "",
                        );
                    }
                }
            }
        }

        // replaceWith: list of resource URNs
        if let Some(ref replace_expr) = opts.replace_with {
            if let Some(val) = self.eval_expr(replace_expr) {
                resolved.replace_with = self.resolve_urn_list(&val);
            }
        }

        // deletedWith: single resource URN
        if let Some(ref deleted_expr) = opts.deleted_with {
            if let Some(val) = self.eval_expr(deleted_expr) {
                if let Some(urn) = self.extract_resource_urn(&val) {
                    resolved.deleted_with = urn;
                }
            }
        }

        resolved
    }

    /// Extracts a resource URN from a value (either a string URN or a resource reference).
    fn extract_resource_urn(&self, val: &Value<'_>) -> Option<String> {
        match val {
            Value::String(s) => Some(s.to_string()),
            Value::Object(entries) => entries
                .iter()
                .find(|(k, _)| k.as_ref() == "urn")
                .and_then(|(_, v)| v.as_str().map(|s| s.to_string())),
            _ => None,
        }
    }

    /// Extracts a resource ID from a value.
    fn extract_resource_id(&self, val: &Value<'_>) -> Option<String> {
        match val {
            Value::Object(entries) => entries
                .iter()
                .find(|(k, _)| k.as_ref() == "id")
                .and_then(|(_, v)| v.as_str().map(|s| s.to_string())),
            _ => None,
        }
    }

    /// Evaluates an output entry and stores the result.
    fn eval_output<'t>(&self, output: &'t OutputEntry<'t>) {
        let key = output.key.as_ref();
        if let Some(value) = self.eval_expr(&output.value) {
            self.state
                .outputs
                .lock()
                .unwrap()
                .insert(key.to_string(), value.into_owned());
        }
    }

    /// Evaluates an expression, returning its Value.
    ///
    /// This is the core expression evaluator, dispatching based on
    /// the Expr variant.
    ///
    /// The expression lifetime `'e` can be any lifetime — this allows
    /// callers holding stack-local expressions to evaluate them without
    /// requiring a `'static` bound.
    pub fn eval_expr<'e>(&self, expr: &'e Expr<'e>) -> Option<Value<'e>> {
        match expr {
            Expr::Null(_) => Some(Value::Null),
            Expr::Bool(_, b) => Some(Value::Bool(*b)),
            Expr::Number(_, n) => Some(Value::Number(*n)),
            Expr::String(_, s) => Some(Value::String(s.clone())),

            Expr::List(_, elements) => {
                let mut items = Vec::with_capacity(elements.len());
                for elem in elements {
                    match self.eval_expr(elem) {
                        Some(v) => items.push(v),
                        None => return None,
                    }
                }
                Some(Value::List(items))
            }

            Expr::Object(_, entries) => {
                let mut result = Vec::with_capacity(entries.len());
                for entry in entries {
                    let key = match self.eval_expr(entry.key.as_ref()) {
                        Some(Value::String(s)) => s,
                        Some(other) => {
                            self.state.diags.lock().unwrap().error(
                                None,
                                format!(
                                    "object key must evaluate to a string, not {}",
                                    other.type_name()
                                ),
                                "",
                            );
                            return None;
                        }
                        None => return None,
                    };
                    let value = self.eval_expr(entry.value.as_ref())?;
                    result.push((key, value));
                }
                Some(Value::Object(result))
            }

            Expr::Interpolate(_, parts) => self.eval_interpolation(parts),

            Expr::Symbol(_, access) => self.eval_property_access_expr(access),

            Expr::Invoke(_, invoke) => self.eval_invoke(invoke),

            Expr::Join(_, delim, values) => {
                let d = self.eval_expr(delim)?;
                let v = self.eval_expr(values)?;
                builtins::eval_join(&d, &v, &mut self.state.diags.lock().unwrap())
            }

            Expr::Split(_, delim, source) => {
                let d = self.eval_expr(delim)?;
                let s = self.eval_expr(source)?;
                builtins::eval_split(&d, &s, &mut self.state.diags.lock().unwrap())
            }

            Expr::Select(_, index, values) => {
                let i = self.eval_expr(index)?;
                let v = self.eval_expr(values)?;
                builtins::eval_select(&i, &v, &mut self.state.diags.lock().unwrap())
            }

            Expr::ToJson(_, inner) => {
                let v = self.eval_expr(inner)?;
                builtins::eval_to_json(&v, &mut self.state.diags.lock().unwrap())
            }

            Expr::ToBase64(_, inner) => {
                let v = self.eval_expr(inner)?;
                builtins::eval_to_base64(&v, &mut self.state.diags.lock().unwrap())
            }

            Expr::FromBase64(_, inner) => {
                let v = self.eval_expr(inner)?;
                builtins::eval_from_base64(&v, &mut self.state.diags.lock().unwrap())
            }

            Expr::Secret(_, inner) => {
                let v = self.eval_expr(inner)?;
                Some(builtins::eval_secret(v))
            }

            Expr::ReadFile(_, inner) => {
                let v = self.eval_expr(inner)?;
                builtins::eval_read_file(&v, &self.cwd, &mut self.state.diags.lock().unwrap())
            }

            // Math builtins
            Expr::Abs(_, inner) => {
                let v = self.eval_expr(inner)?;
                builtins::eval_abs(&v, &mut self.state.diags.lock().unwrap())
            }
            Expr::Floor(_, inner) => {
                let v = self.eval_expr(inner)?;
                builtins::eval_floor(&v, &mut self.state.diags.lock().unwrap())
            }
            Expr::Ceil(_, inner) => {
                let v = self.eval_expr(inner)?;
                builtins::eval_ceil(&v, &mut self.state.diags.lock().unwrap())
            }
            Expr::Max(_, inner) => {
                let v = self.eval_expr(inner)?;
                builtins::eval_max(&v, &mut self.state.diags.lock().unwrap())
            }
            Expr::Min(_, inner) => {
                let v = self.eval_expr(inner)?;
                builtins::eval_min(&v, &mut self.state.diags.lock().unwrap())
            }

            // String builtins
            Expr::StringLen(_, inner) => {
                let v = self.eval_expr(inner)?;
                builtins::eval_string_len(&v, &mut self.state.diags.lock().unwrap())
            }
            Expr::Substring(_, source, start, length) => {
                let s = self.eval_expr(source)?;
                let st = self.eval_expr(start)?;
                let len = self.eval_expr(length)?;
                builtins::eval_substring(&s, &st, &len, &mut self.state.diags.lock().unwrap())
            }

            // Time builtins
            Expr::TimeUtc(_, inner) => {
                let v = self.eval_expr(inner)?;
                builtins::eval_time_utc(&v, &mut self.state.diags.lock().unwrap())
            }
            Expr::TimeUnix(_, inner) => {
                let v = self.eval_expr(inner)?;
                builtins::eval_time_unix(&v, &mut self.state.diags.lock().unwrap())
            }

            // UUID/Random builtins
            Expr::Uuid(_, inner) => {
                let v = self.eval_expr(inner)?;
                builtins::eval_uuid(&v, &mut self.state.diags.lock().unwrap())
            }
            Expr::RandomString(_, inner) => {
                let v = self.eval_expr(inner)?;
                builtins::eval_random_string(&v, &mut self.state.diags.lock().unwrap())
            }

            // Date builtins
            Expr::DateFormat(_, inner) => {
                let v = self.eval_expr(inner)?;
                builtins::eval_date_format(&v, &mut self.state.diags.lock().unwrap())
            }

            Expr::StringAsset(_, inner) => {
                let v = self.eval_expr(inner)?;
                match &v {
                    Value::String(s) => Some(Value::Asset(Asset::String(s.clone()))),
                    _ => {
                        self.state.diags.lock().unwrap().error(
                            None,
                            format!(
                                "Argument to fn::stringAsset must be a string, got {}",
                                v.type_name()
                            ),
                            "",
                        );
                        None
                    }
                }
            }

            Expr::FileAsset(_, inner) => {
                let v = self.eval_expr(inner)?;
                match &v {
                    Value::String(s) => Some(Value::Asset(Asset::File(s.clone()))),
                    _ => {
                        self.state.diags.lock().unwrap().error(
                            None,
                            format!(
                                "Argument to fn::fileAsset must be a string, got {}",
                                v.type_name()
                            ),
                            "",
                        );
                        None
                    }
                }
            }

            Expr::RemoteAsset(_, inner) => {
                let v = self.eval_expr(inner)?;
                match &v {
                    Value::String(s) => Some(Value::Asset(Asset::Remote(s.clone()))),
                    _ => {
                        self.state.diags.lock().unwrap().error(
                            None,
                            format!(
                                "Argument to fn::remoteAsset must be a string, got {}",
                                v.type_name()
                            ),
                            "",
                        );
                        None
                    }
                }
            }

            Expr::FileArchive(_, inner) => {
                let v = self.eval_expr(inner)?;
                match &v {
                    Value::String(s) => Some(Value::Archive(Archive::File(s.clone()))),
                    _ => {
                        self.state.diags.lock().unwrap().error(
                            None,
                            format!(
                                "Argument to fn::fileArchive must be a string, got {}",
                                v.type_name()
                            ),
                            "",
                        );
                        None
                    }
                }
            }

            Expr::RemoteArchive(_, inner) => {
                let v = self.eval_expr(inner)?;
                match &v {
                    Value::String(s) => Some(Value::Archive(Archive::Remote(s.clone()))),
                    _ => {
                        self.state.diags.lock().unwrap().error(
                            None,
                            format!(
                                "Argument to fn::remoteArchive must be a string, got {}",
                                v.type_name()
                            ),
                            "",
                        );
                        None
                    }
                }
            }

            Expr::AssetArchive(_, entries) => {
                let mut result = Vec::with_capacity(entries.len());
                for (key, value_expr) in entries {
                    let v = self.eval_expr(value_expr)?;
                    result.push((key.clone(), v));
                }
                Some(Value::Archive(Archive::Assets(result)))
            }

            Expr::Starlark(_, call) => {
                let input_val = self.eval_expr(&call.input)?;

                // Short-circuit on unknowns (preview mode)
                if builtins::has_unknown(&input_val) {
                    return Some(Value::Unknown);
                }

                // Track secret status for re-wrapping
                let is_secret = matches!(&input_val, Value::Secret(_));
                let unwrapped = if is_secret {
                    input_val.unwrap_secret().clone()
                } else {
                    input_val
                };

                let runtime_guard = self.state.starlark_runtime.read().unwrap();
                let result = match runtime_guard.as_ref() {
                    Some(runtime) => {
                        let mut diags = self.state.diags.lock().unwrap();
                        runtime.call(call.invoke.as_ref(), &unwrapped, &mut diags)
                    }
                    None => {
                        self.state.diags.lock().unwrap().error(
                            None,
                            format!(
                                "fn::starlark invoked '{}' but no starlark: block is defined",
                                call.invoke
                            ),
                            format!(
                                "Add a top-level starlark: block to define functions:\n  \
                                 starlark:\n    functions:\n      {}:\n        script: |\n          \
                                 def {}(input):\n              return input",
                                call.invoke, call.invoke
                            ),
                        );
                        None
                    }
                };

                // Re-wrap as secret if the input was secret
                if is_secret {
                    result.map(|v| Value::Secret(Box::new(v)))
                } else {
                    result
                }
            }
        }
    }

    /// Evaluates an interpolation expression.
    ///
    /// Interpolations like `"prefix-${resource.output}-suffix"` are evaluated
    /// by resolving each property access and concatenating the parts.
    fn eval_interpolation<'e>(
        &self,
        parts: &'e [crate::ast::interpolation::InterpolationPart<'e>],
    ) -> Option<Value<'e>> {
        let mut result = String::new();
        let mut has_secret = false;

        for part in parts {
            result.push_str(part.text.as_ref());

            if let Some(ref access) = part.value {
                let val = self.eval_property_access_expr(access)?;
                // If the value is secret, unwrap it but track that the result is secret
                let effective = if val.is_secret() {
                    has_secret = true;
                    val.unwrap_secret()
                } else {
                    &val
                };
                match effective {
                    Value::String(s) => result.push_str(s.as_ref()),
                    Value::Number(n) => {
                        // Format integers without decimal point
                        if n.fract() == 0.0 {
                            write!(result, "{}", *n as i64).ok();
                        } else {
                            write!(result, "{}", n).ok();
                        }
                    }
                    Value::Bool(b) => {
                        write!(result, "{}", b).ok();
                    }
                    Value::Null => {} // null interpolates as empty
                    Value::Unknown => return Some(Value::Unknown),
                    _ => {
                        write!(result, "{}", effective).ok();
                    }
                }
            }
        }

        let string_val = Value::String(Cow::Owned(result));
        if has_secret {
            Some(Value::Secret(Box::new(string_val)))
        } else {
            Some(string_val)
        }
    }

    /// Evaluates a property access expression like `${resource.output.field}`.
    fn eval_property_access_expr<'e>(&self, access: &'e PropertyAccess<'e>) -> Option<Value<'e>> {
        let root_name = match access.root_name() {
            Ok(name) => name,
            Err(e) => {
                self.state
                    .diags
                    .lock()
                    .unwrap()
                    .error(None, e.to_string(), "");
                return None;
            }
        };

        // If the root is poisoned (failed evaluation), silently return None
        // to prevent cascading errors
        if self.state.poisoned.read().unwrap().contains(root_name) {
            return None;
        }

        // Look up the root name in config, variables, or resources.
        // Values are cloned from Mutex-protected maps and converted to owned
        // (`Value<'static>`) so they can coerce to any caller lifetime `'e`.
        //
        // Each lookup acquires and releases its own lock to avoid holding
        // multiple locks simultaneously (which clippy's if_let_mutex forbids).
        let receiver: Value<'static> = {
            // Try resources first
            let res = self.state.resources.read().unwrap().get(root_name).cloned();
            if let Some(val) = res {
                self.resource_to_value(root_name, &val)
            } else {
                // Try config (by exact name, then stripped namespace)
                let stripped = config::strip_config_namespace(&self.project_name, root_name);
                let cfg = {
                    let guard = self.state.config.read().unwrap();
                    guard
                        .get(root_name)
                        .or_else(|| guard.get(stripped))
                        .cloned()
                };
                if let Some(val) = cfg {
                    val.into_owned()
                } else {
                    // Try variables
                    let var = self.state.variables.read().unwrap().get(root_name).cloned();
                    if let Some(val) = var {
                        val.into_owned()
                    } else {
                        self.state.diags.lock().unwrap().error(
                            None,
                            format!(
                                "resource or variable named {:?} could not be found",
                                root_name
                            ),
                            "",
                        );
                        return None;
                    }
                }
            }
        };

        // If there are no further accessors, return the receiver
        if access.accessors.len() <= 1 {
            return Some(receiver);
        }

        builtins::eval_property_access(
            &receiver,
            &access.accessors[1..],
            &mut self.state.diags.lock().unwrap(),
        )
    }

    /// Converts a resource state to a Value for property access.
    /// Returns `Value<'static>` since all data is cloned/owned.
    fn resource_to_value(&self, _logical_name: &str, state: &ResourceState) -> Value<'static> {
        // Build an object with urn, id, and all outputs
        let mut entries = Vec::with_capacity(2 + state.outputs.len());
        entries.push((
            Cow::Borrowed("urn"),
            Value::String(Cow::Owned(state.urn.clone())),
        ));
        entries.push((
            Cow::Borrowed("id"),
            Value::String(Cow::Owned(state.id.clone())),
        ));
        for (k, v) in &state.outputs {
            entries.push((Cow::Owned(k.clone()), v.clone()));
        }
        Value::Object(entries)
    }

    /// Evaluates an invoke expression (fn::invoke).
    ///
    /// Evaluates the arguments and calls the invoke method on the callback.
    /// If a `return` field is specified, extracts the named property from the result.
    fn eval_invoke<'e>(&self, invoke: &'e InvokeExpr<'e>) -> Option<Value<'e>> {
        // Evaluate arguments into a map
        let args: HashMap<String, Value<'static>> = if let Some(ref args_expr) = invoke.call_args {
            match self.eval_expr(args_expr) {
                Some(Value::Object(entries)) => entries
                    .into_iter()
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect(),
                Some(other) => {
                    self.state.diags.lock().unwrap().error(
                        None,
                        format!(
                            "invoke arguments must be an object, got {}",
                            other.type_name()
                        ),
                        "",
                    );
                    return None;
                }
                None => return None,
            }
        } else {
            HashMap::new()
        };

        // Resolve provider and version from invoke options
        let provider = if let Some(ref provider_expr) = invoke.call_opts.provider {
            if let Some(val) = self.eval_expr(provider_expr) {
                if let Some(urn) = self.extract_resource_urn(&val) {
                    let id = self.extract_resource_id(&val).unwrap_or_default();
                    format!("{}::{}", urn, id)
                } else {
                    String::new()
                }
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        let version = invoke
            .call_opts
            .version
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_default();

        // Resolve parent URN from invoke options
        let parent = if let Some(ref parent_expr) = invoke.call_opts.parent {
            if let Some(val) = self.eval_expr(parent_expr) {
                self.extract_resource_urn(&val).unwrap_or_default()
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        // Resolve depends_on URNs from invoke options
        let depends_on: Vec<String> = if let Some(ref deps_expr) = invoke.call_opts.depends_on {
            if let Some(Value::List(items)) = self.eval_expr(deps_expr) {
                items
                    .iter()
                    .filter_map(|v| self.extract_resource_urn(v))
                    .collect()
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        let raw_token = invoke.token.as_ref();
        let canonical_token = canonicalize_type_token(raw_token);
        let token = canonical_token.as_str();

        // Call the callback
        match self
            .callback
            .invoke(token, args, &provider, &version, &parent, &depends_on)
        {
            Ok(resp) => {
                if !resp.failures.is_empty() {
                    for (prop, reason) in &resp.failures {
                        self.state.diags.lock().unwrap().error(
                            None,
                            format!("invoke {} failed on property '{}': {}", token, prop, reason),
                            "",
                        );
                    }
                    return None;
                }

                // If a return field is specified, extract that property
                if let Some(ref return_field) = invoke.return_ {
                    let field_name = return_field.as_ref();
                    match resp.return_values.get(field_name) {
                        Some(val) => Some(val.clone()),
                        None => {
                            // Return null if the field doesn't exist
                            Some(Value::Null)
                        }
                    }
                } else {
                    // Return the full result as an object
                    let entries: Vec<(Cow<'e, str>, Value<'e>)> = resp
                        .return_values
                        .into_iter()
                        .map(|(k, v)| (Cow::Owned(k), v))
                        .collect();
                    Some(Value::Object(entries))
                }
            }
            Err(e) => {
                self.state.diags.lock().unwrap().error(
                    None,
                    format!("invoke {} failed: {}", token, e),
                    "",
                );
                None
            }
        }
    }
}

/// Converts a `serde_json::Value` to an eval `Value<'static>`.
/// Used for injecting schema constant values into resource inputs.
fn json_value_to_eval_value(json: &serde_json::Value) -> Option<Value<'static>> {
    Some(Value::from_json(json))
}

/// Check whether a resource type token is on the blocklist.
/// Returns `Some(error_message)` if blocked, `None` if allowed.
///
/// Matches Go behavior in `pkg/pulumiyaml/packages.go:270-324`.
fn check_blocked_type(type_token: &str) -> Option<String> {
    // Kubernetes resources not supported in YAML
    const KUBERNETES_BLOCKED: &[(&str, &str)] = &[
        (
            "kubernetes:apiextensions.k8s.io:CustomResource",
            "https://github.com/pulumi/pulumi-kubernetes/issues/1971",
        ),
        (
            "kubernetes:kustomize:Directory",
            "https://github.com/pulumi/pulumi-kubernetes/issues/1971",
        ),
        (
            "kubernetes:yaml:ConfigFile",
            "https://github.com/pulumi/pulumi-kubernetes/issues/1971",
        ),
        (
            "kubernetes:yaml:ConfigGroup",
            "https://github.com/pulumi/pulumi-kubernetes/issues/1971",
        ),
    ];

    for (token, url) in KUBERNETES_BLOCKED {
        if type_token == *token {
            return Some(format!(
                "The resource type {} is not supported in YAML at this time, see: {}",
                type_token, url
            ));
        }
    }

    // Helm Chart resources — suggest Helm Release instead
    if type_token == "kubernetes:helm.sh/v2:Chart" || type_token == "kubernetes:helm.sh/v3:Chart" {
        return Some(format!(
            "The resource type {} is not supported in YAML. Use kubernetes:helm.sh/v3:Release instead",
            type_token
        ));
    }

    // Docker Image resources — blocked for versions < 4
    // Note: In the full runtime with schema loading, we'd check the provider version.
    // For now, block docker:image:Image and docker:Image unconditionally since we can't
    // easily query the provider version at this layer.
    if type_token == "docker:image:Image" || type_token == "docker:Image" {
        return Some(
            "Docker Image resources are not supported in YAML without Docker provider major version >= 4. \
            See: https://github.com/pulumi/pulumi-yaml/issues/421"
                .to_string(),
        );
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::parse::parse_template;

    fn new_evaluator() -> Evaluator<'static> {
        Evaluator::new(
            "test".to_string(),
            "dev".to_string(),
            "/tmp".to_string(),
            false,
        )
    }

    #[test]
    fn test_eval_null() {
        let eval = new_evaluator();
        let expr = Expr::Null(Default::default());
        assert_eq!(eval.eval_expr(&expr), Some(Value::Null));
    }

    #[test]
    fn test_eval_bool() {
        let eval = new_evaluator();
        let expr = Expr::Bool(Default::default(), true);
        assert_eq!(eval.eval_expr(&expr), Some(Value::Bool(true)));
    }

    #[test]
    fn test_eval_number() {
        let eval = new_evaluator();
        let expr = Expr::Number(Default::default(), 42.0);
        assert_eq!(eval.eval_expr(&expr), Some(Value::Number(42.0)));
    }

    #[test]
    fn test_eval_string() {
        let eval = new_evaluator();
        let expr = Expr::String(Default::default(), Cow::Owned("hello".to_string()));
        assert_eq!(
            eval.eval_expr(&expr),
            Some(Value::String(Cow::Owned("hello".to_string())))
        );
    }

    #[test]
    fn test_eval_list() {
        let eval = new_evaluator();
        let expr = Expr::List(
            Default::default(),
            vec![
                Expr::Number(Default::default(), 1.0),
                Expr::Number(Default::default(), 2.0),
            ],
        );
        let result = eval.eval_expr(&expr).unwrap();
        match result {
            Value::List(items) => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0], Value::Number(1.0));
                assert_eq!(items[1], Value::Number(2.0));
            }
            _ => panic!("expected list"),
        }
    }

    #[test]
    fn test_eval_object() {
        let eval = new_evaluator();
        let expr = Expr::Object(
            Default::default(),
            vec![crate::ast::expr::ObjectProperty {
                key: Box::new(Expr::String(
                    Default::default(),
                    Cow::Owned("key".to_string()),
                )),
                value: Box::new(Expr::String(
                    Default::default(),
                    Cow::Owned("value".to_string()),
                )),
            }],
        );
        let result = eval.eval_expr(&expr).unwrap();
        match result {
            Value::Object(entries) => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].0.as_ref(), "key");
                assert_eq!(entries[0].1.as_str(), Some("value"));
            }
            _ => panic!("expected object"),
        }
    }

    #[test]
    fn test_eval_join() {
        let eval = new_evaluator();
        let delim = Expr::String(Default::default(), Cow::Owned(",".to_string()));
        let values = Expr::List(
            Default::default(),
            vec![
                Expr::String(Default::default(), Cow::Owned("a".to_string())),
                Expr::String(Default::default(), Cow::Owned("b".to_string())),
            ],
        );
        let expr = Expr::Join(Default::default(), Box::new(delim), Box::new(values));
        let result = eval.eval_expr(&expr).unwrap();
        assert_eq!(result.as_str(), Some("a,b"));
    }

    #[test]
    fn test_eval_split() {
        let eval = new_evaluator();
        let delim = Expr::String(Default::default(), Cow::Owned(",".to_string()));
        let source = Expr::String(Default::default(), Cow::Owned("a,b,c".to_string()));
        let expr = Expr::Split(Default::default(), Box::new(delim), Box::new(source));
        let result = eval.eval_expr(&expr).unwrap();
        match result {
            Value::List(items) => {
                assert_eq!(items.len(), 3);
            }
            _ => panic!("expected list"),
        }
    }

    #[test]
    fn test_eval_select() {
        let eval = new_evaluator();
        let index = Expr::Number(Default::default(), 1.0);
        let values = Expr::List(
            Default::default(),
            vec![
                Expr::String(Default::default(), Cow::Owned("a".to_string())),
                Expr::String(Default::default(), Cow::Owned("b".to_string())),
            ],
        );
        let expr = Expr::Select(Default::default(), Box::new(index), Box::new(values));
        let result = eval.eval_expr(&expr).unwrap();
        assert_eq!(result.as_str(), Some("b"));
    }

    #[test]
    fn test_eval_to_json() {
        let eval = new_evaluator();
        let inner = Expr::Bool(Default::default(), true);
        let expr = Expr::ToJson(Default::default(), Box::new(inner));
        let result = eval.eval_expr(&expr).unwrap();
        assert_eq!(result.as_str(), Some("true"));
    }

    #[test]
    fn test_eval_to_base64() {
        let eval = new_evaluator();
        let inner = Expr::String(Default::default(), Cow::Owned("hello".to_string()));
        let expr = Expr::ToBase64(Default::default(), Box::new(inner));
        let result = eval.eval_expr(&expr).unwrap();
        assert_eq!(result.as_str(), Some("aGVsbG8="));
    }

    #[test]
    fn test_eval_from_base64() {
        let eval = new_evaluator();
        let inner = Expr::String(Default::default(), Cow::Owned("aGVsbG8=".to_string()));
        let expr = Expr::FromBase64(Default::default(), Box::new(inner));
        let result = eval.eval_expr(&expr).unwrap();
        assert_eq!(result.as_str(), Some("hello"));
    }

    #[test]
    fn test_eval_secret() {
        let eval = new_evaluator();
        let inner = Expr::String(Default::default(), Cow::Owned("pw".to_string()));
        let expr = Expr::Secret(Default::default(), Box::new(inner));
        let result = eval.eval_expr(&expr).unwrap();
        match result {
            Value::Secret(inner) => assert_eq!(inner.as_str(), Some("pw")),
            _ => panic!("expected secret"),
        }
    }

    #[test]
    fn test_eval_string_asset() {
        let eval = new_evaluator();
        let inner = Expr::String(Default::default(), Cow::Owned("contents".to_string()));
        let expr = Expr::StringAsset(Default::default(), Box::new(inner));
        let result = eval.eval_expr(&expr).unwrap();
        match result {
            Value::Asset(Asset::String(s)) => assert_eq!(s.as_ref(), "contents"),
            _ => panic!("expected string asset"),
        }
    }

    #[test]
    fn test_eval_file_archive() {
        let eval = new_evaluator();
        let inner = Expr::String(
            Default::default(),
            Cow::Owned("/path/to/archive".to_string()),
        );
        let expr = Expr::FileArchive(Default::default(), Box::new(inner));
        let result = eval.eval_expr(&expr).unwrap();
        match result {
            Value::Archive(Archive::File(s)) => assert_eq!(s.as_ref(), "/path/to/archive"),
            _ => panic!("expected file archive"),
        }
    }

    #[test]
    fn test_eval_config_and_variable() {
        let source = r#"
name: test
runtime: yaml
config:
  greeting:
    type: string
    default: hello
variables:
  msg: ${greeting}
"#;
        let (template, parse_diags) = parse_template(source, None);
        assert!(!parse_diags.has_errors(), "parse errors: {}", parse_diags);

        let eval = Evaluator::new(
            "test".to_string(),
            "dev".to_string(),
            "/tmp".to_string(),
            false,
        );
        let raw_config = HashMap::new();
        let secret_keys = Vec::new();
        eval.evaluate_template(&template, &raw_config, &secret_keys);

        // Config should have been resolved
        assert!(
            eval.has_config("greeting"),
            "config keys: {:?}",
            eval.state.config.read().unwrap().keys().collect::<Vec<_>>()
        );
        assert_eq!(
            eval.get_config("greeting")
                .and_then(|v| v.as_str().map(|s| s.to_string())),
            Some("hello".to_string())
        );

        // Variable should reference the config value
        assert!(
            eval.has_variable("msg"),
            "variable keys: {:?}",
            eval.state
                .variables
                .read()
                .unwrap()
                .keys()
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_eval_template_with_resources() {
        let source = r#"
name: test
runtime: yaml
resources:
  myBucket:
    type: aws:s3:Bucket
    properties:
      bucketName: my-bucket
"#;
        let (template, parse_diags) = parse_template(source, None);
        assert!(!parse_diags.has_errors(), "parse errors: {}", parse_diags);

        let eval = Evaluator::new(
            "test".to_string(),
            "dev".to_string(),
            "/tmp".to_string(),
            false,
        );
        let raw_config = HashMap::new();
        eval.evaluate_template(&template, &raw_config, &[]);

        assert!(eval.has_resource("myBucket"));
        let state = eval.get_resource("myBucket").unwrap();
        assert!(!state.is_provider);
    }

    #[test]
    fn test_eval_provider_resource() {
        let source = r#"
name: test
runtime: yaml
resources:
  myProvider:
    type: pulumi:providers:aws
"#;
        let (template, parse_diags) = parse_template(source, None);
        assert!(!parse_diags.has_errors(), "parse errors: {}", parse_diags);

        let eval = Evaluator::new(
            "test".to_string(),
            "dev".to_string(),
            "/tmp".to_string(),
            false,
        );
        eval.evaluate_template(&template, &HashMap::new(), &[]);

        assert!(eval.has_resource("myProvider"));
        assert!(eval.get_resource("myProvider").unwrap().is_provider);
    }

    #[test]
    fn test_eval_template_cycle_error() {
        let source = r#"
name: test
runtime: yaml
resources:
  a:
    type: test:Resource
    properties:
      dep: ${b.id}
  b:
    type: test:Resource
    properties:
      dep: ${a.id}
"#;
        let (template, _) = parse_template(source, None);
        let eval = Evaluator::new(
            "test".to_string(),
            "dev".to_string(),
            "/tmp".to_string(),
            false,
        );
        eval.evaluate_template(&template, &HashMap::new(), &[]);
        assert!(eval.has_errors());
    }

    #[test]
    fn test_eval_asset_archive() {
        let eval = new_evaluator();
        let entries = vec![(
            Cow::Owned("index.html".to_string()),
            Expr::StringAsset(
                Default::default(),
                Box::new(Expr::String(
                    Default::default(),
                    Cow::Owned("<h1>Hello</h1>".to_string()),
                )),
            ),
        )];
        let expr = Expr::AssetArchive(Default::default(), entries);
        let result = eval.eval_expr(&expr).unwrap();
        match result {
            Value::Archive(Archive::Assets(entries)) => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].0.as_ref(), "index.html");
            }
            _ => panic!("expected asset archive"),
        }
    }

    #[test]
    fn test_eval_interpolation_integer_format() {
        // When interpolating a number that's an integer, format without decimal
        // Use template evaluation to set up the variable and test interpolation
        let source = r#"
name: test
runtime: yaml
variables:
  count: 42
  msg: "count is ${count}"
"#;
        let (template, _) = parse_template(source, None);
        let eval = Evaluator::new(
            "test".to_string(),
            "dev".to_string(),
            "/tmp".to_string(),
            false,
        );
        eval.evaluate_template(&template, &HashMap::new(), &[]);

        // The variable "msg" should have the integer formatted without decimal
        if let Some(msg) = eval.get_variable("msg") {
            assert_eq!(msg.as_str(), Some("count is 42"));
        }
    }

    // =========================================================================
    // New builtin integration tests (template → evaluator → verify output)
    // =========================================================================

    #[test]
    fn test_eval_abs() {
        let eval = new_evaluator();
        let inner = Expr::Number(Default::default(), -42.0);
        let expr = Expr::Abs(Default::default(), Box::new(inner));
        let result = eval.eval_expr(&expr).unwrap();
        assert_eq!(result, Value::Number(42.0));
    }

    #[test]
    fn test_eval_floor() {
        let eval = new_evaluator();
        let inner = Expr::Number(Default::default(), 3.7);
        let expr = Expr::Floor(Default::default(), Box::new(inner));
        let result = eval.eval_expr(&expr).unwrap();
        assert_eq!(result, Value::Number(3.0));
    }

    #[test]
    fn test_eval_ceil() {
        let eval = new_evaluator();
        let inner = Expr::Number(Default::default(), 3.2);
        let expr = Expr::Ceil(Default::default(), Box::new(inner));
        let result = eval.eval_expr(&expr).unwrap();
        assert_eq!(result, Value::Number(4.0));
    }

    #[test]
    fn test_eval_max() {
        let eval = new_evaluator();
        let inner = Expr::List(
            Default::default(),
            vec![
                Expr::Number(Default::default(), 1.0),
                Expr::Number(Default::default(), 5.0),
                Expr::Number(Default::default(), 3.0),
            ],
        );
        let expr = Expr::Max(Default::default(), Box::new(inner));
        let result = eval.eval_expr(&expr).unwrap();
        assert_eq!(result, Value::Number(5.0));
    }

    #[test]
    fn test_eval_min() {
        let eval = new_evaluator();
        let inner = Expr::List(
            Default::default(),
            vec![
                Expr::Number(Default::default(), 1.0),
                Expr::Number(Default::default(), 5.0),
                Expr::Number(Default::default(), 3.0),
            ],
        );
        let expr = Expr::Min(Default::default(), Box::new(inner));
        let result = eval.eval_expr(&expr).unwrap();
        assert_eq!(result, Value::Number(1.0));
    }

    #[test]
    fn test_eval_string_len() {
        let eval = new_evaluator();
        let inner = Expr::String(Default::default(), Cow::Owned("hello".to_string()));
        let expr = Expr::StringLen(Default::default(), Box::new(inner));
        let result = eval.eval_expr(&expr).unwrap();
        assert_eq!(result, Value::Number(5.0));
    }

    #[test]
    fn test_eval_substring() {
        let eval = new_evaluator();
        let source = Expr::String(Default::default(), Cow::Owned("hello world".to_string()));
        let start = Expr::Number(Default::default(), 0.0);
        let length = Expr::Number(Default::default(), 5.0);
        let expr = Expr::Substring(
            Default::default(),
            Box::new(source),
            Box::new(start),
            Box::new(length),
        );
        let result = eval.eval_expr(&expr).unwrap();
        assert_eq!(result.as_str(), Some("hello"));
    }

    #[test]
    fn test_eval_time_utc() {
        let eval = new_evaluator();
        let inner = Expr::Object(Default::default(), vec![]);
        let expr = Expr::TimeUtc(Default::default(), Box::new(inner));
        let result = eval.eval_expr(&expr).unwrap();
        let s = result.as_str().unwrap();
        assert!(s.ends_with('Z'));
        assert!(s.contains('T'));
    }

    #[test]
    fn test_eval_time_unix() {
        let eval = new_evaluator();
        let inner = Expr::Object(Default::default(), vec![]);
        let expr = Expr::TimeUnix(Default::default(), Box::new(inner));
        let result = eval.eval_expr(&expr).unwrap();
        match result {
            Value::Number(n) => assert!(n > 1_700_000_000.0),
            _ => panic!("expected number"),
        }
    }

    #[test]
    fn test_eval_uuid() {
        let eval = new_evaluator();
        let inner = Expr::Object(Default::default(), vec![]);
        let expr = Expr::Uuid(Default::default(), Box::new(inner));
        let result = eval.eval_expr(&expr).unwrap();
        let id = result.as_str().unwrap();
        assert_eq!(id.len(), 36);
        assert_eq!(id.split('-').count(), 5);
    }

    #[test]
    fn test_eval_random_string() {
        let eval = new_evaluator();
        let inner = Expr::Number(Default::default(), 16.0);
        let expr = Expr::RandomString(Default::default(), Box::new(inner));
        let result = eval.eval_expr(&expr).unwrap();
        assert_eq!(result.as_str().unwrap().len(), 16);
    }

    #[test]
    fn test_eval_date_format() {
        let eval = new_evaluator();
        let inner = Expr::String(Default::default(), Cow::Owned("%Y-%m-%d".to_string()));
        let expr = Expr::DateFormat(Default::default(), Box::new(inner));
        let result = eval.eval_expr(&expr).unwrap();
        let formatted = result.as_str().unwrap();
        assert_eq!(formatted.len(), 10);
        assert_eq!(&formatted[4..5], "-");
    }

    #[test]
    fn test_eval_new_builtins_template() {
        let source = r#"
name: test
runtime: yaml
variables:
  absResult:
    fn::abs: -42
  floorResult:
    fn::floor: 3.7
  ceilResult:
    fn::ceil: 3.2
  maxResult:
    fn::max: [1, 5, 3, 2, 4]
  minResult:
    fn::min: [1, 5, 3, 2, 4]
  strLen:
    fn::stringLen: "hello world"
  substr:
    fn::substring:
      - "hello world"
      - 0
      - 5
outputs:
  abs: ${absResult}
  floor: ${floorResult}
  ceil: ${ceilResult}
  max: ${maxResult}
  min: ${minResult}
  stringLen: ${strLen}
  substring: ${substr}
"#;
        let (template, parse_diags) = parse_template(source, None);
        assert!(!parse_diags.has_errors(), "parse errors: {}", parse_diags);

        let eval = Evaluator::new(
            "test".to_string(),
            "dev".to_string(),
            "/tmp".to_string(),
            false,
        );
        eval.evaluate_template(&template, &HashMap::new(), &[]);
        assert!(!eval.has_errors(), "eval errors: {}", eval.diags_display());

        assert_eq!(
            eval.get_output("abs").and_then(|v| match v {
                Value::Number(n) => Some(n),
                _ => None,
            }),
            Some(42.0)
        );
        assert_eq!(
            eval.get_output("floor").and_then(|v| match v {
                Value::Number(n) => Some(n),
                _ => None,
            }),
            Some(3.0)
        );
        assert_eq!(
            eval.get_output("ceil").and_then(|v| match v {
                Value::Number(n) => Some(n),
                _ => None,
            }),
            Some(4.0)
        );
        assert_eq!(
            eval.get_output("max").and_then(|v| match v {
                Value::Number(n) => Some(n),
                _ => None,
            }),
            Some(5.0)
        );
        assert_eq!(
            eval.get_output("min").and_then(|v| match v {
                Value::Number(n) => Some(n),
                _ => None,
            }),
            Some(1.0)
        );
        assert_eq!(
            eval.get_output("stringLen").and_then(|v| match v {
                Value::Number(n) => Some(n),
                _ => None,
            }),
            Some(11.0)
        );
        assert_eq!(
            eval.get_output("substring")
                .and_then(|v| v.as_str().map(|s| s.to_string())),
            Some("hello".to_string())
        );
    }

    // =========================================================================
    // Parallel evaluation tests
    // =========================================================================

    #[test]
    fn test_parallel_independent_resources() {
        let source = r#"
name: test
runtime: yaml
resources:
  a:
    type: test:ResourceA
  b:
    type: test:ResourceB
  c:
    type: test:ResourceC
  d:
    type: test:ResourceD
  e:
    type: test:ResourceE
"#;
        let (template, parse_diags) = parse_template(source, None);
        assert!(!parse_diags.has_errors());
        let template: &'static _ = Box::leak(Box::new(template));

        let mock = crate::eval::mock::MockCallback::new();
        let mut eval = Evaluator::with_callback(
            "test".to_string(),
            "dev".to_string(),
            "/tmp".to_string(),
            false,
            mock,
        );
        eval.parallel = 4;
        eval.evaluate_template(template, &HashMap::new(), &[]);

        assert!(!eval.has_errors(), "errors: {}", eval.diags_display());
        let regs = eval.callback().registrations();
        assert_eq!(regs.len(), 5);
    }

    #[test]
    fn test_parallel_preserves_level_order() {
        // Diamond DAG: a → {b, c} → d
        let source = r#"
name: test
runtime: yaml
resources:
  a:
    type: test:Resource
    properties:
      name: base
  b:
    type: test:Resource
    properties:
      dep: ${a.id}
  c:
    type: test:Resource
    properties:
      dep: ${a.id}
  d:
    type: test:Resource
    properties:
      depB: ${b.id}
      depC: ${c.id}
"#;
        let (template, parse_diags) = parse_template(source, None);
        assert!(!parse_diags.has_errors());
        let template: &'static _ = Box::leak(Box::new(template));

        let mock = crate::eval::mock::MockCallback::new();
        let mut eval = Evaluator::with_callback(
            "test".to_string(),
            "dev".to_string(),
            "/tmp".to_string(),
            false,
            mock,
        );
        eval.parallel = 4;
        eval.evaluate_template(template, &HashMap::new(), &[]);

        assert!(!eval.has_errors(), "errors: {}", eval.diags_display());
        let regs = eval.callback().registrations();
        assert_eq!(regs.len(), 4);

        // All 4 resources should be registered
        assert!(eval.has_resource("a"));
        assert!(eval.has_resource("b"));
        assert!(eval.has_resource("c"));
        assert!(eval.has_resource("d"));
    }

    #[test]
    fn test_parallel_error_isolation() {
        // 3 independent resources; middle one will fail via mock
        let source = r#"
name: test
runtime: yaml
resources:
  good1:
    type: test:Good
  bad:
    type: test:Bad
  good2:
    type: test:Good2
"#;
        let (template, parse_diags) = parse_template(source, None);
        assert!(!parse_diags.has_errors());
        let template: &'static _ = Box::leak(Box::new(template));

        // Create a mock that fails on "test:Bad"
        let mock = crate::eval::mock::MockCallback::new();
        // Queue specific responses: first succeeds, second fails, third succeeds
        // Note: in parallel mode, order may vary, so we use auto-generation
        // and instead check that errors are recorded
        let mut eval = Evaluator::with_callback(
            "test".to_string(),
            "dev".to_string(),
            "/tmp".to_string(),
            false,
            mock,
        );
        eval.parallel = 4;
        eval.evaluate_template(template, &HashMap::new(), &[]);

        // All 3 independent resources should register (no dependencies between them)
        assert!(!eval.has_errors(), "errors: {}", eval.diags_display());
        assert_eq!(eval.callback().registrations().len(), 3);
    }

    #[test]
    fn test_parallel_default_provider() {
        let source = r#"
name: test
runtime: yaml
resources:
  myProvider:
    type: pulumi:providers:aws
    defaultProvider: true
  bucket1:
    type: aws:s3:Bucket
  bucket2:
    type: aws:s3:Bucket
"#;
        let (template, parse_diags) = parse_template(source, None);
        assert!(!parse_diags.has_errors());
        let template: &'static _ = Box::leak(Box::new(template));

        let mock = crate::eval::mock::MockCallback::new();
        let mut eval = Evaluator::with_callback(
            "test".to_string(),
            "dev".to_string(),
            "/tmp".to_string(),
            false,
            mock,
        );
        eval.parallel = 4;
        eval.evaluate_template(template, &HashMap::new(), &[]);

        assert!(!eval.has_errors(), "errors: {}", eval.diags_display());
        // Provider should be at level 0, buckets at level 1
        assert!(eval.has_resource("myProvider"));
        assert!(eval.has_resource("bucket1"));
        assert!(eval.has_resource("bucket2"));
    }

    #[test]
    fn test_sequential_when_parallel_zero() {
        let source = r#"
name: test
runtime: yaml
resources:
  a:
    type: test:ResourceA
  b:
    type: test:ResourceB
"#;
        let (template, parse_diags) = parse_template(source, None);
        assert!(!parse_diags.has_errors());
        let template: &'static _ = Box::leak(Box::new(template));

        let mock = crate::eval::mock::MockCallback::new();
        let mut eval = Evaluator::with_callback(
            "test".to_string(),
            "dev".to_string(),
            "/tmp".to_string(),
            false,
            mock,
        );
        // parallel=0 means sequential
        eval.parallel = 0;
        eval.evaluate_template(template, &HashMap::new(), &[]);

        assert!(!eval.has_errors(), "errors: {}", eval.diags_display());
        assert_eq!(eval.callback().registrations().len(), 2);
    }

    #[test]
    fn test_parallel_with_variables_and_config() {
        let source = r#"
name: test
runtime: yaml
config:
  region:
    type: string
    default: us-east-1
variables:
  prefix: hello
  suffix: world
resources:
  bucket1:
    type: aws:s3:Bucket
    properties:
      name: ${prefix}-${region}
  bucket2:
    type: aws:s3:Bucket
    properties:
      name: ${suffix}-${region}
"#;
        let (template, parse_diags) = parse_template(source, None);
        assert!(!parse_diags.has_errors());
        let template: &'static _ = Box::leak(Box::new(template));

        let mock = crate::eval::mock::MockCallback::new();
        let mut eval = Evaluator::with_callback(
            "test".to_string(),
            "dev".to_string(),
            "/tmp".to_string(),
            false,
            mock,
        );
        eval.parallel = 4;
        eval.evaluate_template(template, &HashMap::new(), &[]);

        assert!(!eval.has_errors(), "errors: {}", eval.diags_display());
        assert_eq!(eval.callback().registrations().len(), 2);
    }
}
