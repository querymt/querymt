-- V2 attempts have no prompt estimate; their missing usage still charges the reservation.
ALTER TABLE quota_attempts ADD COLUMN prompt_tokens INTEGER CHECK (prompt_tokens >= 0);
DROP TABLE quota_binding_faults;
