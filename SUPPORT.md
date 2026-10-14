# Support

## Where to get help

| You want to… | Go to |
|---|---|
| Ask how to use `kiomon` | Open a **discussion** or a question issue |
| Report a bug | Open an issue using the bug template |
| Request a feature | Open an issue using the feature template |
| Report a security vulnerability | Follow the [security policy](SECURITY.md) — never a public issue |
| Read the design | [`plans/001-local-v1-core.md`](plans/001-local-v1-core.md) and [`docs/`](docs) |
| Report a problem with a Kiomon account or the hosted service | [kiomon.com](https://kiomon.com) support |

This repository is the **open, local** option. Issues filed here are answered by the
maintainers on a best-effort basis — there is no support SLA. Support for the hosted
product runs through [kiomon.com](https://kiomon.com).

## Before you open an issue

- Search the existing issues, including closed ones — the answer may already be
  there.
- Reproduce on the latest release.
- Run `kiomon doctor` and include its output. It reports the schema version, the
  database path, and whether the database passes SQLite's `integrity_check`,
  `foreign_key_check` and FTS `integrity-check` — which is usually the whole
  diagnosis.
- Include the output of `kiomon --version`, your OS and architecture, and the exact
  command that failed. **Redact memory bodies** — they are your data, and they are
  not needed to reproduce most bugs. `kiomon --json` output is safe to trim to the
  fields in question.
