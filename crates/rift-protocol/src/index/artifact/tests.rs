use super::{
    PACKAGE_ARTIFACT_BYTES_MAX, PACKAGE_ARTIFACT_TAGS_MAX, PackageArtifact, WheelTag,
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

fn error_violation(error: &rift_error::RiftError) -> String {
    assert_eq!(
        error.slug().as_str(),
        "rift.analysis.package_artifact_invalid"
    );
    assert!(
        error
            .context()
            .any(|(key, value)| key == "field" && value == "artifact")
    );
    error
        .context()
        .find(|(key, _)| *key == "violation")
        .expect("registered violation evidence")
        .1
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
            WheelTag::parse(value).map_err(|error| error_violation(&error)),
            Err("Tag".to_owned()),
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

// Issue #1057.
#[test]
fn published_compressed_tags_expand_past_64() {
    let python = "cp38.cp39.cp310.cp311.cp312.cp313";
    let abi = "cp38.cp39.cp310.cp311.cp312.cp313";
    let platform = "manylinux_2_17_x86_64.manylinux2014_x86_64";
    let tags = WheelTag::parse(&format!("{python}-{abi}-{platform}")).expect("72 expanded tags");
    assert_eq!(tags.len(), 72);
    let filename = format!("beacon-1.0.0-{python}-{abi}-{platform}.whl");
    let digest = SourceDigest::parse(&"f".repeat(64)).expect("artifact digest");
    assert!(PackageArtifact::new(&filename, digest, &tags).is_ok());
}

#[test]
fn a_python_part_that_names_no_interpreter_adds_no_tag() {
    let py2 = WheelTag::parse("py2-none-any").expect("py2 tag");
    for value in ["py2.7-none-any", "py2.7.2-none-any", "2.py2.7-none-any"] {
        assert_eq!(WheelTag::parse(value).expect(value), py2, "{value}");
    }
    let digest = SourceDigest::parse(&"f".repeat(64)).expect("artifact digest");
    assert!(PackageArtifact::new("beacon-1.0.0-py2.7-none-any.whl", digest, &py2).is_ok());
    for value in [
        "7-none-any",
        "2.7-none-any",
        "py2..7-none-any",
        "py2.7 -none-any",
    ] {
        assert_eq!(
            WheelTag::parse(value).map_err(|error| error_violation(&error)),
            Err("Tag".to_owned()),
            "{value}"
        );
    }
}

#[test]
fn length_refusals_name_the_observed_quantity_and_its_bound() {
    let quantities = |error: &rift_error::RiftError| {
        ["observed", "bound"].map(|name| {
            error
                .context()
                .find(|(key, _)| *key == name)
                .expect("registered quantity")
                .1
        })
    };
    let python = (0..=PACKAGE_ARTIFACT_TAGS_MAX)
        .map(|number| format!("p{number}"))
        .collect::<Vec<_>>()
        .join(".");
    let error = WheelTag::parse(&format!("{python}-none-any")).expect_err("one tag too many");
    assert_eq!(quantities(&error), ["1025", "1024"]);
    let abi = "a".repeat(PACKAGE_ARTIFACT_BYTES_MAX - 5);
    let error = WheelTag::new("py3", &abi, "any").expect_err("one byte too many");
    assert_eq!(quantities(&error), ["65537", "65536"]);
}

#[test]
fn tag_limits_apply_before_cartesian_expansion_and_lowercase_copy() {
    let python = (0..PACKAGE_ARTIFACT_TAGS_MAX)
        .map(|number| format!("py{number}"))
        .collect::<Vec<_>>()
        .join(".");
    assert_eq!(
        WheelTag::parse(&format!("{python}-none-any"))
            .expect("tags at the bound")
            .len(),
        PACKAGE_ARTIFACT_TAGS_MAX
    );
    assert_eq!(
        WheelTag::parse(&format!("{python}.py1024-none-any"))
            .map_err(|error| error_violation(&error)),
        Err("Length".to_owned())
    );
    assert_eq!(
        WheelTag::parse(&format!("{python}-none.abi3-any"))
            .map_err(|error| error_violation(&error)),
        Err("Length".to_owned())
    );
    let exact_abi = "a".repeat(PACKAGE_ARTIFACT_BYTES_MAX - "py3".len() - "any".len());
    assert!(WheelTag::new("py3", &exact_abi, "any").is_ok());
    assert_eq!(
        WheelTag::new("py3", &format!("{exact_abi}a"), "any")
            .map_err(|error| error_violation(&error)),
        Err("Length".to_owned())
    );
    assert_eq!(
        WheelTag::parse(&format!(
            "py3-{}-any",
            "a".repeat(PACKAGE_ARTIFACT_BYTES_MAX)
        ))
        .map_err(|error| error_violation(&error)),
        Err("Length".to_owned())
    );
    let repeated = format!("py2.py3-{}-any", "a".repeat(PACKAGE_ARTIFACT_BYTES_MAX / 2));
    assert_eq!(
        WheelTag::parse(&repeated).map_err(|error| error_violation(&error)),
        Err("Length".to_owned())
    );
    let expanding_lowercase = "İ".repeat(PACKAGE_ARTIFACT_BYTES_MAX / 2);
    assert_eq!(
        WheelTag::new(&expanding_lowercase, "none", "any").map_err(|error| error_violation(&error)),
        Err("Length".to_owned())
    );
}

#[test]
fn artifact_factories_preserve_filename_digest() {
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
}

#[test]
fn artifact_factories_refuse_unbounded_fields() {
    let digest = SourceDigest::parse(&"f".repeat(64)).expect("artifact digest");
    let tags = WheelTag::parse("py2.py3-none-any").expect("selected tags");
    assert_eq!(
        PackageArtifact::new(
            "six-1.17.0-py2.py3-none-any.whl",
            digest.clone(),
            &tags[..1]
        )
        .map_err(|error| error_violation(&error)),
        Err("Tag".to_owned())
    );
    assert_eq!(
        PackageArtifact::new("six-1.17.0-py3-none-any.whl", digest.clone(), &tags)
            .map_err(|error| error_violation(&error)),
        Err("Tag".to_owned())
    );
    assert_eq!(
        PackageArtifact::new("click-8.3.3.tar.gz", digest.clone(), &tags)
            .map_err(|error| error_violation(&error)),
        Err("Tag".to_owned())
    );
    assert_eq!(
        PackageArtifact::new("six-1.17.0-py2.py3-none-any.WHL", digest.clone(), &tags)
            .map_err(|error| error_violation(&error)),
        Err("Tag".to_owned())
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
            PackageArtifact::new(filename, digest.clone(), &tags)
                .map_err(|error| error_violation(&error)),
            Err("Path".to_owned()),
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
            PackageArtifact::new(filename, digest.clone(), &[])
                .map_err(|error| error_violation(&error)),
            Err("Path".to_owned()),
            "{filename}"
        );
    }
    assert_eq!(
        PackageArtifact::new(&"a".repeat(4097), digest.clone(), &[])
            .map_err(|error| error_violation(&error)),
        Err("Path".to_owned())
    );
    assert_eq!(
        PackageArtifact::new(
            "six.whl",
            digest.clone(),
            &vec![tags[0].clone(); PACKAGE_ARTIFACT_TAGS_MAX + 1]
        )
        .map_err(|error| error_violation(&error)),
        Err("Length".to_owned())
    );
    let large_tag = WheelTag::new("py3", &"a".repeat(PACKAGE_ARTIFACT_BYTES_MAX - 6), "any")
        .expect("tag at bound");
    assert_eq!(
        PackageArtifact::new("six.whl", digest, &[large_tag])
            .map_err(|error| error_violation(&error)),
        Err("Length".to_owned())
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
        serde_json::json!(PACKAGE_ARTIFACT_TAGS_MAX)
    );
}
