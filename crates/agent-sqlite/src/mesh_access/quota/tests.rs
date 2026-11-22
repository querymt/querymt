use super::*;
use querymt_remote::peer_access::quota::QuotaEstimate;
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

    async fn request(&self, model: &str, reserved_tokens: i64, now: i64) -> QuotaAttempt {
        QuotaAttempt {
            peer: self.key.clone(),
            protocol: SharingProtocol::V1,
            provider_id: "provider".into(),
            model_id: model.into(),
            request_id: Uuid::now_v7(),
            estimate: QuotaEstimate {
                reserved_tokens,
                prompt_tokens: reserved_tokens / 2,
            },
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
async fn parallel_requests_can_cross_both_caps_on_the_last_admission() {
    let f = Fixture::new().await;
    let provider = scope(None);
    let model = scope(Some("a"));
    f.limit(&provider, 100, QuotaWindow::Manual).await;
    f.limit(&model, 100, QuotaWindow::Daily).await;
    let second = SqliteMeshAccess::open(&f.path).await.unwrap();
    let third = SqliteMeshAccess::open(&f.path).await.unwrap();
    let (a, b, c) = tokio::join!(
        f.store.reserve(f.request("a", 60, TUESDAY).await),
        second.reserve(f.request("a", 60, TUESDAY).await),
        third.reserve(f.request("a", 60, TUESDAY).await),
    );
    let mut admitted = Vec::new();
    for result in [a, b, c] {
        match result {
            Ok(id) => admitted.push(id),
            Err(QuotaError::Exhausted(scopes)) => assert_eq!(
                scopes,
                [
                    LimitingQuota {
                        scope: provider.clone(),
                        available_tokens: 0,
                        next_reset_at: None
                    },
                    LimitingQuota {
                        scope: model.clone(),
                        available_tokens: 0,
                        next_reset_at: Some(WEDNESDAY)
                    },
                ]
            ),
            Err(err) => panic!("unexpected admission failure: {err}"),
        }
    }
    assert_eq!(admitted.len(), 2);
    for scope in [&provider, &model] {
        let status = f.status(scope, TUESDAY).await;
        assert_eq!(status.reserved_tokens, 120);
        assert_eq!(status.available_tokens, Some(0));
    }
    for id in admitted {
        f.store.mark_dispatched(id).await.unwrap();
        f.store
            .settle(id, AttemptUsage::Observed(55))
            .await
            .unwrap();
    }
    for scope in [&provider, &model] {
        let status = f.status(scope, TUESDAY).await;
        assert_eq!(status.known_tokens, 110);
        assert_eq!(status.reserved_tokens, 0);
        assert_eq!(status.available_tokens, Some(0));
    }
}

#[tokio::test]
async fn provider_and_model_limits_are_independently_binding() {
    let f = Fixture::new().await;
    let provider = scope(None);
    let model = scope(Some("a"));
    let other_model = scope(Some("b"));
    f.limit(&model, 10, QuotaWindow::Daily).await;
    let a = f
        .store
        .reserve(f.request("a", 11, TUESDAY).await)
        .await
        .unwrap();
    assert_eq!(f.status(&provider, TUESDAY).await.reserved_tokens, 0);
    assert_eq!(f.status(&model, TUESDAY).await.reserved_tokens, 11);
    assert!(matches!(
        f.store.reserve(f.request("a", 1, TUESDAY).await).await,
        Err(QuotaError::Exhausted(ref scopes)) if scopes == &[LimitingQuota { scope: model.clone(), available_tokens: 0, next_reset_at: Some(WEDNESDAY) }]
    ));
    f.store.mark_dispatched(a).await.unwrap();
    f.store.settle(a, AttemptUsage::Observed(11)).await.unwrap();

    f.limit(&provider, 10, QuotaWindow::Weekly).await;
    assert!(matches!(
        f.store.reserve(f.request("a", 1, TUESDAY).await).await,
        Err(QuotaError::Exhausted(ref scopes)) if scopes.len() == 1 && scopes[0].scope == model
    ));
    assert_eq!(f.status(&provider, TUESDAY).await.reserved_tokens, 0);
    let b = f
        .store
        .reserve(f.request("b", 11, TUESDAY).await)
        .await
        .unwrap();
    assert_eq!(f.status(&provider, TUESDAY).await.reserved_tokens, 11);
    assert_eq!(f.status(&other_model, TUESDAY).await.reserved_tokens, 0);
    assert!(matches!(
        f.store.reserve(f.request("b", 1, TUESDAY).await).await,
        Err(QuotaError::Exhausted(ref scopes)) if scopes == &[LimitingQuota { scope: provider, available_tokens: 0, next_reset_at: Some(MONDAY) }]
    ));
    f.store.mark_dispatched(b).await.unwrap();
    f.store.settle(b, AttemptUsage::Unknown).await.unwrap();
    assert!(f.status(&other_model, TUESDAY).await.has_unknown_usage);
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
        .reserve(f.request("daily", 20, TUESDAY).await)
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
    let req = f.request("a", 10, TUESDAY).await;
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
async fn quota_rollover_keeps_two_periods_and_an_older_open_reservation() {
    let f = Fixture::new().await;
    let model = scope(Some("a"));
    let provider = scope(None);
    f.limit(&model, 30, QuotaWindow::Daily).await;
    f.limit(&provider, 30, QuotaWindow::Daily).await;
    let mut request = f.request("a", 15, TUESDAY).await;
    let attempt = f.store.reserve(request.clone()).await.unwrap();
    f.store.mark_dispatched(attempt).await.unwrap();
    for now in [WEDNESDAY, FRIDAY, MONDAY] {
        assert_eq!(f.status(&model, now).await.available_tokens, Some(30));
        assert_eq!(f.status(&provider, now).await.available_tokens, Some(30));
    }
    let periods = |sql: &'static str| {
        f.store.with_conn(move |conn| {
            conn.query_row(sql, [], |row| row.get::<_, i64>(0))
                .map_err(backend)
        })
    };
    let count = "SELECT count(*) FROM quota_periods p JOIN quota_scopes s ON s.id=p.scope_id WHERE s.model_id='a'";
    assert_eq!(periods(count).await.unwrap(), 3);
    assert_eq!(
        f.store
            .settle(attempt, AttemptUsage::Observed(8))
            .await
            .unwrap(),
        QuotaSettlement::Recorded
    );
    assert_eq!(
        periods("SELECT sum(known) FROM quota_reservations")
            .await
            .unwrap(),
        16
    );

    let current = f.request("a", 1, MONDAY).await;
    let unused = f.store.reserve(current.clone()).await.unwrap();
    f.store.release_unused(unused).await.unwrap();

    let reopened = SqliteMeshAccess::open(&f.path).await.unwrap();
    reopened.reconcile_startup(&f.key.host).await.unwrap();
    assert_eq!(periods(count).await.unwrap(), 2);
    assert_eq!(f.status(&model, MONDAY).await.known_tokens, 0);
    assert!(matches!(
        reopened.reserve(current).await,
        Err(QuotaError::DuplicateAttempt)
    ));
    request.now = MONDAY;
    reopened.reserve(request).await.expect("expired replay ID");
}

#[tokio::test]
async fn replay_id_expires_only_after_both_scopes_age_out() {
    let f = Fixture::new().await;
    let model = scope(Some("a"));
    let provider = scope(None);
    f.limit(&model, 30, QuotaWindow::Daily).await;
    f.limit(&provider, 30, QuotaWindow::Daily).await;

    let mut expired = f.request("a", 10, TUESDAY).await;
    let settled = f.store.reserve(expired.clone()).await.unwrap();
    f.store.mark_dispatched(settled).await.unwrap();
    f.store
        .settle(settled, AttemptUsage::Observed(8))
        .await
        .unwrap();
    let open = f
        .store
        .reserve(f.request("a", 1, TUESDAY).await)
        .await
        .unwrap();

    f.status(&model, WEDNESDAY).await;
    let mut previous = f.request("a", 10, FRIDAY).await;
    let unused = f.store.reserve(previous.clone()).await.unwrap();
    f.store.release_unused(unused).await.unwrap();
    f.status(&model, MONDAY).await;

    expired.now = MONDAY;
    assert!(matches!(
        f.store.reserve(expired.clone()).await,
        Err(QuotaError::DuplicateAttempt)
    ));
    f.status(&provider, MONDAY).await;
    previous.now = MONDAY;
    assert!(matches!(
        f.store.reserve(previous).await,
        Err(QuotaError::DuplicateAttempt)
    ));
    f.store.reserve(expired).await.expect("expired replay ID");
    f.store.mark_dispatched(open).await.unwrap();
    assert_eq!(
        f.store
            .settle(open, AttemptUsage::Observed(1))
            .await
            .unwrap(),
        QuotaSettlement::Recorded
    );
}

#[tokio::test]
async fn committed_settlement_notifies_only_its_peer_without_changing_revision() {
    let f = Fixture::new().await;
    f.limit(&scope(None), 20, QuotaWindow::Weekly).await;
    let request = f.request("a", 10, TUESDAY).await;
    let revision = f.store.grant(&f.key).await.unwrap().revision;
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
    let reserved = ReservedAttempt::reserve(store.clone(), f.request("a", 5, TUESDAY).await)
        .await
        .unwrap();
    let dispatched = ReservedAttempt::reserve(store, f.request("b", 10, TUESDAY).await)
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
async fn reservation_checks_current_permission_without_consuming_replay_on_denial() {
    let f = Fixture::new().await;
    let provider = scope(None);
    f.limit(&provider, 20, QuotaWindow::Weekly).await;
    let request = f.request("a", 15, TUESDAY).await;
    let revision = f.store.grant(&f.key).await.unwrap().revision;
    let revoked = f
        .store
        .replace_grant(&f.key, revision, PeerGrant::None)
        .await
        .unwrap();
    assert!(matches!(
        f.store.reserve(request.clone()).await,
        Err(QuotaError::Denied)
    ));
    let status = f.store.quota(&f.key, &provider, TUESDAY).await.unwrap();
    assert_eq!(status.reserved_tokens, 0);
    // No attempt or replay tombstone was created for the rejected reservation.
    f.store
        .replace_grant(&f.key, revoked.revision, f.grant.clone())
        .await
        .unwrap();
    f.store.reserve(request).await.unwrap();
}

#[tokio::test]
async fn missing_usage_policy_applies_to_live_and_recovered_attempts() {
    for (charge, expected) in [
        (UnknownUsageCharge::default(), 18),
        (UnknownUsageCharge::EstimatedPrompt, 9),
    ] {
        let mut f = Fixture::new().await;
        f.store = f.store.with_unknown_usage_charge(charge);
        let provider = scope(None);
        let model = scope(Some("a"));
        f.limit(&provider, 50, QuotaWindow::Weekly).await;
        f.limit(&model, 50, QuotaWindow::Daily).await;
        let first = f
            .store
            .reserve(f.request("a", 18, TUESDAY).await)
            .await
            .unwrap();
        f.store.mark_dispatched(first).await.unwrap();
        assert_eq!(
            f.store.settle(first, AttemptUsage::Unknown).await.unwrap(),
            QuotaSettlement::Recorded
        );
        for scope in [&provider, &model] {
            let status = f.status(scope, TUESDAY).await;
            assert_eq!(status.estimated_tokens, expected);
            assert!(!status.has_unknown_usage);
        }

        let pending = f
            .store
            .reserve(f.request("a", 18, TUESDAY).await)
            .await
            .unwrap();
        let dispatched = f
            .store
            .reserve(f.request("a", 18, TUESDAY).await)
            .await
            .unwrap();
        f.store.mark_dispatched(dispatched).await.unwrap();
        let reopened = SqliteMeshAccess::open(&f.path)
            .await
            .unwrap()
            .with_unknown_usage_charge(charge);
        reopened.reconcile_startup(&f.key.host).await.unwrap();
        reopened.reconcile_startup(&f.key.host).await.unwrap();
        assert!(matches!(
            reopened.mark_dispatched(pending).await,
            Err(QuotaError::InvalidAttempt)
        ));
        for scope in [&provider, &model] {
            let status = reopened.quota(&f.key, scope, TUESDAY).await.unwrap();
            assert_eq!(status.known_tokens, 0);
            assert_eq!(status.estimated_tokens, expected * 2);
            assert_eq!(status.reserved_tokens, 0);
            assert_eq!(status.available_tokens, Some(50 - expected * 2));
            assert!(!status.has_unknown_usage);
        }
    }
}

#[tokio::test]
async fn v2_upgrade_preserves_usage_and_replay_but_removes_binding_faults() {
    let f = Fixture::new().await;
    let path = f._dir.path().join("old-mesh.db");
    let provider = scope(None);
    let model = scope(Some("a"));
    let settled = f.request("a", 5, TUESDAY).await;
    let unresolved = f.request("a", 9, TUESDAY).await;
    let revision = {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(GRANT_SCHEMA_V1).unwrap();
        conn.execute_batch(include_str!("../quota_v2.sql")).unwrap();
        conn.pragma_update(None, "application_id", APPLICATION_ID)
            .unwrap();
        conn.pragma_update(None, "user_version", 2).unwrap();
        let revision = save_grant(&conn, &f.key, &f.grant).unwrap();
        let host = f.key.host.to_string();
        let peer = f.key.peer.to_string();
        let mut periods = Vec::new();
        for scope in [&provider, &model] {
            let id = scope_id(&conn, &f.key, scope, true).unwrap().unwrap();
            let limit = QuotaLimit::Finite {
                tokens: 30,
                window: QuotaWindow::Manual,
            };
            periods.push(open_period(&conn, id, limit, TUESDAY, None).unwrap().id);
        }
        for (request, state, known) in [(&settled, "settled", 7), (&unresolved, "dispatched", 0)] {
            let id = Uuid::now_v7().to_string();
            conn.execute(
                "INSERT INTO quota_attempts (id,host_id,peer_id,provider_id,model_id,request_id,state)
                 VALUES (?1,?2,?3,'provider','a',?4,?5)",
                params![id, host, peer, request.request_id.to_string(), state],
            ).unwrap();
            for period in &periods {
                conn.execute(
                    "INSERT INTO quota_reservations (attempt_id,period_id,reserved,known) VALUES (?1,?2,?3,?4)",
                    params![id, period, request.estimate.reserved_tokens, known],
                ).unwrap();
            }
        }
        conn.execute(
            "INSERT INTO quota_binding_faults VALUES (?1,'provider','a')",
            [host],
        )
        .unwrap();
        revision
    };
    let upgraded = SqliteMeshAccess::open(&path)
        .await
        .unwrap()
        .with_unknown_usage_charge(UnknownUsageCharge::EstimatedPrompt);
    upgraded.reconcile_startup(&f.key.host).await.unwrap();
    assert_eq!(upgraded.grant(&f.key).await.unwrap().revision, revision);
    assert!(
        !upgraded
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='quota_binding_faults')",
                    [],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(backend)
            })
            .await
            .unwrap()
    );
    for scope in [&provider, &model] {
        let status = upgraded.quota(&f.key, scope, TUESDAY).await.unwrap();
        assert_eq!(status.known_tokens, 7);
        assert_eq!(status.estimated_tokens, 9);
        assert_eq!(status.available_tokens, Some(14));
    }
    for request in [settled, unresolved] {
        assert!(matches!(
            upgraded.reserve(request).await,
            Err(QuotaError::DuplicateAttempt)
        ));
    }
    let fresh = upgraded
        .reserve(f.request("a", 20, TUESDAY).await)
        .await
        .unwrap();
    upgraded.mark_dispatched(fresh).await.unwrap();
    upgraded.settle(fresh, AttemptUsage::Unknown).await.unwrap();
    for scope in [&provider, &model] {
        assert_eq!(
            upgraded
                .quota(&f.key, scope, TUESDAY)
                .await
                .unwrap()
                .estimated_tokens,
            19
        );
    }
}

#[tokio::test]
async fn restart_and_regrant_do_not_top_up_an_allowance() {
    let f = Fixture::new().await;
    let provider = scope(None);
    f.limit(&provider, 20, QuotaWindow::Weekly).await;
    let pending = f
        .store
        .reserve(f.request("a", 5, TUESDAY).await)
        .await
        .unwrap();
    let dispatched = f
        .store
        .reserve(f.request("b", 10, TUESDAY).await)
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
        reopened.reserve(f.request("b", 1, TUESDAY).await).await,
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
        .reserve(f.request("a", 15, TUESDAY).await)
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
async fn actual_usage_above_the_estimate_does_not_suspend_the_binding() {
    let f = Fixture::new().await;
    let provider = scope(None);
    let model = scope(Some("a"));
    f.limit(&provider, 100, QuotaWindow::Weekly).await;
    f.limit(&model, 20, QuotaWindow::Daily).await;
    let first = f
        .store
        .reserve(f.request("a", 5, TUESDAY).await)
        .await
        .unwrap();
    let waiting = f
        .store
        .reserve(f.request("a", 5, TUESDAY).await)
        .await
        .unwrap();
    f.store.mark_dispatched(first).await.unwrap();
    assert_eq!(
        f.store
            .settle(first, AttemptUsage::Observed(8))
            .await
            .unwrap(),
        QuotaSettlement::Recorded
    );
    assert_eq!(f.status(&provider, TUESDAY).await.known_tokens, 8);
    assert_eq!(f.status(&model, TUESDAY).await.known_tokens, 8);
    f.store.mark_dispatched(waiting).await.unwrap();
    let crossing = f
        .store
        .reserve(f.request("a", 8, TUESDAY).await)
        .await
        .unwrap();
    assert!(matches!(
        f.store.reserve(f.request("a", 1, TUESDAY).await).await,
        Err(QuotaError::Exhausted(ref scopes)) if scopes.len() == 1 && scopes[0].scope == model
    ));
    f.store.mark_dispatched(crossing).await.unwrap();
    f.store
        .settle(waiting, AttemptUsage::Observed(4))
        .await
        .unwrap();
    f.store
        .settle(crossing, AttemptUsage::Observed(11))
        .await
        .unwrap();
    assert_eq!(f.status(&model, TUESDAY).await.known_tokens, 23);
    assert_eq!(f.status(&provider, TUESDAY).await.known_tokens, 23);
    assert!(matches!(
        f.store.reserve(f.request("a", 1, TUESDAY).await).await,
        Err(QuotaError::Exhausted(ref scopes)) if scopes.len() == 1 && scopes[0].scope == model
    ));
    assert!(
        f.store
            .reserve(f.request("a", 1, WEDNESDAY).await)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn overshoot_beyond_i64_totals_still_exhausts_the_scope() {
    let f = Fixture::new().await;
    let provider = scope(None);
    f.limit(&provider, i64::MAX, QuotaWindow::Manual).await;
    let first = f
        .store
        .reserve(f.request("a", i64::MAX - 1, TUESDAY).await)
        .await
        .unwrap();
    let last = f
        .store
        .reserve(f.request("a", i64::MAX, TUESDAY).await)
        .await
        .unwrap();
    let status = f.status(&provider, TUESDAY).await;
    assert_eq!(status.reserved_tokens, i64::MAX);
    assert_eq!(status.available_tokens, Some(0));
    for id in [first, last] {
        f.store.mark_dispatched(id).await.unwrap();
        f.store
            .settle(id, AttemptUsage::Observed(i64::MAX))
            .await
            .unwrap();
    }
    let status = f.status(&provider, TUESDAY).await;
    assert_eq!(status.known_tokens, i64::MAX);
    assert_eq!(status.available_tokens, Some(0));
    assert!(matches!(
        f.store.reserve(f.request("a", 1, TUESDAY).await).await,
        Err(QuotaError::Exhausted(_))
    ));
}

#[tokio::test]
async fn revocation_invalidates_reserved_but_undispatched_attempt() {
    let f = Fixture::new().await;
    let provider = scope(None);
    f.limit(&provider, 100, QuotaWindow::Weekly).await;
    let pending = f
        .store
        .reserve(f.request("a", 60, TUESDAY).await)
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
async fn lowering_a_cap_denies_new_work_without_interrupting_admitted_attempts() {
    let mut f = Fixture::new().await;
    f.store = SqliteMeshAccess::open_exclusive(&f.path).await.unwrap();
    let provider = scope(None);
    let model = scope(Some("a"));
    f.limit(&provider, 150, QuotaWindow::Manual).await;
    f.limit(&model, 100, QuotaWindow::Manual).await;
    let running = f
        .store
        .reserve(f.request("a", 40, TUESDAY).await)
        .await
        .unwrap();
    f.store.mark_dispatched(running).await.unwrap();
    let waiting = f
        .store
        .reserve(f.request("a", 30, TUESDAY).await)
        .await
        .unwrap();
    let mut changes = f.store.subscribe_changes().unwrap();
    f.edit(
        &model,
        QuotaEdit::Update(QuotaLimit::Finite {
            tokens: 35,
            window: QuotaWindow::Manual,
        }),
        TUESDAY,
    )
    .await
    .unwrap();
    assert!(matches!(
        changes.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    assert_eq!(f.status(&provider, TUESDAY).await.reserved_tokens, 70);
    assert_eq!(f.status(&model, TUESDAY).await.reserved_tokens, 70);
    assert!(matches!(
        f.store.reserve(f.request("a", 1, TUESDAY).await).await,
        Err(QuotaError::Exhausted(ref scopes)) if scopes.len() == 1 && scopes[0].scope == model
    ));
    f.store.mark_dispatched(waiting).await.unwrap();
    f.store
        .settle(running, AttemptUsage::Observed(40))
        .await
        .unwrap();
    f.store
        .settle(waiting, AttemptUsage::Observed(20))
        .await
        .unwrap();
    assert_eq!(f.status(&model, TUESDAY).await.known_tokens, 60);
    assert_eq!(f.status(&model, TUESDAY).await.reserved_tokens, 0);
    assert_eq!(
        f.status(&provider, TUESDAY).await.available_tokens,
        Some(90)
    );
}

#[tokio::test]
async fn settled_requests_cannot_replay_across_restart_and_quota_reset() {
    let f = Fixture::new().await;
    let provider = scope(None);
    f.limit(&provider, 100, QuotaWindow::Weekly).await;
    let mut request = f.request("a", 40, TUESDAY).await;
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
    f.edit(&provider, QuotaEdit::Reset, TUESDAY).await.unwrap();
    let reopened = SqliteMeshAccess::open(&f.path).await.unwrap();
    assert!(matches!(
        reopened.reserve(request.clone()).await,
        Err(QuotaError::DuplicateAttempt)
    ));
    request.request_id = Uuid::now_v7();
    request.estimate.reserved_tokens = 0;
    assert!(matches!(
        reopened.reserve(request.clone()).await,
        Err(QuotaError::InvalidTokens)
    ));
    request.estimate.reserved_tokens = 40;
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
    let mut f = Fixture::new().await;
    f.store = f
        .store
        .with_unknown_usage_charge(UnknownUsageCharge::EstimatedPrompt);
    let provider = scope(None);
    let attempt = f
        .store
        .reserve(f.request("a", 0, TUESDAY).await)
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
        .reserve(f.request("a", 20, TUESDAY).await)
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

    let mut legacy = f.request("a", 0, TUESDAY).await;
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
        f.store.reserve(f.request("a", 1, TUESDAY).await).await,
        Err(QuotaError::Exhausted(_))
    ));
    assert_eq!(f.status(&provider, TUESDAY).await, exhausted);
}
