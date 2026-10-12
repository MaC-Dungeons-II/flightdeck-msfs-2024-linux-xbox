//! A redraw after hovering or scrolling must leave the same pixels as drawing
//! the window from scratch. The window compositor repaints only the regions the
//! renderer reports as changed, which the snapshot tests never exercise.
mod common;
use flightdeck_ui::{App, Language, Message, Page, settings, theme};
use iced::advanced::{clipboard, graphics::Viewport, graphics::damage, renderer};
use iced::{Event, Point, Rectangle, Size, mouse};
use iced_runtime::{UserInterface, user_interface::Cache};
use iced_tiny_skia::Layer;
use serde_json::json;

// Hard-wrapped list items, as GitHub returns them for a Flightdeck release.
const NOTES: &str = "Stable release with full installer and corresponding source archives.\nExisting launchers can use the in-app updater.\n\n- End the remaining processes of an interrupted or stopped session instead of\n  fencing the launch until a reboot. **Check session** and the automatic\n  post-exit release end processes bound to the runtime, verify they are gone\n  and back up local saves.\n- End all Fenix processes with the simulator, including the Fenix app and its\n  WebView. An open Fenix app previously kept the next launch blocked.\n- Leave another Windows application in the profile, such as an installer,\n  running when the simulator exits and name it as the reason for the block;\n  **Check session** ends it on request.\n- Name a surviving process with its PID in the launch note and in diagnostics\n  (`cloud_sync.error_details`). A process stuck in the kernel after a GPU device\n  loss still requires a restart, and the message now says so.\n- Ignore processes bound to a Wine prefix that no longer exists; they previously\n  blocked the launch without a stated reason.\n- Use Fenix patch **0.1.0-preview.5**. Its Flightdeck and earlier Proton\n  overlays are identical to preview.4; verified preview.4 installations update\n  without reinstalling Fenix.\n";

/// Presents frames like `iced_tiny_skia::window::compositor::present`.
struct Window {
    renderer: iced::Renderer,
    cache: Option<Cache>,
    viewport: Viewport,
    pixels: tiny_skia::Pixmap,
    mask: tiny_skia::Mask,
    previous: Option<Vec<Layer>>,
}

impl Window {
    fn new(width: u32, height: u32, scale: f32) -> Self {
        let settings = settings();
        Self {
            renderer: iced_tiny_skia::Renderer::new(
                settings.default_font,
                settings.default_text_size,
            ),
            cache: Some(Cache::default()),
            viewport: Viewport::with_physical_size(Size::new(width, height), scale),
            pixels: tiny_skia::Pixmap::new(width, height).expect("pixels"),
            mask: tiny_skia::Mask::new(width, height).expect("mask"),
            previous: None,
        }
    }
    /// Returns how many presented pixels differ from a complete redraw.
    fn present(&mut self, app: &App, events: &[Event], cursor: mouse::Cursor) -> usize {
        let theme = theme();
        let background = theme.palette().background;
        let mut interface = UserInterface::build(
            app.view(),
            self.viewport.logical_size(),
            self.cache.take().expect("cache"),
            &mut self.renderer,
        );
        let mut events = events.to_vec();
        events.push(Event::Window(iced::window::Event::RedrawRequested(
            std::time::Instant::now(),
        )));
        let mut messages: Vec<Message> = Vec::new();
        let _ = interface.update(
            &events,
            cursor,
            &mut self.renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        interface.draw(
            &mut self.renderer,
            &theme,
            &renderer::Style {
                text_color: theme.palette().text,
            },
            cursor,
        );
        self.cache = Some(interface.into_cache());
        let window = Rectangle::with_size(self.viewport.logical_size());
        let layers = self.renderer.layers().to_vec();
        let changed = match &self.previous {
            Some(previous) => {
                damage::diff(previous, &layers, |layer| vec![layer.bounds], Layer::damage)
            }
            None => vec![window],
        };
        if !changed.is_empty() {
            self.renderer.draw(
                &mut self.pixels.as_mut(),
                &mut self.mask,
                &self.viewport,
                &damage::group(changed, window),
                background,
            );
        }
        self.previous = Some(layers);
        let (width, height) = (self.pixels.width(), self.pixels.height());
        let mut complete = tiny_skia::Pixmap::new(width, height).expect("pixels");
        let mut mask = tiny_skia::Mask::new(width, height).expect("mask");
        self.renderer.draw(
            &mut complete.as_mut(),
            &mut mask,
            &self.viewport,
            &[window],
            background,
        );
        self.pixels
            .pixels()
            .iter()
            .zip(complete.pixels())
            .filter(|(presented, expected)| presented != expected)
            .count()
    }
}

#[test]
fn hovered_and_scrolled_release_notes_repaint_without_stale_text() {
    let mut app = common::localized_fixture(Language::De);
    app.page = Page::Updates;
    let update = app
        .snapshot
        .get_mut("launcher-update")
        .expect("launcher update");
    update["notes"] = json!(NOTES);
    update["release_url"] = json!("https://example.com/releases/1");
    for scale in [1.0, 1.5] {
        let mut window = Window::new((1280.0 * scale) as u32, (900.0 * scale) as u32, scale);
        assert_eq!(window.present(&app, &[], mouse::Cursor::Unavailable), 0);
        let notes = Point::new(700.0, 620.0);
        let page = Point::new(700.0, 150.0);
        // Scrolling the notes and then the page nests two translations.
        for (step, position, lines) in [
            ("hover notes", notes, 0.0),
            ("scroll notes", notes, -2.0),
            ("scroll notes further", notes, -3.0),
            ("hover page", page, 0.0),
            ("scroll page", page, -3.0),
            (
                "hover notes on the scrolled page",
                Point::new(700.0, 500.0),
                0.0,
            ),
            ("scroll page back", page, 2.0),
        ] {
            let mut events = vec![Event::Mouse(mouse::Event::CursorMoved { position })];
            if lines != 0.0 {
                events.push(Event::Mouse(mouse::Event::WheelScrolled {
                    delta: mouse::ScrollDelta::Lines { x: 0.0, y: lines },
                }));
            }
            let cursor = mouse::Cursor::Available(position);
            // The frame after an event is the one a still pointer leaves behind.
            for frame in ["first", "next"] {
                assert_eq!(
                    window.present(&app, &events, cursor),
                    0,
                    "{step}, {frame} frame at scale {scale}"
                );
                events.clear();
            }
        }
    }
}
