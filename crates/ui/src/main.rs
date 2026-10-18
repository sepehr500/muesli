//! `muesli` — the GTK4/libadwaita notes app.
//!
//! Single instance (`dev.muesli.Muesli`); `muesli --meeting <id>` opens that meeting,
//! forwarding to the running instance if there is one.

mod daemon;
mod icons;
mod settings;
mod widgets;
mod window;

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use muesli_ui::vm;
use std::cell::RefCell;
use std::rc::Rc;
use window::Win;

const APP_ID: &str = "dev.muesli.Muesli";
const STYLE: &str = include_str!("style.css");
const DARK: &str = include_str!("dark.css");

/// `--meeting <id>` / `--meeting=<id>`.
fn meeting_arg(args: &[String]) -> Option<String> {
    args.iter().enumerate().find_map(|(i, a)| match a.strip_prefix("--meeting=") {
        Some(v) => Some(v.to_string()),
        None if a == "--meeting" => args.get(i + 1).cloned(),
        None => None,
    })
}

fn has_family(widget: &impl IsA<gtk::Widget>, name: &str) -> bool {
    widget.pango_context().list_families().iter().any(|f| f.name().eq_ignore_ascii_case(name))
}

fn load_css(win: &Win) {
    let Some(display) = gdk::Display::default() else { return };
    let base = gtk::CssProvider::new();
    base.load_from_string(STYLE);
    gtk::style_context_add_provider_for_display(&display, &base, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);

    // Inter Tight missing: its fallback (Inter/Adwaita Sans) is wider, so tighten a bit more.
    if !has_family(&win.window, "Inter Tight") {
        let tight = gtk::CssProvider::new();
        tight.load_from_string(
            "textview.title { letter-spacing: -2.05px; } .brand { letter-spacing: -0.95px; } \
             .empty-title { letter-spacing: -1.5px; }",
        );
        gtk::style_context_add_provider_for_display(&display, &tight, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1);
    }

    let dark = gtk::CssProvider::new();
    dark.load_from_string(DARK);
    let apply = move |sm: &adw::StyleManager| match sm.is_dark() {
        true => gtk::style_context_add_provider_for_display(&display, &dark, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 2),
        false => gtk::style_context_remove_provider_for_display(&display, &dark),
    };
    let sm = adw::StyleManager::default();
    apply(&sm);
    sm.connect_dark_notify(apply);
}

/// Dev aid: render the window offscreen to a PNG (used with the Broadway backend, so
/// screenshots never involve the compositor).
fn snapshot_png(window: &adw::ApplicationWindow, path: &str) -> Result<(), glib::Error> {
    let paintable = gtk::WidgetPaintable::new(Some(window));
    let (w, h) = (window.width() as f64, window.height() as f64);
    let snap = gtk::Snapshot::new();
    paintable.snapshot(&snap, w, h);
    let node = snap.to_node().ok_or_else(|| glib::Error::new(glib::FileError::Failed, "empty snapshot"))?;
    let renderer = gtk::gsk::CairoRenderer::new();
    renderer.realize_for_display(&WidgetExt::display(window))?;
    let texture = renderer.render_texture(&node, None);
    renderer.unrealize();
    texture.save_to_png(path).map_err(|e| glib::Error::new(glib::FileError::Failed, &e.to_string()))
}

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE).build();
    let win: Rc<RefCell<Option<Rc<Win>>>> = Rc::default();

    app.connect_command_line(move |app, cmd| {
        let args: Vec<String> = cmd.arguments().iter().map(|a| a.to_string_lossy().into_owned()).collect();
        let existing = win.borrow().clone();
        let w = existing.unwrap_or_else(|| {
            let w = Win::new(app);
            load_css(&w);
            *win.borrow_mut() = Some(w.clone());
            w
        });
        if let Some(id) = meeting_arg(&args) {
            w.select(&id);
        }
        if let Ok(what) = std::env::var("MUESLI_UI_DEMO") {
            w.demo(&what);
        }
        w.window.present();
        w.settle_focus();
        if let Ok(path) = std::env::var("MUESLI_UI_SHOT") {
            let w = w.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(1500), move || {
                if let Err(e) = snapshot_png(&w.window, &path) {
                    eprintln!("screenshot failed: {e}");
                }
                if let Some(app) = w.window.application() {
                    app.quit();
                }
            });
        }
        glib::ExitCode::SUCCESS
    });
    app.run()
}

#[cfg(test)]
mod tests {
    use super::meeting_arg;

    #[test]
    fn parses_meeting_arg() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(meeting_arg(&a(&["muesli", "--meeting", "abc"])), Some("abc".into()));
        assert_eq!(meeting_arg(&a(&["muesli", "--meeting=xyz"])), Some("xyz".into()));
        assert_eq!(meeting_arg(&a(&["muesli"])), None);
    }
}
