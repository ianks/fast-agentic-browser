# Security policy

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub's private vulnerability
reporting: open the repository's **Security** tab and choose **Report a
vulnerability**
(<https://github.com/ianks/fast-agentic-browser/security/advisories/new>).
Please don't open a public issue for a security problem.

## How fab handles secrets

fab lets agents sign in and fill forms with values from your password manager
(1Password, Bitwarden, the macOS Keychain, or a `fab-secret-<name>` helper)
without any model seeing them. Agents write `{{plain words}}` where a value
goes; the value is fetched only when fab types it into a field.

- A login is typed only on the sites it is saved for.
- Any other item (a card, an identity, a key) needs your approval once per
  site, asked outside the model (a system dialog, or `fab secrets allow`).
- Passwords, codes, card numbers and keys stay masked (`••••`) in page
  listings and are scrubbed from everything fab returns.
- A `{{new password}}` is generated and saved before it's typed.

The details are in the README ("Secrets never reach your agent") and in the
docs of `crates/fab-core/src/secrets/mod.rs`.
