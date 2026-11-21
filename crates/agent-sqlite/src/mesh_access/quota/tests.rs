use super::*;
use querymt_remote::peer_access::{ModelSelection, ProviderGrant, SharingProtocol};
use time::macros::datetime;

const TUESDAY: i64 = datetime!(2024-02-27 12:00 UTC).unix_timestamp();
const WEDNESDAY: i64 = datetime!(2024-02-28 0:00 UTC).unix_timestamp();
const FRIDAY: i64 = datetime!(2024-03-01 0:00 UTC).unix_timestamp();
const MONDAY: i64 = datetime!(2024-03-04 0:00 UTC).unix_timestamp();

struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
    store: SqliteMeshAccess,
    key: PeerGrantKey,
    grant: PeerGrant,
}

impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mesh.db");
        let store = SqliteMeshAccess::open(&path).await.unwrap();
        let key = super::super::tests::grant_key();
        let grant = PeerGrant::Custom {
            sessions: false,
            filesystem: false,
            providers: vec![ProviderGrant {
                provider_id: "provider".into(),
                models: ModelSelection::All,
            }],
        };
        store
            .replace_grant(&key, Uuid::nil(), grant.clone())
            .await
            .unwrap();
        Self {
            _dir: dir,
            path,
            store,
            key,
            grant,
        }
    }

    async fn edit(
        &self,
        scope: &QuotaScope,
        edit: QuotaEdit,
        now: i64,
    ) -> Result<Uuid, QuotaError> {
        let rev = self.store.grant(&self.key).await.unwrap().revision;
        self.store
            .edit_quota(&self.key, rev, scope, edit, now)
            .await
    }

    async fn limit(&self, scope: &QuotaScope, tokens: i64, window: QuotaWindow) {
        self.edit(
            scope,
            QuotaEdit::Reconfigure(QuotaLimit::Finite { tokens, window }),
            TUESDAY,
        )
        .await
        .unwrap();
    }

    async fn request(&self, model: &str, tokens: Option<i64>, now: i64) -> QuotaAttempt {
        QuotaAttempt {
            peer: self.key.clone(),
            grant_revision: self.store.grant(&self.key).await.unwrap().revision,
            protocol: SharingProtocol::V1,
            provider_id: "provider".into(),
            model_id: model.into(),
            request_id: Uuid::now_v7(),
            bound_tokens: tokens,
            now,
        }
    }

    async fn status(&self, scope: &QuotaScope, now: i64) -> QuotaStatus {
        self.store.quota(&self.key, scope, now).await.unwrap()
    }
}

fn scope(model: Option<&str>) -> QuotaScope {
    QuotaScope {
        provider_id: "provider".into(),
        model_id: model.map(str::to_owned),
    }
}

#[tokio::test]
async fn concurrent_requests_cannot_overcommit_quota() {
    let f = Fixture::new().await;
    let provider = scope(None);
    let a = scope(Some("a"));
    let b = scope(Some("b"));
    f.limit(&provider, 100, QuotaWindow::Manual).await;
    f.limit(&a, 100, QuotaWindow::Daily).await;
    f.limit(&b, 100, QuotaWindow::Weekly).await;
    let second = SqliteMeshAccess::open(&f.path).await.unwrap();
    let (res_a, res_b) = tokio::join!(
        f.store.reserve(f.request("a", Some(60), TUESDAY).await),
        second.reserve(f.request("b", Some(60), TUESDAY).await)
    );
    let (rejected, rejected_scope) = match (res_a, res_b) {
        (Ok(_), Err(error)) => (error, b),
        (Err(error), Ok(_)) => (error, a),
        other => panic!("exactly one reservation must succeed: {other:?}"),
    };
    assert!(
        matches!(rejected, QuotaError::Exhausted(ref scopes) if scopes.len() == 1 && scopes[0].scope == provider)
    );
    assert_eq!(f.status(&rejected_scope, TUESDAY).await.reserved_tokens, 0);
    assert_eq!(f.status(&provider, TUESDAY).await.reserved_tokens, 60);
}

#[tokio::test]
async fn provider_and_model_limits_are_independently_binding() {
    let f = Fixture::new().await;
    let model = scope(Some("a"));
    f.limit(&model, 10, QuotaWindow::Daily).await;
    assert!(
        matches!(f.store.reserve(f.request("a", Some(11), TUESDAY).await).await,
        Err(QuotaError::Exhausted(ref scopes)) if scopes == &[LimitingQuota { scope: model, available_tokens: 10, next_reset_at: Some(WEDNESDAY) }])
    );
    assert!(matches!(
        f.store.reserve(f.request("a", None, TUESDAY).await).await,
        Err(QuotaError::Unsupported(_))
    ));
    let unlimited = f
        .store
        .reserve(f.request("b", None, TUESDAY).await)
        .await
        .unwrap();
    f.store.mark_dispatched(unlimited).await.unwrap();
    f.store
        .settle(unlimited, AttemptUsage::Unknown)
        .await
        .unwrap();
    assert!(f.status(&scope(Some("b")), TUESDAY).await.has_unknown_usage);
}

#[tokio::test]
async fn recurring_windows_renew_at_utc_calendar_boundaries() {
    let f = Fixture::new().await;
    let provider = scope(None);
    let daily = scope(Some("daily"));
    let monthly = scope(Some("monthly"));
    f.limit(&provider, 20, QuotaWindow::Weekly).await;
    f.limit(&daily, 20, QuotaWindow::Daily).await;
    f.limit(&monthly, 20, QuotaWindow::Monthly).await;
    assert_eq!(
        f.status(&provider, TUESDAY).await.next_reset_at,
        Some(MONDAY)
    );
    assert_eq!(
        f.status(&daily, TUESDAY).await.next_reset_at,
        Some(WEDNESDAY)
    );
    assert_eq!(
        f.status(&monthly, TUESDAY).await.next_reset_at,
        Some(FRIDAY)
    );
    let attempt = f
        .store
        .reserve(f.request("daily", Some(20), TUESDAY).await)
        .await
        .unwrap();
    f.store.mark_dispatched(attempt).await.unwrap();
    f.store
        .settle(attempt, AttemptUsage::Observed(11))
        .await
        .unwrap();
    assert_eq!(
        f.status(&daily, WEDNESDAY - 1).await.available_tokens,
        Some(9)
    );
    assert_eq!(f.status(&daily, WEDNESDAY).await.available_tokens, Some(20));
    assert_eq!(
        f.status(&provider, WEDNESDAY).await.available_tokens,
        Some(9)
    );
    let reopen = SqliteMeshAccess::open(&f.path).await.unwrap();
    assert_eq!(
        reopen
            .quota(&f.key, &daily, TUESDAY)
            .await
            .unwrap()
            .window_started_at,
        Some(WEDNESDAY)
    );
    assert_eq!(
        reopen
            .quota(&f.key, &monthly, MONDAY)
            .await
            .unwrap()
            .window_started_at,
        Some(FRIDAY)
    );
    assert_eq!(
        reopen
            .quota(&f.key, &provider, MONDAY)
            .await
            .unwrap()
            .available_tokens,
        Some(20)
    );
}

#[tokio::test]
async fn a_stream_spanning_renewal_settles_against_its_original_windows() {
    let f = Fixture::new().await;
    let provider = scope(None);
    let model = scope(Some("a"));
    f.limit(&provider, 30, QuotaWindow::Weekly).await;
    f.limit(&model, 10, QuotaWindow::Daily).await;
    let req = f.request("a", Some(10), TUESDAY).await;
    let attempt = f.store.reserve(req.clone()).await.unwrap();
    f.store.mark_dispatched(attempt).await.unwrap();
    assert_eq!(f.status(&model, WEDNESDAY).await.available_tokens, Some(10));
    assert_eq!(
        f.store
            .settle(attempt, AttemptUsage::Observed(8))
            .await
            .unwrap(),
        QuotaSettlement::Recorded
    );
    assert_eq!(
        f.store
            .settle(attempt, AttemptUsage::Observed(8))
            .await
            .unwrap(),
        QuotaSettlement::AlreadyFinal
    );
    assert!(matches!(
        f.store.reserve(req).await,
        Err(QuotaError::DuplicateAttempt)
    ));
    assert_eq!(f.status(&model, WEDNESDAY).await.known_tokens, 0);
    assert_eq!(f.status(&provider, WEDNESDAY).await.known_tokens, 8);
    // The completed usage did not vanish: its original model period still owns it.
    let recorded = f.store.with_conn(|conn| -> Result<i64, GrantStoreError> {
        conn.query_row("SELECT r.known FROM quota_reservations r JOIN quota_periods p ON p.id=r.period_id JOIN quota_scopes s ON s.id=p.scope_id WHERE s.model_id='a' AND p.current=0", [], |row| row.get(0)).map_err(backend)
    }).await.unwrap();
    assert_eq!(recorded, 8);
}

#[tokio::test]
async fn committed_settlement_notifies_only_its_peer_without_changing_revision() {
    let f = Fixture::new().await;
    f.limit(&scope(None), 20, QuotaWindow::Weekly).await;
    let request = f.request("a", Some(10), TUESDAY).await;
    let revision = request.grant_revision;
    let mut hints = f.store.usage_changes.subscribe();
    let attempt = f.store.reserve(request).await.unwrap();
    f.store.mark_dispatched(attempt).await.unwrap();
    assert!(hints.try_recv().is_err());

    assert_eq!(
        f.store
            .settle(attempt, AttemptUsage::Observed(7))
            .await
            .unwrap(),
        QuotaSettlement::Recorded
    );
    assert_eq!(hints.try_recv().unwrap(), f.key);
    assert_eq!(f.store.grant(&f.key).await.unwrap().revision, revision);
    assert_eq!(
        f.store
            .settle(attempt, AttemptUsage::Observed(7))
            .await
            .unwrap(),
        QuotaSettlement::AlreadyFinal
    );
    assert!(hints.try_recv().is_err());
}

#[tokio::test]
async fn dropped_attempts_release_only_work_that_could_not_dispatch() {
    use querymt_remote::peer_access::quota::ReservedAttempt;

    let f = Fixture::new().await;
    let provider = scope(None);
    f.limit(&provider, 20, QuotaWindow::Weekly).await;
    let store = Arc::new(f.store.clone());
    let reserved = ReservedAttempt::reserve(store.clone(), f.request("a", Some(5), TUESDAY).await)
        .await
        .unwrap();
    let dispatched = ReservedAttempt::reserve(store, f.request("b", Some(10), TUESDAY).await)
        .await
        .unwrap()
        .begin()
        .await
        .unwrap();
    assert_eq!(
        f.store
            .quota(&f.key, &provider, TUESDAY)
            .await
            .unwrap()
            .reserved_tokens,
        15
    );
    drop(reserved);
    drop(dispatched);

    let status = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let status = f.store.quota(&f.key, &provider, TUESDAY).await.unwrap();
            if status.reserved_tokens == 0 {
                break status;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cleanup must settle both attempts without a restart");
    assert_eq!(status.known_tokens, 0);
    assert_eq!(status.estimated_tokens, 10);
    assert_eq!(status.available_tokens, Some(10));
}

#[tokio::test]
async fn a_stale_authorization_cannot_reserve_under_a_regrant() {
    let f = Fixture::new().await;
    let provider = scope(None);
    f.limit(&provider, 20, QuotaWindow::Weekly).await;
    let stale = f.request("a", Some(15), TUESDAY).await;
    let revoked = f
        .store
        .replace_grant(&f.key, stale.grant_revision, PeerGrant::None)
        .await
        .unwrap();
    let regranted = f
        .store
        .replace_grant(&f.key, revoked.revision, f.grant.clone())
        .await
        .unwrap();

    assert!(matches!(
        f.store.reserve(stale.clone()).await,
        Err(QuotaError::Denied)
    ));
    let status = f.store.quota(&f.key, &provider, TUESDAY).await.unwrap();
    assert_eq!(status.reserved_tokens, 0);
    // No attempt or replay tombstone was created for the rejected reservation.
    f.store
        .reserve(QuotaAttempt {
            grant_revision: regranted.revision,
            ..stale
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn restart_and_regrant_do_not_top_up_an_allowance() {
    let f = Fixture::new().await;
    let provider = scope(None);
    f.limit(&provider, 20, QuotaWindow::Weekly).await;
    let pending = f
        .store
        .reserve(f.request("a", Some(5), TUESDAY).await)
        .await
        .unwrap();
    let dispatched = f
        .store
        .reserve(f.request("b", Some(10), TUESDAY).await)
        .await
        .unwrap();
    f.store.mark_dispatched(dispatched).await.unwrap();
    let reopened = SqliteMeshAccess::open(&f.path).await.unwrap();
    reopened.reconcile_startup(&f.key.host).await.unwrap();
    assert!(matches!(
        reopened.mark_dispatched(pending).await,
        Err(QuotaError::InvalidAttempt)
    ));
    let current = reopened.grant(&f.key).await.unwrap();
    let revoked = reopened
        .replace_grant(&f.key, current.revision, PeerGrant::None)
        .await
        .unwrap();
    assert!(matches!(
        reopened
            .reserve(f.request("b", Some(1), TUESDAY).await)
            .await,
        Err(QuotaError::Denied)
    ));
    reopened
        .replace_grant(&f.key, revoked.revision, f.grant.clone())
        .await
        .unwrap();
    let status = reopened.quota(&f.key, &provider, TUESDAY).await.unwrap();
    assert_eq!(status.reserved_tokens, 0);
    assert_eq!(status.estimated_tokens, 10);
    assert_eq!(status.available_tokens, Some(10));
}

#[tokio::test]
async fn reset_waits_for_outstanding_attempts() {
    let f = Fixture::new().await;
    let model = scope(Some("a"));
    let provider = scope(None);
    f.limit(&provider, 50, QuotaWindow::Weekly).await;
    f.limit(&model, 25, QuotaWindow::Daily).await;
    let attempt = f
        .store
        .reserve(f.request("a", Some(15), TUESDAY).await)
        .await
        .unwrap();
    f.store.mark_dispatched(attempt).await.unwrap();
    assert!(matches!(
        f.edit(&model, QuotaEdit::Reset, TUESDAY).await,
        Err(QuotaError::InFlight)
    ));
    f.store
        .settle(attempt, AttemptUsage::Observed(12))
        .await
        .unwrap();
    f.edit(
        &model,
        QuotaEdit::Update(QuotaLimit::Finite {
            tokens: 10,
            window: QuotaWindow::Daily,
        }),
        TUESDAY,
    )
    .await
    .unwrap();
    assert_eq!(f.status(&model, TUESDAY).await.available_tokens, Some(0));
    assert!(matches!(
        f.edit(
            &model,
            QuotaEdit::Update(QuotaLimit::Finite {
                tokens: 10,
                window: QuotaWindow::Monthly
            }),
            TUESDAY
        )
        .await,
        Err(QuotaError::ReconfigureRequired)
    ));
    f.edit(&model, QuotaEdit::Reset, TUESDAY + 30)
        .await
        .unwrap();
    let state = f.status(&model, TUESDAY + 30).await;
    assert_eq!(state.available_tokens, Some(10));
    assert_eq!(state.next_reset_at, Some(WEDNESDAY));
    assert_eq!(f.status(&provider, TUESDAY + 30).await.known_tokens, 12);
}

#[tokio::test]
async fn records_actual_usage_and_suspends_finite_binding_on_bound_violation() {
    let f = Fixture::new().await;
    let provider = scope(None);
    f.limit(&provider, 100, QuotaWindow::Weekly).await;
    let attempt = f
        .store
        .reserve(f.request("a", Some(5), TUESDAY).await)
        .await
        .unwrap();
    let waiting = f
        .store
        .reserve(f.request("a", Some(5), TUESDAY).await)
        .await
        .unwrap();
    f.store.mark_dispatched(attempt).await.unwrap();
    assert_eq!(
        f.store
            .settle(attempt, AttemptUsage::Observed(8))
            .await
            .unwrap(),
        QuotaSettlement::BoundExceeded
    );
    assert!(matches!(
        f.store.mark_dispatched(waiting).await,
        Err(QuotaError::InvalidAttempt)
    ));
    f.store.release_unused(waiting).await.unwrap();
    assert_eq!(f.status(&provider, TUESDAY).await.known_tokens, 8);
    assert!(matches!(
        f.store
            .reserve(f.request("a", Some(5), TUESDAY).await)
            .await,
        Err(QuotaError::Unsupported(_))
    ));
    assert!(
        f.store
            .reserve(f.request("b", Some(5), TUESDAY).await)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn revocation_invalidates_reserved_but_undispatched_attempt() {
    let f = Fixture::new().await;
    let provider = scope(None);
    f.limit(&provider, 100, QuotaWindow::Weekly).await;
    let pending = f
        .store
        .reserve(f.request("a", Some(60), TUESDAY).await)
        .await
        .unwrap();
    let revision = f.store.grant(&f.key).await.unwrap().revision;
    f.store
        .replace_grant(&f.key, revision, PeerGrant::None)
        .await
        .unwrap();
    assert!(matches!(
        f.store.mark_dispatched(pending).await,
        Err(QuotaError::InvalidAttempt)
    ));
    f.store.release_unused(pending).await.unwrap();
    assert_eq!(
        f.status(&provider, TUESDAY).await.available_tokens,
        Some(100)
    );
}

#[tokio::test]
async fn settled_requests_cannot_replay_across_restart_and_quota_reset() {
    let f = Fixture::new().await;
    let provider = scope(None);
    f.limit(&provider, 100, QuotaWindow::Weekly).await;
    let mut request = f.request("a", Some(40), TUESDAY).await;
    let attempt = f.store.reserve(request.clone()).await.unwrap();
    assert!(matches!(
        f.store.settle(attempt, AttemptUsage::Observed(20)).await,
        Err(QuotaError::InvalidAttempt)
    ));
    f.store.mark_dispatched(attempt).await.unwrap();
    assert!(matches!(
        f.store.settle(attempt, AttemptUsage::Observed(-1)).await,
        Err(QuotaError::InvalidTokens)
    ));
    assert_eq!(f.status(&provider, TUESDAY).await.reserved_tokens, 40);
    assert_eq!(
        f.store
            .settle(attempt, AttemptUsage::Observed(20))
            .await
            .unwrap(),
        QuotaSettlement::Recorded
    );
    assert_eq!(
        f.store
            .settle(attempt, AttemptUsage::Unknown)
            .await
            .unwrap(),
        QuotaSettlement::AlreadyFinal
    );
    let status = f.status(&provider, TUESDAY).await;
    assert_eq!(status.known_tokens, 20);
    assert_eq!(status.estimated_tokens, 0);
    request.grant_revision = f.edit(&provider, QuotaEdit::Reset, TUESDAY).await.unwrap();
    let reopened = SqliteMeshAccess::open(&f.path).await.unwrap();
    assert!(matches!(
        reopened.reserve(request.clone()).await,
        Err(QuotaError::DuplicateAttempt)
    ));
    request.request_id = Uuid::now_v7();
    request.bound_tokens = None;
    assert!(
        matches!(reopened.reserve(request.clone()).await, Err(QuotaError::Unsupported(scope)) if scope == provider)
    );
    request.bound_tokens = Some(40);
    let fresh = reopened
        .reserve(request)
        .await
        .expect("denial did not reserve or consume the request ID");
    reopened.release_unused(fresh).await.unwrap();
    let status = reopened.quota(&f.key, &provider, TUESDAY).await.unwrap();
    assert_eq!(status.available_tokens, Some(100));
    assert_eq!(status.known_tokens, 0);
}

#[tokio::test]
async fn unlimited_unknown_usage_survives_reconfigure_only_in_its_original_period() {
    let f = Fixture::new().await;
    let provider = scope(None);
    let attempt = f
        .store
        .reserve(f.request("a", None, TUESDAY).await)
        .await
        .unwrap();
    f.store.mark_dispatched(attempt).await.unwrap();
    f.store
        .settle(attempt, AttemptUsage::Unknown)
        .await
        .unwrap();
    let unlimited = f.status(&provider, TUESDAY).await;
    assert_eq!(unlimited.limit, QuotaLimit::Unlimited);
    assert!(unlimited.has_unknown_usage);
    assert!(unlimited.available_tokens.is_none());
    assert!(matches!(
        f.edit(&provider, QuotaEdit::Reset, TUESDAY).await,
        Err(QuotaError::NotFinite)
    ));
    let finite = QuotaLimit::Finite {
        tokens: 25,
        window: QuotaWindow::Manual,
    };
    assert!(matches!(
        f.edit(&provider, QuotaEdit::Update(finite), TUESDAY).await,
        Err(QuotaError::ReconfigureRequired)
    ));
    assert_eq!(f.status(&provider, TUESDAY).await, unlimited);
    let revision = f
        .edit(&provider, QuotaEdit::Reconfigure(finite), TUESDAY)
        .await
        .unwrap();
    assert_eq!(
        f.edit(&provider, QuotaEdit::Update(finite), TUESDAY)
            .await
            .unwrap(),
        revision
    );
    let fresh = f.status(&provider, TUESDAY).await;
    assert_eq!(fresh.available_tokens, Some(25));
    assert!(!fresh.has_unknown_usage);
    assert!(fresh.next_reset_at.is_none());
}

#[tokio::test]
async fn full_access_suspends_custom_limits_without_changing_their_balance() {
    let f = Fixture::new().await;
    let provider = scope(None);
    f.limit(&provider, 20, QuotaWindow::Manual).await;
    let attempt = f
        .store
        .reserve(f.request("a", Some(20), TUESDAY).await)
        .await
        .unwrap();
    f.store.mark_dispatched(attempt).await.unwrap();
    f.store
        .settle(attempt, AttemptUsage::Observed(20))
        .await
        .unwrap();
    let exhausted = f.status(&provider, TUESDAY).await;
    assert_eq!(exhausted.available_tokens, Some(0));
    let revision = f.store.grant(&f.key).await.unwrap().revision;
    let full = f
        .store
        .replace_grant(&f.key, revision, PeerGrant::Full)
        .await
        .unwrap();

    let mut legacy = f.request("a", None, TUESDAY).await;
    legacy.protocol = SharingProtocol::Legacy;
    let attempt = f.store.reserve(legacy).await.unwrap();
    f.store.mark_dispatched(attempt).await.unwrap();
    f.store
        .settle(attempt, AttemptUsage::Observed(500))
        .await
        .unwrap();
    assert_eq!(f.status(&provider, TUESDAY).await, exhausted);
    f.store
        .replace_grant(&f.key, full.revision, f.grant.clone())
        .await
        .unwrap();
    assert!(matches!(
        f.store
            .reserve(f.request("a", Some(1), TUESDAY).await)
            .await,
        Err(QuotaError::Exhausted(_))
    ));
    assert_eq!(f.status(&provider, TUESDAY).await, exhausted);
}
