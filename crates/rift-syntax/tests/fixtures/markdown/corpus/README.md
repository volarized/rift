# Markdown parser corpus

`gfm_spec.txt`, `gfm_pipe_tables.txt`, and `gfm_task_lists.txt` copy the upstream Tree-sitter Markdown test corpus at tree-sitter-markdown commit `f969cd3ae3f9fbd4e43205431d0ae286014c05b5`. They contain 303 GFM spec examples, 9 pipe-table examples, and 3 task-list examples. `commonmark_spec.txt` copies CommonMark 0.31.2 source at commonmark-spec commit `9103e341a973013013bb1a80e13567007c5cef6f`; integration test extracts its 652 Markdown examples using the upstream `test/spec_tests.py` delimiters.

The tree-sitter-markdown repository is MIT licensed. Example text in both specification corpora is CC-BY-SA 4.0, as stated by CommonMark 0.31.2 and GFM 0.29-gfm: <https://spec.commonmark.org/0.31.2/> and <https://github.github.com/gfm/>. The malformed heading fixture is based on the existing Rift Markdown malformed-input case and is not an upstream spec example.
