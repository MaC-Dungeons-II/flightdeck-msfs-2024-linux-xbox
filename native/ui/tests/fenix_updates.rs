//! Independent patch discovery uses the same update screen and runtime guards.
mod common;
use flightdeck_ui::{Action, App, Language, Message, Page, settings, theme};
use iced::{Size, Task};
use serde_json::json;
use std::time::Duration;

fn fixture(language: Language, state: &str) -> App {
    let mut app = common::localized_fixture(language);
    app.page = Page::Updates;
    app.snapshot.insert("launcher-update",json!({"managed":true,"installed_version":"0.2.20","latest_version":"0.2.20","update_available":false,"can_check":true,"can_install":false,"checked_at":"2026-10-23T16:00:00Z","job":{"operation":"check","state":"complete"}}));
    app.snapshot.insert("fenix",json!({"runtime_path":"/synthetic/msfs2024","state":"installed","installed":true,"installed_version":"0.1.0-preview.11","version":"0.1.0-preview.12","can_change":true,"can_check_updates":true,"update_available":state=="available","patch_update":{"state":if state=="available"{"complete"}else{state},"requires_launcher_update":state=="requires-launcher"}}));
    if state.starts_with("legacy") {
        app.snapshot.insert("fenix",json!({"runtime_path":"/synthetic/msfs2024", "state":"legacy", "installed":false,
            "migration_required":true,"version":"0.1.0","can_change":true,"can_check_updates":true,"patch_update":{"state":"complete"}}));
        if state == "legacy-overview" {
            app.page = Page::Overview;
        }
        if state == "legacy-paused" {
            app.snapshot.get_mut("fenix").expect("Fenix")["automatic_adoption_paused"] =
                json!(true);
        }
    }
    app
}

#[test]
fn patch_checks_work_during_a_game_but_installation_requires_an_idle_current_runtime()
-> Result<(), Box<dyn std::error::Error>> {
    for language in [Language::De, Language::En] {
        let mut app = fixture(language, "available");
        let title = if language == Language::De {
            "Fenix-Updates prüfen"
        } else {
            "Check Fenix updates"
        };
        for width in [750.0, 1280.0] {
            let mut ui =
                iced_test::Simulator::with_size(settings(), Size::new(width, 1300.0), app.view());
            ui.find(if language == Language::De {
                "Patch-Version: 0.1.0-preview.11"
            } else {
                "Patch version: 0.1.0-preview.11"
            })?;
            ui.click(title)?;
            assert!(matches!(
                ui.into_messages().next(),
                Some(Message::Action(Action::Fenix("check-update")))
            ));
        }
        let request = app
            .request(&Action::Fenix("check-update"))
            .expect("background check");
        assert_eq!(request.path, "fenix/check-update");
        assert!(app.request(&Action::Fenix("install")).is_some());
        app.snapshot.get_mut("status").expect("status")["game"]["state"] = json!("running");
        app.snapshot.get_mut("fenix").expect("fenix")["can_change"] = json!(false);
        assert!(app.request(&Action::Fenix("check-update")).is_some());
        assert!(app.request(&Action::Fenix("install")).is_none());
        app.snapshot.get_mut("fenix").expect("fenix")["runtime_path"] = json!("/synthetic/other");
        assert!(app.request(&Action::Fenix("check-update")).is_none());
        assert!(app.request(&Action::Fenix("install")).is_none());
    }
    Ok(())
}

#[test]
fn local_installations_show_the_available_version_and_an_idle_migration_action()
-> Result<(), Box<dyn std::error::Error>> {
    for language in [Language::De, Language::En] {
        let mut app = fixture(language, "available");
        app.snapshot.insert("fenix",json!({"runtime_path":"/synthetic/msfs2024", "state":"legacy", "installed":false,
            "migration_required":true,"version":"0.1.0","can_change":true,"can_check_updates":true,"patch_update":{"state":"complete"}}));
        let title = if language == Language::De {
            "Einrichtung übernehmen & aktualisieren"
        } else {
            "Adopt setup & update"
        };
        let mut ui =
            iced_test::Simulator::with_size(settings(), Size::new(750.0, 1300.0), app.view());
        ui.find(if language == Language::De {
            "Verfügbarer Patch: 0.1.0"
        } else {
            "Available patch: 0.1.0"
        })?;
        assert!(
            ui.find(if language == Language::De {
                "Patch-Version: 0.1.0"
            } else {
                "Patch version: 0.1.0"
            })
            .is_err()
        );
        ui.click(title)?;
        assert!(matches!(
            ui.into_messages().next(),
            Some(Message::Action(Action::Fenix("migrate")))
        ));
        assert_eq!(
            app.request(&Action::Fenix("migrate"))
                .expect("migration action")
                .path,
            "fenix/migrate"
        );
        app.snapshot.get_mut("fenix").expect("Fenix")["automatic_adoption_paused"] = json!(true);
        let mut paused =
            iced_test::Simulator::with_size(settings(), Size::new(750.0, 1300.0), app.view());
        paused.find(if language==Language::De {"Nach der Wiederherstellung ist die automatische Übernahme pausiert. Du kannst sie hier erneut starten."}else{"Automatic adoption is paused after restoration. You can start it again here."})?;
        drop(paused);
        assert!(app.request(&Action::Fenix("migrate")).is_some());
        app.page = Page::Overview;
        let mut overview =
            iced_test::Simulator::with_size(settings(), Size::new(750.0, 1300.0), app.view());
        overview.find(format!("Fenix: {title}"))?;
        overview.find("●")?;
        drop(overview);
        app.snapshot.get_mut("status").expect("status")["game"]["state"] = json!("running");
        assert!(app.request(&Action::Fenix("migrate")).is_none());
        app.snapshot.get_mut("status").expect("status")["game"]["state"] = json!("stopped");
        app.snapshot.get_mut("fenix").expect("Fenix")["runtime_path"] = json!("/different/runtime");
        assert!(app.request(&Action::Fenix("migrate")).is_none());
    }
    Ok(())
}

#[test]
fn failed_or_incompatible_checks_do_not_claim_the_patch_is_current()
-> Result<(), Box<dyn std::error::Error>> {
    for language in [Language::De, Language::En] {
        for state in ["failed", "requires-launcher"] {
            let app = fixture(language, state);
            let mut ui =
                iced_test::Simulator::with_size(settings(), Size::new(750.0, 1300.0), app.view());
            assert!(
                ui.find(if language == Language::De {
                    "Der Fenix-Patch ist aktuell."
                } else {
                    "The Fenix patch is up to date."
                })
                .is_err()
            );
            if state == "requires-launcher" {
                ui.find(if language == Language::De {
                    "Für den neuesten Fenix-Patch ist zuerst ein Flightdeck-Update erforderlich."
                } else {
                    "Update Flightdeck first to use the latest Fenix patch."
                })?;
            }
            assert!(app.request(&Action::Fenix("install")).is_none());
            assert!(app.request(&Action::Fenix("check-update")).is_some());
        }
    }
    Ok(())
}

#[test]
#[ignore = "writes independent Fenix update states with the production software renderer"]
fn capture_fenix_update_states() -> Result<(), Box<dyn std::error::Error>> {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/native-ui");
    std::fs::create_dir_all(&directory)?;
    for language in [Language::De, Language::En] {
        for state in [
            "available",
            "checking",
            "failed",
            "requires-launcher",
            "legacy",
            "legacy-overview",
            "legacy-paused",
        ] {
            for width in [750, 1280] {
                let app = iced::application(
                    move || fixture(language, state),
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
                        "fenix-updates-{state}-{}-{width}.png",
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
    Ok(())
}
