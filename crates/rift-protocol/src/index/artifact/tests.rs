use super::{
    PACKAGE_ARTIFACT_BYTES_MAX, PackageArtifact, PackageArtifactViolation, WheelTag,
    python_identifier_is_valid,
};
use crate::identity::SourceDigest;

#[test]
fn compressed_tags_expand_one_bounded_cartesian_set() {
    let tags = WheelTag::parse("py2.py3-none-any").expect("Six wheel tags");
    assert_eq!(tags.len(), 2);
    assert_eq!(tags[0].python(), "py2");
    assert_eq!(tags[1].python(), "py3");
    assert_eq!(tags[0].abi(), "none");
    assert_eq!(tags[0].platform(), "any");
    assert_eq!(
        WheelTag::parse("PY3.py3-NONE-ANY").expect("case and duplicates"),
        vec![tags[1].clone()]
    );
    let tags = WheelTag::parse("cp313.cp314-abi3.none-win_amd64.manylinux_2_17_x86_64")
        .expect("expanded platform set");
    assert_eq!(tags.len(), 8);
    assert!(tags.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(tags.iter().all(|tag| tag.python().starts_with("cp3")));
}

#[test]
fn tags_preserve_base64_abi_and_validate_python_identifiers() {
    let tag = WheelTag::new("Py3", "Ab+/=_12", "Any").expect("base64 ABI");
    assert_eq!(tag.abi(), "ab+/=_12");
    assert_eq!(tag.python(), "py3");
    assert!(python_identifier_is_valid("café3"));
    assert!(python_identifier_is_valid("_runtime"));
    for value in ["", "3py", "py 3", "py-3", "py.3", "☃"] {
        assert!(!python_identifier_is_valid(value), "{value}");
    }
    for value in [
        "py3-none",
        "py3-none-any-extra",
        "-none-any",
        "py3--any",
        "py3-none-",
        "py3.-none-any",
        "py3-none.-any",
        "py3-none-any.",
        "3py-none-any",
        "py3-no ne-any",
        "py3-none-any\n",
    ] {
        assert_eq!(
            WheelTag::parse(value),
            Err(PackageArtifactViolation::Tag),
            "{value}"
        );
    }
    let wire = serde_json::to_value(&tag).expect("serialize expanded tag");
    assert_eq!(
        wire,
        serde_json::json!({"python":"py3","abi":"ab+/=_12","platform":"any"})
    );
    assert_eq!(
        serde_json::from_value::<WheelTag>(wire).expect("validated tag JSON"),
        tag
    );
    assert!(
        serde_json::from_value::<WheelTag>(
            serde_json::json!({"python":"3py","abi":"none","platform":"any"})
        )
        .is_err()
    );
    assert!(
        serde_json::from_value::<WheelTag>(
            serde_json::json!({"python":"py3","abi":"none","platform":"any","extra":true})
        )
        .is_err()
    );
}

#[test]
fn tag_limits_apply_before_cartesian_expansion_and_lowercase_copy() {
    let python = (0..64)
        .map(|number| format!("py{number}"))
        .collect::<Vec<_>>()
        .join(".");
    assert_eq!(
        WheelTag::parse(&format!("{python}-none-any"))
            .expect("exact64 tags")
            .len(),
        64
    );
    assert_eq!(
        WheelTag::parse(&format!("{python}.py64-none-any")),
        Err(PackageArtifactViolation::Length)
    );
    assert_eq!(
        WheelTag::parse(&format!("{python}-none.abi3-any")),
        Err(PackageArtifactViolation::Length)
    );
    let exact_abi = "a".repeat(PACKAGE_ARTIFACT_BYTES_MAX - "py3".len() - "any".len());
    assert!(WheelTag::new("py3", &exact_abi, "any").is_ok());
    assert_eq!(
        WheelTag::new("py3", &format!("{exact_abi}a"), "any"),
        Err(PackageArtifactViolation::Length)
    );
    assert_eq!(
        WheelTag::parse(&format!(
            "py3-{}-any",
            "a".repeat(PACKAGE_ARTIFACT_BYTES_MAX)
        )),
        Err(PackageArtifactViolation::Length)
    );
    let repeated = format!("py2.py3-{}-any", "a".repeat(PACKAGE_ARTIFACT_BYTES_MAX / 2));
    assert_eq!(
        WheelTag::parse(&repeated),
        Err(PackageArtifactViolation::Length)
    );
    let expanding_lowercase = "İ".repeat(PACKAGE_ARTIFACT_BYTES_MAX / 2);
    assert_eq!(
        WheelTag::new(&expanding_lowercase, "none", "any"),
        Err(PackageArtifactViolation::Length)
    );
}

#[test]
fn artifact_factories_preserve_filename_digest_and_refuse_unbounded_fields() {
    let digest = SourceDigest::parse(&"f".repeat(64)).expect("artifact digest");
    let tags = WheelTag::parse("py2.py3-none-any").expect("selected tags");
    let artifact = PackageArtifact::new("six-1.17.0-py2.py3-none-any.whl", digest.clone(), &tags)
        .expect("captured artifact");
    assert_eq!(artifact.filename().0, "six-1.17.0-py2.py3-none-any.whl");
    assert_eq!(artifact.content_digest(), &digest);
    assert_eq!(artifact.tags(), tags);
    let wire = serde_json::to_value(&artifact).expect("serialize artifact");
    assert_eq!(
        serde_json::from_value::<PackageArtifact>(wire).expect("validated artifact JSON"),
        artifact
    );
    let source =
        PackageArtifact::new("click-8.3.3.tar.gz", digest.clone(), &[]).expect("source archive");
    assert!(source.tags().is_empty());
    assert_eq!(
        PackageArtifact::new(
            "six-1.17.0-py2.py3-none-any.whl",
            digest.clone(),
            &tags[..1]
        ),
        Err(PackageArtifactViolation::Tag)
    );
    assert_eq!(
        PackageArtifact::new("six-1.17.0-py3-none-any.whl", digest.clone(), &tags),
        Err(PackageArtifactViolation::Tag)
    );
    assert_eq!(
        PackageArtifact::new("click-8.3.3.tar.gz", digest.clone(), &tags),
        Err(PackageArtifactViolation::Tag)
    );
    assert_eq!(
        PackageArtifact::new("six-1.17.0-py2.py3-none-any.WHL", digest.clone(), &tags),
        Err(PackageArtifactViolation::Tag)
    );
    assert!(
        PackageArtifact::new(
            "six-1.17.0-1build-py2.py3-none-any.whl",
            digest.clone(),
            &tags
        )
        .is_ok()
    );
    for filename in [
        "six.whl",
        "-1.17.0-py2.py3-none-any.whl",
        "six-invalid-py2.py3-none-any.whl",
        "six-1.17.0-build-py2.py3-none-any.whl",
        "six__other-1.17.0-py2.py3-none-any.whl",
    ] {
        assert_eq!(
            PackageArtifact::new(filename, digest.clone(), &tags),
            Err(PackageArtifactViolation::Path),
            "{filename}"
        );
    }
    for filename in [
        "",
        ".",
        "..",
        "../six.whl",
        "archive/six.whl",
        "C:six.whl",
        "six\\archive.whl",
        "six\n.whl",
    ] {
        assert_eq!(
            PackageArtifact::new(filename, digest.clone(), &[]),
            Err(PackageArtifactViolation::Path),
            "{filename}"
        );
    }
    assert_eq!(
        PackageArtifact::new(&"a".repeat(4097), digest.clone(), &[]),
        Err(PackageArtifactViolation::Path)
    );
    assert_eq!(
        PackageArtifact::new("six.whl", digest.clone(), &vec![tags[0].clone(); 65]),
        Err(PackageArtifactViolation::Length)
    );
    let large_tag = WheelTag::new("py3", &"a".repeat(PACKAGE_ARTIFACT_BYTES_MAX - 6), "any")
        .expect("tag at bound");
    assert_eq!(
        PackageArtifact::new("six.whl", digest, &[large_tag]),
        Err(PackageArtifactViolation::Length)
    );
    assert!(
        serde_json::from_value::<PackageArtifact>(
            serde_json::json!({"filename":"six.whl","content_digest":"short","tags":[]})
        )
        .is_err()
    );
    let schema =
        serde_json::to_value(schemars::schema_for!(PackageArtifact)).expect("artifact schema");
    assert_eq!(
        schema["properties"]["tags"]["maxItems"],
        serde_json::json!(64)
    );
}
