use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use rift_collect::{Error, Inventory};
use rift_core::{FileDigest, SourceUnitId};
use serde::Serialize;
use tempfile::TempDir;
use tokio::process::Command;

use crate::read_bounded;

const GRADLE_TIMEOUT: Duration = Duration::from_secs(180);
const REPORT_BYTES_MAX: u64 = 8 << 20;
const INIT_SCRIPT: &str = include_str!("gradle.init.gradle");

#[derive(Serialize)]
struct Source {
    unit: String,
    path: PathBuf,
    selected: bool,
}

#[derive(Default)]
pub(super) struct GradleSources(BTreeMap<SourceUnitId, Source>);

impl GradleSources {
    pub(super) fn add(&mut self, unit: SourceUnitId, path: PathBuf, selected: bool) {
        let source = Source {
            unit: unit.to_string(),
            path,
            selected,
        };
        self.0.insert(unit, source);
    }

    pub(super) async fn export(
        self,
        root: &Path,
        inventory: &Inventory,
        context: FileDigest,
        arguments: &mut Vec<OsString>,
    ) -> Result<(TempDir, FileDigest), Error> {
        if arguments
            .iter()
            .any(|argument| argument == "--gradle-context")
        {
            return Err(Error::Input(
                "--gradle-root supplies the Gradle context to the collector".into(),
            ));
        }
        let root = root.canonicalize()?;
        let directory = tempfile::tempdir()?;
        let init = directory.path().join("rift.init.gradle");
        let sources = directory.path().join("sources.json");
        let report = directory.path().join("context.json");
        std::fs::write(&init, INIT_SCRIPT)?;
        serde_json::to_writer(
            File::create(&sources)?,
            &self.0.values().collect::<Vec<_>>(),
        )?;

        let wrapper = if cfg!(windows) {
            "gradlew.bat"
        } else {
            "gradlew"
        };
        let wrapper = root.join(wrapper);
        let mut command = Command::new(if wrapper.is_file() {
            wrapper.into_os_string()
        } else {
            "gradle".into()
        });
        command
            .current_dir(root)
            .args([
                "--no-daemon",
                "--console=plain",
                "-q",
                "-Dorg.gradle.configuration-cache=false",
                "-I",
            ])
            .arg(init)
            .arg(":riftCollectContext")
            .env("RIFT_GRADLE_SOURCES", sources)
            .env("RIFT_GRADLE_CONTEXT", &report)
            .stdin(Stdio::null())
            .stdout(Stdio::from(std::io::stderr()))
            .kill_on_drop(true);
        let status = tokio::time::timeout(GRADLE_TIMEOUT, command.status())
            .await
            .map_err(|_| Error::Input("Gradle context export exceeded its deadline".into()))??;
        if !status.success() {
            return Err(Error::Input(format!(
                "Gradle context export exited with {status}"
            )));
        }

        let report_digest = FileDigest::of(&read_bounded(&report, REPORT_BYTES_MAX)?);
        let mut fingerprint = b"gradle context\0".to_vec();
        fingerprint.extend_from_slice(context.as_bytes());
        fingerprint.extend_from_slice(report_digest.as_bytes());
        for unit in self.0.keys() {
            let source = inventory
                .get(unit)
                .ok_or_else(|| Error::Input(format!("missing Gradle source: {unit}")))?;
            fingerprint.extend_from_slice(source.revision().as_bytes());
        }
        arguments.extend(["--gradle-context".into(), report.into_os_string()]);
        Ok((directory, FileDigest::of(&fingerprint)))
    }
}
