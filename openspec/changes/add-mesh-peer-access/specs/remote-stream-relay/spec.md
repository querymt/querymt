## MODIFIED Requirements

### Requirement: Unsequenced and mixed-version relays keep current behaviour

For permitted streams, relay messages without a sequence number SHALL be delivered immediately. Differences in sequencing support SHALL NOT prevent peers on supported protocols from decoding each other's messages. An upgraded host SHALL enforce its grant on legacy streams, and an upgraded client SHALL accept legacy relay data only from the authenticated host for the active request. Relay compatibility SHALL NOT grant resource access.

#### Scenario: Old host, new client
- **WHEN** a confirmed legacy host that does not sequence messages replies to a stream request initiated by an upgraded client
- **THEN** the client delivers messages from that host for the active stream as received and the stream completes

#### Scenario: New host, old client
- **WHEN** an upgraded host sequences a supported stream for a legacy client without ordered delivery whose peer has Full access
- **THEN** the legacy client decodes every message and ignores the sequence number

#### Scenario: Heartbeat during a held gap
- **WHEN** a heartbeat arrives on an authorized stream while sequenced messages are held for a gap
- **THEN** the heartbeat is handled immediately and does not release or reorder held messages
