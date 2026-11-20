//! Transactional provider and model accounting for the standalone mesh store.

use querymt_remote::peer_access::quota::{
    AttemptUsage, LimitingQuota, QuotaAttempt, QuotaEdit, QuotaError, QuotaLimit, QuotaScope,
    QuotaSettlement, QuotaStatus, QuotaStore, QuotaWindow, ReservationId,
};
use time::{Date, Duration, Month, OffsetDateTime};

use super::*;

#[derive(Debug, Clone, Copy)]
struct Period {
    id: i64,
    limit: QuotaLimit,
    started_at: i64,
    next_reset_at: Option<i64>,
}

fn begin(conn: &mut Connection) -> Result<rusqlite::Transaction<'_>, GrantStoreError> {
    conn.transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(backend)
}

fn cadence(window: QuotaWindow) -> &'static str {
    match window {
        QuotaWindow::Manual => "manual",
        QuotaWindow::Daily => "daily",
        QuotaWindow::Weekly => "weekly",
        QuotaWindow::Monthly => "monthly",
    }
}

fn utc_date(now: i64) -> Result<Date, QuotaError> {
    OffsetDateTime::from_unix_timestamp(now)
        .map(|dt| dt.date())
        .map_err(|_| QuotaError::InvalidTime)
}

fn boundaries(window: QuotaWindow, now: i64) -> Result<Option<(i64, i64)>, QuotaError> {
    let day = utc_date(now)?;
    let (start, end) = match window {
        QuotaWindow::Manual => return Ok(None),
        QuotaWindow::Daily => (day, day.next_day()),
        QuotaWindow::Weekly => {
            let monday = day
                .checked_sub(Duration::days(i64::from(
                    day.weekday().number_days_from_monday(),
                )))
                .ok_or(QuotaError::InvalidTime)?;
            (monday, monday.checked_add(Duration::days(7)))
        }
        QuotaWindow::Monthly => {
            let first = Date::from_calendar_date(day.year(), day.month(), 1);
            let next = if day.month() == Month::December {
                Date::from_calendar_date(day.year() + 1, Month::January, 1)
            } else {
                Date::from_calendar_date(day.year(), day.month().next(), 1)
            };
            (first.map_err(|_| QuotaError::InvalidTime)?, next.ok())
        }
    };
    let end = end.ok_or(QuotaError::InvalidTime)?;
    Ok(Some((
        start.midnight().assume_utc().unix_timestamp(),
        end.midnight().assume_utc().unix_timestamp(),
    )))
}

fn scope_id(
    conn: &Connection,
    key: &PeerGrantKey,
    scope: &QuotaScope,
    create: bool,
) -> Result<Option<i64>, GrantStoreError> {
    let host = key.host.to_string();
    let peer = key.peer.to_string();
    // Empty ID is reserved for the provider-wide scope; validate model IDs first.
    let model = scope.model_id.as_deref().unwrap_or_default();
    if create {
        conn.execute(
            "INSERT INTO quota_scopes (host_id,peer_id,provider_id,model_id) VALUES (?1,?2,?3,?4)
             ON CONFLICT (host_id,peer_id,provider_id,model_id) DO NOTHING",
            params![host, peer, scope.provider_id, model],
        )
        .map_err(backend)?;
    }
    conn.query_row(
        "SELECT id FROM quota_scopes WHERE host_id=?1 AND peer_id=?2 AND provider_id=?3 AND model_id=?4",
        params![host, peer, scope.provider_id, model], |row| row.get(0),
    ).optional().map_err(backend)
}

fn current_period(conn: &Connection, id: i64) -> Result<Option<Period>, GrantStoreError> {
    let row = conn.query_row(
        "SELECT id,cap,cadence,started_at,next_reset_at FROM quota_periods WHERE scope_id=?1 AND current=1",
        [id], |row| Ok((row.get(0)?, row.get::<_, Option<i64>>(1)?, row.get::<_, String>(2)?, row.get(3)?, row.get(4)?)),
    ).optional().map_err(backend)?;
    row.map(|(id, cap, window, started_at, next_reset_at)| {
        let window = match window.as_str() {
            "manual" => QuotaWindow::Manual,
            "daily" => QuotaWindow::Daily,
            "weekly" => QuotaWindow::Weekly,
            "monthly" => QuotaWindow::Monthly,
            _ => {
                return Err(backend(std::io::Error::other(
                    "invalid stored quota cadence",
                )));
            }
        };
        let limit = cap.map_or(QuotaLimit::Unlimited, |tokens| QuotaLimit::Finite {
            tokens,
            window,
        });
        Ok(Period {
            id,
            limit,
            started_at,
            next_reset_at,
        })
    })
    .transpose()
}

fn open_period(
    conn: &Connection,
    scope: i64,
    limit: QuotaLimit,
    started_at: i64,
    next_reset_at: Option<i64>,
) -> Result<Period, GrantStoreError> {
    let (cap, window) = match limit {
        QuotaLimit::Unlimited => (None, QuotaWindow::Manual),
        QuotaLimit::Finite { tokens, window } => (Some(tokens), window),
    };
    conn.execute(
        "UPDATE quota_periods SET current=0 WHERE scope_id=?1 AND current=1",
        [scope],
    )
    .map_err(backend)?;
    conn.execute("INSERT INTO quota_periods (scope_id,cap,cadence,started_at,next_reset_at,current) VALUES (?1,?2,?3,?4,?5,1)",
        params![scope, cap, cadence(window), started_at, next_reset_at],
    ).map_err(backend)?;
    Ok(Period {
        id: conn.last_insert_rowid(),
        limit,
        started_at,
        next_reset_at,
    })
}

fn roll_forward(
    conn: &Connection,
    scope: i64,
    period: Period,
    now: i64,
) -> Result<Period, QuotaError> {
    if let QuotaLimit::Finite { window, .. } = period.limit
        && period.next_reset_at.is_some_and(|reset| now >= reset)
        && let Some((start, end)) = boundaries(window, now)?
    {
        return Ok(open_period(conn, scope, period.limit, start, Some(end))?);
    }
    Ok(period)
}

fn status(
    conn: &Connection,
    scope: QuotaScope,
    period: Option<Period>,
) -> Result<QuotaStatus, GrantStoreError> {
    let mut status = QuotaStatus {
        scope,
        limit: QuotaLimit::Unlimited,
        known_tokens: 0,
        estimated_tokens: 0,
        has_unknown_usage: false,
        reserved_tokens: 0,
        available_tokens: None,
        window_started_at: None,
        next_reset_at: None,
    };
    if let Some(period) = period {
        let (known, estimated, unknown, reserved) = conn.query_row(
            "SELECT COALESCE(SUM(r.known),0), COALESCE(SUM(r.estimated),0), COALESCE(MAX(r.unknown),0),
               COALESCE(SUM(CASE WHEN a.state IN ('reserved','dispatched') THEN r.reserved END),0)
             FROM quota_reservations r JOIN quota_attempts a ON a.id=r.attempt_id WHERE r.period_id=?1",
            [period.id], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get(2)?, row.get::<_, i64>(3)?)),
        ).map_err(backend)?;
        status.known_tokens = known;
        status.estimated_tokens = estimated;
        status.has_unknown_usage = unknown;
        status.limit = period.limit;
        if let QuotaLimit::Finite { tokens, .. } = period.limit {
            status.reserved_tokens = reserved;
            status.available_tokens = Some(
                tokens
                    .saturating_sub(known)
                    .saturating_sub(estimated)
                    .saturating_sub(reserved)
                    .max(0),
            );
            status.window_started_at = Some(period.started_at);
            status.next_reset_at = period.next_reset_at;
        }
    }
    Ok(status)
}

fn has_open_attempts(
    conn: &Connection,
    key: &PeerGrantKey,
    scope: &QuotaScope,
) -> Result<bool, GrantStoreError> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM quota_attempts WHERE host_id=?1 AND peer_id=?2 AND provider_id=?3
          AND (?4 IS NULL OR model_id=?4) AND state IN ('reserved','dispatched'))",
        params![key.host.to_string(), key.peer.to_string(), scope.provider_id, scope.model_id],
        |row| row.get(0),
    ).map_err(backend)
}

fn settle_attempt(
    conn: &Connection,
    id: &str,
    usage: AttemptUsage,
) -> Result<QuotaSettlement, QuotaError> {
    let state = conn
        .query_row(
            "SELECT state FROM quota_attempts WHERE id=?1",
            [id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(backend)?;
    match state.as_deref() {
        Some("settled" | "released") => return Ok(QuotaSettlement::AlreadyFinal),
        Some("dispatched") => {}
        _ => return Err(QuotaError::InvalidAttempt),
    }
    let mut outcome = QuotaSettlement::Recorded;
    match usage {
        AttemptUsage::Observed(actual) if actual < 0 => return Err(QuotaError::InvalidTokens),
        AttemptUsage::Observed(actual) => {
            conn.execute(
                "UPDATE quota_reservations SET known=?2 WHERE attempt_id=?1",
                params![id, actual],
            )
            .map_err(backend)?;
            let faulty = conn.execute(
                "INSERT INTO quota_binding_faults (host_id, provider_id, model_id)
                 SELECT a.host_id,a.provider_id,a.model_id FROM quota_attempts a WHERE a.id=?1
                 AND EXISTS(SELECT 1 FROM quota_reservations WHERE attempt_id=?1 AND reserved<?2)
                 ON CONFLICT(host_id,provider_id,model_id) DO UPDATE SET model_id=excluded.model_id",
                params![id, actual],
            ).map_err(backend)?;
            if faulty != 0 {
                outcome = QuotaSettlement::BoundExceeded;
            }
        }
        AttemptUsage::Unknown => {
            conn.execute(
                "UPDATE quota_reservations SET estimated=COALESCE(reserved,0), unknown=reserved IS NULL WHERE attempt_id=?1",
                [id],
            ).map_err(backend)?;
        }
    }
    conn.execute(
        "UPDATE quota_attempts SET state='settled' WHERE id=?1",
        [id],
    )
    .map_err(backend)?;
    Ok(outcome)
}

#[async_trait]
impl QuotaStore for SqliteMeshAccess {
    async fn quota(
        &self,
        key: &PeerGrantKey,
        scope: &QuotaScope,
        now: i64,
    ) -> Result<QuotaStatus, QuotaError> {
        scope.validate()?;
        utc_date(now)?;
        let (key, scope) = (key.clone(), scope.clone());
        self.with_conn(move |conn| {
            let tx = begin(conn)?;
            let mut period = None;
            if let Some(id) = scope_id(&tx, &key, &scope, false)?
                && let Some(current) = current_period(&tx, id)?
            {
                period = Some(roll_forward(&tx, id, current, now)?);
            }
            let status = status(&tx, scope, period)?;
            tx.commit().map_err(backend)?;
            Ok(status)
        })
        .await
    }

    async fn edit_quota(
        &self,
        key: &PeerGrantKey,
        expected: Uuid,
        scope: &QuotaScope,
        edit: QuotaEdit,
        now: i64,
    ) -> Result<Uuid, QuotaError> {
        scope.validate()?;
        utc_date(now)?;
        if let QuotaEdit::Update(limit) | QuotaEdit::Reconfigure(limit) = edit {
            limit.validate()?;
        }
        let (key, scope) = (key.clone(), scope.clone());
        self.with_conn(move |conn| {
            let tx = begin(conn)?;
            let grant = load_grant(&tx, &key)?;
            if expected != grant.revision {
                return Err(GrantStoreError::Conflict {
                    expected,
                    actual: grant.revision,
                }
                .into());
            }
            let id = scope_id(&tx, &key, &scope, true)?.ok_or(QuotaError::InvalidScope)?;
            let current = current_period(&tx, id)?
                .map(|period| roll_forward(&tx, id, period, now))
                .transpose()?;
            match edit {
                QuotaEdit::Update(limit) => {
                    let Some(period) = current else {
                        return Err(QuotaError::ReconfigureRequired);
                    };
                    let (
                        QuotaLimit::Finite { window, .. },
                        QuotaLimit::Finite {
                            window: proposed,
                            tokens,
                        },
                    ) = (period.limit, limit)
                    else {
                        return Err(QuotaError::ReconfigureRequired);
                    };
                    if window != proposed {
                        return Err(QuotaError::ReconfigureRequired);
                    }
                    if period.limit == limit {
                        tx.commit().map_err(backend)?;
                        return Ok(grant.revision);
                    }
                    tx.execute(
                        "UPDATE quota_periods SET cap=?2 WHERE id=?1",
                        params![period.id, tokens],
                    )
                    .map_err(backend)?;
                }
                QuotaEdit::Reset | QuotaEdit::Reconfigure(_) => {
                    let clock = current.map_or(now, |period| now.max(period.started_at));
                    let (limit, next) = match edit {
                        QuotaEdit::Reset => match current {
                            Some(period) if matches!(period.limit, QuotaLimit::Finite { .. }) => {
                                (period.limit, period.next_reset_at)
                            }
                            _ => return Err(QuotaError::NotFinite),
                        },
                        QuotaEdit::Reconfigure(limit) => {
                            let next = match limit {
                                QuotaLimit::Unlimited => None,
                                QuotaLimit::Finite { window, .. } => {
                                    boundaries(window, clock)?.map(|(_, end)| end)
                                }
                            };
                            (limit, next)
                        }
                        QuotaEdit::Update(_) => unreachable!(),
                    };
                    if has_open_attempts(&tx, &key, &scope)? {
                        return Err(QuotaError::InFlight);
                    }
                    open_period(&tx, id, limit, clock, next)?;
                }
            }
            let revision = save_grant(&tx, &key, &grant.grant)?;
            tx.commit().map_err(backend)?;
            Ok(revision)
        })
        .await
    }

    async fn reserve(&self, attempt: QuotaAttempt) -> Result<ReservationId, QuotaError> {
        utc_date(attempt.now)?;
        if attempt.bound_tokens.is_some_and(|tokens| tokens <= 0) {
            return Err(QuotaError::InvalidTokens);
        }
        let provider = QuotaScope {
            provider_id: attempt.provider_id.clone(),
            model_id: None,
        };
        let model = QuotaScope {
            provider_id: attempt.provider_id.clone(),
            model_id: Some(attempt.model_id.clone()),
        };
        model.validate()?;
        self.with_conn(move |conn| {
            let tx = begin(conn)?;
            let grant = load_grant(&tx, &attempt.peer)?;
            if grant.revision != attempt.grant_revision
                || !grant.grant.allows_model(attempt.protocol, &attempt.provider_id, &attempt.model_id)
            {
                return Err(QuotaError::Denied);
            }
            let host = attempt.peer.host.to_string();
            let peer = attempt.peer.peer.to_string();
            let replay_id = attempt.request_id.to_string();
            let duplicate: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM quota_attempts WHERE host_id=?1 AND peer_id=?2 AND request_id=?3)",
                params![host, peer, replay_id], |row| row.get(0),
            ).map_err(backend)?;
            if duplicate {
                return Err(QuotaError::DuplicateAttempt);
            }
            let id = ReservationId(Uuid::now_v7());
            let mut periods = Vec::with_capacity(2);
            let mut limiting = Vec::new();
            let mut unsupported = None;
            if grant.grant != PeerGrant::Full {
                let faulty: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM quota_binding_faults WHERE host_id=?1 AND provider_id=?2 AND model_id=?3)",
                    params![host, attempt.provider_id, attempt.model_id], |row| row.get(0),
                ).map_err(backend)?;
                for scope in [provider, model] {
                    let scope_id = scope_id(&tx, &attempt.peer, &scope, true)?.ok_or(QuotaError::InvalidScope)?;
                    let period = if let Some(period) = current_period(&tx, scope_id)? {
                        roll_forward(&tx, scope_id, period, attempt.now)?
                    } else {
                        open_period(&tx, scope_id, QuotaLimit::Unlimited, attempt.now, None)?
                    };
                    let mut reservation = None;
                    if let QuotaLimit::Finite { .. } = period.limit {
                        match attempt.bound_tokens {
                            Some(bound) if !faulty => {
                                let status = status(&tx, scope, Some(period))?;
                                let available = status.available_tokens.ok_or(QuotaError::InvalidScope)?;
                                if bound > available {
                                    limiting.push(LimitingQuota { scope: status.scope, available_tokens: available, next_reset_at: status.next_reset_at });
                                } else {
                                    reservation = Some(bound);
                                }
                            }
                            _ => { unsupported.get_or_insert(scope); }
                        }
                    }
                    periods.push((period.id, reservation));
                }
            }
            // Persist any observed rollover but never write part of a reservation.
            if let Some(scope) = unsupported {
                tx.commit().map_err(backend)?;
                return Err(QuotaError::Unsupported(scope));
            }
            if !limiting.is_empty() {
                tx.commit().map_err(backend)?;
                return Err(QuotaError::Exhausted(limiting));
            }
            let id_text = id.0.to_string();
            tx.execute(
                "INSERT INTO quota_attempts (id,host_id,peer_id,provider_id,model_id,request_id,grant_revision,state)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,'reserved')",
                params![id_text, host, peer, attempt.provider_id, attempt.model_id, replay_id, grant.revision.to_string()],
            ).map_err(backend)?;
            for (period, bound) in periods {
                tx.execute("INSERT INTO quota_reservations (attempt_id,period_id,reserved) VALUES (?1,?2,?3)", params![id_text, period, bound]).map_err(backend)?;
            }
            tx.commit().map_err(backend)?;
            Ok(id)
        }).await
    }

    async fn mark_dispatched(&self, attempt: ReservationId) -> Result<(), QuotaError> {
        self.with_conn(move |conn| {
            let changed = conn.execute(
                "UPDATE quota_attempts AS a SET state='dispatched' WHERE id=?1 AND state='reserved'
                 AND EXISTS(SELECT 1 FROM peer_grants g WHERE g.host_id=a.host_id AND g.peer_id=a.peer_id AND g.revision=a.grant_revision)
                 AND NOT (EXISTS(SELECT 1 FROM quota_binding_faults f WHERE f.host_id=a.host_id AND f.provider_id=a.provider_id AND f.model_id=a.model_id)
                     AND EXISTS(SELECT 1 FROM quota_reservations r WHERE r.attempt_id=a.id AND r.reserved IS NOT NULL))",
                [attempt.0.to_string()],
            ).map_err(backend)?;
            if changed == 1 { Ok(()) } else { Err(QuotaError::InvalidAttempt) }
        }).await
    }

    async fn release_unused(&self, attempt: ReservationId) -> Result<(), QuotaError> {
        self.with_conn(move |conn| {
            let changed = conn.execute(
                "UPDATE quota_attempts SET state='released' WHERE id=?1 AND state IN ('reserved','released')",
                [attempt.0.to_string()],
            ).map_err(backend)?;
            if changed == 1 { Ok(()) } else { Err(QuotaError::InvalidAttempt) }
        }).await
    }

    async fn settle(
        &self,
        attempt: ReservationId,
        usage: AttemptUsage,
    ) -> Result<QuotaSettlement, QuotaError> {
        self.with_conn(move |conn| {
            let tx = begin(conn)?;
            let result = settle_attempt(&tx, &attempt.0.to_string(), usage)?;
            tx.commit().map_err(backend)?;
            Ok(result)
        })
        .await
    }

    async fn reconcile_startup(&self, host: &NodeId) -> Result<(), QuotaError> {
        let host = host.to_string();
        self.with_conn(move |conn| {
            let tx = begin(conn)?;
            tx.execute(
                "UPDATE quota_attempts SET state='released' WHERE host_id=?1 AND state='reserved'",
                [&host],
            )
            .map_err(backend)?;
            let dispatched = {
                let mut stmt = tx
                    .prepare(
                        "SELECT id FROM quota_attempts WHERE host_id=?1 AND state='dispatched'",
                    )
                    .map_err(backend)?;
                let rows = stmt
                    .query_map([host], |row| row.get::<_, String>(0))
                    .map_err(backend)?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(backend)?
            };
            for attempt in dispatched {
                settle_attempt(&tx, &attempt, AttemptUsage::Unknown)?;
            }
            tx.commit().map_err(backend)?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests;
