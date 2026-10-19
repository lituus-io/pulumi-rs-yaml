// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Canonical resource keys: the one join key between a program's graph and
//! everything Google records about the resources it deploys.
//!
//! A key is the resource's full resource name,
//! `//{service}.googleapis.com/{relative-name}` -- the form Cloud Asset
//! Inventory, audit logs and IAM conditions use. [`resource_key`] builds it
//! from a Pulumi type and the provider id the deploy recorded;
//! [`bq_to_resource_key`] from a lineage id (`bq://p/d/t`). The two agree on
//! every BigQuery object, so a table seen by static lineage, by the deploy
//! and by a job's audit log has one key.
//!
//! # Normalization (shared with [`crate::sql_lineage`]'s table ids)
//!
//! - The project is lowercased (GCP project ids are lowercase by rule); a
//!   domain-scoped project (`example.com:analytics`) is accepted as written.
//! - Every other segment keeps its case (BigQuery datasets and tables are
//!   case-sensitive).
//! - A table's partition decorator is stripped (`t$20240101` -> `t`).
//! - A segment that is empty, or holds `/`, `#`, `?`, whitespace or a control
//!   character, makes the id unusable: `None`, never an escaped guess.
//!
//! The per-type table is [`TEMPLATES`]; `GRAPH.md` carries the same table for
//! SQL and Workflow authors.

/// How a type's provider id relates to its relative name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdForm {
    /// The provider id is the relative name.
    Relative,
    /// A bucket's bare name (or `projects/_/buckets/{name}`): bucket names
    /// are global, so the key's project is `_`.
    BucketName,
    /// Cloud Run v1: `locations/{l}/namespaces/{p}/services/{s}`, the same
    /// service Cloud Run v2 names `projects/{p}/locations/{l}/services/{s}`.
    RunV1,
    /// `projects/{p}/jobs/{id}`: the id holds no location, which the key
    /// needs; it comes from the `location` property, or there is no key.
    LocationInProperty,
}

/// One supported type: the Pulumi type module and name, the service, the
/// collections of the key's relative name (`projects/{p}/datasets/{d}` is
/// `["projects", "datasets"]`), and how the provider id spells it.
#[derive(Debug, Clone, Copy)]
pub struct KeyTemplate {
    /// `module/name` as it appears in the canonical token
    /// `gcp:{module}/{camel}:{Name}` -- compared on the module and the name.
    pub module: &'static str,
    pub name: &'static str,
    pub service: &'static str,
    pub collections: &'static [&'static str],
    pub id_form: IdForm,
}

impl KeyTemplate {
    const fn relative(
        module: &'static str,
        name: &'static str,
        service: &'static str,
        collections: &'static [&'static str],
    ) -> Self {
        Self::with_form(module, name, service, collections, IdForm::Relative)
    }

    const fn with_form(
        module: &'static str,
        name: &'static str,
        service: &'static str,
        collections: &'static [&'static str],
        id_form: IdForm,
    ) -> Self {
        Self {
            module,
            name,
            service,
            collections,
            id_form,
        }
    }
}

/// Every type [`resource_key`] answers for.
pub const TEMPLATES: &[KeyTemplate] = &[
    KeyTemplate::relative("bigquery", "Dataset", "bigquery", &["projects", "datasets"]),
    KeyTemplate::relative(
        "bigquery",
        "Table",
        "bigquery",
        &["projects", "datasets", "tables"],
    ),
    KeyTemplate::relative(
        "bigquery",
        "Routine",
        "bigquery",
        &["projects", "datasets", "routines"],
    ),
    KeyTemplate::with_form(
        "bigquery",
        "Job",
        "bigquery",
        &["projects", "locations", "jobs"],
        IdForm::LocationInProperty,
    ),
    KeyTemplate::relative(
        "bigquery",
        "DataTransferConfig",
        "bigquerydatatransfer",
        &["projects", "locations", "transferConfigs"],
    ),
    KeyTemplate::relative(
        "workflows",
        "Workflow",
        "workflows",
        &["projects", "locations", "workflows"],
    ),
    KeyTemplate::relative(
        "cloudscheduler",
        "Job",
        "cloudscheduler",
        &["projects", "locations", "jobs"],
    ),
    KeyTemplate::with_form(
        "cloudrun",
        "Service",
        "run",
        &["projects", "locations", "services"],
        IdForm::RunV1,
    ),
    KeyTemplate::relative(
        "cloudrunv2",
        "Service",
        "run",
        &["projects", "locations", "services"],
    ),
    KeyTemplate::relative(
        "cloudrunv2",
        "Job",
        "run",
        &["projects", "locations", "jobs"],
    ),
    KeyTemplate::with_form(
        "storage",
        "Bucket",
        "storage",
        &["buckets"],
        IdForm::BucketName,
    ),
    KeyTemplate::relative("pubsub", "Topic", "pubsub", &["projects", "topics"]),
    KeyTemplate::relative(
        "pubsub",
        "Subscription",
        "pubsub",
        &["projects", "subscriptions"],
    ),
    KeyTemplate::relative(
        "dataproc",
        "Batch",
        "dataproc",
        &["projects", "locations", "batches"],
    ),
    KeyTemplate::relative("batch", "Job", "batch", &["projects", "locations", "jobs"]),
    KeyTemplate::relative(
        "serviceaccount",
        "Account",
        "iam",
        &["projects", "serviceAccounts"],
    ),
];

/// The template for a Pulumi type token: the canonical
/// `gcp:bigquery/table:Table` or the shorthand `gcp:bigquery:Table`.
pub fn template_for(pulumi_type: &str) -> Option<&'static KeyTemplate> {
    let mut parts = pulumi_type.split(':');
    let (Some("gcp"), Some(module), Some(name), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let module = module.split('/').next().unwrap_or(module);
    TEMPLATES
        .iter()
        .find(|t| t.module == module && t.name == name)
}

/// The canonical project segment: lowercased; a domain-scoped project keeps
/// its `domain:` prefix.
pub fn canonical_project(project: &str) -> std::borrow::Cow<'_, str> {
    if project.chars().any(char::is_uppercase) {
        std::borrow::Cow::Owned(project.to_lowercase())
    } else {
        std::borrow::Cow::Borrowed(project)
    }
}

/// Whether `s` may stand as one segment of a relative name.
pub fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && !s
            .chars()
            .any(|c| matches!(c, '/' | '#' | '?' | '`') || c.is_whitespace() || c.is_control())
}

/// The key from the relative name's ids, one per collection of `t`, in
/// order. Every id is checked by [`valid_segment`] before it is written; the
/// project is canonicalised and a table's partition decorator stripped.
fn key_from_ids<'a>(t: &KeyTemplate, ids: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let mut out = String::with_capacity(t.service.len() + 96);
    out.push_str("//");
    out.push_str(t.service);
    out.push_str(".googleapis.com");
    let mut collections = t.collections.iter();
    for (i, id) in ids.into_iter().enumerate() {
        let collection = *collections.next()?;
        if !valid_segment(id) {
            return None;
        }
        let id = if i == 0 && collection == "projects" {
            canonical_project(id)
        } else if t.name == "Table" && collection == "tables" {
            std::borrow::Cow::Borrowed(id.split('$').next().unwrap_or(id))
        } else {
            std::borrow::Cow::Borrowed(id)
        };
        if id.is_empty() {
            return None;
        }
        out.push('/');
        out.push_str(collection);
        out.push('/');
        out.push_str(&id);
    }
    collections.next().is_none().then_some(out)
}

/// The ids of a provider id spelled `c0/{id0}/c1/{id1}/...` with exactly
/// `collections`, in that order; `None` for any other shape.
fn ids_of<'a, const N: usize>(
    provider_id: &'a str,
    collections: [&str; N],
) -> Option<[&'a str; N]> {
    let mut parts = provider_id.split('/');
    let mut ids = [""; N];
    for (slot, collection) in ids.iter_mut().zip(collections) {
        if parts.next()? != collection {
            return None;
        }
        *slot = parts.next()?;
    }
    parts.next().is_none().then_some(ids)
}

/// Whether a segment is a project NUMBER: a project id starts with a letter.
fn is_project_number(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// `//{service}.googleapis.com/{relative-name}` for a deployed resource, from
/// its Pulumi type and the provider id the deploy recorded. `None` for a type
/// not in [`TEMPLATES`] or an id that does not have the type's shape.
///
/// A storage bucket's provider id is its bare name, and its key is
/// `//storage.googleapis.com/projects/_/buckets/{name}` (Cloud Asset
/// Inventory's form: bucket names are global). A Cloud Run v1 service's id,
/// `locations/{l}/namespaces/{p}/services/{s}`, keys as the v2 service it is.
/// A BigQuery job's id, `projects/{p}/jobs/{id}`, holds no location: it has
/// a key only through [`resource_key_with_properties`]. Every other supported
/// type's provider id is already its relative name.
pub fn resource_key(pulumi_type: &str, provider_id: &str) -> Option<String> {
    let t = template_for(pulumi_type)?;
    match t.id_form {
        IdForm::BucketName => {
            let name = provider_id
                .strip_prefix("projects/_/buckets/")
                .unwrap_or(provider_id);
            if !valid_segment(name) {
                return None;
            }
            Some(format!(
                "//storage.googleapis.com/projects/_/buckets/{}",
                name
            ))
        }
        IdForm::RunV1 => {
            let [l, p, s] = ids_of(provider_id, ["locations", "namespaces", "services"])?;
            if is_project_number(p) {
                return None;
            }
            key_from_ids(t, [p, l, s])
        }
        IdForm::Relative | IdForm::LocationInProperty => {
            let mut parts = provider_id.split('/');
            let mut ids = Vec::with_capacity(t.collections.len());
            for collection in t.collections {
                if parts.next()? != *collection {
                    return None;
                }
                ids.push(parts.next()?);
            }
            if parts.next().is_some() {
                return None;
            }
            key_from_ids(t, ids)
        }
    }
}

/// The key from a deployed resource's provider id and its properties (inputs
/// or outputs): the provider id when it is the relative name, then the
/// properties filling in what the id lacks (a BigQuery job's `location`),
/// then the properties alone ([`resource_key_from_properties`]).
pub fn resource_key_with_properties<'a, I>(
    pulumi_type: &str,
    provider_id: &str,
    properties: I,
) -> Option<String>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let t = template_for(pulumi_type)?;
    if let Some(key) = resource_key(pulumi_type, provider_id) {
        return Some(key);
    }
    let props: Vec<(&str, &str)> = properties.into_iter().collect();
    if t.id_form == IdForm::LocationInProperty {
        if let Some([p, j]) = ids_of(provider_id, ["projects", "jobs"]) {
            let location = props.iter().find(|(k, _)| *k == "location")?.1;
            return key_from_ids(t, [p, location, j]);
        }
    }
    resource_key_from_properties(pulumi_type, props)
}

/// A service account's email from its `accountId` and `project`:
/// `{accountId}@{project}.iam.gserviceaccount.com`, a domain-scoped project
/// `example.com:proj` giving `{accountId}@proj.example.com.iam.gserviceaccount.com`.
fn service_account_email(account_id: &str, project: &str) -> Option<String> {
    if !valid_segment(account_id) || account_id.contains('@') || !valid_segment(project) {
        return None;
    }
    let project = canonical_project(project);
    let host = match project.split_once(':') {
        Some((domain, name)) if !domain.is_empty() && !name.is_empty() => {
            format!("{}.{}", name, domain)
        }
        Some(_) => return None,
        None => project.into_owned(),
    };
    Some(format!("{}@{}.iam.gserviceaccount.com", account_id, host))
}

/// The key built from a resource's literal properties, for when the provider
/// id is not the relative name (a bare name), or before anything is deployed.
/// Each collection reads its usual property: `projects` <- `project`,
/// `locations` <- `location` or `region`, `datasets` <- `datasetId`,
/// `tables` <- `tableId`, `routines` <- `routineId`, `batches` <- `batchId`,
/// `jobs` <- `jobId` or `name`, a service account <- `email` or
/// `accountId` + `project`, everything else <- `name`. A transfer config's
/// id is assigned by the server, so it has no key before deploy.
pub fn resource_key_from_properties<'a, I>(pulumi_type: &str, properties: I) -> Option<String>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let t = template_for(pulumi_type)?;
    let props: Vec<(&str, &str)> = properties.into_iter().collect();
    let get = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| props.iter().find(|(pk, _)| pk == k).map(|(_, v)| *v))
    };
    if t.id_form == IdForm::BucketName {
        return resource_key(pulumi_type, get(&["name"])?);
    }
    let derived_email = match get(&["email"]) {
        None if t.collections.contains(&"serviceAccounts") => {
            service_account_email(get(&["accountId"])?, get(&["project"])?)
        }
        _ => None,
    };
    let mut ids = Vec::with_capacity(t.collections.len());
    for collection in t.collections {
        ids.push(match *collection {
            "projects" => get(&["project"]),
            "locations" => get(&["location", "region"]),
            "datasets" => get(&["datasetId"]),
            "tables" => get(&["tableId"]),
            "routines" => get(&["routineId"]),
            "batches" => get(&["batchId"]),
            "jobs" => get(&["jobId", "name"]),
            "serviceAccounts" => get(&["email"]).or(derived_email.as_deref()),
            "transferConfigs" => None,
            _ => get(&["name"]),
        }?);
    }
    key_from_ids(t, ids)
}

/// The key of a BigQuery JOB -- a runtime object, never a Pulumi resource --
/// as audit logs locate it: `//bigquery.googleapis.com/projects/{p}/locations/{l}/jobs/{id}`.
pub fn bq_job_resource_key(project: &str, location: &str, job_id: &str) -> Option<String> {
    if !valid_segment(project) || !valid_segment(location) || !valid_segment(job_id) {
        return None;
    }
    Some(format!(
        "//bigquery.googleapis.com/projects/{}/locations/{}/jobs/{}",
        canonical_project(project),
        location,
        job_id
    ))
}

/// The resource key of a BigQuery object named by a lineage id --
/// `bq://p`, `bq://p/d`, `bq://p/d/t` (a `#column` fragment names the
/// table), `bq-routine://p/d/r` -- or by a SQL reference `p.d.t`
/// (backticks allowed; a domain-scoped project `example.com:p.d.t` too).
/// A bare project has no BigQuery key; it maps to Resource Manager's.
pub fn bq_to_resource_key(bq_id: &str) -> Option<String> {
    if let Some(rest) = bq_id.strip_prefix("bq-routine://") {
        let mut parts = rest.split('/');
        let (Some(p), Some(d), Some(r), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return None;
        };
        return resource_key(
            "gcp:bigquery:Routine",
            &format!("projects/{}/datasets/{}/routines/{}", p, d, r),
        );
    }
    let path = match bq_id.strip_prefix("bq://") {
        Some(rest) => rest.split('#').next().unwrap_or(rest),
        None => return sql_reference_key(bq_id),
    };
    let parts: Vec<&str> = path.split('/').collect();
    match parts.as_slice() {
        [p] if valid_segment(p) => Some(format!(
            "//cloudresourcemanager.googleapis.com/projects/{}",
            canonical_project(p)
        )),
        [p, d] => resource_key(
            "gcp:bigquery:Dataset",
            &format!("projects/{}/datasets/{}", p, d),
        ),
        [p, d, t] => resource_key(
            "gcp:bigquery:Table",
            &format!("projects/{}/datasets/{}/tables/{}", p, d, t),
        ),
        _ => None,
    }
}

/// A fully-qualified SQL table reference, `p.d.t`.
fn sql_reference_key(raw: &str) -> Option<String> {
    let cleaned = raw.replace('`', "");
    let (project, rest) = split_project(&cleaned)?;
    let mut parts = rest.split('.');
    let (Some(d), Some(t), None) = (parts.next(), parts.next(), parts.next()) else {
        return None;
    };
    resource_key(
        "gcp:bigquery:Table",
        &format!("projects/{}/datasets/{}/tables/{}", project, d, t),
    )
}

/// Splits `project.rest` where the project may be domain-scoped
/// (`example.com:proj`): the colon, when the text before it has a dot, ends
/// the domain, and the project runs to the next dot after it.
pub fn split_project(reference: &str) -> Option<(&str, &str)> {
    if let Some(colon) = reference.find(':') {
        if reference[..colon].contains('.') {
            let dot = reference[colon..].find('.')? + colon;
            return Some((&reference[..dot], &reference[dot + 1..]));
        }
    }
    reference.split_once('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_template_round_trips_a_provider_id() {
        for (ty, id, want) in [
            ("gcp:bigquery/dataset:Dataset", "projects/p/datasets/d", "//bigquery.googleapis.com/projects/p/datasets/d"),
            ("gcp:bigquery:Table", "projects/P/datasets/D/tables/T$20240101", "//bigquery.googleapis.com/projects/p/datasets/D/tables/T"),
            ("gcp:bigquery:Routine", "projects/p/datasets/d/routines/r", "//bigquery.googleapis.com/projects/p/datasets/d/routines/r"),
            ("gcp:bigquery:DataTransferConfig", "projects/p/locations/us/transferConfigs/6a1b", "//bigquerydatatransfer.googleapis.com/projects/p/locations/us/transferConfigs/6a1b"),
            ("gcp:workflows:Workflow", "projects/p/locations/us-central1/workflows/w", "//workflows.googleapis.com/projects/p/locations/us-central1/workflows/w"),
            ("gcp:cloudscheduler:Job", "projects/p/locations/us-central1/jobs/nightly", "//cloudscheduler.googleapis.com/projects/p/locations/us-central1/jobs/nightly"),
            ("gcp:cloudrunv2:Service", "projects/p/locations/l/services/s", "//run.googleapis.com/projects/p/locations/l/services/s"),
            ("gcp:cloudrunv2/job:Job", "projects/p/locations/l/jobs/j", "//run.googleapis.com/projects/p/locations/l/jobs/j"),
            ("gcp:storage:Bucket", "my-bucket", "//storage.googleapis.com/projects/_/buckets/my-bucket"),
            ("gcp:storage:Bucket", "projects/_/buckets/my-bucket", "//storage.googleapis.com/projects/_/buckets/my-bucket"),
            ("gcp:pubsub:Topic", "projects/p/topics/t", "//pubsub.googleapis.com/projects/p/topics/t"),
            ("gcp:pubsub:Subscription", "projects/p/subscriptions/s", "//pubsub.googleapis.com/projects/p/subscriptions/s"),
            ("gcp:dataproc:Batch", "projects/p/locations/l/batches/b", "//dataproc.googleapis.com/projects/p/locations/l/batches/b"),
            ("gcp:batch:Job", "projects/p/locations/l/jobs/j", "//batch.googleapis.com/projects/p/locations/l/jobs/j"),
            ("gcp:serviceaccount:Account", "projects/p/serviceAccounts/sa@p.iam.gserviceaccount.com", "//iam.googleapis.com/projects/p/serviceAccounts/sa@p.iam.gserviceaccount.com"),
            ("gcp:bigquery:Dataset", "projects/example.com:Analytics/datasets/d", "//bigquery.googleapis.com/projects/example.com:analytics/datasets/d"),
        ] {
            assert_eq!(resource_key(ty, id).as_deref(), Some(want), "{ty} {id}");
        }
    }

    #[test]
    fn a_wrong_shape_or_type_is_refused() {
        for (ty, id) in [
            ("gcp:bigquery:Table", "projects/p/datasets/d"),
            ("gcp:bigquery:Table", "projects/p/datasets/d/views/v"),
            ("gcp:bigquery:Dataset", "p:d"),
            ("gcp:bigquery:Dataset", "projects//datasets/d"),
            ("gcp:bigquery:Dataset", "projects/p/datasets/d#frag"),
            ("gcp:bigquery:Dataset", "projects/p/datasets/d?x=1"),
            ("gcp:bigquery:Dataset", "projects/p/datasets/.."),
            ("gcp:bigquery:Dataset", "projects/p/datasets/a b"),
            ("gcp:storage:Bucket", "a/b"),
            ("gcp:storage:Bucket", ""),
            ("gcp:compute:Instance", "projects/p/zones/z/instances/i"),
            ("aws:s3:Bucket", "b"),
            ("gcp:bigquery", "x"),
            ("", ""),
        ] {
            assert_eq!(resource_key(ty, id), None, "{ty} {id}");
        }
    }

    #[test]
    fn lineage_ids_reach_the_same_keys() {
        for (bq, want) in [
            (
                "bq://P/d/t",
                Some("//bigquery.googleapis.com/projects/p/datasets/d/tables/t"),
            ),
            (
                "bq://p/d/t#Col",
                Some("//bigquery.googleapis.com/projects/p/datasets/d/tables/t"),
            ),
            (
                "bq://p/d",
                Some("//bigquery.googleapis.com/projects/p/datasets/d"),
            ),
            (
                "bq://p",
                Some("//cloudresourcemanager.googleapis.com/projects/p"),
            ),
            (
                "bq-routine://p/d/r",
                Some("//bigquery.googleapis.com/projects/p/datasets/d/routines/r"),
            ),
            (
                "`P.d.t$20240101`",
                Some("//bigquery.googleapis.com/projects/p/datasets/d/tables/t"),
            ),
            (
                "example.com:proj.d.t",
                Some("//bigquery.googleapis.com/projects/example.com:proj/datasets/d/tables/t"),
            ),
            (
                "`example.com:proj`.`d`.`t`",
                Some("//bigquery.googleapis.com/projects/example.com:proj/datasets/d/tables/t"),
            ),
            ("d.t", None),
            ("bq://p/d/t/x", None),
            ("bq://", None),
            ("bq-routine://p/d", None),
        ] {
            assert_eq!(bq_to_resource_key(bq).as_deref(), want, "{bq}");
        }
        // The deploy's id and the lineage id of one table agree.
        assert_eq!(
            resource_key("gcp:bigquery:Table", "projects/p/datasets/d/tables/t"),
            bq_to_resource_key("bq://p/d/t")
        );
    }

    #[test]
    fn domain_scoped_projects_split_at_the_right_dot() {
        assert_eq!(
            split_project("example.com:p.d.t"),
            Some(("example.com:p", "d.t"))
        );
        assert_eq!(split_project("p.d.t"), Some(("p", "d.t")));
        assert_eq!(split_project("p:d.t"), Some(("p:d", "t")));
        assert_eq!(split_project("example.com:p"), None);
    }

    /// Keys as the runtime capture builds them from real log entries (project
    /// names neutralised). Byte-for-byte equality is the join.
    #[test]
    fn keys_equal_the_runtime_capture() {
        let p = "sample-proj";
        let l = "northamerica-northeast1";
        // Cloud Scheduler: `//cloudscheduler.googleapis.com/` + jsonPayload.jobName.
        let job_name = format!("projects/{p}/locations/{l}/jobs/probe-sched");
        assert_eq!(
            resource_key("gcp:cloudscheduler/job:Job", &job_name).as_deref(),
            Some(format!("//cloudscheduler.googleapis.com/{job_name}").as_str())
        );
        // Workflows: project id from logName, location, workflow_id.
        assert_eq!(
            resource_key("gcp:workflows/workflow:Workflow", &format!("projects/{p}/locations/{l}/workflows/probe-wf-fail")).as_deref(),
            Some("//workflows.googleapis.com/projects/sample-proj/locations/northamerica-northeast1/workflows/probe-wf-fail")
        );
        // Cloud Run job and service.
        assert_eq!(
            resource_key("gcp:cloudrunv2/job:Job", &format!("projects/{p}/locations/{l}/jobs/probe-job")).as_deref(),
            Some("//run.googleapis.com/projects/sample-proj/locations/northamerica-northeast1/jobs/probe-job")
        );
        assert_eq!(
            resource_key("gcp:cloudrunv2/service:Service", &format!("projects/{p}/locations/{l}/services/probe-svc")).as_deref(),
            Some("//run.googleapis.com/projects/sample-proj/locations/northamerica-northeast1/services/probe-svc")
        );
        // Dataproc: `//dataproc.googleapis.com/` + protoPayload.resourceName.
        let batch = format!("projects/{p}/locations/{l}/batches/probe-batch-01");
        assert_eq!(
            resource_key("gcp:dataproc/batch:Batch", &batch).as_deref(),
            Some(format!("//dataproc.googleapis.com/{batch}").as_str())
        );
        assert_eq!(
            resource_key("gcp:batch:Job", &format!("projects/{p}/locations/{l}/jobs/probe-batch")).as_deref(),
            Some("//batch.googleapis.com/projects/sample-proj/locations/northamerica-northeast1/jobs/probe-batch")
        );
        // A BigQuery job, from protoPayload.resourceName + resource location.
        assert_eq!(
            bq_job_resource_key("Sample-Proj", "US", "job_abc123").as_deref(),
            Some("//bigquery.googleapis.com/projects/sample-proj/locations/US/jobs/job_abc123")
        );
        assert_eq!(bq_job_resource_key("p", "US", "a/b"), None);
    }

    #[test]
    fn a_bare_provider_id_is_built_from_literal_properties() {
        let props = [
            ("project", "P"),
            ("region", "us-central1"),
            ("name", "nightly"),
        ];
        assert_eq!(
            resource_key_from_properties("gcp:cloudscheduler:Job", props).as_deref(),
            Some("//cloudscheduler.googleapis.com/projects/p/locations/us-central1/jobs/nightly")
        );
        let table = [("project", "p"), ("datasetId", "d"), ("tableId", "t")];
        assert_eq!(
            resource_key_from_properties("gcp:bigquery:Table", table),
            resource_key("gcp:bigquery:Table", "projects/p/datasets/d/tables/t")
        );
        assert_eq!(
            resource_key_from_properties("gcp:storage:Bucket", [("name", "b")]).as_deref(),
            Some("//storage.googleapis.com/projects/_/buckets/b")
        );
        assert_eq!(
            resource_key_from_properties("gcp:workflows:Workflow", [("name", "w")]),
            None
        );
        assert_eq!(
            resource_key_from_properties(
                "gcp:bigquery:DataTransferConfig",
                [("project", "p"), ("location", "us")]
            ),
            None
        );
    }

    #[test]
    fn a_cloud_run_v1_service_keys_as_the_v2_service_it_is() {
        let v1 = resource_key(
            "gcp:cloudrun/service:Service",
            "locations/us-central1/namespaces/Sample-Proj/services/api",
        );
        assert_eq!(
            v1.as_deref(),
            Some("//run.googleapis.com/projects/sample-proj/locations/us-central1/services/api")
        );
        assert_eq!(
            v1,
            resource_key(
                "gcp:cloudrunv2/service:Service",
                "projects/sample-proj/locations/us-central1/services/api"
            )
        );
        assert_eq!(
            resource_key_from_properties(
                "gcp:cloudrun:Service",
                [
                    ("project", "sample-proj"),
                    ("location", "us-central1"),
                    ("name", "api")
                ]
            ),
            v1
        );
        for id in [
            // The v2 spelling is not a v1 id.
            "projects/p/locations/l/services/s",
            "locations/l/namespaces/p/services",
            "locations/l/namespaces/p/services/s/revisions/r",
            "locations/l/namespace/p/services/s",
            // A project number is not a project id: no key, never a guess.
            "locations/l/namespaces/123456789012/services/s",
            "locations/../namespaces/p/services/s",
            "locations/l/namespaces/p/services/a b",
        ] {
            assert_eq!(resource_key("gcp:cloudrun:Service", id), None, "{id}");
        }
    }

    #[test]
    fn a_bigquery_job_takes_its_location_from_a_property() {
        let ty = "gcp:bigquery/job:Job";
        let want = bq_job_resource_key("sample-proj", "US", "job_abc123");
        assert_eq!(
            want.as_deref(),
            Some("//bigquery.googleapis.com/projects/sample-proj/locations/US/jobs/job_abc123")
        );
        // The provider id alone holds no location: no key, never a guess.
        assert_eq!(
            resource_key(ty, "projects/sample-proj/jobs/job_abc123"),
            None
        );
        assert_eq!(
            resource_key_with_properties(
                ty,
                "projects/Sample-Proj/jobs/job_abc123",
                [("location", "US"), ("jobId", "job_abc123")]
            ),
            want
        );
        assert_eq!(
            resource_key_with_properties(ty, "projects/sample-proj/jobs/job_abc123", []),
            None
        );
        assert_eq!(
            resource_key_with_properties(
                ty,
                "projects/sample-proj/jobs/job_abc123",
                [("location", "")]
            ),
            None
        );
        assert_eq!(
            resource_key_with_properties(
                ty,
                "projects/sample-proj/jobs/job_abc123",
                [("location", "US/../EU")]
            ),
            None
        );
        // The relative name, when a caller has it, is the key.
        assert_eq!(
            resource_key(ty, "projects/sample-proj/locations/US/jobs/job_abc123"),
            want
        );
        assert_eq!(
            resource_key_from_properties(
                ty,
                [
                    ("project", "sample-proj"),
                    ("location", "US"),
                    ("jobId", "job_abc123")
                ]
            ),
            want
        );
        assert_eq!(
            resource_key_from_properties(ty, [("project", "p"), ("jobId", "j")]),
            None
        );
        // Every other type keeps the provider-id path and falls back to
        // the properties.
        assert_eq!(
            resource_key_with_properties("gcp:pubsub:Topic", "projects/p/topics/t", []),
            resource_key("gcp:pubsub:Topic", "projects/p/topics/t")
        );
        assert_eq!(
            resource_key_with_properties("gcp:storage:Bucket", "bad/id", [("name", "b")])
                .as_deref(),
            Some("//storage.googleapis.com/projects/_/buckets/b")
        );
        assert_eq!(
            resource_key_with_properties("gcp:compute:Instance", "x", []),
            None
        );
    }

    #[test]
    fn a_service_account_is_keyed_from_its_account_id() {
        let ty = "gcp:serviceaccount/account:Account";
        let by_id = resource_key(
            ty,
            "projects/sample-proj/serviceAccounts/runner@sample-proj.iam.gserviceaccount.com",
        );
        assert_eq!(
            by_id.as_deref(),
            Some("//iam.googleapis.com/projects/sample-proj/serviceAccounts/runner@sample-proj.iam.gserviceaccount.com")
        );
        assert_eq!(
            resource_key_from_properties(ty, [("accountId", "runner"), ("project", "Sample-Proj")]),
            by_id
        );
        // An explicit email wins over the derived one.
        assert_eq!(
            resource_key_from_properties(
                ty,
                [
                    ("accountId", "other"),
                    ("project", "sample-proj"),
                    ("email", "runner@sample-proj.iam.gserviceaccount.com"),
                ]
            ),
            by_id
        );
        // A domain-scoped project's accounts live under `name.domain`.
        assert_eq!(
            resource_key_from_properties(ty, [("accountId", "runner"), ("project", "example.com:proj")]).as_deref(),
            Some("//iam.googleapis.com/projects/example.com:proj/serviceAccounts/runner@proj.example.com.iam.gserviceaccount.com")
        );
        for props in [
            [("accountId", "runner"), ("project", "")],
            [("accountId", ""), ("project", "p")],
            [("accountId", "a@b"), ("project", "p")],
            [("accountId", "a/b"), ("project", "p")],
            [("accountId", ".."), ("project", "p")],
            [("accountId", "a"), ("project", "p/q")],
            [("accountId", "a"), ("project", ":p")],
            [("accountId", "a\u{0}"), ("project", "p")],
        ] {
            assert_eq!(resource_key_from_properties(ty, props), None, "{props:?}");
        }
        assert_eq!(
            resource_key_from_properties(ty, [("accountId", "runner")]),
            None
        );
    }

    /// `//{service}.googleapis.com/` then `collection/id` pairs, every id a
    /// valid segment.
    fn assert_well_formed(k: &str) {
        let rest = k.strip_prefix("//").expect(k);
        let (host, path) = rest.split_once('/').expect(k);
        assert!(host.ends_with(".googleapis.com"), "{k}");
        let segments: Vec<&str> = path.split('/').collect();
        assert_eq!(segments.len() % 2, 0, "{k}");
        assert!(segments.iter().all(|s| valid_segment(s)), "{k}");
    }

    /// No key is ever built from a segment that could escape its place in the
    /// name: a separator, a dot segment, whitespace or a control character.
    #[test]
    fn no_key_is_built_from_an_unvalidated_segment() {
        const HOSTILE: &[&str] = &[
            "",
            ".",
            "..",
            "a/b",
            "a/../b",
            "a#b",
            "a?b",
            "a`b",
            "a b",
            "a\tb",
            "a\nb",
            "a\u{0}b",
            "a\u{7f}b",
            "a\u{85}b",
            "a\u{2028}b",
        ];
        for t in TEMPLATES {
            let ty = format!("gcp:{}:{}", t.module, t.name);
            for bad in HOSTILE {
                // Each id slot of the provider id in turn.
                let n = t.collections.len();
                for slot in 0..n {
                    let ids: Vec<&str> = (0..n)
                        .map(|i| if i == slot { *bad } else { "ok" })
                        .collect();
                    let relative: Vec<String> = t
                        .collections
                        .iter()
                        .zip(&ids)
                        .map(|(c, id)| format!("{c}/{id}"))
                        .collect();
                    let id = relative.join("/");
                    assert_eq!(resource_key(&ty, &id), None, "{ty} {id:?}");
                }
                assert_eq!(resource_key(&ty, bad), None, "{ty} {bad:?}");
                for key in [
                    "project",
                    "location",
                    "region",
                    "name",
                    "datasetId",
                    "tableId",
                    "routineId",
                    "batchId",
                    "jobId",
                    "email",
                    "accountId",
                ] {
                    let mut props: Vec<(&str, &str)> = [
                        ("project", "p"),
                        ("location", "l"),
                        ("name", "n"),
                        ("datasetId", "d"),
                        ("tableId", "t"),
                        ("routineId", "r"),
                        ("batchId", "b"),
                        ("jobId", "j"),
                        ("accountId", "a"),
                    ]
                    .into_iter()
                    .filter(|(k, _)| *k != key)
                    .collect();
                    props.insert(0, (key, bad));
                    if let Some(k) = resource_key_from_properties(&ty, props.iter().copied()) {
                        assert_well_formed(&k);
                    }
                    if let Some(k) = resource_key_with_properties(
                        &ty,
                        "projects/p/jobs/j",
                        props.iter().copied(),
                    ) {
                        assert_well_formed(&k);
                    }
                }
            }
        }
        for (l, p, s) in [("..", "p", "s"), ("l", "a/b", "s"), ("l", "p", "a\u{0}")] {
            let id = format!("locations/{l}/namespaces/{p}/services/{s}");
            assert_eq!(resource_key("gcp:cloudrun:Service", &id), None, "{id:?}");
        }
    }

    #[test]
    fn the_templates_are_unique() {
        for (i, a) in TEMPLATES.iter().enumerate() {
            for b in &TEMPLATES[i + 1..] {
                assert!(!(a.module == b.module && a.name == b.name), "{a:?}");
            }
        }
    }
}
