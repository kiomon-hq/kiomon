// SPDX-License-Identifier: Apache-2.0

//! Everything that writes to a stream, and the only module that decides an exit code.
//!
//! Two rules, both asserted in `tests/cli.rs`:
//!
//! - **stdout carries the answer and nothing else.** Progress, warnings and
//!   diagnostics go to stderr, so `kiomon search … | jq` is always safe. This is why
//!   the library never prints: the decision about which stream a message belongs on
//!   is made here, once, where the contract is.
//! - **A parse failure exits 3, not clap's 2.** The contract spends 2 on "not found"
//!   (see [`kiomon_core::ExitCode`]), and letting clap's own default through would
//!   make an unknown flag indistinguishable from a missing memory.

use serde::Serialize;

use kiomon_core::{Error, ExitCode, Result};

/// The `meta` block on every `--json` response.
///
/// An integration reads [`Meta::contract_version`] before anything else, so it can
/// refuse a shape it does not understand rather than mis-parse one it does.
#[derive(Debug, Serialize)]
pub struct Meta {
    /// The JSON contract version — `kiomon_core::CONTRACT_VERSION`.
    pub contract_version: &'static str,
    /// The version of this build.
    pub version: &'static str,
}

impl Meta {
    /// The `meta` block for this build.
    #[must_use]
    pub const fn current() -> Self {
        Self {
            contract_version: kiomon_core::CONTRACT_VERSION,
            version: kiomon_core::VERSION,
        }
    }
}

/// The success envelope: `{"ok":true,"data":…,"meta":…}`.
///
/// Nothing constructs this in a non-test build until WP1 lands the first command that
/// succeeds. It is here from WP0 because the JSON contract is reviewed as a whole, and
/// `out::tests` asserts its shape.
#[allow(dead_code, reason = "the first success path is WP1")]
#[derive(Debug, Serialize)]
pub struct Success<T> {
    /// Always `true`. Present so a caller can branch on one field.
    pub ok: bool,
    /// The command's result.
    pub data: T,
    /// Version information.
    pub meta: Meta,
}

impl<T> Success<T> {
    /// Wrap a value in the success envelope.
    #[must_use]
    #[allow(dead_code, reason = "the first success path is WP1")]
    pub const fn new(data: T) -> Self {
        Self {
            ok: true,
            data,
            meta: Meta::current(),
        }
    }
}

/// The failure envelope: `{"ok":false,"error":{…},"meta":…}`.
#[derive(Debug, Serialize)]
struct Failure {
    ok: bool,
    error: FailureBody,
    meta: Meta,
}

#[derive(Debug, Serialize)]
struct FailureBody {
    code: &'static str,
    message: String,
}

/// Report a `clap` parse failure on the right stream, and return its exit code.
///
/// `--help` and `--version` are clap's two *successful* exits, and they are the cases
/// where it writes to stdout. Everything else is the user having got something wrong,
/// which the contract puts at 3.
pub fn parse_failure(err: &clap::Error) -> std::process::ExitCode {
    let code = if err.use_stderr() {
        ExitCode::InvalidArgs
    } else {
        ExitCode::Success
    };
    // `print()` picks the stream itself — stdout for help and version, stderr for
    // everything else — which is the split this function's exit code encodes.
    let _ = err.print();
    code.into()
}

/// Print a failure and return its exit code.
///
/// `--quiet` never suppresses an error: the flag means "stop telling me things I
/// already know", and a failure is not one of those.
pub fn report(json: bool, err: &Error) -> std::process::ExitCode {
    if json {
        let body = Failure {
            ok: false,
            error: FailureBody {
                code: err.code(),
                message: err.to_string(),
            },
            meta: Meta::current(),
        };
        match serde_json::to_string(&body) {
            Ok(line) => println!("{line}"),
            // Unreachable in practice, and worth handling anyway: falling back to the
            // human form still tells the user what went wrong, whereas a panic here
            // would hide the original error behind a serialization one.
            Err(_) => eprintln!("kiomon: {err}"),
        }
    } else {
        eprintln!("kiomon: {err}");
    }
    err.exit_code().into()
}

/// Emit a successful result: JSON when asked, otherwise through `human`.
///
/// # Errors
///
/// Returns [`Error::Internal`] if `data` cannot be serialized. That is a bug in the
/// response type rather than a user error, so it is reported as one.
#[allow(dead_code, reason = "the first success path is WP1")]
pub fn emit<T, F>(json: bool, data: T, human: F) -> Result<()>
where
    T: Serialize,
    F: FnOnce(&T),
{
    if json {
        let line = serde_json::to_string(&Success::new(data)).map_err(|err| {
            Error::Internal(format!("could not encode the response as JSON: {err}"))
        })?;
        println!("{line}");
    } else {
        human(&data);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_success_envelope_has_the_documented_shape() {
        let json = serde_json::to_value(Success::new(serde_json::json!({"id": "k_01"}))).unwrap();
        assert_eq!(json["ok"], serde_json::json!(true));
        assert_eq!(json["data"]["id"], serde_json::json!("k_01"));
        assert_eq!(
            json["meta"]["contract_version"],
            serde_json::json!(kiomon_core::CONTRACT_VERSION)
        );
        assert!(json["meta"]["version"].is_string());
    }

    #[test]
    fn the_failure_envelope_has_the_documented_shape() {
        let body = Failure {
            ok: false,
            error: FailureBody {
                code: "not_found",
                message: "no memory with id `k_01`".into(),
            },
            meta: Meta::current(),
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["ok"], serde_json::json!(false));
        assert_eq!(json["error"]["code"], serde_json::json!("not_found"));
        assert!(json["error"]["message"].is_string());
        assert!(json["meta"]["contract_version"].is_string());
    }
}
