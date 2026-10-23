use super::{CANDIDATE_BYTES_MAX, CapturedView, CapturedViewId, RiftError, SymbolOwner};
use super::{CandidateViews, SavedCandidateBackend, SavedCandidateBytes, SavedCandidateRead};
use super::{captured_expiry, invalid, saved_backends, view_key};
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

// CapturedViewId is bounded at 4096 ASCII-safe bytes. The local owner emits
// chrono UTC RFC 3339: signed year 7 + date 6 + T 1 + time 8 + fraction 10
// + offset 6 = 38 bytes. Oversized foreign metadata is refused before exposure.
const VIEW_KEY_BYTES_MAX: usize = 4096;
const VIEW_EXPIRY_BYTES_MAX: usize = 38;
const VIEW_LIFETIME: Duration = Duration::from_mins(15);

pub(super) struct CandidateReservation<'a> {
    store: &'a CandidateViews,
    serial: u64,
    finished: bool,
}

impl CandidateViews {
    pub(super) fn reserve_symbol(
        &self,
        backend: &str,
        root: &str,
        owner: &SymbolOwner,
        now: SystemTime,
    ) -> Result<CandidateReservation<'_>, RiftError> {
        self.reserve_backend(backend, root, Some(owner), now)
    }

    pub(super) fn reserve_backend(
        &self,
        backend: &str,
        root: &str,
        owner: Option<&SymbolOwner>,
        now: SystemTime,
    ) -> Result<CandidateReservation<'_>, RiftError> {
        self.reserve_captured(
            &SavedCandidateBackend {
                owner: owner.cloned(),
                backend: backend.to_owned(),
                root: root.to_owned(),
                view: CapturedView {
                    id: CapturedViewId::parse("claims.pending")?,
                    expires_at: String::new(),
                },
                project: None,
                upstream_view: None,
            },
            now,
        )
    }

    pub(super) fn reserve_captured(
        &self,
        selected: &SavedCandidateBackend,
        now: SystemTime,
    ) -> Result<CandidateReservation<'_>, RiftError> {
        let expires = now
            .checked_add(VIEW_LIFETIME)
            .ok_or_else(|| invalid("view", "captured view expiry exceeds the clock bound"))?;
        let mut template = selected.clone();
        template.view = CapturedView {
            id: CapturedViewId::parse(&format!("{}.x", "a".repeat(VIEW_KEY_BYTES_MAX - 2)))?,
            // Worst JSON escaping is charged; final metadata uses these same bounds.
            expires_at: "\u{0001}".repeat(VIEW_EXPIRY_BYTES_MAX),
        };
        let bytes = super::super::encode_bounded(
            &SavedCandidateRead::Symbol(Box::new(template)),
            CANDIDATE_BYTES_MAX,
        )
        .map_err(|source| {
            rift_error::errors::mcp::forward_failed()
                .source(source)
                .error()
        })?;
        let mut rows = self
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        rows.retain(|row| row.expires > now);
        let retained = rows.iter().fold(
            rows.capacity()
                .checked_mul(std::mem::size_of::<SavedCandidateBytes>())
                .and_then(|size| size.checked_add(bytes.capacity())),
            |sum, row| sum.and_then(|sum| sum.checked_add(row.bytes.capacity())),
        );
        if rows.len() == 256 || retained.is_none_or(|size| size > CANDIDATE_BYTES_MAX) {
            return Err(invalid(
                "view",
                "captured search exceeds the byte or view bound",
            ));
        }
        let serial = self
            .reservation_serial
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |serial| {
                serial.checked_add(1)
            })
            .map_err(|_| invalid("view", "captured view sequence exceeds the bound"))?;
        rows.push(SavedCandidateBytes {
            key: [0; 32],
            expires,
            bytes,
            reservation: Some(serial),
        });
        Ok(CandidateReservation {
            store: self,
            serial,
            finished: false,
        })
    }
}

impl CandidateReservation<'_> {
    pub(super) fn finalize(
        &mut self,
        selected: &SavedCandidateBackend,
        now: SystemTime,
    ) -> Result<(), RiftError> {
        let view = &selected.view;
        if view.id.as_str().len() > VIEW_KEY_BYTES_MAX
            || view.expires_at.len() > VIEW_EXPIRY_BYTES_MAX
        {
            return Err(invalid("view", "captured view metadata exceeds the bound"));
        }
        let expires = captured_expiry(view)?;
        let mut rows = self
            .store
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let position = rows
            .iter()
            .position(|row| row.reservation == Some(self.serial))
            .ok_or_else(|| invalid("view", "captured view reservation is unavailable"))?;
        let latest_expiry = now
            .checked_add(VIEW_LIFETIME)
            .ok_or_else(|| invalid("view", "captured view expiry exceeds the clock bound"))?;
        if rows[position].expires <= now || expires <= now || expires > latest_expiry {
            rift_tracing::warn!(
                component = "mcp",
                pending_expired = rows[position].expires <= now,
                returned_expired = expires <= now,
                expiry_over_bound = expires > latest_expiry,
                pending_remaining = ?rows[position].expires.duration_since(now),
                returned_remaining = ?expires.duration_since(now),
                expiry_overflow = ?expires.duration_since(latest_expiry),
                "captured view expiry does not match the reservation"
            );
            return Err(invalid(
                "view",
                "captured view expiry does not match the reservation",
            ));
        }
        let saved: SavedCandidateRead =
            serde_json::from_slice(&rows[position].bytes).map_err(|source| {
                rift_error::errors::mcp::forward_failed()
                    .source(source)
                    .error()
            })?;
        let SavedCandidateRead::Symbol(mut backend) = saved else {
            return Err(invalid("view", "captured view reservation does not match"));
        };
        if backend.owner != selected.owner
            || backend.backend != selected.backend
            || backend.root != selected.root
            || backend.project != selected.project
            || backend.upstream_view != selected.upstream_view
        {
            return Err(invalid(
                "view",
                "captured view context does not match the reservation",
            ));
        }
        let mut retained = None;
        for row in rows.iter().filter(|row| row.reservation.is_none()) {
            for backend in saved_backends(&row.bytes)? {
                if backend.view.id == view.id {
                    retained = Some(backend);
                    break;
                }
            }
            if retained.is_some() {
                break;
            }
        }
        if let Some(retained) = retained {
            if selected.upstream_view.is_some() && retained == *selected {
                rows.remove(position);
                self.finished = true;
                return Ok(());
            }
            return Err(invalid("view", "captured search view is already retained"));
        }
        backend.view = view.clone();
        let bytes =
            super::super::encode_bounded(&SavedCandidateRead::Symbol(backend), CANDIDATE_BYTES_MAX)
                .map_err(|source| {
                    rift_error::errors::mcp::forward_failed()
                        .source(source)
                        .error()
                })?;
        let row = &mut rows[position];
        if bytes.len() > row.bytes.capacity() {
            return Err(invalid(
                "view",
                "captured view metadata exceeds the reservation",
            ));
        }
        row.bytes.clear();
        row.bytes.extend_from_slice(&bytes);
        row.key = view_key(view.id.as_str());
        row.expires = expires;
        row.reservation = None;
        self.finished = true;
        Ok(())
    }
}

impl Drop for CandidateReservation<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.store
                .rows
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|row| row.reservation != Some(self.serial));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_hours(525_960)
    }
    fn view(key: &str) -> CapturedView {
        CapturedView {
            id: CapturedViewId::parse(key).expect("issued view"),
            expires_at: "2030-01-01T00:10:00+00:00".to_owned(),
        }
    }

    fn selected(key: &str) -> SavedCandidateBackend {
        SavedCandidateBackend {
            owner: Some(SymbolOwner::Local),
            backend: "local".to_owned(),
            root: "/fixture/main".to_owned(),
            view: view(key),
            project: None,
            upstream_view: None,
        }
    }

    #[test]
    fn symbol_reservation_accepts_delayed_issue_and_retains_exact_backend_view() {
        let store = CandidateViews::default();
        let mut pending = store
            .reserve_symbol("local", "/fixture/main", &SymbolOwner::Local, clock())
            .expect("reserve");
        let mut response = selected("claims.delayed");
        response.view.expires_at = "2030-01-01T00:15:01+00:00".to_owned();
        let finalized = clock() + Duration::from_secs(2);
        assert!(captured_expiry(&response.view).expect("expiry") > clock() + VIEW_LIFETIME);
        pending
            .finalize(&response, finalized)
            .expect("delayed issue");
        let retained = store
            .backend(&response.view.id, finalized)
            .expect("decode")
            .expect("authority")
            .expect("backend");
        assert_eq!(retained.backend, response.backend);
        assert_eq!(retained.root, response.root);
        assert_eq!(retained.owner, response.owner);
        assert_eq!(retained.view, response.view);
        assert_eq!(
            store.rows.lock().expect("rows")[0].expires,
            captured_expiry(&response.view).expect("expiry")
        );
        assert_eq!(
            store
                .backend(
                    &response.view.id,
                    captured_expiry(&response.view).expect("expiry")
                )
                .expect("expired"),
            Err(rift_protocol::symbol_read::SymbolUnavailableReason::ViewExpired)
        );
    }

    #[test]
    fn dependency_reservations_reuse_identical_concurrent_context_only() {
        let store = std::sync::Arc::new(CandidateViews::default());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut selected = selected("claims.dependency");
        selected.backend = "global".to_owned();
        selected.project = Some("docs".to_owned());
        selected.upstream_view = Some(view("claims.remote"));
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let store = std::sync::Arc::clone(&store);
                    let barrier = std::sync::Arc::clone(&barrier);
                    let selected = selected.clone();
                    scope.spawn(move || {
                        let mut pending =
                            store.reserve_captured(&selected, clock()).expect("reserve");
                        barrier.wait();
                        pending.finalize(&selected, clock()).expect("same context");
                    })
                })
                .collect();
            barrier.wait();
            for handle in handles {
                handle.join().expect("retained dependency");
            }
        });
        assert_eq!(store.rows.lock().expect("rows").len(), 1);
        assert_eq!(
            store
                .backend(&selected.view.id, clock())
                .expect("decode")
                .expect("authority"),
            Some(selected.clone())
        );
        for field in ["project", "owner", "upstream", "expiry"] {
            let mut other = selected.clone();
            match field {
                "project" => other.project = Some("local".to_owned()),
                "owner" => other.owner = None,
                "upstream" => other.upstream_view = Some(view("claims.other")),
                "expiry" => other.view.expires_at = "2030-01-01T00:09:00Z".to_owned(),
                _ => unreachable!(),
            }
            let mut pending = store
                .reserve_captured(&other, clock())
                .expect("reserve conflict");
            assert!(pending.finalize(&other, clock()).is_err(), "{field}");
            drop(pending);
            assert_eq!(store.rows.lock().expect("rows").len(), 1);
        }
        let mut foreign = store
            .reserve_symbol("local", "/fixture/main", &SymbolOwner::Local, clock())
            .expect("foreign reservation");
        assert!(foreign.finalize(&selected, clock()).is_err());
        drop(foreign);
        assert_eq!(store.rows.lock().expect("rows").len(), 1);
        assert_eq!(
            store
                .backend(
                    &selected.view.id,
                    captured_expiry(&selected.view).expect("expiry")
                )
                .expect("expired"),
            Err(rift_protocol::symbol_read::SymbolUnavailableReason::ViewExpired)
        );
    }

    #[test]
    fn symbol_reservation_refuses_returned_expiry_past_finalization_lifetime() {
        let store = CandidateViews::default();
        let mut pending = store
            .reserve_symbol("local", "/fixture/main", &SymbolOwner::Local, clock())
            .expect("reserve");
        let mut response = selected("claims.future");
        response.view.expires_at = "2030-01-01T00:15:02.000000100+00:00".to_owned();
        assert!(
            pending
                .finalize(&response, clock() + Duration::from_secs(2))
                .is_err()
        );
        drop(pending);
        assert!(store.rows.lock().expect("rows").is_empty());
    }

    #[test]
    fn symbol_reservation_refuses_expired_pending_row_after_delayed_issue() {
        let store = CandidateViews::default();
        let mut pending = store
            .reserve_symbol("local", "/fixture/main", &SymbolOwner::Local, clock())
            .expect("reserve");
        let mut response = selected("claims.pending_expired");
        response.view.expires_at = "2030-01-01T00:29:59+00:00".to_owned();
        assert!(
            pending
                .finalize(&response, clock() + VIEW_LIFETIME)
                .is_err()
        );
        drop(pending);
        assert!(store.rows.lock().expect("rows").is_empty());
    }

    #[test]
    fn symbol_reservation_refuses_already_expired_returned_view() {
        let store = CandidateViews::default();
        let mut pending = store
            .reserve_symbol("local", "/fixture/main", &SymbolOwner::Local, clock())
            .expect("reserve");
        let response = selected("claims.expired");
        assert!(
            pending
                .finalize(&response, captured_expiry(&response.view).expect("expiry"))
                .is_err()
        );
        drop(pending);
        assert!(store.rows.lock().expect("rows").is_empty());
    }

    #[test]
    fn symbol_reservation_cancellation_releases_only_pending_row() {
        let store = CandidateViews::default();
        let mut first = store
            .reserve_symbol("local", "/fixture/main", &SymbolOwner::Local, clock())
            .expect("reserve");
        first
            .finalize(&selected("claims.first"), clock())
            .expect("finalize");
        let charge = store.rows.lock().expect("rows")[0].bytes.capacity();
        let pending = store
            .reserve_symbol("extra", "/fixture/extra", &SymbolOwner::Local, clock())
            .expect("reserve extra");
        assert_eq!(store.rows.lock().expect("rows").len(), 2);
        drop(pending);
        let rows = store.rows.lock().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].bytes.capacity(), charge);
        drop(rows);
        assert!(
            store
                .backend(&view("claims.first").id, clock())
                .expect("decode")
                .expect("authority")
                .is_some()
        );
    }

    #[test]
    fn symbol_reservation_refuses_capacity_before_dispatch() {
        let store = CandidateViews::default();
        let reservations = (0..256)
            .map(|_| {
                store
                    .reserve_symbol("local", "/fixture/main", &SymbolOwner::Local, clock())
                    .expect("slot")
            })
            .collect::<Vec<_>>();
        assert!(
            store
                .reserve_symbol("local", "/fixture/main", &SymbolOwner::Local, clock())
                .is_err()
        );
        assert_eq!(store.rows.lock().expect("rows").len(), 256);
        drop(reservations);
        assert!(store.rows.lock().expect("rows").is_empty());
    }

    #[test]
    fn symbol_reservation_refuses_oversized_or_duplicate_finalization_without_exposure() {
        let store = CandidateViews::default();
        let mut first = store
            .reserve_symbol("local", "/fixture/main", &SymbolOwner::Local, clock())
            .expect("reserve");
        first
            .finalize(&selected("claims.first"), clock())
            .expect("finalize");
        for oversized in [false, true] {
            let mut pending = store
                .reserve_symbol("extra", "/fixture/extra", &SymbolOwner::Local, clock())
                .expect("reserve extra");
            let mut response = selected("claims.first");
            if oversized {
                response.view.expires_at = "x".repeat(VIEW_EXPIRY_BYTES_MAX + 1);
            }
            assert!(pending.finalize(&response, clock()).is_err());
            drop(pending);
            assert_eq!(store.rows.lock().expect("rows").len(), 1);
        }
        let selected = store
            .backend(&view("claims.first").id, clock())
            .expect("decode")
            .expect("authority")
            .expect("backend");
        assert_eq!(selected.backend, "local");
        assert_eq!(selected.root, "/fixture/main");
        assert_eq!(selected.owner, Some(SymbolOwner::Local));
    }
    #[test]
    fn symbol_reservation_refuses_foreign_root_owner_and_backend() {
        let store = CandidateViews::default();
        for field in ["root", "owner", "backend"] {
            let mut pending = store
                .reserve_symbol("local", "/fixture/main", &SymbolOwner::Local, clock())
                .expect("reserve");
            let mut response = selected("claims.foreign");
            match field {
                "root" => response.root = "/fixture/foreign".to_owned(),
                "owner" => {
                    response.owner = Some(SymbolOwner::NamedLocal {
                        name: "foreign".to_owned(),
                    });
                }
                _ => response.backend = "foreign".to_owned(),
            }
            assert!(pending.finalize(&response, clock()).is_err());
            drop(pending);
            assert!(store.rows.lock().expect("rows").is_empty());
        }
    }

    #[test]
    fn symbol_reservation_retains_exact_global_context_without_project_owner() {
        let store = CandidateViews::default();
        let mut pending = store
            .reserve_backend("global", "/fixture/global", None, clock())
            .expect("reserve global");
        let mut response = selected("claims.global");
        response.backend = "global".to_owned();
        response.root = "/fixture/global".to_owned();
        response.owner = None;
        pending
            .finalize(&response, clock())
            .expect("global context");
        let retained = store
            .backend(&response.view.id, clock())
            .expect("decode")
            .expect("accepted")
            .expect("backend");
        assert_eq!(retained, response);
    }
}
