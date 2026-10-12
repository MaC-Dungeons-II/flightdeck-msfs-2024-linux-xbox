// SPDX-License-Identifier: MIT
//! The native Store form's origin, against a loopback-only HTTP fixture.
//! The fixture reproduces the empty HTTP 403 response to `Origin: null` and
//! accepts the form only from its own origin. The test program loads the
//! production purchase host in four ways: the previous about:blank host, the
//! public-page bootstrap, a cancellation and a document that is not the
//! expected one. It never uses a real account, Microsoft endpoint, product or
//! payment.
//!
//! It needs the same Cargo environment and desktop display as the Store
//! window test.
use super::common::{self, Fixture, Owned, Reply, Request, exit_code, locked};
use crate::{
    ensure,
    util::{Failure, Result, pretty, require},
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader},
    path::Path,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const PROGRAM: &str = include_str!("programs/store_navigation.rs.txt");
const NAME: &str = "flightdeck-store-navigation-test";
const MODES: [&str; 4] = ["legacy", "fixed", "cancel", "wrong-document"];
const TOKEN: &str = "SYNTHETIC-NOT-A-CREDENTIAL";
const HTML: (&str, &str) = ("Content-Type", "text/html");
const BOOTSTRAP: &str =
    "<!doctype html><html><head><title>Offline public bootstrap</title></head><body></body></html>";
const CONFIRMATION: &str = "<!doctype html><html><head><meta charset=\"utf-8\"></head><body><h1>Offline confirmation loaded</h1><p>No account or payment service.</p><script>parent.postMessage(\"ReactPurchaseReadyToRender\",\"*\")</script></body></html>";

/// One name or value of a submitted form: `+` is a space and `%XX` a byte.
fn unquoted(text: &str) -> String {
    let raw = text.replace('+', " ").into_bytes();
    let (mut decoded, mut index) = (Vec::new(), 0);
    while index < raw.len() {
        let byte = raw
            .get(index + 1..index + 3)
            .filter(|_| raw[index] == b'%')
            .and_then(|digits| std::str::from_utf8(digits).ok())
            .filter(|digits| digits.bytes().all(|digit| digit.is_ascii_hexdigit()))
            .and_then(|digits| u8::from_str_radix(digits, 16).ok());
        decoded.push(byte.unwrap_or(raw[index]));
        index += if byte.is_some() { 3 } else { 1 };
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// Every non-empty value a form body gives a field.
fn field(body: &str, name: &str) -> Vec<String> {
    body.split('&')
        .filter_map(|pair| pair.split_once('='))
        .filter(|(key, value)| !value.is_empty() && unquoted(key) == name)
        .map(|(_, value)| unquoted(value))
        .collect()
}

/// The fixture: any page is the offline bootstrap document, and the form is
/// confirmed only when it is posted from the fixture's own origin. A post
/// from anywhere else gets the empty refusal the real endpoint sends.
fn handle(request: &Request, origin: &str, log: &Mutex<Vec<Value>>) -> Option<Reply> {
    match request.method.as_str() {
        "GET" => {
            locked(log).push(json!({"method": "GET", "path": request.path}));
            Some(Reply {
                status: 200,
                headers: vec![HTML],
                body: BOOTSTRAP.into(),
            })
        }
        "POST" => {
            let body = String::from_utf8(request.body.clone()?).ok()?;
            let sent = request.header("Origin");
            let allowed = sent == Some(origin);
            locked(log).push(
                json!({"method": "POST", "status": if allowed { 200 } else { 403 },
                "origin": sent, "synthetic_token": field(&body, "xToken") == [TOKEN]}),
            );
            Some(if allowed {
                Reply {
                    status: 200,
                    headers: vec![HTML],
                    body: CONFIRMATION.into(),
                }
            } else {
                Reply {
                    status: 403,
                    headers: vec![HTML, ("Content-Security-Policy", "frame-ancestors 'none'")],
                    body: Vec::new(),
                }
            })
        }
        _ => Some(Reply::unsupported()),
    }
}

/// What each way of loading the host must have caused at the fixture.
fn judge(mode: &str, events: &[Value], requests: &[Value], origin: &str) -> Result<()> {
    let phase = |name: &str| events.iter().any(|event| event["phase"] == name);
    let ready = phase("checkout_ready");
    let posts: Vec<&Value> = requests
        .iter()
        .filter(|request| request["method"] == "POST")
        .collect();
    match mode {
        "legacy" => ensure!(
            posts.len() == 1 && posts[0]["origin"] == "null" && posts[0]["status"] == 403 && !ready
        ),
        "fixed" => {
            ensure!(
                posts.len() == 1
                    && posts[0]["origin"] == origin
                    && posts[0]["status"] == 200
                    && ready
            );
            ensure!(
                events
                    .iter()
                    .any(|event| event["heading"] == "Microsoft Store")
            );
        }
        _ => {
            ensure!(posts.is_empty() && !ready);
            if mode == "wrong-document" {
                ensure!(phase("bootstrap_error"));
            }
        }
    }
    Ok(())
}

struct Run<'a> {
    binary: &'a Path,
    backend: &'a str,
    origin: &'a str,
    output: &'a Path,
    requests: &'a Mutex<Vec<Value>>,
}

/// Runs the test program in one mode, captures the window of the two modes
/// that submit the form, and returns the events the program printed. The
/// window id outlives the mode, as it is announced once per process.
fn observe(
    run: &Run<'_>,
    mode: &str,
    window: &mut Option<Value>,
    desktop: &mut dyn FnMut(&str) -> Result<Value>,
) -> Result<Vec<Value>> {
    let errors = fs::File::create(run.output.join(format!("{mode}-stderr.log")))?;
    let mut command = Command::new(run.binary);
    command
        .arg(mode)
        .stdout(Stdio::piped())
        .stderr(errors)
        .env("GDK_BACKEND", run.backend)
        .env("FIXTURE_ORIGIN", run.origin);
    let mut process = Owned::killed(common::spawn(&mut command)?);
    let pid = process.child.id();
    let lines = process
        .child
        .stdout
        .take()
        .ok_or_else(|| Failure("The test program has no output".into()))?;
    let mut events = Vec::new();
    for line in BufReader::new(lines).lines() {
        let value = common::event(&line?)?;
        if let Some(id) = value.get("window") {
            *window = Some(id.clone());
        }
        let rendered = value["probe"] == "render";
        events.push(value);
        if rendered && matches!(mode, "fixed" | "legacy") {
            common::capture(&common::screenshot(
                window.as_ref(),
                pid,
                &run.output.join(format!("{mode}.png")),
                false,
                desktop,
            )?)?;
        }
    }
    fs::write(
        run.output.join(format!("{mode}-events.json")),
        serde_json::to_string_pretty(
            &json!({"events": events, "requests": *locked(run.requests)}),
        )?,
    )?;
    let status = common::wait_until(&mut process.child, Instant::now() + Duration::from_secs(12))?
        .ok_or_else(|| Failure("The test program did not exit".into()))?;
    require(
        status.success(),
        format!(
            "The test program failed with exit code {}",
            exit_code(status)
        ),
    )?;
    Ok(events)
}

pub fn run(stage: &Path, backend: &str) -> Result<u8> {
    let stage = common::stage(stage)?;
    let output = tempfile::Builder::new()
        .prefix("flightdeck-store-navigation-")
        .tempdir()?
        .keep();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let log = requests.clone();
    let server = Fixture::start(move |request, origin| handle(request, origin, &log))?;
    let binary = common::built(
        &stage,
        NAME,
        PROGRAM,
        "Refusing to replace an existing probe",
        false,
        None,
    )?;
    let job = Run {
        binary: &binary,
        backend,
        origin: &server.origin,
        output: &output,
        requests: &requests,
    };
    let (mut reports, mut window) = (Vec::new(), None);
    for mode in MODES {
        locked(&requests).clear();
        let events = observe(&job, mode, &mut window, &mut common::hyprctl)?;
        let seen = locked(&requests).clone();
        judge(mode, &events, &seen, &server.origin)?;
        reports.push(json!({"mode": mode, "requests": seen, "events": events}));
    }
    drop(server);
    let cases = reports.len();
    let report = json!({"passed": true, "backend": backend, "real_account_calls": 0,
        "real_payment_requests": 0, "cases": reports});
    fs::write(output.join("result.json"), pretty(&report))?;
    println!(
        "{}",
        json!({"passed": true, "cases": cases, "backend": backend,
            "artifacts": output.display().to_string()})
    );
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{http, util::fail};

    #[test]
    fn form_fields_are_decoded_and_blank_values_ignored() {
        let body = "productId=ABCD1234EFGH&xToken=SYNTHETIC-NOT-A-CREDENTIAL&note=a+b%2Fc%C3%A4&blank=&bare";
        assert_eq!(field(body, "xToken"), [TOKEN]);
        assert_eq!(field(body, "note"), ["a b/c\u{e4}"]);
        assert!(field(body, "blank").is_empty() && field(body, "bare").is_empty());
        assert_eq!(
            field("x%54oken=a&xToken=b%zz&xToken=%", "xToken"),
            ["a", "b%zz", "%"]
        );
        assert!(field("", "xToken").is_empty());
    }

    #[test]
    fn fixture_confirms_its_own_origin_and_refuses_every_other_with_an_empty_403() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        let server =
            Fixture::start(move |request, origin| handle(request, origin, &log)).expect("fixture");
        let port = common::fixtures::port(&server.origin);
        let timeout = Duration::from_secs(10);
        let form = format!("productId=ABCD1234EFGH&xToken={TOKEN}");
        let post = |origin: Option<&str>, body: &str| {
            let headers: Vec<_> = origin
                .map(|origin| ("Origin", origin))
                .into_iter()
                .collect();
            http::request(
                port,
                "POST",
                "/store/purchase/buynowui/buynow",
                &headers,
                Some(body.as_bytes()),
                timeout,
            )
            .expect("response")
        };
        let page = http::request(
            port,
            "GET",
            "/store/purchase/buynowui/prefetch/buynow",
            &[],
            None,
            timeout,
        )
        .expect("response");
        assert_eq!(
            (page.status, page.headers["content-type"].as_str()),
            (200, "text/html")
        );
        assert_eq!(page.body, BOOTSTRAP.as_bytes());
        let opaque = post(Some("null"), &form);
        assert_eq!(opaque.status, 403);
        assert!(opaque.body.is_empty());
        assert_eq!(
            opaque.headers["content-security-policy"],
            "frame-ancestors 'none'"
        );
        let own = post(Some(&server.origin), &form);
        assert_eq!(own.status, 200);
        assert_eq!(own.body, CONFIRMATION.as_bytes());
        assert!(!own.headers.contains_key("content-security-policy"));
        assert_eq!(post(None, "xToken=other").status, 403);
        assert_eq!(
            http::request(port, "PUT", "/", &[], None, timeout)
                .expect("response")
                .status,
            501
        );
        assert_eq!(
            *locked(&requests),
            [
                json!({"method": "GET", "path": "/store/purchase/buynowui/prefetch/buynow"}),
                json!({"method": "POST", "status": 403, "origin": "null", "synthetic_token": true}),
                json!({"method": "POST", "status": 200, "origin": server.origin,
                    "synthetic_token": true}),
                json!({"method": "POST", "status": 403, "origin": null, "synthetic_token": false}),
            ]
        );
        // A form without a length is no request, and nothing is recorded.
        let unsized_post = common::fixtures::request("POST", "/", &[("Origin", "null")], None);
        assert!(handle(&unsized_post, &server.origin, &requests).is_none());
        let binary = common::fixtures::request("POST", "/", &[], Some(b"xToken=\xff"));
        assert!(handle(&binary, &server.origin, &requests).is_none());
        assert_eq!(locked(&requests).len(), 4);
    }

    #[test]
    fn each_mode_is_judged_by_its_posts_and_phases() {
        let origin = "http://127.0.0.1:49152";
        let post = |origin: &str, status: u16| json!({"method": "POST", "status": status, "origin": origin, "synthetic_token": true});
        let page = [json!({"method": "GET", "path": "/store/purchase/buynowui/prefetch/buynow"})];
        let refused = [page[0].clone(), post("null", 403)];
        let accepted = [page[0].clone(), post(origin, 200)];
        let twice = [post("null", 403), post("null", 403)];
        let foreign = [post("http://127.0.0.1:1", 200)];
        let ready = [json!({"phase": "checkout_ready"})];
        let heading = [json!({"probe": "render", "heading": "Microsoft Store", "forms": 1})];
        let loaded = [ready[0].clone(), heading[0].clone()];
        let cancelled = [json!({"status": "cancel"})];
        let rejected = [json!({"phase": "bootstrap_error"})];
        // Mode, events and requests: whether that is what the mode must cause.
        let cases: [(&str, &[Value], &[Value], bool); 15] = [
            ("legacy", &heading, &refused, true),
            ("legacy", &ready, &refused, false),
            ("legacy", &[], &accepted, false),
            ("legacy", &[], &page, false),
            ("legacy", &[], &twice, false),
            ("fixed", &loaded, &accepted, true),
            ("fixed", &loaded, &refused, false),
            ("fixed", &heading, &accepted, false),
            ("fixed", &ready, &accepted, false),
            ("fixed", &loaded, &foreign, false),
            ("cancel", &cancelled, &page, true),
            ("cancel", &[], &accepted, false),
            ("cancel", &ready, &[], false),
            ("wrong-document", &rejected, &page, true),
            ("wrong-document", &[], &page, false),
        ];
        for (index, (mode, events, requests, expected)) in cases.into_iter().enumerate() {
            assert_eq!(
                judge(mode, events, requests, origin).is_ok(),
                expected,
                "{index} {mode}"
            );
        }
        let error = judge("legacy", &[], &[], origin).expect_err("no post");
        assert!(error.0.contains("posts.len() == 1"), "{}", error.0);
    }

    #[test]
    fn program_is_the_offline_store_navigation_fixture() {
        assert_eq!(PROGRAM.len(), 3969);
        assert!(PROGRAM.starts_with("\n#[allow(dead_code)]\n#[path = \"../store_purchase.rs\"]"));
        assert!(PROGRAM.ends_with("assert!(cancelled || mode==\"wrong-document\");\n}\n"));
        for marker in [
            "FIXTURE_ORIGIN",
            "http://127.0.0.1:",
            "/store/purchase/buynowui/prefetch/buynow",
            "Flightdeck offline Store navigation",
            "store_purchase::bootstrap_listener",
            "store_purchase::bootstrap_script",
            "wrong-document",
            TOKEN,
        ] {
            assert!(PROGRAM.contains(marker), "{marker}");
        }
        assert_eq!(MODES, ["legacy", "fixed", "cancel", "wrong-document"]);
    }

    #[test]
    fn each_mode_runs_with_the_fixture_origin_and_leaves_its_events() {
        let temp = tempfile::tempdir().expect("temp");
        let binary = temp.path().join("program");
        common::fixtures::script(
            &binary,
            "WINDOW=7; [ \"$1\" != fixed ] || WINDOW=null\n\
             echo \"{\\\"window\\\":$WINDOW,\\\"mode\\\":\\\"$1\\\",\\\"origin\\\":\\\"$FIXTURE_ORIGIN\\\",\\\"backend\\\":\\\"$GDK_BACKEND\\\"}\"\n\
             echo \"problem in $1\" >&2\n\
             [ \"$1\" != cancel ] || echo '{\"probe\":\"render\"}'\n\
             [ \"$1\" != fixed ] || { echo '{\"probe\":\"render\"}'; exec sleep 30; }\n\
             [ \"$1\" != wrong-document ] || exit 101",
        );
        let requests = Mutex::new(vec![json!({"method": "GET", "path": "/"})]);
        let run = Run {
            binary: &binary,
            backend: "wayland",
            origin: "http://127.0.0.1:49152",
            output: temp.path(),
            requests: &requests,
        };
        let mut window = None;
        let mut desktop = |_: &str| -> Result<Value> { fail("No desktop in this test") };
        let events = observe(&run, "legacy", &mut window, &mut desktop).expect("events");
        let first = json!({"window": 7, "mode": "legacy", "origin": "http://127.0.0.1:49152",
            "backend": "wayland"});
        assert_eq!(events, std::slice::from_ref(&first));
        assert_eq!(window, Some(json!(7)));
        assert_eq!(
            fs::read_to_string(temp.path().join("legacy-stderr.log")).expect("log"),
            "problem in legacy\n"
        );
        let saved = fs::read_to_string(temp.path().join("legacy-events.json")).expect("events");
        assert!(!saved.ends_with('\n'));
        assert_eq!(
            serde_json::from_str::<Value>(&saved).expect("events"),
            json!({"events": [first], "requests": [{"method": "GET", "path": "/"}]})
        );
        // Only the two modes that submit the form are captured: a render in
        // another mode takes no screenshot.
        let events = observe(&run, "cancel", &mut window, &mut desktop).expect("events");
        assert_eq!(events.len(), 2);
        assert!(!temp.path().join("cancel.png").exists());
        assert_eq!(
            observe(&run, "wrong-document", &mut window, &mut desktop)
                .expect_err("failure")
                .0,
            "The test program failed with exit code 101"
        );
        // The form modes are captured. A native Wayland window is looked up
        // on the desktop, which fails here, and the program, which would
        // stay for half a minute, is killed.
        let started = Instant::now();
        assert_eq!(
            observe(&run, "fixed", &mut window, &mut desktop)
                .expect_err("desktop")
                .0,
            "No desktop in this test"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(window, Some(Value::Null));
    }
}
