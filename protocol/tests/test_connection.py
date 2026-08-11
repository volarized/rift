"""Connection, session-change, and integration contracts."""

from unittest import TestCase

from jsonschema import Draft202012Validator
from jsonschema.exceptions import ValidationError as JsonSchemaValidationError
from pydantic import ValidationError

from rift.models import core, mcp
from rift.models.document import DOCUMENT, RIFT_SERVICE

COMMIT_A = "a" * 40
COMMIT_B = "b" * 40
PROJECTION = "prj_" + "a" * 26
HEAD_TOKEN = "ph_" + "b" * 26
SNAPSHOT = "snap_" + "c" * 64
BASE_SNAPSHOT = "snap_" + "d" * 64
RECOVERY = "rec_" + "d" * 26


def projection_location() -> dict[str, str]:
    return {
        "projection": PROJECTION,
        "path": f"/workspace/.rift/projections/{PROJECTION}",
    }


def projection_state(*, dirty: bool = False) -> dict[str, object]:
    return {
        "projection": PROJECTION,
        "head": HEAD_TOKEN,
        "snapshot": SNAPSHOT,
        "base": {
            "snapshot": BASE_SNAPSHOT if dirty else SNAPSHOT,
            "commit": COMMIT_A,
        },
        "dirty": dirty,
    }


def recovery() -> dict[str, object]:
    return {
        "recovery": RECOVERY,
        "worktree": {
            "path": f"/workspace/.rift/recovery/{RECOVERY}",
            "ref": f"refs/rift/recovery/{RECOVERY}",
        },
        "operation": "1" * 64,
        "expected_source": projection_state(dirty=True),
        "target": "refs/heads/main",
        "expected_target": COMMIT_A,
        "reason": "conflicts",
        "conflicts": [{"path": "src/lib.rs", "status": "both_modified"}],
        "driver_paths": [],
        "manifest": {
            "recovery": RECOVERY,
            "index": "2" * 64,
            "worktree": "3" * 64,
        },
    }


class ConnectionContractTests(TestCase):
    @staticmethod
    def contract() -> mcp.Contract:
        return mcp.Contract(major=1, minor=0, schema_digest="0" * 64)

    def test_connect_is_the_control_stream(self) -> None:
        connect = RIFT_SERVICE.rpcs[0]
        self.assertEqual(connect.name, "Connect")
        self.assertIs(connect.response, mcp.Connected)
        self.assertTrue(connect.response_stream)

    def test_mcp_requires_a_process_generated_session(self) -> None:
        common = {
            "contracts": [self.contract()],
            "features": [],
            "role": mcp.ConnectRole.MCP,
            "canonical_root": "/workspace",
            "client_build": "test",
        }
        with self.assertRaises(ValidationError):
            mcp.ConnectRequest(**common)

        connected = mcp.ConnectRequest(
            **common,
            session="ses_" + "a" * 26,
        )
        self.assertEqual(connected.session.root, "ses_" + "a" * 26)

    def test_scip_has_no_session(self) -> None:
        with self.assertRaises(ValidationError):
            mcp.ConnectRequest(
                contracts=[self.contract()],
                features=[],
                role=mcp.ConnectRole.SCIP,
                session="ses_" + "a" * 26,
                canonical_root="/workspace",
                client_build="test",
            )

    def test_json_schema_expresses_role_session_rules(self) -> None:
        validator = Draft202012Validator(mcp.ConnectRequest.model_json_schema())
        common = {
            "contracts": [{"major": 1, "minor": 0, "schema_digest": "0" * 64}],
            "features": [],
            "canonical_root": "/workspace",
            "client_build": "test",
        }

        with self.assertRaises(JsonSchemaValidationError):
            validator.validate({**common, "role": "mcp"})
        validator.validate({**common, "role": "mcp", "session": "ses_" + "a" * 26})
        validator.validate({**common, "role": "scip"})
        with self.assertRaises(JsonSchemaValidationError):
            validator.validate({**common, "role": "scip", "session": "ses_" + "a" * 26})

    def test_connected_session_always_has_a_projection(self) -> None:
        connected = mcp.Connected(
            contract=self.contract(),
            features=[],
            workspace="/workspace",
            session="ses_" + "a" * 26,
            projection=projection_location(),
            state=projection_state(),
            connection="con_" + "b" * 26,
        )

        self.assertEqual(connected.projection.projection.root, PROJECTION)
        self.assertEqual(connected.state.head.root, HEAD_TOKEN)

    def test_connected_correlates_session_state(self) -> None:
        common = {
            "contract": self.contract(),
            "features": [],
            "workspace": "/workspace",
            "connection": "con_" + "b" * 26,
        }

        with self.assertRaises(ValidationError):
            mcp.Connected(**common, session="ses_" + "a" * 26)
        with self.assertRaises(ValidationError):
            mcp.Connected(**common, projection=projection_location())

        scip = mcp.Connected(**common)
        self.assertIsNone(scip.session)

    def test_connected_schema_requires_complete_projection_state(self) -> None:
        validator = Draft202012Validator(mcp.Connected.model_json_schema())
        common = {
            "contract": {"major": 1, "minor": 0, "schema_digest": "0" * 64},
            "features": [],
            "workspace": "/workspace",
            "connection": "con_" + "b" * 26,
        }

        validator.validate(
            {
                **common,
                "session": "ses_" + "a" * 26,
                "projection": projection_location(),
                "state": projection_state(),
            }
        )
        with self.assertRaises(JsonSchemaValidationError):
            validator.validate({**common, "session": "ses_" + "a" * 26})
        with self.assertRaises(JsonSchemaValidationError):
            validator.validate({**common, "projection": projection_location()})

    def test_session_management_is_explicit_and_projection_bound(self) -> None:
        retained = mcp.SessionSummary(
            session="ses_" + "a" * 26,
            projection=projection_location(),
            state=projection_state(dirty=True),
            active=True,
        )
        continued = mcp.SessionContinueResult(session=retained)
        preview = mcp.SessionRemoveResult(
            session=retained.session,
            state=retained.state,
            projection=projection_location(),
            scratch_bytes=256,
            removed=False,
            unintegrated=True,
            reclaimable_bytes=1024,
        )

        self.assertEqual(continued.session.state.base.commit.root, COMMIT_A)
        self.assertTrue(continued.session.active)
        self.assertFalse(preview.removed)
        self.assertTrue(preview.unintegrated)
        self.assertEqual(preview.reclaimable_bytes, 1024)

        rpcs = {rpc.name for rpc in RIFT_SERVICE.rpcs}
        self.assertTrue(
            {"SessionList", "SessionContinue", "SessionRemove"}.issubset(rpcs)
        )

    def test_session_activity_is_single_owner_state(self) -> None:
        properties = mcp.SessionSummary.model_json_schema()["properties"]

        self.assertEqual(properties["active"]["type"], "boolean")
        self.assertNotIn("active_connections", properties)

    def test_revision_has_no_global_default(self) -> None:
        self.assertNotIn("default", core.Revision.model_json_schema())

    def test_checked_out_target_is_the_only_checkout_refusal(self) -> None:
        values = {reason.value for reason in mcp.RefusalReason}

        self.assertIn("checked_out_target", values)
        self.assertNotIn("dirty_target", values)

    def test_configuration_failure_has_a_stable_code(self) -> None:
        self.assertEqual(
            mcp.ErrorCode.CONFIGURATION_INVALID.value, "configuration_invalid"
        )
        self.assertEqual(
            mcp.ErrorCode.PROJECTION_HEAD_MOVED.value, "projection_head_moved"
        )
        self.assertEqual(
            mcp.ErrorCode.CAPABILITY_UNAVAILABLE.value, "capability_unavailable"
        )

    def test_projection_busy_error_identifies_the_projection(self) -> None:
        validator = Draft202012Validator(mcp.ErrorData.model_json_schema())
        common = {
            "message": "session projection has open handles",
            "retry": "operator_action",
            "phase": "change",
            "at": None,
            "operation": None,
            "diagnostics": [],
            "causes": [],
        }

        with self.assertRaises(JsonSchemaValidationError):
            validator.validate({**common, "code": "projection_busy"})
        validator.validate(
            {
                **common,
                "code": "projection_busy",
                "projection": projection_location(),
            }
        )
        with self.assertRaises(JsonSchemaValidationError):
            validator.validate(
                {
                    **common,
                    "code": "internal_error",
                    "projection": projection_location(),
                }
            )

    def test_repository_reports_availability_without_relisting_tools(self) -> None:
        properties = mcp.RepositoryResourcePayload.model_json_schema()["properties"]

        self.assertNotIn("tools", properties)
        self.assertIn("languages", properties)


class SessionChangeContractTests(TestCase):
    def test_revision_namespaces_are_tagged(self) -> None:
        git = core.Revision.model_validate({"kind": "git", "revision": "HEAD~3"})
        snapshot = core.Revision.model_validate(
            {"kind": "snapshot", "snapshot": SNAPSHOT}
        )
        projection = core.Revision.model_validate(
            {"kind": "projection", "projection": PROJECTION}
        )

        self.assertEqual(git.root.kind, "git")
        self.assertEqual(snapshot.root.snapshot.root, SNAPSHOT)
        self.assertEqual(projection.root.projection.root, PROJECTION)
        with self.assertRaises(ValidationError):
            core.Revision.model_validate("main")

    def test_projection_head_prevents_content_aba(self) -> None:
        first = core.ProjectionState.model_validate(projection_state())
        returned = core.ProjectionState.model_validate(
            {
                **projection_state(),
                "head": "ph_" + "d" * 26,
            }
        )

        self.assertEqual(first.snapshot, returned.snapshot)
        self.assertNotEqual(first.head, returned.head)

    def test_projection_dirty_flag_matches_its_single_base(self) -> None:
        with self.assertRaises(ValidationError):
            core.ProjectionState.model_validate(
                {**projection_state(dirty=True), "dirty": False}
            )

        resolved = core.ResolvedSnapshot.model_validate(
            {"snapshot": {"id": SNAPSHOT}, "imported_from": COMMIT_A}
        )
        self.assertEqual(resolved.snapshot.id.root, SNAPSHOT)
        self.assertEqual(resolved.imported_from.root, COMMIT_A)

    def test_resource_revisions_are_namespaced_and_percent_encoded(self) -> None:
        self.assertIn(
            "path_is_canonical", core.FileId.__pydantic_decorators__.model_validators
        )
        self.assertIn(
            "value_is_canonical_base64url",
            mcp.Cursor.__pydantic_decorators__.model_validators,
        )
        with self.assertRaises(ValidationError):
            mcp.FileResourceUri.model_validate("rift://file/a.rs?rev=main")
        with self.assertRaises(ValidationError):
            mcp.RepositoryResourceUri.model_validate(
                "rift://repository?rev=git:feature/one"
            )

        uri = mcp.RepositoryResourceUri.model_validate(
            "rift://repository?rev=git:feature%2Fone"
        )
        self.assertEqual(uri.root, "rift://repository?rev=git:feature%2Fone")

        for invalid in (
            "rift://file/%2E%2E",
            "rift://file/a.rs?rev=git:%6Dain",
        ):
            with self.assertRaises(ValidationError):
                core.FileId.model_validate(invalid)
        with self.assertRaises(ValidationError):
            core.NodeId.model_validate("rift://node/rust/a.rs@10-2")
        with self.assertRaises(ValidationError):
            core.NodeId.model_validate("rift://node/rust/a.rs@00-01")
        with self.assertRaises(ValidationError):
            mcp.FsResourceUri.model_validate("rift://fs?cursor=A")
        with self.assertRaises(ValidationError):
            mcp.ActionsResourceUri.model_validate("rift://actions/node/xxxxxxxx")
        with self.assertRaises(ValidationError):
            mcp.FileResourceUri.model_validate(
                "rift://file/a.rs?start=9007199254740991&length=1"
            )
        with self.assertRaises(ValidationError):
            mcp.FileResourceUri.model_validate("rift://file/a.rs?start=00&length=1")
        with self.assertRaises(ValidationError):
            mcp.ActionsResourceUri.model_validate(
                f"rift://actions/file/a.rs?only={'x' * 129}"
            )
        with self.assertRaises(ValidationError):
            core.GitRevision.model_validate("é" * 129)
        with self.assertRaises(ValidationError):
            core.ProjectPath.model_validate("é" * 501)

    def test_fs_resource_reports_live_projection_inventory(self) -> None:
        payload = mcp.FsResourcePayload.model_validate(
            {
                "uri": "rift://fs",
                "projections": [
                    {
                        "location": projection_location(),
                        "kind": "session",
                        "snapshot": {"id": SNAPSHOT},
                        "state": projection_state(dirty=True),
                        "writable": True,
                        "scratch_bytes": 512,
                        "open_handles": 2,
                        "available": True,
                        "degradation": [],
                    }
                ],
                "next": None,
            }
        )

        self.assertEqual(payload.projections[0].kind, mcp.ProjectionKind.SESSION)
        self.assertTrue(payload.projections[0].state.dirty)

    def test_mutation_requires_expected_projection_state(self) -> None:
        with self.assertRaises(ValidationError):
            mcp.PatchParams(
                formatting=core.FormattingPolicy.PRESERVE,
                patch="diff --git a/a b/a",
            )

        change = mcp.PatchParams(
            expected=projection_state(),
            formatting=core.FormattingPolicy.PRESERVE,
            patch="diff --git a/a b/a",
        )
        self.assertEqual(change.expected.head.root, HEAD_TOKEN)
        self.assertEqual(change.confirmations, [])

    def test_mutation_parameters_are_closed(self) -> None:
        with self.assertRaises(ValidationError):
            mcp.PatchParams.model_validate(
                {
                    "expected": projection_state(),
                    "formatting": "preserve",
                    "patch": "diff --git a/a b/a",
                    "unexpected": True,
                }
            )

    def test_public_surface_is_the_documented_tool_and_resource_set(self) -> None:
        tool_names = {tool.name for tool in DOCUMENT.tools}
        resource_names = {resource.name for resource in DOCUMENT.resources}

        self.assertEqual(
            tool_names,
            {
                "tree",
                "outline",
                "search",
                "match",
                "edit",
                "patch",
                "rewrite",
                "revert",
                "rename",
                "move",
                "delete",
                "change_signature",
                "act",
                "integrate",
                "execute",
                "debug_start",
                "debug_get_frame",
                "debug_stop",
                "session_list",
                "session_continue",
                "session_remove",
                "projection_open",
                "projection_close",
                "projection_restore",
                "recovery_list",
                "recovery_continue",
                "recovery_abort",
            },
        )
        self.assertEqual(
            resource_names,
            {"repository", "symbol", "diff", "file", "actions", "action", "fs"},
        )

    def test_integration_requires_the_observed_target_head(self) -> None:
        with self.assertRaises(ValidationError):
            mcp.IntegrateParams(target="refs/heads/main")

        request = mcp.IntegrateParams(
            target="refs/heads/main",
            expected=projection_state(dirty=True),
            expected_target=COMMIT_A,
            message="integrate current source",
        )
        self.assertEqual(request.expected.snapshot.root, SNAPSHOT)

    def test_integration_conflict_retains_explicit_recovery_worktree(
        self,
    ) -> None:
        result = mcp.IntegrateResult.model_validate(
            {
                "status": "merge_conflict",
                "recovery": recovery(),
            }
        )

        self.assertEqual(result.root.status, "merge_conflict")
        self.assertEqual(
            result.root.recovery.conflicts[0].status,
            mcp.GitConflictStatus.BOTH_MODIFIED,
        )

        self.assertIn(
            "recovery", mcp.IntegrationMergeConflict.model_json_schema()["properties"]
        )
        for failure in (mcp.IntegrationRejected, mcp.IntegrationTargetMoved):
            self.assertNotIn("worktree", failure.model_json_schema()["properties"])
        self.assertNotIn(
            "worktree", mcp.IntegrationRefused.model_json_schema()["properties"]
        )

    def test_recovery_list_returns_the_manifest_used_for_cleanup(self) -> None:
        listed = mcp.RecoveryListResult.model_validate(
            {"recoveries": [recovery()], "next_cursor": None}
        )
        abort = mcp.RecoveryAbortParams.model_validate(
            {"expected": recovery()["manifest"], "confirm": True}
        )

        self.assertEqual(listed.recoveries[0].recovery.root, RECOVERY)
        self.assertEqual(abort.expected.recovery.root, RECOVERY)

        preview = mcp.RecoveryAbortResult.model_validate(
            {"status": "preview", "recovery": recovery()}
        )
        removed = mcp.RecoveryAbortResult.model_validate(
            {
                "status": "aborted",
                "recovery": RECOVERY,
                "manifest": recovery()["manifest"],
            }
        )
        self.assertEqual(preview.root.recovery.recovery.root, RECOVERY)
        self.assertEqual(removed.root.recovery.root, RECOVERY)

    def test_deleted_integration_target_is_representable(self) -> None:
        moved = mcp.IntegrationTargetMoved.model_validate(
            {
                "target": "refs/heads/main",
                "expected": COMMIT_A,
                "current": None,
                "source": projection_state(dirty=True),
            }
        )

        self.assertIsNone(moved.current)

    def test_projection_and_recovery_invariants_reject_ambiguous_evidence(self) -> None:
        with self.assertRaises(ValidationError):
            mcp.ProjectionUnchanged.model_validate(
                {"state": projection_state(dirty=True)}
            )

        ambiguous = recovery()
        ambiguous["driver_paths"] = ["src/lib.rs"]
        with self.assertRaises(ValidationError):
            mcp.GitRecovery.model_validate(ambiguous)

        with self.assertRaises(ValidationError):
            mcp.SessionRemoveResult.model_validate(
                {
                    "session": "ses_" + "a" * 26,
                    "state": projection_state(dirty=True),
                    "projection": projection_location(),
                    "scratch_bytes": 0,
                    "removed": False,
                    "unintegrated": False,
                    "reclaimable_bytes": 0,
                }
            )

    def test_cursor_and_symlink_bytes_use_transport_safe_encodings(self) -> None:
        with self.assertRaises(ValidationError):
            mcp.Cursor.model_validate("raw/cursor=")

        symlink = core.FileContentSymlink.model_validate(
            {"kind": "symlink", "target": "/wA="}
        )
        self.assertEqual(symlink.target, "/wA=")
        with self.assertRaises(ValidationError):
            core.FileContentSymlink.model_validate(
                {"kind": "symlink", "target": "not base64"}
            )
        with self.assertRaises(ValidationError):
            core.FileContentSymlink.model_validate(
                {"kind": "symlink", "target": "AB=="}
            )

    def test_file_resource_payload_is_exact_and_pageable(self) -> None:
        file = {
            "id": "rift://file/a.txt",
            "content": {
                "kind": "regular",
                "digest": "4" * 64,
                "size": 2,
                "executable": False,
            },
            "languages": [],
            "regions": [],
            "semantic": False,
        }
        first = mcp.FileResourcePayload.model_validate(
            {
                "encoding": "utf8",
                "uri": f"rift://file/a.txt?rev=snapshot:{SNAPSHOT}&start=0&length=1",
                "at": {"snapshot": {"id": SNAPSHOT}, "imported_from": None},
                "file": file,
                "start": 0,
                "end": 1,
                "total_bytes": 2,
                "content": "a",
                "next": f"rift://file/a.txt?rev=snapshot:{SNAPSHOT}&start=1&length=1",
            }
        )
        self.assertEqual(first.root.end, 1)
        self.assertEqual(first.root.file.content.root.kind, "regular")

        with self.assertRaises(ValidationError):
            mcp.FileResourcePayload.model_validate(
                {
                    "encoding": "base64",
                    "uri": f"rift://file/a.txt?rev=snapshot:{SNAPSHOT}&start=0&length=1",
                    "at": {"snapshot": {"id": SNAPSHOT}, "imported_from": None},
                    "file": file,
                    "start": 0,
                    "end": 1,
                    "total_bytes": 2,
                    "content": "YQ==",
                    "next": f"rift://file/a.txt?rev=snapshot:{SNAPSHOT}&start=1&length=1",
                }
            )
