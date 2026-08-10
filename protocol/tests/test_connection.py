"""Connection, session-change, and integration contracts."""

from unittest import TestCase

from jsonschema import Draft202012Validator
from jsonschema.exceptions import ValidationError as JsonSchemaValidationError
from pydantic import ValidationError

from rift.models import core, mcp
from rift.models.document import DOCUMENT, RIFT_SERVICE

COMMIT_A = "a" * 40
COMMIT_B = "b" * 40


class ConnectionContractTests(TestCase):
    @staticmethod
    def contract() -> mcp.Contract:
        return mcp.Contract(major=2, minor=0, schema_digest="0" * 64)

    def test_connect_is_the_control_stream(self) -> None:
        connect = RIFT_SERVICE.rpcs[0]
        self.assertEqual(connect.name, "Connect")
        self.assertTrue(connect.response_stream)

    def test_mcp_requires_a_client_generated_session(self) -> None:
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
            "contracts": [{"major": 2, "minor": 0, "schema_digest": "0" * 64}],
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

    def test_connected_session_does_not_require_a_worktree(self) -> None:
        connected = mcp.Connected(
            contract=self.contract(),
            features=[],
            workspace="/workspace",
            session="ses_" + "a" * 26,
            session_head=COMMIT_A,
            connection="con_" + "b" * 26,
        )

        self.assertIsNone(connected.worktree)

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
            mcp.Connected(**common, session_head=COMMIT_A)

        scip = mcp.Connected(**common)
        self.assertIsNone(scip.session)

    def test_connected_schema_allows_ref_only_session(self) -> None:
        validator = Draft202012Validator(mcp.Connected.model_json_schema())
        common = {
            "contract": {"major": 2, "minor": 0, "schema_digest": "0" * 64},
            "features": [],
            "workspace": "/workspace",
            "connection": "con_" + "b" * 26,
        }

        validator.validate(
            {
                **common,
                "session": "ses_" + "a" * 26,
                "session_head": COMMIT_A,
            }
        )
        with self.assertRaises(JsonSchemaValidationError):
            validator.validate({**common, "session": "ses_" + "a" * 26})
        with self.assertRaises(JsonSchemaValidationError):
            validator.validate({**common, "session_head": COMMIT_A})


class SessionChangeContractTests(TestCase):
    def test_mutation_requires_expected_head(self) -> None:
        with self.assertRaises(ValidationError):
            mcp.PatchParams(
                formatting=core.FormattingPolicy.PRESERVE,
                patch="diff --git a/a b/a",
            )

        change = mcp.PatchParams(
            expected_head=COMMIT_A,
            formatting=core.FormattingPolicy.PRESERVE,
            patch="diff --git a/a b/a",
        )
        self.assertEqual(change.expected_head.root, COMMIT_A)
        self.assertEqual(change.confirmations, [])

    def test_mutation_parameters_are_closed(self) -> None:
        with self.assertRaises(ValidationError):
            mcp.PatchParams.model_validate(
                {
                    "expected_head": COMMIT_A,
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
                "merge",
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
            },
        )
        self.assertEqual(
            resource_names,
            {"repository", "symbol", "diff", "file", "actions", "action"},
        )

    def test_integration_requires_the_observed_target_head(self) -> None:
        with self.assertRaises(ValidationError):
            mcp.IntegrateParams(target="refs/heads/main")

        request = mcp.IntegrateParams(
            target="refs/heads/main",
            expected_target=COMMIT_A,
        )
        self.assertIsNone(request.source)

    def test_git_conflict_preserves_worktree_identity_and_status(self) -> None:
        result = mcp.IntegrateResult.model_validate(
            {
                "status": "merge_conflict",
                "target": "refs/heads/main",
                "target_head": COMMIT_A,
                "source": COMMIT_B,
                "worktree": "worktrees/integration-test",
                "conflicts": [{"path": "src/lib.rs", "status": "both_modified"}],
            }
        )

        self.assertEqual(result.root.status, "merge_conflict")
        self.assertEqual(
            result.root.conflicts[0].status, mcp.GitConflictStatus.BOTH_MODIFIED
        )
