---
name: tau-self-knowledge-email
description: Use this skill when the user asks how to configure Tau's standard email extension, std-email/tau-ext-email, mail accounts, IMAP/SMTP, email approvals, incoming authentication, DKIM, Authentication-Results, or email security policy.
advertise: false
---

# Tau std-email configuration

Tau's built-in email extension is named `std-email`. It runs `tau ext ext-email`, registers the model-visible `email` tool, and publishes `/email` approval actions.

Use this skill when helping a user configure email. Do not include personal addresses, server names, passwords, authserv-ids, or message contents unless the user explicitly provided them for that answer.


## Secure baseline

Start from fail-closed settings:

- Keep `extensions.std-email.enable: true` only when the user really wants mail access.
- Set the extension's internal `config.enable: true`; it is false by default.
- Set each account's `enable: true`; accounts are disabled by default.
- Keep `policy.incoming_auth.require: true`; this is the default and should normally stay true.
- Keep `policy.incoming_auth.allow_dmarc_only: false`; the default requires aligned DKIM.
- Configure exact `policy.incoming_auth.trusted_authserv_ids` for the user's trusted mailbox provider or final MTA.
- Keep `incoming_allow`, `outgoing_allow`, and `folders.allow` narrow.
- Set `policy.allow_state_policy_extensions: false` if the user wants config-only allowlists and no persistent whitelist changes from `/email ... whitelist` actions.

Never recommend disabling incoming auth just to make a message pass. Prefer manual approval for edge cases.


## Example harness config

Put this in `~/.config/tau/harness.yaml` or a file under `~/.config/tau/harness.d/`, then replace all example values with the user's provider details.

```yaml
extensions:
  std-email:
    enable: true
    secrets:
      mail_password: {}
    config:
      enable: true
      accounts:
        - id: work
          enable: true
          display_name: Work mail
          from: Alice Example <alice@example.com>
          imap:
            host: imap.example.com
            port: 993
            tls: required
            login: alice@example.com
          smtp:
            host: smtp.example.com
            port: 587
            tls: start_tls
            login: alice@example.com
          auth:
            method: password
            password_secret: mail_password
          folders:
            allow:
              - INBOX
      policy:
        incoming_allow:
          - alice@example.com
          - '*@trusted.example'
        incoming_auth:
          require: true
          trusted_authserv_ids:
            - mx.example.com
          allow_dmarc_only: false
        outgoing_allow:
          - alice@example.com
          - '*@trusted.example'
        allow_state_policy_extensions: true
```

Important fields:

- Built-in extension name: `std-email`.
- Model-visible tool name: `email`.
- IMAP default: port 993 with `tls: required`.
- SMTP default: port 587 with `tls: start_tls`.
- Password auth requires `auth.password_secret` and a matching declaration under `extensions.std-email.secrets`.
- `auth.method: none` is only for SMTP-only or relay-style setups; IMAP requires password auth.
- OAuth and command-based password sources are not implemented or are rejected.


## Secrets

Declare the secret name in config:

```yaml
extensions:
  std-email:
    secrets:
      mail_password: {}
```

Then set the value either as a state secret file or as a one-shot environment variable. The secret file is read as trimmed UTF-8 text even though the suffix is `.yaml`.

```sh
mkdir -p ~/.local/state/tau/secrets
printf '%s\n' 'app-password-or-token' > ~/.local/state/tau/secrets/mail_password.yaml
chmod 600 ~/.local/state/tau/secrets/mail_password.yaml
```

Or for one startup:

```sh
TAU_SECRET_MAIL_PASSWORD='app-password-or-token' tau
```

Tau normalizes the environment suffix to `mail_password`. Do not put passwords directly in `harness.yaml`.


## Incoming authentication and DKIM

The visible `From` header is spoofable. The extension only treats an incoming allowlist match as valid when the message also has trusted authentication evidence.

By default, auto-read requires:

1. `From` matches `policy.incoming_allow` or a persisted incoming whitelist pattern.
2. The newest parsed `Authentication-Results` header has an authserv-id listed in `policy.incoming_auth.trusted_authserv_ids`.
3. That trusted header reports `dkim=pass` with `header.d` aligned to the visible `From` domain.

The extension does not cryptographically verify DKIM itself. It trusts `Authentication-Results` added by the user's mailbox provider or final trusted MTA. Tell users to inspect raw headers and copy the exact authserv-id token from a header their provider adds, for example the `mx.example.com` part of:

```text
Authentication-Results: mx.example.com; dkim=pass header.d=trusted.example; dmarc=pass header.from=trusted.example
```

Do not configure the sender domain as `trusted_authserv_ids` unless that is actually the authserv-id emitted by the trusted server.

If authentication fails, the extension returns approval reasons such as `auth missing`, `untrusted auth server`, `auth failed`, `auth unaligned`, `dkim missing`, or `auth truncated`. Those are expected fail-closed outcomes; approve individual messages manually instead of weakening global policy.


## Allowlists and patterns

Incoming and outgoing allowlists accept:

- exact addresses: `alice@example.com`
- globs: `*@example.com`
- regexes with a `re:` prefix, matched against the whole normalized address: `re:.*@trusted\.example`

Advice:

- Prefer exact addresses or narrow domains.
- Avoid broad patterns for domains where many people can send mail.
- Remember incoming allow only controls read exposure; outgoing allow controls delivery without approval.
- `reply_to` is also checked for outgoing sends.
- Bcc is checked for policy and visible in `/email out open`, but hidden from model-facing send status.


## Approval workflow

Incoming reads:

- `email.list` shows bounded metadata and redacts untrusted message details.
- `email.read` returns body content only if policy passes or an exact incoming approval exists.
- If approval is needed, use `/email in list`, `/email in open <id>`, and `/email in approve <id>`.
- After approval, the agent must repeat the matching `email.read` call.

Outgoing sends:

- `email.send` sends immediately only when every `to`, `cc`, `bcc`, and `reply_to` address is allowed.
- Otherwise it queues the full draft and returns `approval_required`.
- Use `/email out list`, `/email out open <id>`, and `/email out approve <id>` to review and send.
- If `email.send` returns `approval_required`, the agent should not call `send` again for the same draft.

Whitelist actions:

- `/email in whitelist <pattern>` persists an incoming pattern.
- `/email out whitelist <pattern>` persists an outgoing pattern.
- These actions only affect policy when `policy.allow_state_policy_extensions` is true.


## Troubleshooting

If the extension is unavailable, check both enable flags: `extensions.std-email.enable` and `extensions.std-email.config.enable`.

If startup reports a missing secret, confirm the secret is declared under `extensions.std-email.secrets` and that the secret file or `TAU_SECRET_*` variable uses the same normalized name.

If all incoming reads require approval, inspect raw message headers and configure `trusted_authserv_ids` for the trusted provider's authserv-id. With `incoming_auth.require: true` and no trusted authserv-id, fail-closed approval is expected.

If a legitimate sender fails with `dkim missing` or `auth unaligned`, do not immediately disable auth. First check whether the provider rewrites mail, a list forwarder changed authentication alignment, or the allowlist is too broad. Manual approval is safer than global weakening.

For logs, use:

```sh
TAU_EXT_LOG=email=debug tau
```
