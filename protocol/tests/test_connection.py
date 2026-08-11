"""Contracts for sessions, projections, the changeset, and publication."""

import json
from unittest import TestCase

from pydantic import ValidationError

from rift.generate import schema_output
from rift.models import core, mcp
from rift.models.document import DOCUMENT, RIFT_SERVICE

SESSION = "ses_" + "d" * 26
CONNECTION = "con_" + "e" * 26
CHANGE = "chg_" + "f" * 26
OTHER_CHANGE = "chg_" + "g" * 26


def state(*, dirty: bool = False, unaccepted: bool = False) -> core.ProjectionState:
    return core.ProjectionState(dirty=dirty, unaccepted=unaccepted)


class ConnectionContractTests(TestCase):


    def test_connect_is_the_control_stream(self) -> None:
        connect = RIFT_SERVICE.rpcs[0]

        self.assertEqual(connect.name, "Connect")
        self.assertIs(connect.response, mcp.Connected)
        self.assertTrue(connect.response_stream)

    def test_mcp_connection_owns_one_projection(self) -> None:
        request = mcp.ConnectRequest(
            protocol_versions=["2026-08-04"],
            features=[],
            role="mcp",
            session=SESSION,
            canonical_root="/workspace",
            client_build="test",
        )
        connected = mcp.Connected(
            protocol_version="2026-08-04",
            features=[],
            workspace="/workspace",
            session=SESSION,
            state=state(),
            connection=CONNECTION,
        )

        self.assertEqual(request.session.root, SESSION)
        self.assertFalse(connected.state.dirty)

    def test_scip_connection_has_no_session(self) -> None:
        request = mcp.ConnectRequest(
            protocol_versions=["2026-08-04"],
            features=[],
            role="scip",
            canonical_root="/workspace",
            client_build="test",
        )
        connected = mcp.Connected(
            protocol_version="2026-08-04",
            features=[],
            workspace="/workspace",
            connection=CONNECTION,
        )

        self.assertIsNone(request.session)
        self.assertIsNone(connected.state)

    def test_protocol_version_is_one_dated_snapshot(self) -> None:
        core.ProtocolVersion.model_validate("2026-08-04")

        for bad in ("v1.2", "2026-8-4", "1", ""):
            with self.assertRaises(ValidationError):
                core.ProtocolVersion.model_validate(bad)


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

    def test_mutations_and_restore_name_no_expected_state(self) -> None:
        patch = mcp.PatchParams(
            formatting="preserve",
            patch="--- a/a.txt\n+++ b/a.txt\n",
        )
        restore = mcp.ProjectionRestoreParams()

        self.assertNotIn("expected", mcp.PatchParams.model_fields)
        self.assertNotIn("confirmations", mcp.PatchParams.model_fields)
        self.assertEqual(patch.formatting, core.FormattingPolicy.PRESERVE)
        self.assertIsNone(restore.paths)

    def test_publish_refuses_on_conflicts_or_unaccepted_changes(self) -> None:
        published = mcp.PublishResult(state=state(), conflicts=[], unaccepted=[])
        conflicted = mcp.PublishResult(
            state=state(dirty=True),
            conflicts=["src/lib.rs"],
            unaccepted=[],
        )
        unaccepted = mcp.PublishResult(
            state=state(dirty=True, unaccepted=True),
            conflicts=[],
            unaccepted=[CHANGE],
        )

        self.assertFalse(published.state.dirty)
        self.assertEqual([path.root for path in conflicted.conflicts], ["src/lib.rs"])
        self.assertEqual([held.root for held in unaccepted.unaccepted], [CHANGE])
        with self.assertRaises(ValidationError):
            mcp.PublishResult(state=state(dirty=True), conflicts=[], unaccepted=[])

    def test_publish_accepts_named_changes(self) -> None:
        params = mcp.PublishParams(accept=[CHANGE, OTHER_CHANGE])

        self.assertEqual(
            [accepted.root for accepted in params.accept], [CHANGE, OTHER_CHANGE]
        )
        self.assertEqual(mcp.PublishParams().accept, [])

    def test_workspace_resource_is_projection_scoped(self) -> None:
        uri = mcp.WorkspaceResourceUri.model_validate("rift://workspace")

        self.assertEqual(uri.root, "rift://workspace")
        with self.assertRaises(ValidationError):
            mcp.FsResourceUri.model_validate("rift://fs/a.txt?rev=git:HEAD")

    def test_projection_resource_addresses_the_projection_directory(self) -> None:
        payload = mcp.ProjectionResourcePayload(
            uri="rift://projection",
            path="/workspace/.rift/projections/" + SESSION,
            workspace="/workspace",
        )

        self.assertEqual(payload.uri.root, "rift://projection")
        self.assertTrue(payload.path.root.endswith(SESSION))
        with self.assertRaises(ValidationError):
            mcp.ProjectionResourceUri.model_validate("rift://projection/src")

    def test_filesystem_resource_reads_directories(self) -> None:
        root = core.ProjectEntry.model_validate({"kind": "directory", "path": ""})
        child = core.ProjectEntry.model_validate({"kind": "directory", "path": "src"})
        payload = mcp.FsResourcePayload(
            uri="rift://fs",
            entry=root,
            encoding="directory",
            entries=[child],
        )

        self.assertEqual(payload.entries, [child])

    def test_rift_state_is_outside_project_paths(self) -> None:
        with self.assertRaises(ValidationError):
            core.ProjectPath.model_validate(".rift/projections")

    def test_public_surface_contains_no_projection_mount_tools(self) -> None:
        tools = {tool.name for tool in DOCUMENT.tools}
        methods = {rpc.name for rpc in RIFT_SERVICE.rpcs}

        self.assertIn("publish", tools)
        self.assertNotIn("projection_open", tools)
        self.assertNotIn("projection_close", tools)
        self.assertNotIn("ProjectionOpen", methods)
        self.assertNotIn("ProjectionClose", methods)

    def test_projection_and_changes_are_advertised_resources(self) -> None:
        resources = {resource.name for resource in DOCUMENT.resources}

        self.assertIn("projection", resources)
        self.assertIn("changes", resources)


class ChangesetContractTests(TestCase):
    def test_refusal_reasons_all_mean_no_edits_were_produced(self) -> None:
        reasons = {reason.value for reason in core.RefusalReason}

        self.assertNotIn("confirmation_required", reasons)
        self.assertNotIn("validation_incomplete", reasons)
        self.assertIn("cardinality_mismatch", reasons)
        self.assertIn("ambiguous_target", reasons)

    def test_refusal_carries_no_confirmations(self) -> None:
        self.assertNotIn("confirmations", mcp.RefusedResult.model_fields)

    def test_an_unvouched_change_lands_carrying_its_confirmations(self) -> None:
        self.assertIn("confirmations", mcp.ChangeSummary.model_fields)
        self.assertIn("id", mcp.ChangeSummary.model_fields)

        kinds = {kind.value for kind in core.ConfirmationRequirementKind}
        self.assertIn("validation_incomplete", kinds)
        self.assertIn("guarantee_unestablished", kinds)

    def test_changes_resource_pages_the_changeset(self) -> None:
        uri = mcp.ChangesResourceUri.model_validate("rift://changes")

        self.assertEqual(uri.root, "rift://changes")
        self.assertIn("changes", mcp.ChangesResourcePayload.model_fields)
        self.assertIn("next", mcp.ChangesResourcePayload.model_fields)

    def test_generated_contract_contains_no_head_or_snapshot_types(self) -> None:
        schema = schema_output()
        definitions = set(schema["$defs"])
        serialized = json.dumps(schema).lower()

        self.assertFalse(
            definitions
            & {
                "ProjectionHead",
                "Snapshot",
                "SnapshotId",
                "GitCommit",
                "GitRevision",
                "IntegrateParams",
                "RecoveryManifest",
                "ProjectionOpenParams",
                "ProjectionCloseParams",
            }
        )
        self.assertNotIn("projection_open", serialized)
        self.assertNotIn("projection_close", serialized)
        self.assertNotIn("projection_head_moved", serialized)
        self.assertNotIn("refresh_projection", serialized)
