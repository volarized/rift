pub(crate) const VERSION: &str = "3.12.3";

pub(crate) fn builtins() -> String {
    ty_vendored::file_system()
        .read_to_string("stdlib/builtins.pyi")
        .expect("original embedded standard library stub")
}
