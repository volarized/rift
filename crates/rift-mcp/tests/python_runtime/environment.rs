use std::path::Path;

use crate::global_api::python_runtime::VERSION;

pub(crate) fn prepare(root: &Path) {
    let home = root.join("runtime/bin");
    for path in [
        &home,
        &root.join("runtime/lib/python3.12"),
        &root.join("runtime/Lib"),
        &home.join("Lib"),
        &root.join(".venv/lib/python3.12/site-packages"),
        &root.join(".venv/Lib/site-packages"),
    ] {
        std::fs::create_dir_all(path).expect("captured runtime directory");
    }
    std::fs::write(home.join("python3.12"), "").expect("captured runtime interpreter path");
    std::fs::write(
        root.join(".venv/pyvenv.cfg"),
        format!(
            "home = {}\nimplementation = CPython\nversion_info = {VERSION}\n",
            home.display()
        ),
    )
    .expect("captured runtime metadata");
    let project = root.join("pyproject.toml");
    let text = std::fs::read_to_string(&project).expect("original project metadata");
    std::fs::write(
        project,
        format!("{text}\n[tool.ty.environment]\npython = \".venv\"\n"),
    )
    .expect("explicit captured runtime environment");
}
