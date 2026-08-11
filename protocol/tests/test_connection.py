"""Contracts for sessions, projections, and publication."""

import json
from unittest import TestCase

from pydantic import ValidationError

from rift.generate import schema_output
from rift.models import core, mcp
from rift.models.document import DOCUMENT, RIFT_SERVICE

HEAD = "ph_" + "b" * 26
NEXT_HEAD = "ph_" + "c" * 26
SESSION = "ses_" + "d" * 26
CONNECTION = "con_" + "e" * 26


def state(*, dirty: bool = False, head: str = HEAD) -> core.ProjectionState:
    return core.ProjectionState(
        head=head,
        dirty=dirty,
    )


class ConnectionContractTests(TestCase):
    @staticmethod
    def contract() -> mcp.Contract:
        return mcp.Contract(major=1, minor=0, schema_digest="0" * 64)

    def test_connect_is_the_control_stream(self) -> None:
        connect = RIFT_SERVICE.rpcs[0]

        self.assertEqual(connect.name, "Connect")
        self.assertIs(connect.response, mcp.Connected)
        self.assertTrue(connect.response_stream)

    def test_mcp_connection_owns_one_projection(self) -> None:
        request = mcp.ConnectRequest(
            contracts=[self.contract()],
            features=[],
            role="mcp",
            session=SESSION,
            canonical_root="/workspace",
            client_build="test",
        )
        connected = mcp.Connected(
            contract=self.contract(),
            features=[],
            workspace="/workspace",
            session=SESSION,
            state=state(),
            connection=CONNECTION,
        )

        self.assertEqual(request.session.root, SESSION)
        self.assertEqual(connected.state.head.root, HEAD)

    def test_scip_connection_has_no_session(self) -> None:
        request = mcp.ConnectRequest(
            contracts=[self.contract()],
            features=[],
            role="scip",
            canonical_root="/workspace",
            client_build="test",
        )
        connected = mcp.Connected(
            contract=self.contract(),
            features=[],
            workspace="/workspace",
            connection=CONNECTION,
        )

        self.assertIsNone(request.session)
        self.assertIsNone(connected.state)

    def test_session_state_is_projection_bound(self) -> None:
        summary = mcp.SessionSummary(
            session=SESSION,
            state=state(dirty=True),
            active=False,
        )

        self.assertTrue(summary.state.dirty)


class ProjectionContractTests(TestCase):
    def test_projection_state_has_no_base_or_snapshot(self) -> None:
        self.assertEqual(
            set(core.ProjectionState.model_fields),
            {"head", "dirty"},
        )
        with self.assertRaises(ValidationError):
            core.ProjectionState.model_validate(
                {
                    "head": HEAD,
                    "dirty": False,
                    "base": "anything",
                }
            )

    def test_mutations_and_restore_expect_only_a_head(self) -> None:
        patch = mcp.PatchParams(
            expected=HEAD,
            formatting="preserve",
            patch="--- a/a.txt\n+++ b/a.txt\n",
        )
        restore = mcp.ProjectionRestoreParams(expected=HEAD)

        self.assertEqual(patch.expected.root, HEAD)
        self.assertEqual(restore.expected.root, HEAD)

    def test_publish_reports_paths_without_a_conflict_entity(self) -> None:
        published = mcp.PublishResult(
            state=state(head=NEXT_HEAD),
            conflicts=[],
        )
        refused = mcp.PublishResult(
            state=state(dirty=True),
            conflicts=["src/lib.rs"],
        )

        self.assertFalse(published.state.dirty)
        self.assertEqual([path.root for path in refused.conflicts], ["src/lib.rs"])
        with self.assertRaises(ValidationError):
            mcp.PublishResult(
                state=state(dirty=True),
                conflicts=[],
            )

    def test_workspace_resource_is_projection_scoped(self) -> None:
        uri = mcp.WorkspaceResourceUri.model_validate("rift://workspace")

        self.assertEqual(uri.root, "rift://workspace")
        with self.assertRaises(ValidationError):
            mcp.FsResourceUri.model_validate("rift://fs/a.txt?rev=git:HEAD")

    def test_filesystem_resource_reads_directories(self) -> None:
        root = core.ProjectEntry.model_validate({"kind": "directory", "path": ""})
        child = core.ProjectEntry.model_validate({"kind": "directory", "path": "src"})
        payload = mcp.FsResourcePayload(
            uri="rift://fs",
            head=HEAD,
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

    def test_generated_contract_contains_no_git_or_snapshot_types(self) -> None:
        schema = schema_output()
        definitions = set(schema["$defs"])
        serialized = json.dumps(schema).lower()

        self.assertFalse(
            definitions
            & {
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
