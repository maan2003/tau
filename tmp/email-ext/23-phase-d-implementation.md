# Phase D implementation: email approval UI actions

## Files changed

- `crates/tau-ext-email/src/lib.rs`
  - Publishes `/email` action schema at startup with incoming/outgoing list/open/approve/whitelist leaves.
  - Subscribes to and handles `ActionInvoke` events, returning `ActionResult::Text` or `ActionError`.
  - Added persisted approval queue listing/loading and allowlist append helpers.
  - Added actions against Phase B state for pending approvals, approvals, and incoming/outgoing whitelist updates.
- `crates/tau-ext-email/src/tests.rs`
  - Added schema publication tests and action tests for list/open/approve/whitelist behavior, invalid actions/IDs, policy effects, and incoming content leak prevention.

## Design choices

- `open` returns plain text for the MVP. No editor mutation is implemented.
- Outgoing `list` hides Bcc and also filters Bcc recipients out of the visible blocked-recipient summary; `open` shows the full draft including Bcc.
- Incoming `open` reads only persisted approval metadata and does not fetch backend message content, so unapproved subject/body cannot leak.
- `approve` moves the pending approval to approved state. The user-facing result tells the user to repeat the matching `email.send` or `email.read`, which then follows the Phase B fake/backend path.
- Whitelist actions persist exact/glob/regex state policy records using the existing state allowlist files. `re:` is accepted and stored as regex state kind.

## Validation run

- `treefmt --fail-on-change` ✅
- `cargo fmt --check` ✅
- `cargo test -p tau-ext-email` ✅
- `cargo test -p tau-harness action::` ✅
- `cargo clippy -p tau-ext-email -- -D warnings` ✅
- `cargo check -p tau` ✅

## Failures encountered

- Initial outgoing action test found `/email out list` leaked a Bcc-only blocked recipient through the blocked-recipient summary. Fixed by filtering Bcc recipients from list output while preserving them in `open`.

## Remaining risks

- Approval/whitelist action outputs are intentionally simple plain text and may need richer UI rendering later.
- Whitelist append uses the existing Phase B timestamp placeholder rather than a real clock source.
- No real IMAP/SMTP behavior is added; this remains fake/backend-state only as intended for Phase D.
