# Licensing (track G8)

`avada_core::license` decides whether a **commercial** module may run on this
machine. It verifies an EdDSA-signed licence JWT offline against keys cached on
disk, checks in with the issuer at most once per interval, and answers a single
question for the host: `Gate::{Run, RunWithBanner(reason), Refuse(reason)}`.

It does **not** spawn or refuse anything itself. The module host is expected to
call `LicenseService::gate_for_major` before spawning a module whose manifest says
`distribution.commercial = true`, and the app is expected to show the banner from
`RunWithBanner`. That wiring lives in `module/host.rs` and is **not** part of this
track (see "Follow-ups").

## The shape of a licence

A licence is a JWT signed `EdDSA` (Ed25519). Its claims are the SDK's
`LicenseClaims` (frozen: `product`, `licensee`, `seats`, `nbf`, `exp`,
`max_major`, `checkin_interval_days`, `jti`), plus the `iss` the host reads
*before* verifying so it knows whose keys to fetch. `exp: 0` is perpetual.

The token is a **bearer credential**: it is its own credential at the issuer's
introspection endpoint. It is stored owner-only, never logged, never put in an
error string, and never returned by a control route. `LicenseToken` has no
`Debug`/`Display` that shows it — only `expose_secret()`, which is spelled that
way on purpose.

## States and the gate

`avada_module_sdk::license::evaluate(claims, checkin, now)` is the whole state
machine and is frozen:

| State | When | Gate |
|---|---|---|
| `Valid` | inside `nbf..exp`, checked in within `checkin_interval_days` | `Run` |
| `GracePeriod` | check-in overdue, but by less than `GRACE_DAYS` | `RunWithBanner` |
| `StaleCheckin` | overdue by more than `GRACE_DAYS` | `Refuse` |
| `NotYet` | `now < nbf` | `Refuse` |
| `Expired` | `exp != 0 && now >= exp` | `Refuse` |
| `Revoked` | the issuer said so at a check-in | `Refuse` |

`GRACE_DAYS` is **14**, from the SDK constant, not the 7 in the plan prose. The
plan's number is not encoded anywhere; the SDK's is, and the SDK is frozen, so
the SDK wins. `checkin_interval_days` is clamped by the SDK's `checkin_days()`
(at most `MAX_CHECKIN_DAYS`).

A token that no longer verifies at all (wrong issuer, unknown `kid`, bad
signature, wrong product) is not a state — it is a refusal, reported as
`state: "invalid"` in a summary and `Gate::Refuse` at the gate.

## Verification is offline

`license/verify.rs` verifies against a `Jwks` (RFC 7517 `kty: OKP`, `crv:
Ed25519`, `x`, looked up by `kid`), with `DEFAULT_SKEW_SECS = 300` of clock skew
on `nbf`/`exp`. The keys are cached in the store next to the licences, so every
launch after the first verifies with no network at all. Keys are refetched only
when a `kid` is unknown (`LicenseService::refresh_keys`).

The skew applies to *verification*, not to the state machine: a licence whose
`nbf` is 100 s in the future installs (inside the skew) and still reports
`NotYet` until it actually starts.

## Where things live

```text
<modules root>/licenses/                 0700
  keys/<issuer-slug>.json                the issuer's JWKS   (0600)
  <owner>__<repo>/license.jwt            the token           (0600; a bearer credential)
  <owner>__<repo>/meta.json              { "product", "issuer" }
  <owner>__<repo>/checkin.json           CheckinRecord
```

`FileLicenseStore::host()` puts this under the same modules root the install
store uses (`install::InstallPaths`); `FileLicenseStore::under(dir)` points it anywhere (tests). Every
write is a temp-file-plus-rename with mode 0600 in a 0700 directory
(`store::write_private`), so a half-written licence is never observable and no
other user on the machine can read the credential. `MemoryLicenseStore` is the
same trait with nothing on disk, for tests.

## Check-in (RFC 7662 + RFC 9701)

At most once per `checkin_days()`, and on demand with `force`:

1. Discover `/.well-known/oauth-authorization-server` at the issuer (RFC 8414);
   `/.well-known/openid-configuration` is accepted as a fallback.
2. `POST /introspect` with the licence as both the `token` parameter **and** the
   bearer credential — the licence authenticates itself.
3. The answer is an RFC 9701 signed introspection response: a JWT with
   `typ: token-introspection+jwt` whose `token_introspection` claim carries
   `{active, reason?}`. It is verified against the issuer's JWKS like the licence
   is, so a proxy cannot forge an `active: false`.
4. `CheckinRecord` is updated: `last_ok`, `revoked`, `reason`.

**A network failure leaves the record untouched.** `checkin` answers
`CheckinOutcome::Unreachable(msg)` — an `Ok`, not an `Err` — so an offline laptop
drifts into `GracePeriod` and then `StaleCheckin` on the clock, which is exactly
what the grace window is for. An issuer may also hand back a *renewed* token in
the introspection answer; it is verified and stored like any other install.

## Three ways to install

| Path | Call | Route |
|---|---|---|
| Manual | `install_file(path)` | `POST /license/install {path}` |
| Corporate | `install_from_url(url)` | `POST /license/install {url}` |
| Store | `device_flow_start` + `device_flow_poll` | `POST /license/device`, `GET /license/device/{code}` |

The device flow is RFC 8628: the host asks the issuer for a device code, shows
the user a `user_code` and a `verification_uri`, and polls the token endpoint
until it answers with the licence (`authorization_pending`, `slow_down`,
`expired_token`, `access_denied` are all forwarded as their own status). The
issuer's `device_code` never leaves the service — the caller polls with the
host's own opaque handle.

Every install path ends the same way: verify against the issuer's JWKS, refuse a
token whose `iss` is not the issuer the product is bound to
(`register_issuer`, from the manifest's `distribution.issuer`), store it, and do
one forced check-in so the record starts fresh.

## Routes

All six `/license/...` routes answer **503 `licensing unavailable`** until the app
calls `Shared::install_license(Arc<LicenseService>)` (`control/server.rs`, same
pattern as `install_marketplace`). They are listed in
`descriptor_table::core_routes` under the `// ---- track G8 license` fence and
mounted from `routes.rs::handlers`; a route in one and not the other is a startup
panic. Reads take `settings.read`, everything else `settings.write` — a licence is
machine-wide configuration and the contract has no `license.*` capability.
`license.device.poll` is a GET but takes `settings.write`, because the poll is
what mints and stores the credential.

| Method | Route | Answer |
|---|---|---|
| `license.list` | `GET /license` | `{licenses: [LicenseSummary]}` |
| `license.show` | `GET /license/modules/{owner}/{repo}` | `{license: LicenseSummary, gate: Gate}` |
| `license.install` | `POST /license/install` `{path}` **or** `{url}` | `{license: LicenseSummary}` |
| `license.device` | `POST /license/device` `{issuer, module}` | `{device: DeviceCode}` |
| `license.device.poll` | `GET /license/device/{code}` | `{status: pending\|slow_down\|expired\|denied\|installed, license?}` |
| `license.remove` | `DELETE /license/modules/{owner}/{repo}` | `{removed: "owner/repo"}` |

The product is `owner/repo`, so it is two path segments — `/license/modules/...`
mirrors `/marketplace/modules/...` and keeps `/license/device/{code}` unambiguous.

`LicenseSummary` is what the UI shows: product, issuer, licensee display name,
seats, dates, `max_major`, the `CheckinRecord`, the state name and a sentence
saying why. It never contains the token. Statuses come from
`LicenseError::http_status`: 400 no issuer known / bad body, 404 not licensed or
no such device flow, 409 issuer mismatch, 422 the token does not verify, 500
store, 502 the issuer.

## The stub issuer

`license::stub_issuer::StubIssuer` is a real, `pub` (not `cfg(test)`) issuer with
an in-memory ledger and a per-instance signing key. It implements every route
§9 of the fan-out plan sketches — discovery, JWKS, introspection, device
authorization, token, and a small admin surface — and it does so **twice over the
same handler functions**:

* `impl LicenseHttp for StubIssuer` dispatches by URL path in-process, so tests
  run the whole loop (issue → install → run → revoke → introspect → grace →
  refuse) with no socket and no wall clock.
* `StubIssuer::router()` mounts the same `handle_get`/`handle_post` behind axum,
  so dev tooling can serve it on a port.

The two cannot drift, because there is only one implementation. Wave 4's real
licence server only has to change a URL.

Admin calls (`/admin/issue`, `/admin/revoke`) need a per-instance bearer compared
in constant time. Test keys are generated in memory per test and never written
anywhere except, for the manual-install path, a scratch file the test removes.

## Testing

`license/tests.rs` drives the service end-to-end against the stub issuer with an
injectable clock (`Clock = Arc<dyn Fn() -> u64>`), so the grace window and the
check-in interval are tested by moving a dial rather than by sleeping:
install → verify → summarise, drift into a banner and then a refusal, check in
inside the window and recover, revoke and be refused, lose the network and be
left alone, wrong issuer, stranger's signature, `nbf` inside the skew, expiry,
`max_major`, stale key cache, renewed token, download link, the device flow in
all four of its endings, `checkin_all`, removal, and restart-from-disk.

`control/routes.rs`'s `license_routes` module runs the six routes over the real
axum stack: the 401/403/503 ladder for each, install-from-file through to a
closed gate after a revoke, the body rules for `path`/`url`, and the device flow
through the routes.

## Follow-ups (outside G8)

* **Host wiring.** `module/host.rs` must call `gate_for_major(product, major)`
  before spawning a module whose manifest is `distribution.commercial`, refuse on
  `Gate::Refuse`, and surface `RunWithBanner`'s reason. `module/` is not this
  track's to edit.
* **App UI.** Showing the banner, the licence list and the device-flow dialog is
  a later track.
* **`checkin_all` on a schedule.** Nothing calls it yet; the app should, once per
  launch and then daily.
