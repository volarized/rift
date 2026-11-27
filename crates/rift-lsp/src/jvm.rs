//! Launch commands for Java and Kotlin language servers.

use std::path::Path;

/// Language served by a JVM collection session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JvmLanguageServer {
    /// Eclipse JDT Language Server.
    Java,
    /// `JetBrains` Kotlin Language Server.
    Kotlin,
}

impl JvmLanguageServer {
    /// Selects a server by exact language identity.
    #[must_use]
    pub fn for_language(language: &str) -> Option<Self> {
        match language {
            "java" => Some(Self::Java),
            "kotlin" => Some(Self::Kotlin),
            _ => None,
        }
    }

    /// Language identifier carried by `textDocument/didOpen`.
    #[must_use]
    pub const fn language_id(self) -> &'static str {
        match self {
            Self::Java => "java",
            Self::Kotlin => "kotlin",
        }
    }

    /// Installed executable name.
    #[must_use]
    pub const fn program(self) -> &'static str {
        match self {
            Self::Java => "jdtls",
            Self::Kotlin => "kotlin-lsp",
        }
    }

    /// Arguments for a stand-alone server on standard streams.
    ///
    /// Both commands place server state under this workspace's `.rift` directory.
    /// Returns `None` when the state directory cannot be represented as text.
    #[must_use]
    pub fn arguments(self, root: &Path) -> Option<Vec<String>> {
        match self {
            Self::Java => Some(vec![
                "-data".to_owned(),
                root.join(".rift/jdtls").to_str()?.to_owned(),
            ]),
            Self::Kotlin => Some(vec![
                "--stdio".to_owned(),
                format!("--system-path={}", root.join(".rift/kotlin-lsp").to_str()?),
            ]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_only_the_document_language_for_each_server() {
        let java = JvmLanguageServer::for_language("java").expect("Java server");
        assert_eq!(java.language_id(), "java");
        assert_eq!(java.program(), "jdtls");

        let kotlin = JvmLanguageServer::for_language("kotlin").expect("Kotlin server");
        assert_eq!(kotlin.language_id(), "kotlin");
        assert_eq!(kotlin.program(), "kotlin-lsp");
        assert!(JvmLanguageServer::for_language("javascript").is_none());
    }

    #[test]
    fn uses_stdio_and_distinct_jdtls_data_directories() {
        let root = Path::new("workspace with spaces");
        let args = JvmLanguageServer::Java.arguments(root).expect("text path");
        assert_eq!(args[0], "-data");
        assert_eq!(Path::new(&args[1]), root.join(".rift/jdtls"));
        assert_ne!(
            JvmLanguageServer::Java.arguments(&root.join("nested")),
            Some(args)
        );
        let kotlin = JvmLanguageServer::Kotlin
            .arguments(root)
            .expect("text path");
        assert_eq!(kotlin[0], "--stdio");
        assert_eq!(
            Path::new(
                kotlin[1]
                    .strip_prefix("--system-path=")
                    .expect("system path")
            ),
            root.join(".rift/kotlin-lsp")
        );
    }
}
