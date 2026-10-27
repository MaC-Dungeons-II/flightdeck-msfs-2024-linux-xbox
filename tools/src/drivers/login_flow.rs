// SPDX-License-Identifier: MIT
//! The sign-in challenge handoff of the production webview, with loopback
//! pages and synthetic tokens. A fixture serves the three steps of an email
//! verification; the test program drives the real GTK/WebKit runtime through
//! them. This establishes that session cookies survive from one view to the
//! next, that callbacks of a previous view are ignored and that a response
//! error keeps its classification. It contacts no Microsoft endpoint and
//! initializes no keyring, and it does not prove the cause of any account's
//! sign-in error.
//!
//! It needs the native build's Cargo environment with its dependencies
//! already fetched, and an active graphical session.
use super::common::{self, Fixture, Reply, Request, exit_code, locked};
use crate::util::{Failure, Result, pretty};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};

const PROGRAM: &str = include_str!("programs/login_flow.rs.txt");
const NAME: &str = "flightdeck-login-flow-test";
const MODES: [&str; 5] = [
    "normal",
    "duplicate",
    "cookie",
    "combined",
    "response-error",
];
const SESSION: &str = "verification=synthetic-session";

/// A string as a JSON literal in ASCII, the form in which the pages embed
/// their callback argument: everything outside printable ASCII is escaped.
fn literal(text: &str) -> String {
    let mut literal = String::from('"');
    for character in text.chars() {
        match character {
            '"' => literal.push_str("\\\""),
            '\\' => literal.push_str("\\\\"),
            '\n' => literal.push_str("\\n"),
            '\r' => literal.push_str("\\r"),
            '\t' => literal.push_str("\\t"),
            '\u{8}' => literal.push_str("\\b"),
            '\u{c}' => literal.push_str("\\f"),
            ' '..='~' => literal.push(character),
            other => {
                for unit in other.encode_utf16(&mut [0; 2]) {
                    literal.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    literal.push('"');
    literal
}

/// The page of one verification step. Its script hands the webview a
/// synthetic sign-in result whose name is the step, twice in the modes that
/// reproduce a duplicate callback. In the cookie modes a later step names
/// itself only if the session cookie of the first one came back.
fn page(step: &str, mode: &str, cookie: bool) -> String {
    let name = if matches!(mode, "cookie" | "combined") && step != "password" && !cookie {
        "missing-cookie"
    } else {
        step
    };
    let payload = format!(
        r#"{{"sDAToken": "SYNTHETIC-TOKEN", "sDASessionKey": "SYNTHETIC-KEY", "sDAStartTime": "2026-09-30T12:00:00Z", "sDAExpires": "2026-09-30T13:00:00Z", "sSTSInlineFlowToken": "SYNTHETIC-FLOW", "sSigninName": {}, "K": "SYNTHETIC-PUID"}}"#,
        literal(name)
    );
    let mut post = format!("window.external.notify({});", literal(&payload));
    if matches!(mode, "duplicate" | "combined") && step != "verified" {
        post = post.repeat(2);
    }
    format!(
        "<!doctype html><html><body><h1>{step}</h1>\
         <p>Offline email-code verification fixture</p><script>\
         requestAnimationFrame(() => setTimeout(() => {{{post}}}, 200));\
         </script></body></html>"
    )
}

/// The fixture: the path names the step and the query the mode. The first
/// step sets a session cookie that only the same browser context returns.
fn handle(request: &Request, log: &Mutex<Vec<Value>>) -> Option<Reply> {
    if request.method != "GET" {
        return Some(Reply::unsupported());
    }
    let (path, mode) = request.path.split_once('?').unwrap_or((&request.path, ""));
    let step = path.trim_start_matches('/');
    let cookie = request
        .header("Cookie")
        .unwrap_or_default()
        .contains(SESSION);
    locked(log).push(json!({"step": step, "cookie_present": cookie}));
    let mut headers = vec![("Content-Type", "text/html; charset=utf-8")];
    if step == "password" {
        headers.push((
            "Set-Cookie",
            "verification=synthetic-session; Path=/; HttpOnly; SameSite=Strict",
        ));
    }
    Some(Reply {
        status: 200,
        headers,
        body: page(step, mode, cookie).into_bytes(),
    })
}

/// The test program with its window backend, the fixture and a profile of
/// its own, so that no configuration, cache or cookie of the user is read.
/// The profile is named as the output directory was, relative if that was.
fn program(binary: &Path, mode: &str, backend: &str, origin: &str, profile: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .arg(mode)
        .env("GDK_BACKEND", backend)
        .env("FIXTURE_ORIGIN", origin);
    for kind in ["config", "cache", "data", "state"] {
        command.env(
            format!("XDG_{}_HOME", kind.to_uppercase()),
            profile.join(kind),
        );
    }
    command
}

/// A mode passed if the program succeeded in time and the fixture served
/// exactly the expected steps, each once and in order.
fn passed(mode: &str, code: Option<i64>, requests: &[Value]) -> bool {
    let expected: &[&str] = if mode == "response-error" {
        &["password"]
    } else {
        &["password", "email-code", "verified"]
    };
    code == Some(0)
        && requests.len() == expected.len()
        && requests
            .iter()
            .zip(expected)
            .all(|(request, step)| request["step"] == *step)
}

pub fn run(stage: &Path, output: &Path, backend: &str) -> Result<u8> {
    let stage = common::stage(stage)?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::create_dir(output).map_err(|error| Failure(format!("{}: {error}", output.display())))?;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let binary = common::built(
        &stage,
        NAME,
        PROGRAM,
        "Probe already exists",
        true,
        Some(&output.join("build.log")),
    )?;
    let log = requests.clone();
    let server = Fixture::start(move |request, _| handle(request, &log))?;
    let mut results = Vec::new();
    for mode in MODES {
        locked(&requests).clear();
        let profile = tempfile::Builder::new()
            .prefix("login-profile-")
            .tempdir_in(output)?;
        let named = profile
            .path()
            .file_name()
            .map_or_else(|| profile.path().to_path_buf(), |name| output.join(name));
        let log = fs::File::create(output.join(format!("{mode}.log")))?;
        let mut command = program(&binary, mode, backend, &server.origin, &named);
        command.stdout(log.try_clone()?).stderr(log);
        let code = common::finished(&mut command, Duration::from_secs(20))?.map(exit_code);
        let seen = locked(&requests).clone();
        results.push(json!({"mode": mode, "passed": passed(mode, code, &seen),
            "exit_code": code.map_or_else(|| json!("timeout"), Value::from), "requests": seen}));
    }
    drop(server);
    let failed = !results.iter().all(|result| result["passed"] == true);
    let report = pretty(&Value::Array(results));
    fs::write(output.join("result.json"), &report)?;
    print!("{}", String::from_utf8_lossy(&report));
    Ok(u8::from(failed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http;

    #[test]
    fn literals_are_ascii_json() {
        assert_eq!(literal("password"), "\"password\"");
        assert_eq!(literal("a\"b\\c\n\r\t\u{8}\u{c}"), r#""a\"b\\c\n\r\t\b\f""#);
        assert_eq!(
            literal("\u{1}\u{7f}\u{e4}\u{20ac}\u{1f600}"),
            "\"\\u0001\\u007f\\u00e4\\u20ac\\ud83d\\ude00\""
        );
        for text in ["", "email-code", "a\"b\\c\n", "\u{e4}\u{1f600}\u{7f}"] {
            assert!(literal(text).is_ascii());
            assert_eq!(
                serde_json::from_str::<String>(&literal(text)).expect("literal"),
                text
            );
        }
    }

    /// The object a page hands to the webview, and how often.
    fn handed(page: &str) -> (Value, usize) {
        let call = "window.external.notify(";
        let start = page.find(call).expect("call") + call.len();
        let length = page[start..].find(");").expect("end");
        let argument: String =
            serde_json::from_str(&page[start..start + length]).expect("argument");
        (
            serde_json::from_str(&argument).expect("payload"),
            page.matches(call).count(),
        )
    }

    #[test]
    fn pages_hand_over_a_synthetic_result_named_after_the_step() {
        assert_eq!(
            page("password", "normal", false),
            "<!doctype html><html><body><h1>password</h1><p>Offline email-code verification fixture</p><script>\
             requestAnimationFrame(() => setTimeout(() => {\
             window.external.notify(\"{\\\"sDAToken\\\": \\\"SYNTHETIC-TOKEN\\\", \\\"sDASessionKey\\\": \\\"SYNTHETIC-KEY\\\", \
             \\\"sDAStartTime\\\": \\\"2026-09-30T12:00:00Z\\\", \\\"sDAExpires\\\": \\\"2026-09-30T13:00:00Z\\\", \
             \\\"sSTSInlineFlowToken\\\": \\\"SYNTHETIC-FLOW\\\", \\\"sSigninName\\\": \\\"password\\\", \
             \\\"K\\\": \\\"SYNTHETIC-PUID\\\"}\");\
             }, 200));</script></body></html>"
        );
        let (payload, calls) = handed(&page("email-code", "normal", false));
        assert_eq!(
            payload,
            json!({"sDAToken": "SYNTHETIC-TOKEN", "sDASessionKey": "SYNTHETIC-KEY",
                "sDAStartTime": "2026-09-30T12:00:00Z", "sDAExpires": "2026-09-30T13:00:00Z",
                "sSTSInlineFlowToken": "SYNTHETIC-FLOW", "sSigninName": "email-code",
                "K": "SYNTHETIC-PUID"})
        );
        assert_eq!(calls, 1);
        // Mode, step, returned cookie: the name handed over and the calls.
        for (mode, step, cookie, name, calls) in [
            ("normal", "verified", false, "verified", 1),
            ("duplicate", "password", false, "password", 2),
            ("duplicate", "email-code", false, "email-code", 2),
            ("duplicate", "verified", false, "verified", 1),
            ("cookie", "password", false, "password", 1),
            ("cookie", "email-code", false, "missing-cookie", 1),
            ("cookie", "email-code", true, "email-code", 1),
            ("cookie", "verified", false, "missing-cookie", 1),
            ("combined", "password", false, "password", 2),
            ("combined", "email-code", false, "missing-cookie", 2),
            ("combined", "email-code", true, "email-code", 2),
            ("combined", "verified", true, "verified", 1),
            ("response-error", "password", false, "password", 1),
            ("", "a\"<b>", false, "a\"<b>", 1),
        ] {
            let (payload, count) = handed(&page(step, mode, cookie));
            assert_eq!(
                (payload["sSigninName"].as_str(), count),
                (Some(name), calls),
                "{mode} {step}"
            );
        }
    }

    #[test]
    fn fixture_sets_the_session_cookie_once_and_records_whether_it_returns() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        let server = Fixture::start(move |request, _| handle(request, &log)).expect("fixture");
        let port = common::fixtures::port(&server.origin);
        let get = |path: &str, headers: &[(&str, &str)]| {
            http::request(port, "GET", path, headers, None, Duration::from_secs(10))
                .expect("response")
        };
        let first = get("/password?cookie", &[]);
        assert_eq!(first.status, 200);
        assert_eq!(first.headers["content-type"], "text/html; charset=utf-8");
        assert_eq!(
            first.headers["set-cookie"],
            "verification=synthetic-session; Path=/; HttpOnly; SameSite=Strict"
        );
        assert_eq!(
            String::from_utf8_lossy(&first.body),
            page("password", "cookie", false)
        );
        let lost = get("/email-code?cookie", &[]);
        assert!(!lost.headers.contains_key("set-cookie"));
        assert_eq!(
            String::from_utf8_lossy(&lost.body),
            page("email-code", "cookie", false)
        );
        let kept = get(
            "/verified?combined",
            &[("Cookie", "other=1; verification=synthetic-session")],
        );
        assert_eq!(
            String::from_utf8_lossy(&kept.body),
            page("verified", "combined", true)
        );
        assert_eq!(get("//email-code", &[]).status, 200);
        assert_eq!(
            http::request(
                port,
                "POST",
                "/password",
                &[],
                Some(b""),
                Duration::from_secs(10)
            )
            .expect("response")
            .status,
            501
        );
        assert_eq!(
            *locked(&requests),
            [
                json!({"step": "password", "cookie_present": false}),
                json!({"step": "email-code", "cookie_present": false}),
                json!({"step": "verified", "cookie_present": true}),
                json!({"step": "email-code", "cookie_present": false}),
            ]
        );
    }

    #[test]
    fn a_mode_passes_with_a_clean_exit_and_exactly_its_steps() {
        let steps = |names: &[&str]| -> Vec<Value> {
            names
                .iter()
                .map(|name| json!({"step": name, "cookie_present": false}))
                .collect()
        };
        let all = steps(&["password", "email-code", "verified"]);
        for mode in ["normal", "duplicate", "cookie", "combined"] {
            assert!(passed(mode, Some(0), &all), "{mode}");
            assert!(!passed(mode, Some(1), &all), "{mode}");
            assert!(!passed(mode, None, &all), "{mode}");
            assert!(!passed(mode, Some(0), &steps(&["password"])), "{mode}");
        }
        // A repeated or foreign request is a different flow.
        assert!(!passed(
            "normal",
            Some(0),
            &steps(&["password", "password", "email-code", "verified"])
        ));
        assert!(!passed(
            "normal",
            Some(0),
            &steps(&["password", "verified", "email-code"])
        ));
        assert!(!passed(
            "normal",
            Some(0),
            &steps(&["password", "email-code", "favicon.ico"])
        ));
        assert!(passed("response-error", Some(0), &steps(&["password"])));
        assert!(!passed("response-error", Some(0), &all));
        assert!(!passed("response-error", Some(0), &[]));
    }

    #[test]
    fn program_runs_with_its_own_profile_backend_and_fixture() {
        let command = program(
            Path::new("/synthetic/target/debug/flightdeck-login-flow-test"),
            "combined",
            "wayland",
            "http://127.0.0.1:49152",
            Path::new("build/login-flow-check/login-profile-a1"),
        );
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["combined"]);
        let set: Vec<_> = command
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect();
        let variable = |name: &str, value: &str| (name.to_string(), Some(value.to_string()));
        assert_eq!(
            set,
            [
                variable("FIXTURE_ORIGIN", "http://127.0.0.1:49152"),
                variable("GDK_BACKEND", "wayland"),
                variable(
                    "XDG_CACHE_HOME",
                    "build/login-flow-check/login-profile-a1/cache"
                ),
                variable(
                    "XDG_CONFIG_HOME",
                    "build/login-flow-check/login-profile-a1/config"
                ),
                variable(
                    "XDG_DATA_HOME",
                    "build/login-flow-check/login-profile-a1/data"
                ),
                variable(
                    "XDG_STATE_HOME",
                    "build/login-flow-check/login-profile-a1/state"
                ),
            ]
        );
    }

    #[test]
    fn program_is_the_synthetic_sign_in_handler() {
        assert_eq!(PROGRAM.len(), 2514);
        assert!(PROGRAM.starts_with("\n// SPDX-License-Identifier: MIT\n// Synthetic pages"));
        assert!(PROGRAM.ends_with("_ => std::process::exit(1),\n    }\n}\n"));
        for marker in [
            "include!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/src/webview.rs\"));",
            "Flightdeck synthetic email verification",
            "impl SessionHandler for Handler",
            "[\"password\", \"email-code\", \"verified\"]",
            "RSTError::InvalidResponse",
            "FIXTURE_ORIGIN",
            "Preserved response error classification",
        ] {
            assert!(PROGRAM.contains(marker), "{marker}");
        }
        assert_eq!(
            MODES,
            [
                "normal",
                "duplicate",
                "cookie",
                "combined",
                "response-error"
            ]
        );
    }

    #[test]
    fn output_directory_must_be_new_and_the_stage_must_hold_the_sources() {
        let temp = tempfile::tempdir().expect("temp");
        let stage = common::fixtures::staged();
        // An existing output directory is refused before anything is built.
        let error = run(stage.path(), temp.path(), "x11").expect_err("existing");
        assert!(error.0.contains("File exists"), "{}", error.0);
        assert!(
            !stage
                .path()
                .join("xodus-src/crates/xodus-cli/src/bin")
                .exists()
        );
        // A directory that is no stage is refused, with nothing left in it.
        let empty = tempfile::tempdir().expect("temp");
        let output = temp.path().join("nested/output");
        let error = run(empty.path(), &output, "x11").expect_err("no stage");
        assert!(
            error
                .0
                .starts_with("The stage has no Xodus command sources: "),
            "{}",
            error.0
        );
        assert_eq!(fs::read_dir(empty.path()).expect("stage").count(), 0);
        assert!(output.is_dir() && !output.join("build.log").exists());
        assert!(
            run(
                &temp.path().join("missing"),
                &temp.path().join("other"),
                "x11"
            )
            .is_err()
        );
        assert!(!temp.path().join("other").exists());
    }
}
