# Copyright (c) 2024-2026 Lituus-io. All rights reserved.

"""Stack identity, backend resolution, host config, IAM and resource keys."""

import json

import pytest

from pulumi_yaml_rs import (
    bq_job_resource_key,
    bq_to_resource_key,
    resource_key_from_properties,
    resource_key_with_properties,
    export_dependency_graph,
    export_iam_graph,
    export_sql_lineage,
    resolve_cross_stack,
    resource_key,
)

PRODUCER = """\
name: producer
runtime: yaml
resources:
  ds:
    type: gcp:bigquery:Dataset
    properties:
      datasetId: d
outputs:
  datasetId: ${ds.datasetId}
"""

CONSUMER = """\
name: consumer
runtime: yaml
resources:
  base:
    type: pulumi:pulumi:StackReference
    properties:
      name: producer/prod
  shared:
    type: pulumi:pulumi:StackReference
    properties:
      name: shared/prod
  t:
    type: gcp:bigquery:Table
    properties:
      datasetId: ${base.outputs.datasetId}
"""

IAM = """\
name: proj
runtime: yaml
config:
  readers:
    type: string
  token:
    type: string
    secret: true
resources:
  sa:
    type: gcp:serviceaccount:Account
    properties:
      accountId: runner
  ds:
    type: gcp:bigquery:Dataset
    properties:
      datasetId: d
  literal:
    type: gcp:bigquery:DatasetIamMember
    properties:
      datasetId: ${ds.datasetId}
      role: roles/bigquery.dataViewer
      member: user:a@example.com
  fromConfig:
    type: gcp:bigquery:DatasetIamMember
    properties:
      datasetId: ${ds.datasetId}
      role: roles/bigquery.dataViewer
      member: group:${readers}
  fromResource:
    type: gcp:bigquery:DatasetIamMember
    properties:
      datasetId: ${ds.datasetId}
      role: roles/bigquery.dataEditor
      member: serviceAccount:${sa.email}
"""


def _node(graph, logical):
    return next(n for n in graph["nodes"] if n["logical_name"] == logical)


def _project(tmp_path, name, source, extras=None):
    d = tmp_path / name
    d.mkdir()
    (d / "Pulumi.yaml").write_text(source)
    for fname, content in (extras or {}).items():
        (d / fname).write_text(content)
    return str(d)


class TestIdentity:
    def test_overrides_rename_ids_and_uris(self, tmp_path):
        d = _project(tmp_path, "consumer", CONSUMER)
        g = export_dependency_graph(
            d, "dev", backend_url="gs://state",
            project_override="svc", stack_override="svc",
        )
        assert g["project"] == "svc" and g["stack"] == "svc"
        assert g["stack_uri"] == "gs://state/.pulumi/stacks/svc/svc.json"
        assert _node(g, "t")["id"] == "urn:pulumi:svc::svc::gcp:bigquery/table:Table::t"

    def test_reference_backends_are_exact(self, tmp_path):
        d = _project(tmp_path, "consumer", CONSUMER)
        g = export_dependency_graph(
            d, "dev", backend_url="gs://state",
            reference_backends={"shared/prod": "gs://elsewhere/team"},
        )
        shared, base = _node(g, "shared"), _node(g, "base")
        assert shared["referenced_stack_uri"] == "gs://elsewhere/team/.pulumi/stacks/shared/prod.json"
        assert shared["resolved"] == "exact"
        assert base["referenced_stack_uri"] == "gs://state/.pulumi/stacks/producer/prod.json"
        assert base["resolved"] == "literal"

    def test_cross_stack_join(self, tmp_path):
        p = export_dependency_graph(_project(tmp_path, "p", PRODUCER), "prod", backend_url="gs://s")
        c = export_dependency_graph(_project(tmp_path, "c", CONSUMER), "prod", backend_url="gs://s")
        links = resolve_cross_stack([p, c])
        assert links == [{
            "consumer_edge": {
                "source_id": "urn:pulumi:prod::consumer::gcp:bigquery/table:Table::t",
                "target_id": "stackoutput::organization/producer/prod::datasetId",
            },
            "exporter_resource_id": "urn:pulumi:prod::producer::gcp:bigquery/dataset:Dataset::ds",
            "output_key": "datasetId",
        }]
        assert resolve_cross_stack([]) == []
        assert resolve_cross_stack([{"nodes": []}]) == []


class TestBackendResolution:
    def test_env_then_project_then_workspace(self, tmp_path, monkeypatch):
        bare = _project(tmp_path, "bare", PRODUCER)
        assert "backend_source" not in export_dependency_graph(bare, "dev")

        home = tmp_path / "home"
        home.mkdir()
        (home / "credentials.json").write_text(json.dumps({
            "current": "gs://workspace-state",
            "accessTokens": {"https://api.pulumi.com": "pul-CANARY"},
        }))
        monkeypatch.setenv("PULUMI_HOME", str(home))
        ws = export_dependency_graph(bare, "dev")
        assert ws["backend_source"] == "workspace"
        assert ws["stack_uri"] == "gs://workspace-state/.pulumi/stacks/producer/dev.json"
        assert "CANARY" not in json.dumps(ws)

        with_project = _project(
            tmp_path, "withproj", PRODUCER + "backend:\n  url: gs://project-state\n")
        assert export_dependency_graph(with_project, "dev")["backend_source"] == "project"

        monkeypatch.setenv("PULUMI_BACKEND_URL", "gs://env-state")
        env = export_dependency_graph(with_project, "dev")
        assert env["backend_source"] == "env"
        assert env["stack_uri"].startswith("gs://env-state/")

        arg = export_dependency_graph(with_project, "dev", backend_url="gs://arg")
        assert arg["backend_source"] == "argument"
        off = export_dependency_graph(with_project, "dev", backend_url="")
        assert "stack_uri" not in off and "backend_source" not in off

    def test_a_credentialed_backend_never_reaches_a_uri(self, tmp_path, monkeypatch):
        monkeypatch.setenv("PULUMI_BACKEND_URL", "gs://user:s3cret@bucket")
        g = export_dependency_graph(_project(tmp_path, "p", PRODUCER), "dev")
        assert "s3cret" not in json.dumps(g)
        assert "stack_uri" not in g


class TestHostConfig:
    def test_config_resolves_and_is_marked(self, tmp_path):
        d = _project(tmp_path, "iam", IAM)
        plain = export_dependency_graph(d, "dev")
        assert "config_properties" not in _node(plain, "fromConfig")
        g = export_dependency_graph(d, "dev", config={"proj:readers": "r@example.com"})
        node = _node(g, "fromConfig")
        assert node["literal_properties"]["member"] == "group:r@example.com"
        assert node["config_properties"] == ["member"]

    def test_secure_config_is_refused(self, tmp_path):
        d = _project(tmp_path, "iam", IAM)
        with pytest.raises(ValueError, match="secure"):
            export_dependency_graph(d, "dev", config={"proj:token": {"secure": "v1:abc"}})
        with pytest.raises(ValueError, match="secure"):
            export_iam_graph(d, "dev", config={"proj:token": {"secure": "v1:abc"}})

    def test_scalars_are_rendered_like_the_stack_file(self, tmp_path):
        src = "name: p\nruntime: yaml\nconfig:\n  n:\n    type: integer\n  b:\n    type: boolean\nresources:\n  r:\n    type: gcp:storage:Bucket\n    properties:\n      n: ${n}\n      b: ${b}\n"
        g = export_dependency_graph(_project(tmp_path, "p", src), "dev", config={"p:n": 7, "p:b": False})
        assert _node(g, "r")["literal_properties"] == {"b": "false", "n": "7"}

    def test_sql_lineage_takes_config(self, tmp_path):
        if export_sql_lineage is None:
            pytest.skip("sql-lineage feature not built")
        src = "name: p\nruntime: yaml\nconfig:\n  gcp:project:\n    type: string\n  ds:\n    type: string\nresources:\n  t:\n    type: gcp:bigquery:Table\n    properties:\n      datasetId: ${ds}\n      tableId: events\n"
        d = _project(tmp_path, "p", src)
        without = export_sql_lineage(d, "dev")
        assert not any(n["id"].startswith("bq://") and n["kind"] == "table" for n in without["nodes"])
        with_cfg = export_sql_lineage(d, "dev", config={"gcp:project": "Data-Proj", "p:ds": "raw"})
        ids = {n["id"] for n in with_cfg["nodes"]}
        assert "bq://data-proj/raw/events" in ids


class TestIamGraph:
    def test_every_member_is_accounted_for(self, tmp_path):
        d = _project(tmp_path, "iam", IAM)
        without = export_iam_graph(d, "dev")
        assert without["summary"]["members"] == 3
        assert without["summary"]["member_unresolved"] == 1
        g = export_iam_graph(d, "dev", config={"proj:readers": "R@Example.com"})
        s = g["summary"]
        assert (s["member_resolved"], s["member_from_resource"], s["member_unresolved"]) == (2, 1, 0)
        assert s["member_from_config"] == 1
        sources = {e["member_source"] for e in g["edges"]}
        assert sources == {"literal", "config", "resource"}
        ids = {n["id"] for n in g["nodes"]}
        assert "principal::group:r@example.com" in ids
        assert "role::roles/bigquery.dataEditor" in ids
        sa_edge = next(e for e in g["edges"] if e["member_source"] == "resource")
        assert sa_edge["source_id"] == "urn:pulumi:dev::proj::gcp:serviceaccount/account:Account::sa"
        assert sa_edge["target_id"] == "urn:pulumi:dev::proj::gcp:bigquery/dataset:Dataset::ds"


class TestResourceKeys:
    @pytest.mark.parametrize("ptype,pid,want", [
        ("gcp:bigquery:Table", "projects/P/datasets/d/tables/t$1", "//bigquery.googleapis.com/projects/p/datasets/d/tables/t"),
        ("gcp:workflows:Workflow", "projects/p/locations/l/workflows/w", "//workflows.googleapis.com/projects/p/locations/l/workflows/w"),
        ("gcp:storage:Bucket", "b", "//storage.googleapis.com/projects/_/buckets/b"),
        ("gcp:compute:Instance", "projects/p/zones/z/instances/i", None),
        ("gcp:bigquery:Table", "projects/p/datasets/d", None),
        ("gcp:cloudrun/service:Service", "locations/l/namespaces/P/services/s", "//run.googleapis.com/projects/p/locations/l/services/s"),
        ("gcp:cloudrun:Service", "locations/l/namespaces/123456/services/s", None),
        ("gcp:bigquery/job:Job", "projects/p/jobs/j", None),
    ])
    def test_resource_key(self, ptype, pid, want):
        assert resource_key(ptype, pid) == want

    def test_bq_ids_agree_with_resource_keys(self):
        assert bq_to_resource_key("bq://p/d/t#c") == resource_key("gcp:bigquery:Table", "projects/p/datasets/d/tables/t")
        assert bq_to_resource_key("example.com:p.d.t") == "//bigquery.googleapis.com/projects/example.com:p/datasets/d/tables/t"
        assert bq_to_resource_key("nonsense") is None

    def test_keys_from_properties_and_jobs(self):
        assert resource_key_from_properties(
            "gcp:cloudscheduler:Job", {"project": "p", "region": "l", "name": "j"}
        ) == "//cloudscheduler.googleapis.com/projects/p/locations/l/jobs/j"
        assert resource_key_from_properties("gcp:workflows:Workflow", {"name": "w"}) is None
        assert bq_job_resource_key("P", "US", "job_1") == "//bigquery.googleapis.com/projects/p/locations/US/jobs/job_1"

    def test_keys_for_the_added_types(self):
        job = "//bigquery.googleapis.com/projects/p/locations/US/jobs/j"
        assert resource_key_with_properties("gcp:bigquery/job:Job", "projects/P/jobs/j", {"location": "US"}) == job
        assert resource_key_with_properties("gcp:bigquery/job:Job", "projects/p/jobs/j", {}) is None
        assert job == bq_job_resource_key("p", "US", "j")
        assert resource_key_with_properties(
            "gcp:cloudrun:Service", "locations/l/namespaces/p/services/s", {}
        ) == resource_key("gcp:cloudrunv2:Service", "projects/p/locations/l/services/s")
        sa = "//iam.googleapis.com/projects/p/serviceAccounts/runner@p.iam.gserviceaccount.com"
        assert resource_key_from_properties("gcp:serviceaccount:Account", {"accountId": "runner", "project": "p"}) == sa
        assert resource_key("gcp:serviceaccount:Account", "projects/p/serviceAccounts/runner@p.iam.gserviceaccount.com") == sa
        assert resource_key_from_properties("gcp:serviceaccount:Account", {"accountId": "a/b", "project": "p"}) is None
