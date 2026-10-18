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

/// One supported type: the Pulumi type module and name, the service, and the
/// collections the provider id walks (`projects/{p}/datasets/{d}` is
/// `["projects", "datasets"]`).
#[derive(Debug, Clone, Copy)]
pub struct KeyTemplate {
    /// `module/name` as it appears in the canonical token
    /// `gcp:{module}/{camel}:{Name}` -- compared on the module and the name.
    pub module: &'static str,
    pub name: &'static str,
    pub service: &'static str,
    pub collections: &'static [&'static str],
}

/// Every type [`resource_key`] answers for.
pub const TEMPLATES: &[KeyTemplate] = &[
    KeyTemplate {
        module: "bigquery",
        name: "Dataset",
        service: "bigquery",
        collections: &["projects", "datasets"],
    },
    KeyTemplate {
        module: "bigquery",
        name: "Table",
        service: "bigquery",
        collections: &["projects", "datasets", "tables"],
    },
    KeyTemplate {
        module: "bigquery",
        name: "Routine",
        service: "bigquery",
        collections: &["projects", "datasets", "routines"],
    },
    KeyTemplate {
        module: "bigquery",
        name: "DataTransferConfig",
        service: "bigquerydatatransfer",
        collections: &["projects", "locations", "transferConfigs"],
    },
    KeyTemplate {
        module: "workflows",
        name: "Workflow",
        service: "workflows",
        collections: &["projects", "locations", "workflows"],
    },
    KeyTemplate {
        module: "cloudscheduler",
        name: "Job",
        service: "cloudscheduler",
        collections: &["projects", "locations", "jobs"],
    },
    KeyTemplate {
        module: "cloudrunv2",
        name: "Service",
        service: "run",
        collections: &["projects", "locations", "services"],
    },
    KeyTemplate {
        module: "cloudrunv2",
        name: "Job",
        service: "run",
        collections: &["projects", "locations", "jobs"],
    },
    KeyTemplate {
        module: "storage",
        name: "Bucket",
        service: "storage",
        collections: &["buckets"],
    },
    KeyTemplate {
        module: "pubsub",
        name: "Topic",
        service: "pubsub",
        collections: &["projects", "topics"],
    },
    KeyTemplate {
        module: "pubsub",
        name: "Subscription",
        service: "pubsub",
        collections: &["projects", "subscriptions"],
    },
    KeyTemplate {
        module: "dataproc",
        name: "Batch",
        service: "dataproc",
        collections: &["projects", "locations", "batches"],
    },
    KeyTemplate {
        module: "batch",
        name: "Job",
        service: "batch",
        collections: &["projects", "locations", "jobs"],
    },
    KeyTemplate {
        module: "serviceaccount",
        name: "Account",
        service: "iam",
        collections: &["projects", "serviceAccounts"],
    },
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

/// `//{service}.googleapis.com/{relative-name}` for a deployed resource, from
/// its Pulumi type and the provider id the deploy recorded. `None` for a type
/// not in [`TEMPLATES`] or an id that does not have the type's shape.
///
/// A storage bucket's provider id is its bare name, and its key is
/// `//storage.googleapis.com/projects/_/buckets/{name}` (Cloud Asset
/// Inventory's form: bucket names are global). Every other supported type's
/// provider id is already its relative name.
pub fn resource_key(pulumi_type: &str, provider_id: &str) -> Option<String> {
    let t = template_for(pulumi_type)?;
    if t.collections == ["buckets"] {
        let name = provider_id
            .strip_prefix("projects/_/buckets/")
            .unwrap_or(provider_id);
        if !valid_segment(name) {
            return None;
        }
        return Some(format!(
            "//storage.googleapis.com/projects/_/buckets/{}",
            name
        ));
    }
    let segments: Vec<&str> = provider_id.split('/').collect();
    if segments.len() != t.collections.len() * 2 {
        return None;
    }
    let mut out = String::with_capacity(provider_id.len() + t.service.len() + 20);
    out.push_str("//");
    out.push_str(t.service);
    out.push_str(".googleapis.com");
    for (i, pair) in segments.chunks_exact(2).enumerate() {
        let (collection, id) = (pair[0], pair[1]);
        if collection != t.collections[i] || !valid_segment(id) {
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
    Some(out)
}

/// The key built from a resource's literal properties, for when the provider
/// id is not the relative name (a bare name), or before anything is deployed.
/// Each collection reads its usual property: `projects` <- `project`,
/// `locations` <- `location` or `region`, `datasets` <- `datasetId`,
/// `tables` <- `tableId`, `routines` <- `routineId`, `batches` <- `batchId`,
/// a service account <- `email`, everything else <- `name`. A transfer
/// config's id is assigned by the server, so it has no key before deploy.
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
    let mut relative = String::new();
    for collection in t.collections {
        let value = match *collection {
            "projects" => get(&["project"]),
            "locations" => get(&["location", "region"]),
            "datasets" => get(&["datasetId"]),
            "tables" => get(&["tableId"]),
            "routines" => get(&["routineId"]),
            "batches" => get(&["batchId"]),
            "serviceAccounts" => get(&["email"]),
            "transferConfigs" => None,
            _ => get(&["name"]),
        }?;
        if t.collections == ["buckets"] {
            return resource_key(pulumi_type, value);
        }
        if !relative.is_empty() {
            relative.push('/');
        }
        relative.push_str(collection);
        relative.push('/');
        relative.push_str(value);
    }
    resource_key(pulumi_type, &relative)
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
    fn the_templates_are_unique() {
        for (i, a) in TEMPLATES.iter().enumerate() {
            for b in &TEMPLATES[i + 1..] {
                assert!(!(a.module == b.module && a.name == b.name), "{a:?}");
            }
        }
    }
}
