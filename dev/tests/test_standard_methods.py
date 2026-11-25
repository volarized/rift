"""The table of inherent methods of the Rust standard library."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from rift_dev import standard_methods
from rift_dev.standard_methods import Method


def _method(name: str, parameters: str, stable: bool = True) -> str:
    since = '<span class="since" title="Stable">1.0.0</span>' if stable else ""
    unstable = "" if stable else '<div class="stab unstable">nightly</div>'
    return (
        f'<section id="method.{name}" class="method">{since}<h4 class="code-header">'
        f'pub fn <a href="#method.{name}" class="fn">{name}</a>{parameters}</h4>'
        f'</section>{unstable}<div class="docblock">text</div>'
    )


def _page(header: str, *methods: str) -> str:
    return (
        f'<h2 id="implementations"></h2><section id="impl-1"><h3 class="code-header">'
        f"{header}</h3></section>{''.join(methods)}"
        f'<h2 id="trait-implementations"></h2><section id="impl-2">'
        f'<h3 class="code-header">impl Trait</h3></section>{_method("absent", "(&amp;self)")}'
    )


class StandardMethodTests(unittest.TestCase):
    def test_stable_methods_with_a_receiver_are_read(self) -> None:
        page = _page(
            "impl&lt;T&gt; <a>[T]</a>",
            _method("len", "(&amp;self) -&gt; usize"),
            _method("reverse", "(&amp;mut self)"),
            _method("into_x", "(self)"),
            _method("split_off", "(self: &amp;mut &amp;'a [T], range: R)"),
            _method("new", "(value: T)"),
            _method("nightly", "(&amp;self)", stable=False),
        )
        self.assertEqual(
            standard_methods.page_methods("slice", page),
            [
                Method("slice", "&[Shared]", "len"),
                Method("slice", "&[Unique]", "reverse"),
                Method("slice", "&[]", "into_x"),
                Method("slice", "&[Unique, Shared]", "split_off"),
            ],
        )

    def test_slice_and_array_headers_name_their_element(self) -> None:
        for page, header, owner in [
            ("slice", "impl [u8]", "slice:u8"),
            ("slice", "impl&lt;T, const N: usize&gt; [[T; N]]", "slice:array"),
            ("array", "impl&lt;T, const N: usize&gt; [T; N]", "array"),
            ("array", "impl&lt;const N: usize&gt; [u8; N]", "array:u8"),
            ("vec::Vec", "impl&lt;T&gt; Vec&lt;T&gt;", "vec::Vec"),
        ]:
            found = standard_methods.page_methods(
                page, _page(header, _method("name", "(&amp;self)"))
            )
            self.assertEqual(found, [Method(owner, "&[Shared]", "name")], header)

    def test_rows_join_types_and_an_older_release_is_refused(self) -> None:
        rows = standard_methods.table_rows(
            {
                Method("u8", "&[]", "pow"),
                Method("u16", "&[]", "pow"),
                Method("u8", "&[Shared]", "is_ascii"),
            }
        )
        self.assertEqual(
            rows,
            "const METHODS: [Methods; 2] = [\n"
            '    (&["u16", "u8"], &[], &["pow"]),\n'
            '    (&["u8"], &[Shared], &["is_ascii"]),\n'
            "];\n",
        )
        with tempfile.TemporaryDirectory() as folder:
            pages = Path(folder)
            (pages / "std").mkdir()
            (pages / "crates1.2.0.js").touch()
            table = pages / "standard.rs"
            for release, refused in [("1.3.0", True), ("1.2.0", False)]:
                table.write_text(
                    f"documentation of `std` {release}.\n"
                    "const METHODS: [Methods; 1] = [\n    row,\n];\n",
                    encoding="utf-8",
                )
                if refused:
                    with self.assertRaises(ValueError):
                        standard_methods.write_table(pages, table)
                else:
                    self.assertEqual(
                        standard_methods.write_table(pages, table), "1.2.0"
                    )


if __name__ == "__main__":
    unittest.main()
