"""The retrieval corpus's Python surface."""


class DocumentPipeline:
    """Publishes one workspace snapshot into the searchable corpus."""

    def __init__(self, corpus_revision: str) -> None:
        self.corpus_revision = corpus_revision

    def publish(self, documents: list[str]) -> int:
        """Writes every document and answers how many landed."""
        return len(documents)


def derive_identifier_terms(name: str) -> list[str]:
    """Splits one identifier into the terms a caller might type."""
    return name.replace("_", " ").split()
