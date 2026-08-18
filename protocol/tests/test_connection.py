"""Contracts for the server lock, projections, the changeset, and publication."""

import json
from unittest import TestCase

from pydantic import ValidationError

from rift.generate import schema_output
from rift.models import core, mcp
from rift.models.document import DOCUMENT, RIFT_SERVICE

PROJECTION = "rift://projection/prj_" + "d" * 26
CHANGE = "chg_" + "f" * 26
OTHER_CHANGE = "chg_" + "g" * 26
DIGEST = "a" * 64


def state(*, dirty: bool = False, unaccepted: bool = False) -> core.ProjectionState:
    return core.ProjectionState(dirty=dirty, unaccepted=unaccepted)


def snapshot() -> core.ReadSnapshot:
    return core.ReadSnapshot(
        tree_revision=DIGEST,
        source_revision=DIGEST,
        index=None,
    )


class ServerLockContractTests(TestCase):
    def test_lock_names_the_endpoint_and_its_token(self) -> None:
        lock = mcp.ServerLock(
            port=52341,
            pid=4242,
            token="t" * 32,
            workspace="/workspace",
        )

        self.assertEqual(lock.port, 52341)
        self.assertEqual(lock.workspace.root, "/workspace")
        with self.assertRaises(ValidationError):
            mcp.ServerLock(
                port=0,
                pid=4242,
                token="t" * 32,
                workspace="/workspace",
            )
        with self.assertRaises(ValidationError):
            mcp.ServerLock(
                port=52341,
                pid=4242,
                token="short",
                workspace="/workspace",
            )

    def test_service_holds_no_connection_stream(self) -> None:
        methods = {rpc.name for rpc in RIFT_SERVICE.rpcs}

        self.assertNotIn("Connect", methods)
        self.assertNotIn("SessionList", methods)
        self.assertNotIn("SessionContinue", methods)
        self.assertNotIn("SessionRemove", methods)


class ProjectionContractTests(TestCase):
    def test_projection_state_carries_no_concurrency_token(self) -> None:
        self.assertEqual(
            set(core.ProjectionState.model_fields),
            {"dirty", "unaccepted"},
        )
        with self.assertRaises(ValidationError):
            core.ProjectionState.model_validate(
                {
                    "dirty": False,
                    "unaccepted": False,
                    "head": "anything",
                }
            )

    def test_changes_target_the_workspace_unless_they_name_a_projection(self) -> None:
        direct = mcp.PatchParams(
            patch="--- a/a.txt\n+++ b/a.txt\n",
        )
        projected = mcp.PatchParams(
            patch="--- a/a.txt\n+++ b/a.txt\n",
            projection=PROJECTION,
        )

        self.assertIsNone(direct.projection)
        self.assertEqual(projected.projection.root, PROJECTION)
        self.assertNotIn("expected", mcp.PatchParams.model_fields)
        self.assertNotIn("confirmations", mcp.PatchParams.model_fields)

    def test_restore_and_publish_name_their_projection(self) -> None:
        restore = mcp.ProjectionRestoreParams(projection=PROJECTION)
        publish = mcp.PublishParams(projection=PROJECTION)

        self.assertIsNone(restore.paths)
        self.assertEqual(publish.projection.root, PROJECTION)
        self.assertEqual(publish.accept, [])
        self.assertEqual(publish.accept_dependencies, [])
        with self.assertRaises(ValidationError):
            mcp.PublishParams()

    def test_publish_refuses_on_conflicts_or_unaccepted_changes(self) -> None:
        published = mcp.PublishResult(
            state=state(), conflicts=[], dependency_conflicts=[], unaccepted=[]
        )
        conflicted = mcp.PublishResult(
            state=state(dirty=True),
            conflicts=["src/lib.rs"],
            dependency_conflicts=[],
            unaccepted=[],
        )
        unaccepted = mcp.PublishResult(
            state=state(dirty=True, unaccepted=True),
            conflicts=[],
            dependency_conflicts=[],
            unaccepted=[CHANGE],
        )

        self.assertFalse(published.state.dirty)
        self.assertEqual([path.root for path in conflicted.conflicts], ["src/lib.rs"])
        self.assertEqual([held.root for held in unaccepted.unaccepted], [CHANGE])
        with self.assertRaises(ValidationError):
            mcp.PublishResult(
                state=state(dirty=True),
                conflicts=[],
                dependency_conflicts=[],
                unaccepted=[],
            )

    def test_publish_accepts_named_changes(self) -> None:
        params = mcp.PublishParams(
            projection=PROJECTION, accept=[CHANGE, OTHER_CHANGE]
        )

        self.assertEqual(
            [accepted.root for accepted in params.accept], [CHANGE, OTHER_CHANGE]
        )

    def test_projection_resource_addresses_the_projection_directory(self) -> None:
        payload = mcp.ProjectionResourcePayload(
            uri=PROJECTION,
            projection=mcp.Projection(
                id=PROJECTION,
                path="/workspace/.rift/projections/prj_" + "d" * 26,
                state=state(),
                base_revision=DIGEST,
            ),
            workspace="/workspace",
        )

        self.assertEqual(payload.uri.root, PROJECTION)
        self.assertTrue(payload.projection.path.root.endswith("d" * 26))
        with self.assertRaises(ValidationError):
            mcp.ProjectionResourceUri.model_validate("rift://projection/src")

    def test_read_snapshot_correlates_index_freshness(self) -> None:
        current = core.ReadSnapshot(
            tree_revision=DIGEST,
            source_revision=DIGEST,
            index=core.IndexSnapshot(
                revision="b" * 64,
                tree_revision=DIGEST,
                source_revision=DIGEST,
                freshness="current",
            ),
        )

        self.assertEqual(current.index.freshness, core.Freshness.CURRENT)
        with self.assertRaises(ValidationError):
            core.ReadSnapshot(
                tree_revision=DIGEST,
                source_revision=DIGEST,
                index=core.IndexSnapshot(
                    revision="b" * 64,
                    tree_revision="c" * 64,
                    source_revision=DIGEST,
                    freshness="current",
                ),
            )

    def test_filesystem_resource_reads_directories(self) -> None:
        root = core.ProjectEntry.model_validate({"kind": "directory", "path": ""})
        child = core.ProjectEntry.model_validate({"kind": "directory", "path": "src"})
        payload = mcp.FsResourcePayload(
            uri="rift://fs",
            snapshot=snapshot(),
            entry=root,
            encoding="directory",
            entries=[child],
        )

        self.assertEqual(payload.entries, [child])

    def test_rift_state_is_outside_project_paths(self) -> None:
        with self.assertRaises(ValidationError):
            core.ProjectPath.model_validate(".rift/projections")

    def test_projection_lifecycle_is_explicit_tools(self) -> None:
        tools = {tool.name for tool in DOCUMENT.tools}
        methods = {rpc.name for rpc in RIFT_SERVICE.rpcs}

        self.assertIn("publish", tools)
        self.assertIn("projection_create", tools)
        self.assertIn("projection_list", tools)
        self.assertIn("projection_remove", tools)
        self.assertNotIn("session_continue", tools)
        self.assertNotIn("SessionContinue", methods)

    def test_projection_and_changes_are_advertised_resources(self) -> None:
        resources = {resource.name for resource in DOCUMENT.resources}

        self.assertIn("projection", resources)
        self.assertIn("changes", resources)


class ChangesetContractTests(TestCase):
    def test_refusal_reasons_all_mean_no_edits_were_produced(self) -> None:
        reasons = {reason.value for reason in core.RefusalReason}

        self.assertNotIn("confirmation_required", reasons)
        self.assertNotIn("validation_incomplete", reasons)
        self.assertIn("ambiguous_target", reasons)

    def test_refusal_carries_no_confirmations(self) -> None:
        self.assertNotIn("confirmations", mcp.RefusedResult.model_fields)

    def test_an_unvouched_change_lands_carrying_its_confirmations(self) -> None:
        self.assertIn("confirmations", mcp.ChangeSummary.model_fields)
        self.assertIn("advisories", mcp.ChangeSummary.model_fields)
        self.assertIn("id", mcp.ChangeSummary.model_fields)

        kinds = {kind.value for kind in core.ConfirmationRequirementKind}
        self.assertIn("hook", kinds)
        self.assertIn("advisory", kinds)
        self.assertIn("external", kinds)
        self.assertIn("origin", mcp.ChangeSummary.model_fields)
        self.assertIn("paths", mcp.ChangeSummary.model_fields)

    def test_a_checked_advisory_is_never_a_warning(self) -> None:
        checked = core.Advisory(
            code="hooks.tests",
            severity="info",
            message="checked: no other non-test file references the name",
            checked=True,
            instruction=None,
            addresses=[],
            paths=[],
        )

        self.assertTrue(checked.checked)
        with self.assertRaises(ValidationError):
            core.Advisory(
                code="hooks.tests",
                severity="warning",
                message="verified and also open",
                checked=True,
                instruction=None,
                addresses=[],
                paths=[],
            )

    def test_changes_resource_pages_one_changeset(self) -> None:
        journal = mcp.ChangesResourceUri.model_validate("rift://changes")
        projected = mcp.ChangesResourceUri.model_validate(
            "rift://changes?projection=rift%3A%2F%2Fprojection%2Fprj_" + "d" * 26
        )

        self.assertEqual(journal.root, "rift://changes")
        self.assertIn("projection=", projected.root)
        self.assertIn("changes", mcp.ChangesResourcePayload.model_fields)
        self.assertIn("next", mcp.ChangesResourcePayload.model_fields)

    def test_node_identity_carries_its_witness(self) -> None:
        core.NodeId.model_validate("rift://node/python/pkg/util.py@1204-1266#3f9a1c2e")

        with self.assertRaises(ValidationError):
            core.NodeId.model_validate("rift://node/python/pkg/util.py@1204-1266")

    def test_generated_contract_contains_no_session_or_span_edit_types(self) -> None:
        schema = schema_output()
        definitions = set(schema["$defs"])
        serialized = json.dumps(schema).lower()

        self.assertFalse(
            definitions
            & {
                "SessionId",
                "SessionSummary",
                "ConnectRequest",
                "Connected",
                "ConnectionId",
                "EditParams",
                "RewriteParams",
                "MatchCardinality",
                "ProjectionHead",
                "Snapshot",
                "SnapshotId",
            }
        )
        self.assertNotIn("session_continue", serialized)
        self.assertNotIn("session_list", serialized)
        self.assertNotIn("session_remove", serialized)
        self.assertNotIn("cardinality_mismatch", serialized)
