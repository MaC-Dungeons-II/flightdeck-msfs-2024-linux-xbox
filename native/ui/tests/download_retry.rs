//! A failed initial download stays recoverable before any runtime is configured.
mod common;
use flightdeck_ui::{Action, App, Edition, Language, Message, Page, settings, theme};
use iced::{Size, Task};
use serde_json::json;
use std::time::Duration;

fn fixture(language: Language, edition: Edition, page: Page) -> App {
    let mut app = common::localized_fixture(language);
    app.edition = edition;
    app.page = page;
    app.snapshot.get_mut("status").expect("status")["setup"]["busy"] = json!(true);
    app.snapshot.get_mut("status").expect("status")["runtime"]["configured"] = json!(false);
    app.snapshot.get_mut("status").expect("status")["runtime"]["path"] = json!("");
    let message = if language == Language::De {
        "Der Download wurde unterbrochen. Erneut versuchen verwendet bereits vollständig geladene Dateien nach erneuter Prüfung weiter."
    } else {
        "The download was interrupted. Retry rechecks and reuses files that have already finished downloading."
    };
    app.snapshot.get_mut("setup").expect("setup")["job"] = json!({
        "id":"failed-download","state":"failed","phase":"download_failed","failure_phase":"download",
        "mode":if page==Page::Updates{"update"}else{"install"},"can_retry":true,"can_pause":false,"can_resume":false,
        "download_exit_code":42,"message":message
    });
    app
}

#[test]
fn retry_is_visible_and_uses_the_held_job_without_enabling_another_installation()
-> Result<(), Box<dyn std::error::Error>> {
    for language in [Language::De, Language::En] {
        for edition in [Edition::Msfs2020, Edition::Msfs2024] {
            for page in [Page::Setup, Page::Updates] {
                let mut app = fixture(language, edition, page);
                assert!(app.request(&Action::SetupCheck).is_none());
                assert!(app.request(&Action::SetupStart).is_none());
                assert!(app.request(&Action::SetupControl("cancel")).is_some());
                let request = app.request(&Action::SetupControl("retry")).expect("retry");
                assert_eq!(request.path, "setup/retry");
                assert_eq!(request.body["job_id"], "failed-download");
                for width in [750.0, 1280.0] {
                    let mut ui = iced_test::Simulator::with_size(
                        settings(),
                        Size::new(width, 900.0),
                        app.view(),
                    );
                    ui.find(if language == Language::De {
                        "Downloadcode: 42"
                    } else {
                        "Download code: 42"
                    })?;
                    ui.click(if language == Language::De {
                        "Download erneut versuchen"
                    } else {
                        "Retry download"
                    })?;
                    assert!(matches!(
                        ui.into_messages().next(),
                        Some(Message::Action(Action::SetupControl("retry")))
                    ));
                }
                app.snapshot.get_mut("setup").expect("setup")["job"]["can_retry"] = json!(false);
                assert!(app.request(&Action::SetupControl("retry")).is_none());
            }
        }
    }
    Ok(())
}

#[test]
#[ignore = "writes native failed-download views in both languages, editions and widths"]
fn capture_download_retry_states() -> Result<(), Box<dyn std::error::Error>> {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/native-ui");
    std::fs::create_dir_all(&directory)?;
    for language in [Language::De, Language::En] {
        for edition in [Edition::Msfs2020, Edition::Msfs2024] {
            for page in [Page::Setup, Page::Updates] {
                for width in [750, 1280] {
                    let app = iced::application(
                        move || fixture(language, edition, page),
                        |_: &mut App, _: Message| Task::none(),
                        App::view,
                    )
                    .settings(settings())
                    .theme(|_: &App| theme());
                    let image = iced_test::screenshot(
                        &app,
                        &theme(),
                        Size::new(width as f32, 900.0),
                        1.0,
                        Duration::from_millis(80),
                    );
                    image::save_buffer(
                        directory.join(format!(
                            "download-retry-{edition:?}-{page:?}-{}-{width}.png",
                            language.code()
                        )),
                        &image.rgba,
                        width,
                        900,
                        image::ColorType::Rgba8,
                    )?;
                }
            }
        }
    }
    Ok(())
}
