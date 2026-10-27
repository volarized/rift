use super::*;

struct Fixture {
    _root: tempfile::TempDir,
    _issuer: rift_server::LocalSymbolViews,
    selected: SavedCandidateBackend,
    now: SystemTime,
}

fn fixture() -> Fixture {
    let root = tempfile::tempdir().expect("workspace");
    let issuer = rift_server::LocalSymbolViews::new(1 << 20, [41; 32]);
    let now = SystemTime::now();
    let view = issuer
        .capture_query_with(
            rift_server::QueryViewInput::new(
                root.path().to_path_buf(),
                SymbolOwner::Local,
                now + Duration::from_secs(300),
            )
            .expect("original selection"),
            |_| Ok(true),
        )
        .expect("issued view")
        .expect("retained view");
    let selected = SavedCandidateBackend {
        backend: "extra".to_owned(),
        root: root.path().to_str().expect("UTF-8 workspace").to_owned(),
        owner: Some(SymbolOwner::Local),
        view,
        project: None,
        upstream_view: None,
    };
    Fixture {
        _root: root,
        _issuer: issuer,
        selected,
        now,
    }
}

fn retain(store: &CandidateViews, selected: &SavedCandidateBackend, now: SystemTime) {
    store
        .reserve_captured(selected, now)
        .expect("reserve original context")
        .finalize(selected, now)
        .expect("retain original context");
}

#[test]
fn backend_metadata_preserves_original_owner_root_view_and_expiry() {
    let fixture = fixture();
    let store = CandidateViews::default();
    retain(&store, &fixture.selected, fixture.now);
    assert_eq!(
        store
            .backend(&fixture.selected.view.id, fixture.now)
            .expect("decode")
            .expect("accepted context"),
        Some(fixture.selected.clone())
    );
    assert_eq!(
        store
            .backend(
                &fixture.selected.view.id,
                captured_expiry(&fixture.selected.view).expect("original expiry"),
            )
            .expect("decode"),
        Err(SymbolUnavailableReason::ViewExpired)
    );
    assert_eq!(
        store
            .backend(
                &CapturedViewId::parse("claims.unknown").expect("unknown view"),
                fixture.now,
            )
            .expect("decode")
            .expect("unselected view"),
        None
    );
}

#[test]
fn backend_reservation_refuses_context_replacement_before_retention() {
    let fixture = fixture();
    for field in ["owner", "root", "backend", "project", "upstream"] {
        let store = CandidateViews::default();
        let mut pending = store
            .reserve_captured(&fixture.selected, fixture.now)
            .expect("reserve original context");
        let mut changed = fixture.selected.clone();
        match field {
            "owner" => {
                changed.owner = Some(SymbolOwner::NamedLocal {
                    name: "other".into(),
                });
            }
            "root" => changed.root.push_str("/other"),
            "backend" => changed.backend = "other".into(),
            "project" => changed.project = Some("other".into()),
            "upstream" => changed.upstream_view = Some(fixture.selected.view.clone()),
            _ => unreachable!(),
        }
        assert!(pending.finalize(&changed, fixture.now).is_err(), "{field}");
        drop(pending);
        assert!(store.rows.lock().expect("rows").is_empty());
    }
}

#[test]
fn backend_metadata_refuses_duplicate_view_without_replacing_original() {
    let fixture = fixture();
    let store = CandidateViews::default();
    retain(&store, &fixture.selected, fixture.now);
    for changed in [None, Some("owner"), Some("root")] {
        let mut selected = fixture.selected.clone();
        match changed {
            Some("owner") => selected.owner = None,
            Some("root") => selected.root.push_str("/other"),
            _ => {}
        }
        let mut pending = store
            .reserve_captured(&selected, fixture.now)
            .expect("reserve context");
        assert!(pending.finalize(&selected, fixture.now).is_err());
        drop(pending);
        assert_eq!(store.rows.lock().expect("rows").len(), 1);
        assert_eq!(
            store
                .backend(&fixture.selected.view.id, fixture.now)
                .expect("decode")
                .expect("original context"),
            Some(fixture.selected.clone())
        );
    }
}

#[test]
fn backend_metadata_refuses_invalid_and_expired_returned_view() {
    let fixture = fixture();
    let store = CandidateViews::default();
    let mut pending = store
        .reserve_captured(&fixture.selected, fixture.now)
        .expect("reserve original context");
    let mut changed = fixture.selected.clone();
    changed.view.expires_at = "not-a-time".into();
    assert!(pending.finalize(&changed, fixture.now).is_err());
    assert!(
        pending
            .finalize(
                &fixture.selected,
                captured_expiry(&fixture.selected.view).expect("original expiry"),
            )
            .is_err()
    );
    drop(pending);
    assert!(store.rows.lock().expect("rows").is_empty());
}

#[test]
fn backend_metadata_bounds_full_escaped_context_before_retention() {
    let fixture = fixture();
    let store = CandidateViews::default();
    let mut selected = fixture.selected.clone();
    selected.root = "\u{0001}".repeat(CANDIDATE_BYTES_MAX / 4);
    assert!(store.reserve_captured(&selected, fixture.now).is_err());
    assert!(store.rows.lock().expect("rows").is_empty());
}
