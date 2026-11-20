CREATE TABLE quota_scopes (
    id INTEGER PRIMARY KEY,
    host_id TEXT NOT NULL,
    peer_id TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    model_id TEXT NOT NULL,
    UNIQUE (host_id, peer_id, provider_id, model_id)
) STRICT;

CREATE TABLE quota_periods (
    id INTEGER PRIMARY KEY,
    scope_id INTEGER NOT NULL REFERENCES quota_scopes(id),
    cap INTEGER CHECK (cap > 0),
    cadence TEXT NOT NULL CHECK (cadence IN ('manual', 'daily', 'weekly', 'monthly')),
    started_at INTEGER NOT NULL,
    next_reset_at INTEGER,
    current INTEGER NOT NULL CHECK (current IN (0, 1))
) STRICT;
CREATE UNIQUE INDEX quota_current_period ON quota_periods(scope_id) WHERE current = 1;

CREATE TABLE quota_attempts (
    id TEXT PRIMARY KEY NOT NULL,
    host_id TEXT NOT NULL,
    peer_id TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    model_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    grant_revision TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('reserved', 'dispatched', 'released', 'settled')),
    UNIQUE (host_id, peer_id, request_id)
) STRICT;
CREATE INDEX quota_open_attempts ON quota_attempts(host_id, peer_id, provider_id, model_id)
    WHERE state IN ('reserved', 'dispatched');

CREATE TABLE quota_reservations (
    attempt_id TEXT NOT NULL REFERENCES quota_attempts(id),
    period_id INTEGER NOT NULL REFERENCES quota_periods(id),
    reserved INTEGER CHECK (reserved > 0),
    known INTEGER NOT NULL DEFAULT 0 CHECK (known >= 0),
    estimated INTEGER NOT NULL DEFAULT 0 CHECK (estimated >= 0),
    unknown INTEGER NOT NULL DEFAULT 0 CHECK (unknown IN (0, 1)),
    PRIMARY KEY (attempt_id, period_id)
) STRICT;
CREATE INDEX quota_reservations_period ON quota_reservations(period_id);

CREATE TABLE quota_binding_faults (
    host_id TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    model_id TEXT NOT NULL,
    PRIMARY KEY (host_id, provider_id, model_id)
) STRICT;
