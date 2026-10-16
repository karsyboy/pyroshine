# Pairing, identity and revocation

Pair using the [web interface](MANAGEMENT.md#pair-a-client) or
[`pyroshine pair`](INSTALLATION.md#pair-from-a-terminal). Both approve through
the server's management operations, using the exact pending request token and
the PIN shown by the client. The CLI waits for that request's completion;
accepted PIN submission alone is not proof of trust.

Approve only pairings you initiated. Check the requester address and, when you
can compare it, the certificate fingerprint. A remote client cannot approve
itself. Keep streaming on a trusted LAN/VPN.

The GameStream listeners offer no approval route: the former host-local
`/pin` page answers 410, because any local user or process could use it.

## Web management

The [web interface](MANAGEMENT.md) has its own listener (default
`127.0.0.1:47990`), separate from the GameStream ports, and its own
administrator authentication. Nothing else grants access: not a loopback
address (other local users and the streamed applications share it), not a
paired Moonlight certificate, and not the desktop tray, which only opens pages.

| Protection | Behavior |
| --- | --- |
| First password | Set with a one-time setup code (`pyroshine web setup`, valid 15 minutes, 5 wrong entries withdraw it). Only the streaming user can request one, over the same-user D-Bus interface. `--reset` replaces a forgotten password and signs out every browser. |
| Password storage | PBKDF2-HMAC-SHA256 with a random salt in `~/.local/share/moonshine/management.json` (mode 0600). The password is never logged. |
| Sessions | A new random 256-bit token at every sign-in, in an `HttpOnly`, `SameSite=Strict` cookie (`Secure` and `__Host-` over HTTPS). Only its SHA-256 is stored. Expiry after 7 idle days or 30 days; sign-out, password change and **Maintenance → Sign out** end sessions at once, including open event streams. |
| Guessing | 5 failed passwords or codes from an address lock it out for 30 s, doubling up to 15 minutes; a global budget slows distributed guessing. |
| Cross-site requests | Changes need a JSON body, a matching `Origin`/`Sec-Fetch-Site` and the session's CSRF token (`X-CSRF-Token`). No CORS is offered. |
| DNS rebinding | `Host` must be an IP address, `localhost`, this machine's name (on non-loopback listeners) or an `allowed_hosts` entry. |
| Remote access | Any non-loopback `address` requires `tls = true`; startup refuses otherwise. |
| Page content | Only the installed files are served (read once at startup; request paths are never file paths), with a strict Content-Security-Policy, `frame-ancestors 'none'` and `nosniff`. |
| Limits | 128 connections, 16 event streams, 1 MiB bodies (16 KiB except configuration saves), header and body deadlines. |
| Audit | Sign-ins, failures, lockouts, password changes and every change (pairing, revocation, configuration, session end, support bundle) are logged under the `pyroshine::audit` target with the session's public identifier and peer address. |

If `management.json` is damaged, sign-in is refused (the file is left as it
is) until `pyroshine web setup --reset` replaces it. Deleting the file has the
same effect as a reset: the next browser needs a setup code.

Support bundles are downloads of the same redacted archive `pyroshine diag`
writes; they cannot include other files.

## Local management interface

The tray and the `pyroshine pair`/`web`/`diag` commands use the service's
management interface on the D-Bus session bus of the user running Pyroshine.
Only that user can reach the bus, and the service also rejects calls from any
other Unix user. It is available to every process of that user (including
streamed applications): approving a request still requires the PIN shown by the
client, and the interface never exposes session keys, pairing secrets or private
keys. It is not reachable over the network.

## Pending transactions

The server admits at most 32 pending pairings. Client IDs are limited to 256
bytes and certificates to 16 KiB. Approval has five minutes; the complete
transaction expires one minute later, including requests abandoned after PIN
submission. A single sweep task removes expired transactions within another
second. Expiry, replacement and cancellation wake the approval waiter. Disconnect
before approval removes its request, and completed or failed approved protocol steps remove
their transaction. Approved requests abandoned between protocol steps expire.
Older approval waiters cannot remove replacement requests. Challenges and PINs
are accepted once per transaction.

While the tray runs it notifies the operator about pairing requests;
repeated requests replace one notification. Without the tray, the server's own
desktop notifications are coalesced to at most one per 30 seconds, with at most
one notification worker, and do not wait for a click. They and the service log
name where to approve; no link approves anything by itself. Global shutdown
clears pending requests. New approval is required after restart.

## Durable trust and migration

The existing data-directory `moonshine/state.toml` remains the source of trust.
`unique_id`, legacy `clients`, and `paired_certs` survive migration. New approved
pairings also store `client_certs`, a client-ID-to-fingerprint-set association.
An ID may have several certificates; one certificate may have several IDs.
The server does not guess associations between legacy sets. Legacy certificates
continue working; a newly approved pairing records its explicit association.
A legacy client ID alone does not prove ownership of any certificate.

Pair completion and revocation each replace the entire state file in one
serialized transaction. A private staging file is fully written and synced,
renamed over the old state, and the containing directory is synced before
publishing trust in memory or reporting success. An interrupted update leaves a
complete old or complete new file. The production service holds an exclusive
`state.lock` writer lock; do not run multiple servers against one state directory.

If persistence fails, authorization is disabled until restart. Repair disk space,
ownership, permissions or filesystem errors first. If a directory sync failed
after rename, the replacement may already be present; inspect the complete state
before retrying. Keep a protected backup of state and the matching TLS identity.
Stop the service before restoring a known-good backup. Do not delete state to
repair a parse error: that discards the server UUID and unrelated trust. A damaged
legacy file needs restoration from backup; migration cannot reconstruct missing
records. Restoring an old backup can restore revoked trust: reapply revocations
before exposing the server.

`state.toml` may also hold `client_metadata`: an operator name and pairing time
per certificate fingerprint. It is display-only and never consulted for trust;
versions without it ignore the table. Revocation removes a certificate's entry.

## Revoke a client

In the web interface, select **Revoke** on the client in **Clients**; it revokes
that certificate. Without it, the host operator can use **POST** to the
loopback HTTP `/unpair` route, with peer, Host and browser-origin checks. It can
only remove trust, never grant it:

```sh
curl --fail -X POST 'http://localhost:47989/unpair?uniqueid=CLIENT_ID'
# For legacy state, or to remove one specific certificate:
curl --fail -X POST 'http://localhost:47989/unpair?fingerprint=SHA256_HEX'
```

URL-encode client IDs. Supply one target selector; a fingerprint takes precedence
if both are supplied. ID revocation removes all explicitly associated
certificates. Fingerprint revocation removes that certificate and its known ID
relationships. IDs still associated with other certificates remain paired.
Legacy unassociated IDs remain in the compatibility set when a fingerprint is
revoked, but cannot authorize HTTPS. ID-only revocation of an unassociated legacy
client fails with a request to use its fingerprint. Read `paired_certs` in the
protected state file to find legacy fingerprints; do not infer ownership from
set ordering. To revoke a known credential even when its legacy ID is ambiguous,
use its fingerprint. An unknown/already revoked target reports failure.

A paired client can also send **GET /unpair over HTTPS** for self-revocation.
The TLS certificate selects the credential: caller-supplied IDs/fingerprints
cannot revoke another client. Unauthenticated remote HTTP deletion is rejected,
and plain-HTTP GET `/unpair` returns 404. Older clients that issue an
unauthenticated GET during pairing cleanup must retry pairing normally.

Revocation clears pending approvals, drains in-flight launch/resume/cancel
operations, and stops the active stream before returning success. Sessions do
not yet record the TLS credential owner, so **any revocation stops any active
stream**, including another client's. Other paired certificates remain valid
and may launch again. If teardown fails, trust remains revoked but the operation
reports failure. Existing TLS connections recheck trust on each protected
request. Revoked credentials cannot launch, resume or call other protected HTTPS
routes after restart without another operator-approved pairing.

## TLS identity permissions and recovery

Generated keys, recovery journals and staging files use mode **0600**, including
under umask 000 or 022. Packaged systemd services also use `UMask=0077`. Existing
valid certificate/key files are loaded without replacement or chmod, including
custom administrator-managed paths. Broad existing key permissions produce an
operator warning; have the key's administrator set appropriate ownership and
mode 0600, accounting for any external certificate renewal workflow.

Initial creation uses an exclusive kernel lock at `PRIVATE_KEY.creation.lock`
and a private, synced `PRIVATE_KEY.creation.toml` recovery journal containing
the complete intended certificate/key pair. Publication never overwrites an
existing file. After interruption, startup completes only missing files from
the journal, requiring any existing file to match, then removes the journal.
Never publish or print this journal: it contains the private key. The lock file
is retained so concurrent creators always lock the same inode.

A partial externally provided identity without a journal, an incomplete pair
conflicting with its journal, or an invalid existing pair causes startup failure. Restore its matching file
from backup or complete the external deployment; the server will not silently
rotate a usable deployed identity. Provisioning does not follow dangling target
symlinks or recovery/lock symlinks. Usable administrator-managed certificate/key
symlinks remain supported. Interrupted temporary files are private and are not
authoritative; with the service stopped they can be removed after identifying
them. Keep identity directories owned by the service user and inaccessible to
untrusted writers.
