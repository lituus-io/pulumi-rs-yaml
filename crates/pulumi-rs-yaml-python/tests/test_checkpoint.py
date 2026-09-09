# Copyright (c) 2024-2026 Lituus-io. All rights reserved.

"""Tests for index_checkpoint / index_checkpoints — the checkpoint reader.

The contract under test is a refusal contract. These functions feed an
ownership gate, where an empty answer and an unread document are the same
value and the second one authorises a delete. So every case that cannot be
read with certainty must raise (or, in a batch, occupy its own slot with an
`error` key) rather than come back empty.

The batch is also required not to block: the caller reads a bucket from a
thread pool, and a parse that held the GIL would serialise every one of
those threads behind it.
"""

import json
import threading
import time

import pytest

from pulumi_yaml_rs import index_checkpoint, index_checkpoints

URN = "urn:pulumi:dev::app::gcp:workflows/workflow:Workflow::w"
ID = "projects/p/locations/l/workflows/w"


def _resource(urn=URN, rid=ID):
    return {"urn": urn, "id": rid, "type": "gcp:workflows/workflow:Workflow"}


DISK = json.dumps(
    {"version": 3, "checkpoint": {"latest": {"resources": [_resource()]}}}
).encode()

EXPORT = json.dumps({"version": 3, "deployment": {"resources": [_resource()]}}).encode()

NEVER_DEPLOYED = json.dumps({"version": 3, "checkpoint": {"latest": None}}).encode()


class TestIndexCheckpoint:
    def test_reads_the_on_disk_shape(self):
        assert index_checkpoint(DISK) == {"shape": "resources", "entries": [(ID, URN)]}

    def test_reads_the_exported_shape(self):
        assert index_checkpoint(EXPORT) == {"shape": "resources", "entries": [(ID, URN)]}

    def test_a_stack_that_never_deployed_is_empty(self):
        assert index_checkpoint(NEVER_DEPLOYED) == {"shape": "empty", "entries": []}

    def test_entries_are_id_urn_tuples(self):
        [entry] = index_checkpoint(DISK)["entries"]
        assert isinstance(entry, tuple)
        assert entry == (ID, URN)

    def test_a_full_id_target_keeps_it(self):
        assert index_checkpoint(DISK, [ID])["entries"] == [(ID, URN)]

    def test_a_leaf_target_keeps_it(self):
        assert index_checkpoint(DISK, ["w"])["entries"] == [(ID, URN)]

    def test_an_unrelated_target_drops_it(self):
        assert index_checkpoint(DISK, ["elsewhere"])["entries"] == []

    def test_an_empty_target_list_keeps_nothing(self):
        assert index_checkpoint(DISK, [])["entries"] == []

    @pytest.mark.parametrize(
        "data",
        [
            pytest.param(b"{}", id="no version"),
            pytest.param(b"not json", id="not json"),
            pytest.param(b'{"version": 4, "deployment": {}}', id="version 4"),
            pytest.param(b'{"version": 3}', id="no deployment"),
            pytest.param(
                b'{"version": 3, "checkpoint": {}, "deployment": {}}', id="both keys"
            ),
        ],
    )
    def test_an_unreadable_document_raises(self, data):
        with pytest.raises(ValueError, match=r"^Not a Pulumi checkpoint: "):
            index_checkpoint(data)

    @pytest.mark.parametrize("data", ["a str", bytearray(DISK), None, 7])
    def test_a_non_bytes_argument_is_a_type_error(self, data):
        with pytest.raises(TypeError):
            index_checkpoint(data)

    def test_an_escaped_id_is_decoded(self):
        doc = (
            '{"version":3,"checkpoint":{"latest":{"resources":'
            '[{"urn":"' + URN + '","id":"a\\/b"}]}}}'
        ).encode()
        assert index_checkpoint(doc)["entries"] == [("a/b", URN)]


class TestIndexCheckpoints:
    def test_one_result_per_input_in_order(self):
        out = index_checkpoints([DISK, EXPORT, NEVER_DEPLOYED])
        assert [r["shape"] for r in out] == ["resources", "resources", "empty"]

    def test_a_bad_document_occupies_its_own_slot(self):
        out = index_checkpoints([DISK, b"{", DISK])
        assert out[0]["entries"] == [(ID, URN)]
        assert out[1]["error"].startswith("Not a Pulumi checkpoint: ")
        assert "entries" not in out[1]
        assert out[2]["entries"] == [(ID, URN)]

    @pytest.mark.parametrize("parallel", [0, 1, 8])
    def test_every_parallelism_agrees(self, parallel):
        docs = [DISK, b"{", EXPORT, NEVER_DEPLOYED] * 5
        assert index_checkpoints(docs, None, parallel) == index_checkpoints(docs, None, 1)

    def test_targets_apply_to_the_whole_batch(self):
        out = index_checkpoints([DISK, EXPORT], ["w"])
        assert all(r["entries"] == [(ID, URN)] for r in out)

    def test_an_empty_batch_is_an_empty_list(self):
        assert index_checkpoints([]) == []

    def test_three_thousand_documents_are_read_in_under_two_seconds(self):
        docs = [DISK] * 3000
        started = time.perf_counter()
        out = index_checkpoints(docs, None, 8)
        elapsed = time.perf_counter() - started
        assert len(out) == 3000
        assert elapsed < 2.0, f"3,000 documents took {elapsed:.2f}s"


class TestReleasesTheGil:
    """No Python thread ever waits on a parse.

    The consumer reads a bucket from a 32-thread pool. If a parse held the
    GIL, those threads would queue behind one another and the concurrency
    would be decorative. The observable form of the contract: another thread
    keeps running while the batch is being read.
    """

    def test_another_thread_makes_progress_during_a_batch(self):
        counter = 0
        stop = threading.Event()

        def spin():
            nonlocal counter
            while not stop.is_set():
                counter += 1

        # A big enough document that the parse dominates the call.
        big = json.dumps(
            {
                "version": 3,
                "checkpoint": {
                    "latest": {
                        "resources": [
                            _resource(rid=f"{ID}-{i}", urn=f"{URN}-{i}")
                            for i in range(100)
                        ]
                    }
                },
            }
        ).encode()
        docs = [big] * 3000

        worker = threading.Thread(target=spin, daemon=True)
        worker.start()
        try:
            # Let the spinner establish a baseline rate before the parse.
            time.sleep(0.05)
            before = counter
            started = time.perf_counter()
            out = index_checkpoints(docs, None, 8)
            elapsed = time.perf_counter() - started
            during = counter - before
        finally:
            stop.set()
            worker.join(timeout=5)

        assert len(out) == 3000
        assert elapsed > 0.01, "the batch was too fast to prove anything"
        assert during > 0, (
            "the counter did not advance while 3,000 documents were indexed: "
            "the GIL was held for the parse"
        )


ACCESS_TYPE = "gcp:bigquery/datasetAccess:DatasetAccess"
ACCESS_ID = "projects/p/datasets/d"


def _access(name, role, member, view=None):
    """One element of a dataset's access array: its id is the dataset's."""
    inputs = {
        "__defaults": [],
        "datasetId": "d",
        "project": "p",
        "role": role,
        "userByEmail": member,
    }
    if view is not None:
        inputs["view"] = view
    return {
        "urn": f"urn:pulumi:dev::app::{ACCESS_TYPE}::{name}",
        "id": ACCESS_ID,
        "type": ACCESS_TYPE,
        "inputs": inputs,
        "outputs": {"role": role},
    }


def _doc(*resources):
    return json.dumps(
        {"version": 3, "checkpoint": {"latest": {"resources": list(resources)}}}
    ).encode()


ELEMENTS = {ACCESS_TYPE: ["role", "userByEmail", "view", "authorizedDataset"]}


class TestElements:
    """Telling two members of one parent array apart.

    Every entry of the array carries the parent's id, so a caller that
    indexes ids alone sees one resource where there are several. Asking for
    an element by type projects the requested `inputs` keys out of each one —
    and asking is the only way to get it: a scan that names no type is the
    scan there has always been, tuple for tuple.
    """

    def test_a_requested_type_carries_its_element(self):
        out = index_checkpoint(_doc(_access("a", "READER", "one@example.com")), None, ELEMENTS)
        [(rid, _urn, element)] = out["entries"]
        assert rid == ACCESS_ID
        assert element == {"role": "READER", "userByEmail": "one@example.com"}

    def test_only_the_requested_keys_come_back(self):
        out = index_checkpoint(_doc(_access("a", "READER", "one@example.com")), None, ELEMENTS)
        [(_rid, _urn, element)] = out["entries"]
        assert "__defaults" not in element
        assert "datasetId" not in element and "project" not in element

    def test_two_entries_on_one_id_are_told_apart(self):
        out = index_checkpoint(
            _doc(_access("a", "READER", "one@example.com"),
                 _access("b", "WRITER", "two@example.com")),
            None, ELEMENTS,
        )
        ids = [rid for rid, _urn, _el in out["entries"]]
        elements = [el for _rid, _urn, el in out["entries"]]
        assert ids == [ACCESS_ID, ACCESS_ID], "one parent, two rows"
        assert elements[0] != elements[1]

    def test_a_nested_value_round_trips_as_python_objects(self):
        view = {"projectId": "p", "datasetId": "d", "tableId": "t", "__defaults": []}
        out = index_checkpoint(
            _doc(_access("a", "READER", "one@example.com", view=view)), None, ELEMENTS)
        [(_rid, _urn, element)] = out["entries"]
        assert element["view"] == view, "nested objects arrive whole, not as text"

    def test_an_unrequested_type_has_no_element(self):
        out = index_checkpoint(DISK, None, ELEMENTS)
        assert out["entries"] == [(ID, URN, None)]

    def test_a_requested_type_without_inputs_has_no_element(self):
        bare = {"urn": f"urn:pulumi:dev::app::{ACCESS_TYPE}::a", "id": ACCESS_ID,
                "type": ACCESS_TYPE}
        out = index_checkpoint(_doc(bare), None, ELEMENTS)
        assert out["entries"] == [(ACCESS_ID, bare["urn"], None)]

    def test_asking_for_nothing_is_the_scan_it_always_was(self):
        """The property an older caller on a newer reader depends on."""
        doc = _doc(_access("a", "READER", "one@example.com"))
        assert index_checkpoint(doc) == index_checkpoint(doc, None, None)
        [entry] = index_checkpoint(doc)["entries"]
        assert entry == (ACCESS_ID, f"urn:pulumi:dev::app::{ACCESS_TYPE}::a")
        assert len(entry) == 2

    def test_an_empty_spec_is_also_that_scan(self):
        doc = _doc(_access("a", "READER", "one@example.com"))
        assert index_checkpoint(doc, None, {})["entries"] == [
            (ACCESS_ID, f"urn:pulumi:dev::app::{ACCESS_TYPE}::a", None)
        ]

    def test_inputs_that_are_not_an_object_raise_for_a_requested_type(self):
        broken = {"urn": f"urn:pulumi:dev::app::{ACCESS_TYPE}::a", "id": ACCESS_ID,
                  "type": ACCESS_TYPE, "inputs": ["role"]}
        with pytest.raises(ValueError, match=r"^Not a Pulumi checkpoint: "):
            index_checkpoint(_doc(broken), None, ELEMENTS)
        # …and never for a scan that did not ask.
        assert len(index_checkpoint(_doc(broken))["entries"]) == 1

    def test_targets_still_apply_first(self):
        out = index_checkpoint(
            _doc(_access("a", "READER", "one@example.com"), _resource()),
            [ID], ELEMENTS,
        )
        assert out["entries"] == [(ID, URN, None)]

    @pytest.mark.parametrize("parallel", [1, 4])
    def test_a_batch_projects_on_every_slot(self, parallel):
        doc = _doc(_access("a", "READER", "one@example.com"))
        out = index_checkpoints([doc, b"{", doc], None, parallel, ELEMENTS)
        assert out[0] == index_checkpoint(doc, None, ELEMENTS)
        assert out[1]["error"].startswith("Not a Pulumi checkpoint: ")
        assert out[2] == out[0]

    def test_a_batch_that_asks_for_nothing_is_unchanged(self):
        docs = [DISK, EXPORT]
        assert index_checkpoints(docs) == index_checkpoints(docs, None, 0, None)
        assert all(r["entries"] == [(ID, URN)] for r in index_checkpoints(docs))
