# TETRA Review Checklist

Use this checklist before committing protocol or BS stability changes.

## Standards Gate

- Name the affected ETSI part and clause when known.
- Identify the exact PDU, field, SAP primitive, and direction.
- Mark any non-standard behavior as a compatibility deviation with a reason and a rollback path.
- Keep implementation comments short; put longer rationale in tests or commit messages.

## UMAC / Random Access

- `random_access_flag` is set only for a real random-access acknowledgement.
- Downlink ISSI addressing alone does not imply random access acknowledgement.
- RA ACK and grant integration preserve the target SSI and timeslot.
- Energy-saving subscribers still receive RA responses immediately.
- Fragmentation does not add Null PDU bits inside STCH/FACCH payloads.

## MM / Attach

- Location update finishes with `D-LOCATION-UPDATE-ACCEPT` or `REJECT`.
- `D-LOCATION-UPDATE-COMMAND` is not sent as an automatic post-accept cleanup.
- Group identity attach/detach follows the demand/accept fields, not local assumptions.
- Energy-saving mode is advertised only when scheduling support matches the behavior.
- Register/affiliate updates are replayed after backhaul reconnect without causing detach churn.

## MLE / Broadcast

- `D-NWRK-BROADCAST` remains periodic and does not starve signalling.
- Cell identity, MCC/MNC, color code, frequency and time zone match config.
- Neighbour information is encoded only when configured and understood by the scheduler.

## CMCE / Supplementary Services

- Call-control messages stay on the correct MCCH or FACCH/STCH path.
- TX grant/interrupt/ceased timing is not coupled to audio frame cadence.
- SS-TPI INFORM generation is centralized and respects CLIR/subscription/name availability.
- Identity/radioid lookups happen at setup or speaker changes, not per frame.

## RF / PHY

- RF calibration is not allowed to leave RX/TX paused or loopback enabled after startup.
- Any RF profile change is validated separately from protocol changes.
- Frequency correction, gains, bandwidth and timing are logged at startup.
- TX guardrails avoid clipping without widening the 25 kHz useful channel.

## Live Acceptance

- Terminal sees service for more than one broadcast interval.
- At least one known ISSI can complete LU demand, accept, LLC ACK, register, and affiliate.
- Logs show no recurring unexpected deregister/deaffiliate/detach after attach.
- A/B comparison against stable Bluestation is recorded, but spec compliance remains the decision gate.
