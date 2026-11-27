//! Maven and Gradle project files.

/// One JVM build system recognized by its project files.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JvmBuildSystem {
    /// A Maven project declared by a POM.
    Maven,
    /// A Gradle project declared by a settings or build script.
    Gradle,
}

/// One file that identifies a Maven or Gradle project directory.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum JvmBuildFile {
    /// Gradle settings in Groovy.
    GradleSettings,
    /// Gradle settings in Kotlin.
    GradleKotlinSettings,
    /// Maven project object model.
    MavenPom,
    /// Gradle build script in Groovy.
    GradleBuild,
    /// Gradle build script in Kotlin.
    GradleKotlinBuild,
}

impl JvmBuildFile {
    /// Recognized filenames in detection order.
    const ALL: [Self; 5] = [
        Self::GradleSettings,
        Self::GradleKotlinSettings,
        Self::MavenPom,
        Self::GradleBuild,
        Self::GradleKotlinBuild,
    ];

    /// Classifies an exact filename from the captured source inventory.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|file| file.name() == name)
    }

    /// Exact project filename.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::GradleSettings => "settings.gradle",
            Self::GradleKotlinSettings => "settings.gradle.kts",
            Self::MavenPom => "pom.xml",
            Self::GradleBuild => "build.gradle",
            Self::GradleKotlinBuild => "build.gradle.kts",
        }
    }

    /// Build system that reads this file.
    #[must_use]
    pub const fn system(self) -> JvmBuildSystem {
        match self {
            Self::MavenPom => JvmBuildSystem::Maven,
            _ => JvmBuildSystem::Gradle,
        }
    }

    /// Whether this file specifies the root of a Gradle build.
    #[must_use]
    pub const fn is_gradle_settings(self) -> bool {
        matches!(self, Self::GradleSettings | Self::GradleKotlinSettings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_exact_build_filenames() {
        for (name, expected, settings) in [
            ("pom.xml", JvmBuildSystem::Maven, false),
            ("settings.gradle", JvmBuildSystem::Gradle, true),
            ("settings.gradle.kts", JvmBuildSystem::Gradle, true),
            ("build.gradle", JvmBuildSystem::Gradle, false),
            ("build.gradle.kts", JvmBuildSystem::Gradle, false),
        ] {
            let marker = JvmBuildFile::from_name(name).expect("known build file");
            assert_eq!(marker.system(), expected);
            assert_eq!(marker.is_gradle_settings(), settings);
        }
        assert_eq!(JvmBuildFile::from_name("POM.xml"), None);
        assert_eq!(JvmBuildFile::from_name("build.gradle.kts.bak"), None);
    }
}
