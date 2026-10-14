# Security Policy

## Reporting a vulnerability

**Please do not report security vulnerabilities through public issues, pull requests
or discussions.**

Report privately through GitHub's private vulnerability reporting on this
repository: open the **Security** tab and choose **Report a vulnerability**. This
opens a private advisory visible only to you and the maintainers, and lets us credit
you in the published advisory if you would like.

If you cannot use that form, email **security@kiomon.com**.

Please include, as far as you can:

- the version affected (`kiomon --version`);
- what an attacker can do, and what access they need to do it;
- reproduction steps or a proof of concept;
- any suggested fix or mitigation.

## What to expect

| Stage | Target |
|---|---|
| Acknowledgement of your report | within 3 business days |
| Initial assessment and severity | within 7 business days |
| Fix or mitigation for a confirmed high-severity issue | as fast as the fix can be shipped safely |
| Public advisory | published when a fixed version is available, coordinated with you |

We will keep you informed of progress, and we will credit you in the advisory unless
you prefer otherwise. Please give us a reasonable window to release a fix before any
public disclosure.

## Supported versions

This project is pre-1.0 and is released from `main`. Security fixes are issued as a
patch release of the latest published version; older versions are not maintained.

## Scope

In scope: this repository's source, the `kiomon` binary and the `kiomon-core` crate
published from it, and the database and configuration files those write.

Particularly interesting to us, because they are the places a local tool can be
surprising:

- a path where memory bodies, tags, or metadata leave the machine, or are written
  somewhere the user did not choose;
- a workspace-isolation bug: a query that returns rows from a workspace it was not
  scoped to, including through `--workspace '*'` or an absent `--workspace`;
- a path where a `pending_approval` memory becomes retrievable before approval;
- SQL injection through a query, tag, workspace name, or import file;
- memory-unsafety, a panic on untrusted input (a malformed database or JSONL
  import), or resource exhaustion from a hostile file;
- anything that lets a process other than the user read the database, or that
  makes the file world-readable or world-writable.

Out of scope: vulnerabilities in the hosted Kiomon service — report those to
**security@kiomon.com** instead — and issues that require a compromised developer
machine, a malicious dependency you install yourself, or a browser or runtime that
is already unsupported.

## Safe harbour

We will not pursue legal action against researchers who act in good faith under this
policy: testing only against their own data, not accessing or modifying other
people's data, not degrading any service, and reporting findings to us before
disclosing them publicly.
