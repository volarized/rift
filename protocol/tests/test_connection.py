"""Connection and publication contracts introduced by the current wire schema."""

from unittest import TestCase

from jsonschema import Draft202012Validator
from jsonschema.exceptions import ValidationError as JsonSchemaValidationError
from pydantic import ValidationError

from rift.models import core, mcp
from rift.models.document import RIFT_SERVICE


class ConnectionContractTests(TestCase):
    @staticmethod
    def contract() -> mcp.Contract:
        return mcp.Contract(major=2, minor=0, schema_digest="0" * 64)

    def test_connect_is_the_control_stream(self) -> None:
        connect = RIFT_SERVICE.rpcs[0]
        self.assertEqual(connect.name, "Connect")
        self.assertTrue(connect.response_stream)

    def test_session_creation_requires_a_retry_key(self) -> None:
        with self.assertRaises(ValidationError):
            mcp.ConnectRequest(
                contracts=[self.contract()],
                features=[],
                role=mcp.ConnectRole.MCP,
                canonical_root="/workspace",
                client_build="test",
            )

        created = mcp.ConnectRequest(
            contracts=[self.contract()],
            features=[],
            role=mcp.ConnectRole.MCP,
            connect_attempt_id="try_" + "a" * 26,
            canonical_root="/workspace",
            client_build="test",
        )
        self.assertIsNone(created.session)

    def test_session_attachment_and_scip_exclude_creation_fields(self) -> None:
        attached = mcp.ConnectRequest(
            contracts=[self.contract()],
            features=[],
            role=mcp.ConnectRole.MCP,
            session="ses_" + "a" * 26,
            canonical_root="/workspace",
            client_build="test",
        )
        self.assertIsNone(attached.connect_attempt_id)

        with self.assertRaises(ValidationError):
            mcp.ConnectRequest(
                contracts=[self.contract()],
                features=[],
                role=mcp.ConnectRole.SCIP,
                session="ses_" + "a" * 26,
                canonical_root="/workspace",
                client_build="test",
            )


class PublicationPlanContractTests(TestCase):
    def test_root_candidate_requires_a_publication_plan(self) -> None:
        with self.assertRaises(ValidationError):
            mcp.PatchParams(
                formatting=core.FormattingPolicy.PRESERVE,
                patch="diff --git a/a b/a",
            )

        candidate = mcp.PatchParams(
            formatting=core.FormattingPolicy.PRESERVE,
            patch="diff --git a/a b/a",
            publication=mcp.PublicationPlan(validators=[]),
        )
        self.assertEqual(candidate.publication.validators, [])

    def test_chained_candidate_inherits_and_publish_cannot_replace(self) -> None:
        preview = "prv_" + "a" * 52
        chained = mcp.PatchParams(
            on=preview,
            formatting=core.FormattingPolicy.PRESERVE,
            patch="diff --git a/a b/a",
        )
        self.assertIsNone(chained.publication)

        with self.assertRaises(ValidationError):
            mcp.PublishParams.model_validate(
                {"preview": preview, "confirmations": [], "validators": []}
            )

    def test_json_schema_requires_the_root_plan(self) -> None:
        validator = Draft202012Validator(mcp.PatchParams.model_json_schema())
        root = {"formatting": "preserve", "patch": "diff --git a/a b/a"}
        with self.assertRaises(JsonSchemaValidationError):
            validator.validate(root)

        validator.validate(
            {
                **root,
                "publication": {"validators": []},
            }
        )
        validator.validate(
            {
                **root,
                "on": "prv_" + "a" * 52,
            }
        )
