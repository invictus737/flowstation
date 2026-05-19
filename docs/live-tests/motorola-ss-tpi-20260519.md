# Motorola SS-TPI display tests - 2026-05-19

## Baseline

- Host: `tetraHS` (`chris@192.168.1.179`)
- Service: `tetra-bluestation`
- Baseline binary: `/home/chris/flowstation-bs_0.1.5-mod-stability-82efc54`
- Baseline binary backup: `/home/chris/flowstation-bs_0.1.5-mod-stability-82efc54.bak-ss-tpi-20260519-124839`
- Baseline commit: `82efc54c6f41bd944a6ce844fa19512e05b819e0`
- Git backup branch: `backup/ss-tpi-before-motorola-tests-20260519-124839`
- Git backup tag: `ss-tpi-before-motorola-tests-20260519-124839`
- Git bundle: `/private/tmp/flowstation-v014-facchfix-backups/ss-tpi-before-motorola-tests-20260519-124839.bundle`

## Runtime controls

The Motorola display experiments are disabled by default.

- `FLOWSTATION_MOTOROLA_TPI_GSSI=91`
- `FLOWSTATION_MOTOROLA_TPI_TEST=tx-granted`
- `FLOWSTATION_MOTOROLA_TPI_TEST=setup-mnemonic-only`
- `FLOWSTATION_MOTOROLA_TPI_TEST=setup-pulse`

Only network-originated group calls matching the configured GSSI are affected.

## Identity source

Brew v1 `GROUP_TX` mnemonic fields are decoded and cached as network identity
records before CMCE builds SS-TPI for the RX call setup. This avoids depending
on a Motorola codeplug agenda and avoids waiting for the asynchronous RadioID
lookup on the first call from a new source SSI. RadioID remains a fallback for
numeric IDs that arrive without a Brew mnemonic.

The dashboard Brew protocol status is also refreshed when v1 is detected from
message length after the WebSocket connection is already marked online.

## Test A: `tx-granted`

Keep standard `D-SETUP` caller SSI and SS-TPI mnemonic. Immediately after call setup, send a RX-side `D-TX GRANTED` on the traffic channel with:

- `transmission_grant = GrantedToOtherUser`
- transmitting party SSI = real numeric source SSI
- SS-TPI INFORM mnemonic name

Goal: see whether Motorola updates the RX current-speaker display from `D-TX GRANTED` instead of `D-SETUP`.

## Test B: `setup-mnemonic-only`

For TG 91 only, suppress `D-SETUP.calling_party_address_ssi` and keep only SS-TPI mnemonic in the Facility IE.

Goal: see whether Motorola displays the mnemonic when the numeric caller field is absent.

## Test C: `setup-pulse`

Combine B and A:

- initial `D-SETUP` has mnemonic-only SS-TPI and no numeric caller SSI;
- immediate `D-TX GRANTED` restores the real numeric transmitting-party SSI while also carrying SS-TPI mnemonic.

Goal: try to make the call screen latch the mnemonic, then restore a valid numeric current-speaker identity during the call.

## Rollback

Disable the experiment:

```sh
sudo systemctl unset-environment FLOWSTATION_MOTOROLA_TPI_TEST FLOWSTATION_MOTOROLA_TPI_GSSI
sudo systemctl restart tetra-bluestation
```

Restore baseline binary:

```sh
ln -sfn /home/chris/flowstation-bs_0.1.5-mod-stability-82efc54 /home/chris/bluestation-bs-rpi4
sudo systemctl restart tetra-bluestation
```
