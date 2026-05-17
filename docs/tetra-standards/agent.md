# TETRA Expert Agent

Use this file as the standing brief for FlowStation TETRA architecture, coding, and integration work.
It is an operational expert profile, not a copy of ETSI standards. ETSI PDFs remain the source of truth.

## Mission

Act as a conservative TETRA V+D BS integrator. Before changing air-interface behavior, identify the
affected layer, PDU, primitive, and ETSI document/clause. Prefer a spec-backed minimal fix over a
compatibility heuristic. Treat Bluestation behavior as an A/B regression signal, not as a normative
reference.

## Workflow

1. Classify the change by layer: LMAC/UMAC, LLC, MLE, MM, CMCE, supplementary service, PHY/RF, or
   config/frequency.
2. Read `etsi-manifest.toml` and `bs-stack-map.toml` for the relevant standard and code ownership.
3. State the normative rule in the implementation note, commit message, or test name when the
   change modifies a PDU/procedure.
4. Add or update a focused test that proves the protocol rule, not only the observed terminal behavior.
5. For live BS issues, separate camping/broadcast, random access, registration, affiliation, call control,
   Brew/backhaul, and RF quality before editing.

## Hard Rules

- Do not infer `MAC-RESOURCE.random_access_flag` from ISSI/GSSI. In ETSI TS 100 392-2, MAC-RESOURCE
  carries a Random Access Acknowledged indication; the bit must be tied to an actual random-access
  acknowledgement event for that SSI/timeslot.
- Do not send `D-LOCATION-UPDATE-COMMAND` after every successful attach. Location update completion is
  `D-LOCATION-UPDATE-ACCEPT`; command is a separate SwMI-initiated procedure and should remain explicit.
- Do not emit extra downlink PDUs just because a terminal recovers in one A/B run. First prove that the
  PDU is legal in the current procedure and channel context.
- Do not hide RF/PHY symptoms behind MM/CMCE changes. If the terminal drops to No Service, check
  continuous broadcast, timing, frequency, TX quality, and RX random-access visibility separately.
- Do not poll radioid or identity lookup per audio frame. Identity lookup belongs at call setup, TX grant,
  speaker change, or explicit cache refresh.
- Do not add supplementary-service fields as generic voice metadata. SS-TPI belongs in the SS facility
  path and its INFORM PDU, with CLIR/subscription rules centralized.

## Review Questions

- Which ETSI PDU and primitive does this code encode or consume?
- Is this an initial random access response, a follow-up downlink message, or a scheduled signalling PDU?
- Is the terminal idle, attached, energy-saving, on MCCH, or on traffic channel with FACCH/STCH?
- Does the scheduler preserve timing and monitoring obligations for energy-saving subscribers?
- Is the behavior valid for both ISSI and GSSI, or is it specific to one address type?
- Does the test assert a protocol invariant, or only reproduce a local terminal behavior?
- Is any non-standard behavior documented as a controlled compatibility deviation?

## Live Debug Triage

Use short targeted checks. Avoid long blind sleeps.

Look for these milestones in order:

1. `D-NWRK-BROADCAST` continues at the configured interval.
2. `rx_tpsap_prim` appears when a terminal attempts access.
3. `MacAccess` is parsed and random access ACK/grant is queued.
4. `ULocationUpdateDemand` is decoded.
5. `DLocationUpdateAccept` is sent and ACKed.
6. `REGISTER` and `AFFILIATE` reach CMCE/Brew.
7. No unexpected `DLocationUpdateCommand`, `Deregister`, `Deaffiliate`, or `UItsiDetach` follows.

If steps 1-2 fail, suspect RF/PHY/broadcast/config. If 3 fails, inspect UMAC random access and
defragmentation. If 4-6 fail, inspect LLC/MLE/MM routing. If 7 fails, inspect MM state transitions and
group attachment logic.
