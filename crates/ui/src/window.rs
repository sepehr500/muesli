//! The main window: a thin GTK shell over `vm` + `store` + daemon IPC.

use crate::daemon::{self, Msg};
use crate::icons::{self, IconView};
use crate::settings;
use crate::vm::{self, FooterKind, Indicator, TitleAction};
use crate::widgets::*;
use adw::prelude::*;
use chrono::Local;
use gtk::{gdk, gio, glib};
use muesli_core::ipc::{Request, SessionView, Status};
use muesli_core::model::{EnhancedNotes, Meeting, MeetingState, Notes, Segment};
use muesli_core::openrouter::Message;
use muesli_core::{config, prompts, store, transcript};
use muesli_audio::playback::Player;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;

const COLUMN: i32 = 688;

fn dbg_on() -> bool {
    std::env::var_os("MUESLI_UI_DEBUG").is_some()
}

macro_rules! dbg_log {
    ($($t:tt)*) => {
        if dbg_on() {
            eprintln!("[dbg {}] {}", chrono::Local::now().format("%H:%M:%S%.3f"), format!($($t)*));
        }
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Mine,
    Enhanced,
}

#[derive(Default)]
struct State {
    meetings: Vec<Meeting>,
    fts: Vec<String>,
    query: String,
    search_gen: u64,
    selected: Option<String>,
    current: Option<Meeting>,
    notes: Option<Notes>,
    status: Option<Status>,
    skipped: HashSet<String>,
    mode: Option<(String, Mode)>,
    /// Programmatic widget updates in progress: ignore change signals.
    loading: bool,
    notes_for: Option<String>,
    /// The user typed in the title since it was last loaded.
    title_dirty: bool,
    notes_dirty: bool,
    notes_gen: u64,
    /// Switch to Enhanced once notes newer than this land.
    awaiting_enhance: Option<(String, i64)>,
}

struct FooterUi {
    root: gtk::Box,
    icon: IconView,
    title: gtk::Label,
    sub: gtk::Label,
    actions: gtk::Box,
}

pub struct Win {
    pub window: adw::ApplicationWindow,
    db: rusqlite::Connection,
    st: RefCell<State>,
    toasts: adw::ToastOverlay,
    // sidebar
    list: gtk::Box,
    search: gtk::Text,
    plus_pop: gtk::Box,
    footer: FooterUi,
    // main
    split: adw::OverlaySplitView,
    pages: gtk::Stack,
    top_left: gtk::Box,
    top_right: gtk::Box,
    /// "Your notes": edit what you typed (on) or read the finished notes (off).
    btn_mine: gtk::ToggleButton,
    enhance_btn: gtk::Button,
    column: gtk::Box,
    meta: gtk::Box,
    eyebrow: gtk::Label,
    title_wrap: gtk::Box,
    title: gtk::TextView,
    naming: gtk::Box,
    chips: adw::WrapBox,
    body: gtk::Stack,
    enhanced: gtk::Box,
    notes_view: gtk::TextView,
    notes_placeholder: gtk::Label,
    wrapping: gtk::Box,
    // ask
    ask_root: gtk::Widget,
    ask_entry: gtk::Text,
    ask_model: gtk::Label,
    ask_models: gtk::Box,
    thread_rev: gtk::Revealer,
    thread: gtk::Box,
    thread_scroll: gtk::ScrolledWindow,
    // transcript
    drawer_body: gtk::Box,
    /// Dev aid: widget to screenshot instead of the window (e.g. an open popover).
    pub demo_target: RefCell<Option<gtk::Widget>>,
    // playback
    drawer_scroll: gtk::ScrolledWindow,
    player_bar: gtk::Box,
    player_icon: IconView,
    player_time: gtk::Label,
    player_scale: gtk::Scale,
    /// Player for the meeting it was opened for.
    player: RefCell<Option<(String, Rc<Player>)>>,
    /// Rendered transcript rows and their widgets (for seeking, highlighting, sources).
    rows: RefCell<Vec<(vm::TranscriptRow, gtk::Box)>>,
    playing_row: Cell<Option<usize>>,
    ticking: Cell<bool>,
    /// Row to keep in view while the drawer finishes laying out, and a generation counter
    /// so an old request's expiry doesn't cancel a newer one.
    scroll_target: Cell<Option<usize>>,
    scroll_gen: Cell<u64>,
}

fn text_of(view: &gtk::TextView) -> String {
    let b = view.buffer();
    b.text(&b.start_iter(), &b.end_iter(), false).to_string()
}

fn tview(classes: &[&str]) -> gtk::TextView {
    let t = gtk::TextView::new();
    t.set_wrap_mode(gtk::WrapMode::WordChar);
    t.set_css_classes(classes);
    t.set_hexpand(true);
    t
}

impl Win {
    pub fn new(app: &adw::Application) -> Rc<Win> {
        let db = store::open_default().expect("open muesli database");
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Muesli")
            .default_width(1280)
            .default_height(820)
            .width_request(860)
            .height_request(560)
            .build();
        window.add_css_class("muesli");

        // ---------------------------------------------------------------- sidebar
        let plus_pop = vbox(10, &[]);
        let plus = gtk::MenuButton::new();
        plus.set_child(Some(&IconView::new(&icons::PLUS)));
        plus.set_css_classes(&["boxed-icon"]);
        plus.set_valign(gtk::Align::End);
        plus.set_tooltip_text(Some("New recording"));
        let pop = gtk::Popover::new();
        pop.add_css_class("muesli-pop");
        pop.set_child(Some(&plus_pop));
        plus.set_popover(Some(&pop));

        let brand = with(vbox(10, &[]), &[IconView::new(&icons::LOGO).upcast_ref(), label("muesli", &["brand"]).upcast_ref()]);
        brand.set_hexpand(true);
        brand.set_halign(gtk::Align::Start);
        let brand_row = with(hbox(0, &["brand-box"]), &[brand.upcast_ref(), plus.upcast_ref()]);

        let search = gtk::Text::new();
        search.set_placeholder_text(Some("Search meetings"));
        search.set_hexpand(true);
        search.set_width_chars(1);
        let search_box = with(
            hbox(8, &["search"]),
            &[IconView::new(&icons::SEARCH).upcast_ref(), search.upcast_ref(), label("/", &["kbd"]).upcast_ref()],
        );

        let list = vbox(0, &["meeting-list"]);
        let list_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&list)
            .build();
        let top = with(vbox(0, &["sidebar-top"]), &[brand_row.upcast_ref(), search_box.upcast_ref(), list_scroll.upcast_ref()]);
        top.set_vexpand(true);

        let footer = FooterUi {
            root: hbox(10, &["footer"]),
            icon: IconView::new(&icons::LISTENING),
            title: ellipsized("", &["footer-title"]),
            sub: ellipsized("", &["footer-sub"]),
            actions: hbox(4, &[]),
        };
        let footer_text = with(vbox(1, &[]), &[footer.title.upcast_ref(), footer.sub.upcast_ref()]);
        footer_text.set_hexpand(true);
        footer_text.set_valign(gtk::Align::Center);
        let settings_btn = gtk::Button::new();
        settings_btn.set_child(Some(&IconView::new(&icons::SLIDERS)));
        settings_btn.set_css_classes(&["flat-icon"]);
        settings_btn.set_tooltip_text(Some("Settings"));
        footer.actions.set_valign(gtk::Align::Center);
        [footer.icon.upcast_ref::<gtk::Widget>(), footer_text.upcast_ref(), footer.actions.upcast_ref(), settings_btn.upcast_ref()]
            .iter()
            .for_each(|w| footer.root.append(*w));

        let sidebar = with(
            vbox(0, &["sidebar"]),
            &[top.upcast_ref(), IconView::sized(&icons::VIGNETTE, 272, 132).upcast_ref(), footer.root.upcast_ref()],
        );
        sidebar.set_size_request(272, -1);
        sidebar.set_hexpand(false);

        // ---------------------------------------------------------------- top bar
        let btn_mine = gtk::ToggleButton::new();
        btn_mine.set_child(Some(&with(hbox(8, &[]), &[IconView::new(&icons::PENCIL).upcast_ref(), label("Your notes", &[]).upcast_ref()])));
        btn_mine.set_css_classes(&["outline"]);
        btn_mine.set_tooltip_text(Some("Edit the notes you typed"));
        let seg = with(hbox(0, &[]), &[btn_mine.upcast_ref()]);
        seg.set_valign(gtk::Align::Center);
        let enhance_btn = icon_button(&icons::SPARKLE, Some("Enhance"), &["outline", "accent"]);
        enhance_btn.set_valign(gtk::Align::Center);
        let top_left = with(hbox(12, &[]), &[seg.upcast_ref(), enhance_btn.upcast_ref()]);
        top_left.set_hexpand(true);

        let transcript_btn = gtk::ToggleButton::new();
        transcript_btn.set_child(Some(&with(
            hbox(8, &[]),
            &[IconView::new(&icons::WAVEFORM).upcast_ref(), label("Transcript", &[]).upcast_ref()],
        )));
        transcript_btn.set_css_classes(&["outline"]);
        transcript_btn.set_valign(gtk::Align::Center);
        let copy_btn = icon_button(&icons::COPY, Some("Copy"), &["outline"]);
        copy_btn.set_valign(gtk::Align::Center);
        let menu = gio::Menu::new();
        menu.append(Some("Rename"), Some("win.rename"));
        menu.append(Some("Copy transcript"), Some("win.copy-transcript"));
        menu.append(Some("Re-enhance notes"), Some("win.enhance"));
        menu.append(Some("Reprocess recording"), Some("win.reprocess"));
        let danger = gio::Menu::new();
        danger.append(Some("Delete meeting…"), Some("win.delete"));
        menu.append_section(None, &danger);
        let more = gtk::MenuButton::new();
        more.set_child(Some(&IconView::new(&icons::ELLIPSIS)));
        more.set_css_classes(&["flat-icon"]);
        more.set_menu_model(Some(&menu));
        more.set_valign(gtk::Align::Center);
        let top_right = with(hbox(8, &[]), &[transcript_btn.upcast_ref(), copy_btn.upcast_ref(), more.upcast_ref()]);
        let topbar = with(hbox(0, &["topbar"]), &[top_left.upcast_ref(), top_right.upcast_ref()]);
        let handle = gtk::WindowHandle::new();
        handle.set_child(Some(&topbar));

        // ---------------------------------------------------------------- meeting column
        let meta = hbox(20, &[]);
        let eyebrow = label("NAME THIS MEETING", &["eyebrow"]);
        let title = tview(&["title"]);
        title.set_accepts_tab(false);
        let title_wrap = with(vbox(0, &["title-wrap"]), &[title.upcast_ref()]);

        let chips = adw::WrapBox::new();
        chips.set_child_spacing(8);
        chips.set_line_spacing(8);
        let suggestion_hint = with(
            hbox(6, &[]),
            &[IconView::sized(&icons::SPARKLE, 14, 14).upcast_ref(), label("Or pick a suggestion", &["hint"]).upcast_ref()],
        );
        let save = gtk::Button::new();
        save.set_child(Some(&with(hbox(10, &[]), &[label("Save", &[]).upcast_ref(), label("↵", &["key"]).upcast_ref()])));
        save.set_css_classes(&["primary"]);
        let skip = gtk::Button::with_label("Skip for now");
        skip.set_css_classes(&["skip"]);
        let buttons = with(hbox(16, &[]), &[save.upcast_ref(), skip.upcast_ref()]);
        buttons.set_margin_top(28);
        buttons.set_halign(gtk::Align::Start);
        let naming = with(vbox(12, &[]), &[suggestion_hint.upcast_ref(), chips.upcast_ref()]);
        naming.set_margin_top(20);
        let naming = with(vbox(0, &[]), &[naming.upcast_ref(), buttons.upcast_ref()]);

        let enhanced = vbox(0, &[]);
        let notes_view = tview(&["user-notes"]);
        notes_view.set_vexpand(true);
        notes_view.set_size_request(-1, 240);
        let notes_placeholder = para(
            "Jot down anything worth remembering. After the call, muesli turns these bullets and the transcript into clean notes.",
            &["notes-placeholder"],
        );
        notes_placeholder.set_valign(gtk::Align::Start);
        notes_placeholder.set_can_target(false);
        let mine = gtk::Overlay::new();
        mine.set_child(Some(&notes_view));
        mine.add_overlay(&notes_placeholder);
        let wrapping = vbox(0, &[]);
        wrapping.set_vexpand(true);
        let body = gtk::Stack::new();
        body.set_vhomogeneous(false);
        body.set_transition_type(gtk::StackTransitionType::Crossfade);
        body.set_transition_duration(150);
        body.set_vexpand(true);
        body.add_named(&enhanced, Some("enhanced"));
        body.add_named(&mine, Some("mine"));
        body.add_named(&wrapping, Some("wrapping"));

        let column = with(
            vbox(0, &["column"]),
            &[meta.upcast_ref(), eyebrow.upcast_ref(), title_wrap.upcast_ref(), naming.upcast_ref(), body.upcast_ref()],
        );
        let clamp = LeftClamp::new(COLUMN, &column);
        clamp.set_margin_start(96);
        clamp.set_margin_end(56);
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&clamp)
            .build();

        // ---------------------------------------------------------------- ask bar
        let ask_entry = gtk::Text::new();
        ask_entry.set_placeholder_text(Some("Ask about this meeting…"));
        ask_entry.set_hexpand(true);
        ask_entry.set_width_chars(1);
        let ask_model = label("", &["model"]);
        let ask_models = vbox(2, &["model-list"]);
        let ask_custom = gtk::Entry::new();
        ask_custom.set_placeholder_text(Some("Other OpenRouter model id"));
        ask_custom.set_width_chars(28);
        let ask_pop = gtk::Popover::new();
        ask_pop.set_position(gtk::PositionType::Top);
        ask_pop.set_child(Some(&with(
            vbox(8, &["model-pop"]),
            &[label("MODEL FOR QUESTIONS AND TITLES", &["section-label"]).upcast_ref(), ask_models.upcast_ref(), ask_custom.upcast_ref()],
        )));
        let ask_model_btn = gtk::MenuButton::new();
        ask_model_btn.set_child(Some(&with(hbox(4, &[]), &[ask_model.upcast_ref(), IconView::new(&icons::CHEVRON).upcast_ref()])));
        ask_model_btn.set_css_classes(&["model-picker"]);
        ask_model_btn.set_valign(gtk::Align::Center);
        ask_model_btn.set_tooltip_text(Some("Choose model"));
        ask_model_btn.set_popover(Some(&ask_pop));
        let send = gtk::Button::new();
        send.set_child(Some(&IconView::new(&icons::SEND)));
        send.set_css_classes(&["send"]);
        send.set_valign(gtk::Align::Center);
        send.set_tooltip_text(Some("Ask"));
        let ask_bar = with(
            hbox(12, &["ask"]),
            &[IconView::new(&icons::ASK).upcast_ref(), ask_entry.upcast_ref(), ask_model_btn.upcast_ref(), send.upcast_ref()],
        );
        let thread = vbox(0, &["thread-body"]);
        let thread_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(320)
            .child(&thread)
            .build();
        let thread_close = gtk::Button::new();
        thread_close.set_child(Some(&IconView::new(&icons::CLOSE)));
        thread_close.set_css_classes(&["flat-icon"]);
        let thread_head_label = label("ASKED ABOUT THIS MEETING", &["section-label"]);
        thread_head_label.set_hexpand(true);
        let thread_card = with(
            vbox(0, &["thread"]),
            &[with(hbox(0, &["thread-head"]), &[thread_head_label.upcast_ref(), thread_close.upcast_ref()]).upcast_ref(), thread_scroll.upcast_ref()],
        );
        let thread_rev = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideUp)
            .transition_duration(150)
            .child(&thread_card)
            .build();
        let ask_col = with(vbox(0, &[]), &[thread_rev.upcast_ref(), ask_bar.upcast_ref()]);
        let ask_clamp = LeftClamp::new(COLUMN, &ask_col);
        ask_clamp.set_margin_start(96);
        ask_clamp.set_margin_end(56);
        ask_clamp.set_margin_bottom(28);
        ask_clamp.set_valign(gtk::Align::End);
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&scroll));
        overlay.add_overlay(&ask_clamp);

        // ---------------------------------------------------------------- empty state
        let empty_title = label("Listening for calls.", &["empty-title"]);
        empty_title.set_margin_top(32);
        empty_title.set_halign(gtk::Align::Center);
        let empty_body = para("Join a Zoom or Discord call and muesli will take it from there.", &["empty-body"]);
        empty_body.set_margin_top(10);
        empty_body.set_justify(gtk::Justification::Center);
        empty_body.set_xalign(0.5);
        empty_body.set_max_width_chars(40);
        let empty = with(
            vbox(0, &[]),
            &[IconView::sized(&icons::VIGNETTE, 408, 198).upcast_ref(), empty_title.upcast_ref(), empty_body.upcast_ref()],
        );
        empty.set_valign(gtk::Align::Center);
        empty.set_halign(gtk::Align::Center);
        empty.set_margin_bottom(56);

        let pages = gtk::Stack::new();
        pages.set_transition_type(gtk::StackTransitionType::Crossfade);
        pages.set_transition_duration(150);
        pages.add_named(&overlay, Some("meeting"));
        pages.add_named(&empty, Some("empty"));
        pages.set_vexpand(true);

        let main = with(vbox(0, &[]), &[handle.upcast_ref(), pages.upcast_ref()]);
        main.set_hexpand(true);

        // ---------------------------------------------------------------- transcript drawer
        let drawer_close = gtk::Button::new();
        drawer_close.set_child(Some(&IconView::new(&icons::CLOSE)));
        drawer_close.set_css_classes(&["flat-icon"]);
        drawer_close.set_valign(gtk::Align::Center);
        let drawer_title = label("TRANSCRIPT", &["drawer-title"]);
        drawer_title.set_hexpand(true);
        let drawer_copy = gtk::Button::new();
        drawer_copy.set_child(Some(&IconView::new(&icons::COPY)));
        drawer_copy.set_css_classes(&["flat-icon"]);
        drawer_copy.set_valign(gtk::Align::Center);
        drawer_copy.set_tooltip_text(Some("Copy transcript"));
        let drawer_body = vbox(18, &["drawer-body"]);
        let drawer_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&drawer_body)
            .build();
        // Player: play/pause, scrubber, time.
        let player_icon = IconView::new(&icons::PLAY);
        let player_btn = gtk::Button::new();
        player_btn.set_child(Some(&player_icon));
        player_btn.set_css_classes(&["play"]);
        player_btn.set_valign(gtk::Align::Center);
        player_btn.set_tooltip_text(Some("Play recording"));
        let player_scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.1);
        player_scale.set_draw_value(false);
        player_scale.set_hexpand(true);
        player_scale.set_valign(gtk::Align::Center);
        player_scale.add_css_class("scrub");
        let player_time = label("00:00 / 00:00", &["ts", "play-time"]);
        let player_bar = with(hbox(10, &["player"]), &[player_btn.upcast_ref(), player_scale.upcast_ref(), player_time.upcast_ref()]);
        let drawer = with(
            vbox(0, &["drawer"]),
            &[
                with(hbox(0, &["drawer-head"]), &[drawer_title.upcast_ref(), drawer_copy.upcast_ref(), drawer_close.upcast_ref()]).upcast_ref(),
                drawer_scroll.upcast_ref(),
                player_bar.upcast_ref(),
            ],
        );
        let split = adw::OverlaySplitView::builder()
            .content(&main)
            .sidebar(&drawer)
            .sidebar_position(gtk::PackType::End)
            .collapsed(false)
            .show_sidebar(false)
            .min_sidebar_width(320.0)
            .max_sidebar_width(400.0)
            .sidebar_width_fraction(0.32)
            .build();
        split.set_hexpand(true);

        // Natural width = design width; long titles ellipsize instead of widening the sidebar.
        let sidebar = LeftClamp::new(273, &sidebar);
        sidebar.set_hexpand(false);
        let root = with(hbox(0, &[]), &[sidebar.upcast_ref(), split.upcast_ref()]);
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&root));
        window.set_content(Some(&toasts));

        let win = Rc::new(Win {
            window,
            db,
            st: RefCell::new(State::default()),
            toasts,
            list,
            search,
            plus_pop,
            footer,
            split,
            pages,
            top_left,
            top_right,
            btn_mine,
            enhance_btn,
            column,
            meta,
            eyebrow,
            title_wrap,
            title,
            naming,
            chips,
            body,
            enhanced,
            notes_view,
            notes_placeholder,
            wrapping,
            ask_root: ask_clamp.upcast(),
            ask_entry,
            ask_model,
            ask_models,
            thread_rev,
            thread,
            thread_scroll,
            drawer_body,
            demo_target: RefCell::default(),
            drawer_scroll,
            player_bar,
            player_icon,
            player_time,
            player_scale,
            player: RefCell::default(),
            rows: RefCell::default(),
            playing_row: Cell::new(None),
            ticking: Cell::new(false),
            scroll_target: Cell::new(None),
            scroll_gen: Cell::new(0),
        });

        // ---------------------------------------------------------------- wiring
        transcript_btn.bind_property("active", &win.split, "show-sidebar").bidirectional().sync_create().build();
        drawer_close.connect_clicked(glib::clone!(#[weak] transcript_btn, move |_| transcript_btn.set_active(false)));
        thread_close.connect_clicked(glib::clone!(#[strong] win, move |_| win.thread_rev.set_reveal_child(false)));
        settings_btn.connect_clicked(glib::clone!(#[strong] win, move |_| {
            let w = win.clone();
            settings::show(&win.window, move || w.render_ask_model());
        }));
        copy_btn.connect_clicked(glib::clone!(#[strong] win, move |_| win.copy()));
        drawer_copy.connect_clicked(glib::clone!(#[strong] win, move |_| win.copy_transcript()));
        // The user scrolling takes over from any pending automatic scroll.
        let user_scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        user_scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
        user_scroll.connect_scroll(glib::clone!(#[weak] win, #[upgrade_or] glib::Propagation::Proceed, move |_, _, _| {
            win.scroll_target.set(None);
            glib::Propagation::Proceed
        }));
        win.drawer_scroll.add_controller(user_scroll);
        // Layout changes (drawer opening, text re-wrapping) move rows: re-apply a pending scroll.
        win.drawer_scroll.vadjustment().connect_changed(glib::clone!(#[weak] win, move |_| {
            if let Some(i) = win.scroll_target.get() {
                win.apply_scroll(i);
            }
        }));
        player_btn.connect_clicked(glib::clone!(#[strong] win, move |_| win.toggle_play()));
        win.player_scale.connect_change_value(glib::clone!(#[strong] win, move |_, _, v| {
            win.seek_to(v, false);
            glib::Propagation::Proceed
        }));
        ask_custom.connect_activate(glib::clone!(#[strong] win, #[weak] ask_pop, move |e| {
            ask_pop.popdown();
            win.set_ask_model(&e.text());
            e.set_text("");
        }));
        save.connect_clicked(glib::clone!(#[strong] win, move |_| win.commit_title()));
        skip.connect_clicked(glib::clone!(#[strong] win, move |_| win.skip_naming()));
        win.enhance_btn.connect_clicked(glib::clone!(#[strong] win, move |_| win.enhance()));
        send.connect_clicked(glib::clone!(#[strong] win, move |_| win.ask()));
        win.ask_entry.connect_activate(glib::clone!(#[strong] win, move |_| win.ask()));
        let focus = gtk::EventControllerFocus::new();
        focus.connect_enter(glib::clone!(#[strong] win, move |_| {
            if win.thread.first_child().is_some() {
                win.thread_rev.set_reveal_child(true);
            }
        }));
        win.ask_entry.add_controller(focus);

        win.btn_mine.connect_toggled(glib::clone!(#[strong] win, move |b| {
            if !win.st.borrow().loading {
                win.set_mode(if b.is_active() { Mode::Mine } else { Mode::Enhanced });
            }
        }));

        win.search.connect_changed(glib::clone!(#[strong] win, move |e| win.on_search(e.text().to_string())));
        let search_keys = gtk::EventControllerKey::new();
        search_keys.connect_key_pressed(glib::clone!(#[strong] win, move |_, key, _, _| {
            match key {
                gdk::Key::Escape => {
                    win.search.set_text("");
                    win.unfocus();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        }));
        win.search.add_controller(search_keys);

        let title_keys = gtk::EventControllerKey::new();
        title_keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        title_keys.connect_key_pressed(glib::clone!(#[strong] win, move |_, key, _, _| {
            match key {
                gdk::Key::Return | gdk::Key::KP_Enter => {
                    win.commit_title();
                    glib::Propagation::Stop
                }
                gdk::Key::Escape => {
                    win.unfocus();
                    win.render_meeting(true);
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        }));
        win.title.add_controller(title_keys);
        let title_focus = gtk::EventControllerFocus::new();
        title_focus.connect_leave(glib::clone!(#[strong] win, move |_| {
            let (naming, dirty) = (win.naming.is_visible(), win.st.borrow().title_dirty);
            if !naming && dirty {
                win.commit_title();
            }
        }));
        win.title.add_controller(title_focus);
        win.title.buffer().connect_changed(glib::clone!(#[strong] win, move |_| {
            if !win.st.borrow().loading {
                win.st.borrow_mut().title_dirty = true;
            }
            win.sync_chip_selection();
        }));

        win.notes_view.buffer().connect_changed(glib::clone!(#[strong] win, move |b| win.on_notes_changed(b.char_count() == 0)));

        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(glib::clone!(#[strong] win, move |_, key, _, state| {
            let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
            match (key, ctrl) {
                (gdk::Key::slash, false) | (gdk::Key::k, true) | (gdk::Key::f, true) => {
                    win.search.grab_focus();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        }));
        win.window.add_controller(keys);

        win.install_actions();
        win.render_ask_model();
        win.render_footer();
        win.render_plus();
        win.reload_list();

        let (tx, rx) = async_channel::unbounded();
        daemon::spawn_watch(tx);
        glib::spawn_future_local(glib::clone!(#[strong] win, async move {
            while let Ok(msg) = rx.recv().await {
                win.on_daemon(msg);
            }
        }));
        glib::timeout_add_seconds_local(1, glib::clone!(#[strong] win, move || {
            let recording = matches!(win.st.borrow().status.as_ref().map(|s| &s.session), Some(SessionView::Recording { .. }));
            if recording {
                win.render_footer();
            }
            glib::ControlFlow::Continue
        }));
        if dbg_on() {
            win.window.connect_focus_widget_notify(|w| {
                dbg_log!("focus -> {}", GtkWindowExt::focus(w).map(|f| format!("{} {:?}", f.type_(), f.css_classes())).unwrap_or("none".into()));
            });
        }
        win.window.connect_close_request(glib::clone!(#[strong] win, move |_| {
            win.flush_notes();
            glib::Propagation::Proceed
        }));
        win
    }

    /// Initial focus: the title while naming, otherwise nothing (no stray focus ring).
    pub fn settle_focus(&self) {
        match self.naming.is_visible() && self.pages.visible_child_name().as_deref() == Some("meeting") {
            true => {
                self.title.grab_focus();
                let b = self.title.buffer();
                b.place_cursor(&b.end_iter());
            }
            false => self.unfocus(),
        }
    }

    /// Dev aid for screenshots (`MUESLI_UI_DEMO=mine|transcript|settings|plus|ask`).
    pub fn demo(self: &Rc<Self>, what: &str) {
        match what {
            "mine" => self.btn_mine.set_active(true),
            "transcript" => self.split.set_show_sidebar(true),
            "rename" => {
                self.split.set_show_sidebar(true);
                let (body, w) = (self.drawer_body.clone(), self.clone());
                glib::timeout_add_local_once(Duration::from_millis(400), move || {
                    let first = std::iter::successors(body.first_child(), |w| w.next_sibling())
                        .flat_map(|row| std::iter::successors(row.first_child(), |w| w.next_sibling()))
                        .flat_map(|head| std::iter::successors(head.first_child(), |w| w.next_sibling()))
                        .find_map(|w| w.downcast::<gtk::MenuButton>().ok());
                    if let Some(b) = first {
                        b.popup();
                        *w.demo_target.borrow_mut() = b.popover().map(|p| p.upcast());
                    }
                });
            }
            "models" => {
                let w = self.clone();
                glib::timeout_add_local_once(Duration::from_millis(400), move || {
                    let btn = w.ask_model.ancestor(gtk::MenuButton::static_type()).and_then(|b| b.downcast::<gtk::MenuButton>().ok());
                    if let Some(b) = btn {
                        b.popup();
                        *w.demo_target.borrow_mut() = b.popover().map(|p| p.upcast());
                    }
                });
            }
            "cite" => {
                if std::env::var_os("MUESLI_UI_DEMO_OPEN").is_some() {
                    self.split.set_show_sidebar(true);
                }
                let delay = std::env::var("MUESLI_UI_DEMO_CLICK_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(300);
                let (w, root) = (self.clone(), self.enhanced.clone());
                glib::timeout_add_local_once(Duration::from_millis(delay), move || {
                    fn find(w: &gtk::Widget) -> Option<gtk::Button> {
                        if w.has_css_class("source") {
                            return w.clone().downcast::<gtk::Button>().ok();
                        }
                        std::iter::successors(w.first_child(), |c| c.next_sibling()).find_map(|c| find(&c))
                    }
                    if let Some(b) = find(root.upcast_ref()) {
                        b.emit_clicked();
                    }
                    let w2 = w.clone();
                    glib::timeout_add_local_once(Duration::from_millis(900), move || w2.toggle_play());
                });
            }
            "seek" => {
                self.split.set_show_sidebar(true);
                let w = self.clone();
                glib::timeout_add_local_once(Duration::from_millis(300), move || w.seek_to(31.0, true));
            }
            "play" => {
                let w = self.clone();
                glib::timeout_add_local_once(Duration::from_millis(300), move || w.play_from(31.0));
                let w = self.clone();
                glib::timeout_add_local_once(Duration::from_millis(1400), move || w.toggle_play());
            }
            "settings" => {
                let w = self.clone();
                glib::timeout_add_local_once(Duration::from_millis(300), move || settings::show(&w.window, || {}));
            }
            "ask" => {
                self.ask_entry.set_text("What did we decide about Windows?");
                self.thread.append(&chat_label("user", "What did we decide about Windows?"));
                self.thread.append(&chat_label("assistant", "Drop the Windows beta for now — it's under 3% of signups. Revisit in Q1 if demand grows."));
                self.thread_rev.set_reveal_child(true);
            }
            _ => {}
        }
    }

    fn install_actions(self: &Rc<Self>) {
        let action = |name: &str, f: fn(&Rc<Win>)| {
            let a = gio::SimpleAction::new(name, None);
            let w = self.clone();
            a.connect_activate(move |_, _| f(&w));
            self.window.add_action(&a);
        };
        action("rename", |w| {
            w.title.grab_focus();
            let b = w.title.buffer();
            b.select_range(&b.start_iter(), &b.end_iter());
        });
        action("enhance", |w| w.enhance());
        action("copy-transcript", |w| w.copy_transcript());
        action("reprocess", |w| w.simple_request(|id| Request::Reprocess { id }, "Reprocessing…"));
        action("delete", |w| w.confirm_delete());
    }

    fn unfocus(&self) {
        GtkWindowExt::set_focus(&self.window, None::<&gtk::Widget>);
    }

    fn toast(&self, text: &str) {
        self.toasts.add_toast(adw::Toast::builder().title(text).timeout(3).build());
    }

    // ------------------------------------------------------------------ daemon events

    fn on_daemon(self: &Rc<Self>, msg: Msg) {
        dbg_log!("daemon msg: {}", match &msg {
            Msg::Status(_) => "status".to_string(),
            Msg::MeetingUpdated(id) => format!("meeting updated {id}"),
            Msg::Disconnected => "disconnected".to_string(),
        });
        match msg {
            Msg::Status(s) => {
                self.st.borrow_mut().status = Some(s);
                self.render_footer();
                self.render_plus();
                self.reload_list();
            }
            Msg::MeetingUpdated(id) => {
                self.reload_list();
                if self.st.borrow().selected.as_deref() == Some(&id) {
                    self.render_meeting(false);
                }
            }
            Msg::Disconnected => {
                let was = self.st.borrow_mut().status.take().is_some();
                if was {
                    self.render_footer();
                    self.render_plus();
                }
            }
        }
    }

    fn send(self: &Rc<Self>, req: Request, ok_toast: Option<&'static str>) {
        let w = self.clone();
        // Request kind only: payloads can contain meeting titles and names.
        dbg_log!("send {}", format!("{req:?}").split([' ', '{', '(']).next().unwrap_or(""));
        glib::spawn_future_local(async move {
            let res = daemon::request(req).await;
            dbg_log!("send result ok={}", res.is_ok());
            match res {
                Ok(_) => {
                    if let Some(t) = ok_toast {
                        w.toast(t);
                    }
                }
                Err(e) => w.toast(&e),
            }
        });
    }

    fn simple_request(self: &Rc<Self>, make: fn(String) -> Request, ok: &'static str) {
        if let Some(id) = self.st.borrow().selected.clone() {
            self.send(make(id), Some(ok));
        }
    }

    // ------------------------------------------------------------------ sidebar

    fn reload_list(self: &Rc<Self>) {
        let meetings = store::list_meetings(&self.db, 500).unwrap_or_default();
        let first = meetings.first().map(|m| m.id.clone());
        let selected_gone = {
            let mut st = self.st.borrow_mut();
            st.meetings = meetings;
            match &st.selected {
                Some(id) => !st.meetings.iter().any(|m| &m.id == id),
                None => true,
            }
        };
        self.render_sidebar();
        match (selected_gone, first) {
            (true, Some(id)) => self.select(&id),
            (true, None) => {
                self.st.borrow_mut().selected = None;
                self.render_meeting(true);
            }
            _ => {}
        }
    }

    fn render_sidebar(self: &Rc<Self>) {
        clear(&self.list);
        let st = self.st.borrow();
        let now = store::now_ms();
        let filtered = vm::filter_meetings(&st.meetings, &st.query, &st.fts);
        if filtered.is_empty() && !st.query.trim().is_empty() {
            self.list.append(&para("No meetings match.", &["list-empty"]));
        }
        vm::group_by_day(&filtered, &Local, now).into_iter().for_each(|g| {
            let group = vbox(2, &["group"]);
            group.append(&label(&g.label.to_uppercase(), &["group-label"]));
            g.meetings.iter().for_each(|m| group.append(&self.meeting_row(m, st.selected.as_deref() == Some(&m.id), now)));
            self.list.append(&group);
        });
    }

    fn meeting_row(self: &Rc<Self>, m: &Meeting, selected: bool, now: i64) -> gtk::Button {
        let icon = IconView::new(icons::for_app(m.app));
        icon.set_valign(gtk::Align::Start);
        icon.set_margin_top(1);
        let meta = ellipsized(&vm::row_meta(m, &Local, now), &["row-meta"]);
        if m.state == MeetingState::Recording {
            meta.add_css_class("recording");
        }
        let text = with(vbox(2, &[]), &[ellipsized(&m.display_title(), &["row-title"]).upcast_ref(), meta.upcast_ref()]);
        text.set_hexpand(true);
        let b = gtk::Button::new();
        b.set_child(Some(&with(hbox(10, &[]), &[icon.upcast_ref(), text.upcast_ref()])));
        b.set_css_classes(if selected { &["meeting-row", "selected"] } else { &["meeting-row"] });
        let id = m.id.clone();
        b.connect_clicked(glib::clone!(#[strong(rename_to = w)] self, move |_| w.select(&id)));
        b
    }

    fn on_search(self: &Rc<Self>, q: String) {
        let g = {
            let mut st = self.st.borrow_mut();
            st.query = q;
            st.search_gen += 1;
            st.search_gen
        };
        let w = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(220), move || {
            if w.st.borrow().search_gen != g {
                return;
            }
            let q = w.st.borrow().query.clone();
            let fts = store::search(&w.db, &q, 500).unwrap_or_default();
            w.st.borrow_mut().fts = fts;
            w.render_sidebar();
        });
    }

    pub fn select(self: &Rc<Self>, id: &str) {
        self.flush_notes();
        self.unfocus();
        self.st.borrow_mut().selected = Some(id.to_string());
        self.thread_rev.set_reveal_child(false);
        self.render_sidebar();
        self.render_meeting(true);
    }

    // ------------------------------------------------------------------ footer + plus

    fn render_footer(self: &Rc<Self>) {
        let st = self.st.borrow();
        let f = vm::footer(st.status.as_ref(), store::now_ms());
        self.footer.title.set_text(&f.title);
        self.footer.sub.set_text(&f.subtitle);
        self.footer.icon.set_icon(match f.kind {
            FooterKind::Listening | FooterKind::Prompting => &icons::LISTENING,
            FooterKind::Recording => &icons::RECORDING,
            FooterKind::Offline => &icons::OFFLINE,
        });
        match f.kind {
            FooterKind::Recording => self.footer.root.add_css_class("recording"),
            _ => self.footer.root.remove_css_class("recording"),
        }
        let kind_changed = self.footer.actions.widget_name() != format!("{:?}", f.kind);
        if !kind_changed {
            return;
        }
        self.footer.actions.set_widget_name(&format!("{:?}", f.kind));
        clear(&self.footer.actions);
        let mini = |text: &str, classes: &[&str], req: Request| {
            let b = gtk::Button::with_label(text);
            b.set_css_classes(classes);
            b.connect_clicked(glib::clone!(#[strong(rename_to = w)] self, move |_| w.send(req.clone(), None)));
            self.footer.actions.append(&b);
        };
        match f.kind {
            FooterKind::Prompting => {
                mini("Record", &["mini"], Request::Record);
                mini("Ignore", &["mini"], Request::Ignore);
            }
            FooterKind::Recording => mini("Stop", &["mini", "danger"], Request::Stop),
            _ => {}
        }
    }

    fn render_plus(self: &Rc<Self>) {
        clear(&self.plus_pop);
        let session = self.st.borrow().status.as_ref().map(|s| s.session.clone());
        let button = |text: &str, classes: &[&str], req: Request| {
            let b = gtk::Button::with_label(text);
            b.set_css_classes(classes);
            b.connect_clicked(glib::clone!(#[strong(rename_to = w)] self, move |b| {
                if let Some(p) = b.ancestor(gtk::Popover::static_type()).and_downcast::<gtk::Popover>() {
                    p.popdown();
                }
                w.send(req.clone(), None);
            }));
            b
        };
        let (title, sub) = match &session {
            None => ("Daemon not running", "Start muesli-daemon to record calls."),
            Some(SessionView::Recording { .. }) => ("Recording in progress", "Stop to process it, or discard it."),
            Some(_) => ("New recording", "Record what you hear and say, right now."),
        };
        self.plus_pop.append(&label(title, &["pop-title"]));
        let sub = para(sub, &["pop-sub"]);
        sub.set_max_width_chars(28);
        self.plus_pop.append(&sub);
        match &session {
            None => {}
            Some(SessionView::Recording { .. }) => {
                let row = with(
                    hbox(8, &[]),
                    &[button("Stop", &["primary"], Request::Stop).upcast_ref(), button("Discard", &["skip"], Request::Discard).upcast_ref()],
                );
                self.plus_pop.append(&row);
            }
            Some(_) => self.plus_pop.append(&button("Start recording now", &["primary"], Request::Start)),
        }
    }

    // ------------------------------------------------------------------ meeting view

    fn render_meeting(self: &Rc<Self>, full: bool) {
        let id = self.st.borrow().selected.clone();
        let m = id.as_deref().and_then(|id| store::get_meeting(&self.db, id).ok().flatten());
        let Some(m) = m else {
            self.pages.set_visible_child_name("empty");
            self.top_left.set_visible(false);
            self.top_right.set_visible(false);
            self.st.borrow_mut().current = None;
            return;
        };
        let notes = store::get_notes(&self.db, &m.id).unwrap_or(Notes {
            meeting_id: m.id.clone(),
            user_md: String::new(),
            enhanced: None,
            updated_at: 0,
        });
        let segs = store::segments(&self.db, &m.id).unwrap_or_default();
        self.pages.set_visible_child_name("meeting");
        self.top_left.set_visible(true);
        self.top_right.set_visible(true);

        let skipped = self.st.borrow().skipped.contains(&m.id);
        let naming = vm::shows_naming(&m, skipped);
        let wrapping = matches!(m.state, MeetingState::Processing | MeetingState::Failed);
        let has_enhanced = notes.enhanced.as_ref().is_some_and(|n| !n.sections.is_empty() || !n.action_items.is_empty());

        let mode = {
            let mut st = self.st.borrow_mut();
            let awaited = matches!(&st.awaiting_enhance, Some((aid, t)) if *aid == m.id && has_enhanced && notes.updated_at > *t);
            if awaited {
                st.awaiting_enhance = None;
            }
            let mode = match &st.mode {
                _ if !has_enhanced => Mode::Mine,
                Some((mid, mode)) if *mid == m.id && !awaited => *mode,
                _ => Mode::Enhanced,
            };
            st.mode = Some((m.id.clone(), mode));
            st.loading = true;
            mode
        };

        // Meta row.
        clear(&self.meta);
        let app = with(
            hbox(6, &[]),
            &[IconView::new(icons::for_app(m.app)).class("meta-app-icon").upcast_ref(), label(vm::app_label(m.app), &["meta-app"]).upcast_ref()],
        );
        self.meta.append(&app);
        let item = |icon: &'static icons::Icon, text: &str| {
            with(hbox(6, &[]), &[IconView::new(icon).upcast_ref(), label(text, &["meta"]).upcast_ref()])
        };
        self.meta.append(&item(&icons::CLOCK, &vm::time_range(&m, &Local)));
        if let Some(people) = vm::people_line(&segs, &m.speaker_names) {
            self.meta.append(&item(&icons::PEOPLE, &people));
        }

        // Title + naming chrome.
        self.eyebrow.set_visible(naming);
        self.naming.set_visible(naming);
        match naming {
            true => self.title_wrap.add_css_class("naming"),
            false => self.title_wrap.remove_css_class("naming"),
        }
        if full || !self.title.has_focus() {
            let text = if naming { vm::initial_title(&m) } else { m.display_title() };
            self.title.buffer().set_text(&text);
            self.st.borrow_mut().title_dirty = false;
            // GtkTextView validates its layout lazily and can miss the relayout when its
            // height grows from zero; nudge it once the layout is ready.
            let t = self.title.clone();
            glib::idle_add_local_once(move || t.queue_resize());
        }
        clear_wrap(&self.chips);
        vm::chips(&m).into_iter().for_each(|c| {
            let b = gtk::Button::with_label(&c.title);
            b.set_css_classes(if c.fallback { &["chip", "fallback"] } else { &["chip"] });
            b.set_widget_name(&c.title);
            b.connect_clicked(glib::clone!(#[strong(rename_to = w)] self, move |b| {
                w.title.buffer().set_text(&b.widget_name());
                w.title.grab_focus();
            }));
            self.chips.append(&b);
        });

        // Body.
        match wrapping {
            true => {
                self.column.add_css_class("wrapping");
                clear(&self.wrapping);
                self.wrapping.append(&self.wrapping_widget(&m));
                self.body.set_visible_child_name("wrapping");
            }
            false => {
                self.column.remove_css_class("wrapping");
                clear(&self.enhanced);
                match &notes.enhanced {
                    Some(n) if has_enhanced => self.enhanced.append(&self.enhanced_widget(n, &segs, has_audio(&m))),
                    _ => self.enhanced.append(&para(
                        "No notes yet. Write a few bullets, then press Enhance.",
                        &["empty-notes"],
                    )),
                }
                self.body.set_visible_child_name(if mode == Mode::Enhanced { "enhanced" } else { "mine" });
            }
        }
        let notes_owner_changed = self.st.borrow().notes_for.as_deref() != Some(&m.id);
        let dirty = self.st.borrow().notes_dirty;
        if full || notes_owner_changed || (!dirty && !self.notes_view.has_focus()) {
            self.notes_view.buffer().set_text(&notes.user_md);
            let t = self.notes_view.clone();
            glib::idle_add_local_once(move || t.queue_resize());
            let mut st = self.st.borrow_mut();
            st.notes_for = Some(m.id.clone());
            st.notes_dirty = false;
        }
        self.notes_placeholder.set_visible(self.notes_view.buffer().char_count() == 0);

        // Top bar.
        if let Some(seg) = self.btn_mine.parent() {
            seg.set_visible(!wrapping && has_enhanced);
        }
        self.btn_mine.set_active(mode == Mode::Mine);
        let can_enhance = !wrapping && mode == Mode::Mine && m.state == MeetingState::Ready;
        self.enhance_btn.set_visible(can_enhance);
        set_button_text(&self.enhance_btn, if has_enhanced { "Re-enhance" } else { "Enhance" });

        // Ask + transcript.
        self.ask_root.set_visible(!wrapping && m.state != MeetingState::Recording);
        if full {
            self.render_thread(&m.id);
        }
        self.render_transcript(&segs, &m);

        {
            let mut st = self.st.borrow_mut();
            st.current = Some(m.clone());
            st.notes = Some(notes);
            st.loading = false;
        }
        self.sync_chip_selection();
        if full && naming && self.window.is_visible() {
            self.settle_focus();
        }
    }

    fn set_mode(self: &Rc<Self>, mode: Mode) {
        let id = self.st.borrow().selected.clone();
        if let Some(id) = id {
            self.st.borrow_mut().mode = Some((id, mode));
            self.flush_notes();
            self.render_meeting(false);
        }
    }

    fn sync_chip_selection(&self) {
        let typed = text_of(&self.title);
        let mut child = self.chips.first_child();
        while let Some(c) = child {
            match c.widget_name().as_str() == typed.trim() {
                true => c.add_css_class("selected"),
                false => c.remove_css_class("selected"),
            }
            child = c.next_sibling();
        }
    }

    fn wrapping_widget(self: &Rc<Self>, m: &Meeting) -> gtk::Box {
        let root = vbox(0, &[]);
        root.append(&spacer());
        if m.state == MeetingState::Failed {
            let err = para(m.error.as_deref().unwrap_or("Processing stopped."), &["error-line"]);
            let retry = gtk::Button::with_label("Reprocess");
            retry.set_css_classes(&["outline"]);
            retry.set_halign(gtk::Align::Start);
            retry.connect_clicked(glib::clone!(#[strong(rename_to = w)] self, move |_| {
                w.simple_request(|id| Request::Reprocess { id }, "Reprocessing…")
            }));
            let fail = with(vbox(10, &[]), &[err.upcast_ref(), retry.upcast_ref()]);
            fail.set_margin_bottom(24);
            root.append(&fail);
        }
        let head_label = label("WRAPPING UP", &["section-label"]);
        head_label.set_hexpand(true);
        let summary = label(&vm::wrapping_summary(&m.pipeline), &["hint"]);
        let head = with(hbox(0, &["wrap-head"]), &[head_label.upcast_ref(), summary.upcast_ref()]);
        head.set_baseline_position(gtk::BaselinePosition::Center);
        root.append(&head);
        let rows = vm::wrapping_rows(&m.pipeline, m.duration_ms());
        let n = rows.len();
        rows.into_iter().enumerate().for_each(|(i, r)| {
            let tile = gtk::CenterBox::new();
            tile.add_css_class("tile");
            tile.set_valign(gtk::Align::Center);
            tile.set_hexpand(false);
            tile.set_center_widget(Some(&IconView::new(icons::for_step(r.kind))));
            let name = ellipsized(&r.label, &["wrap-label"]);
            name.set_size_request(168, -1);
            let detail = ellipsized(&r.detail, &["wrap-detail"]);
            detail.set_hexpand(true);
            let ind: gtk::Widget = match r.indicator {
                Indicator::Check => IconView::new(&icons::CHECK).class("indicator").upcast(),
                Indicator::Spinner => IconView::new(&icons::SPINNER).spinning().upcast(),
                Indicator::Failed => IconView::new(&icons::ALERT).class("indicator").upcast(),
                Indicator::None => gtk::Box::builder().width_request(16).build().upcast(),
            };
            let row = with(hbox(14, &["wrap-row"]), &[tile.upcast_ref(), name.upcast_ref(), detail.upcast_ref(), ind.upcast_ref()]);
            [(r.active, "active"), (r.pending, "pending"), (r.indicator == Indicator::Failed, "failed"), (i + 1 == n, "last")]
                .into_iter()
                .filter(|(on, _)| *on)
                .for_each(|(_, c)| row.add_css_class(c));
            root.append(&row);
        });
        root
    }

    /// Timestamp chip for a note's sources: opens the transcript at those lines (and plays
    /// from the first, when the audio exists).
    fn source_chip(self: &Rc<Self>, sources: &[usize], segs: &[Segment], audio: bool) -> Option<gtk::Button> {
        let t = segs.get(*sources.first()?)?.t_start;
        let chip = gtk::Button::with_label(&vm::timestamp(t));
        chip.set_css_classes(&["source"]);
        chip.set_valign(gtk::Align::Start);
        let what = if audio { "Play the moment this came from" } else { "Show where this came from" };
        chip.set_tooltip_text(Some(&match sources.len() {
            1 => what.to_string(),
            n => format!("{what} ({n} lines)"),
        }));
        let (w, sources) = (self.clone(), sources.to_vec());
        chip.connect_clicked(move |_| w.cite(&sources, t, audio));
        Some(chip)
    }

    /// Fixed-width column holding the source chip (empty when a point has no sources), so
    /// chips and owners line up down the page.
    fn chip_slot(self: &Rc<Self>, sources: &[usize], segs: &[Segment], audio: bool) -> gtk::Box {
        let slot = hbox(0, &["source-slot"]);
        slot.set_size_request(52, -1);
        slot.set_halign(gtk::Align::End);
        slot.set_valign(gtk::Align::Start);
        if let Some(c) = self.source_chip(sources, segs, audio) {
            c.set_halign(gtk::Align::End);
            c.set_hexpand(true);
            slot.append(&c);
        }
        slot
    }

    fn cite(self: &Rc<Self>, sources: &[usize], t: f64, audio: bool) {
        self.split.set_show_sidebar(true);
        let rows: Vec<usize> = {
            let rows: Vec<vm::TranscriptRow> = self.rows.borrow().iter().map(|(r, _)| r.clone()).collect();
            let mut v: Vec<usize> = sources.iter().filter_map(|&l| vm::row_of_line(&rows, l)).collect();
            v.dedup();
            v
        };
        dbg_log!("cite lines {sources:?} -> rows {rows:?}");
        self.mark_row(Some(rows.clone()), "cited");
        if let Some(&first) = rows.first() {
            self.scroll_to_row(first);
        }
        if audio {
            self.play_from(t);
        }
    }

    fn enhanced_widget(self: &Rc<Self>, n: &EnhancedNotes, segs: &[Segment], audio: bool) -> gtk::Box {
        let root = vbox(0, &[]);
        n.sections.iter().for_each(|s| {
            let sec = vbox(14, &["note-section"]);
            sec.append(&label(&s.title.to_uppercase(), &["section-label"]));
            s.items.iter().for_each(|it| {
                let dash = label("–", &["note-dash"]);
                dash.set_valign(gtk::Align::Start);
                let text: gtk::Widget = match (it.from_user, &it.detail) {
                    (true, Some(d)) => with(vbox(2, &[]), &[para(&it.text, &["note-text"]).upcast_ref(), para(d, &["note-detail"]).upcast_ref()]).upcast(),
                    (true, None) => para(&it.text, &["note-text"]).upcast(),
                    (false, Some(d)) => para(&vm::join_sentences(&it.text, d), &["note-ai"]).upcast(),
                    (false, None) => para(&it.text, &["note-ai"]).upcast(),
                };
                if !it.from_user {
                    dash.add_css_class("ai");
                }
                text.set_hexpand(true);
                let slot = self.chip_slot(&it.sources, segs, audio);
                sec.append(&with(hbox(12, &[]), &[dash.upcast_ref(), text.upcast_ref(), slot.upcast_ref()]));
            });
            root.append(&sec);
        });
        if !n.action_items.is_empty() {
            let sec = vbox(10, &["note-section", "actions"]);
            if n.sections.is_empty() {
                sec.remove_css_class("actions");
            }
            sec.append(&label("ACTION ITEMS", &["section-label"]));
            n.action_items.iter().enumerate().for_each(|(i, a)| {
                let check = gtk::CheckButton::new();
                check.set_css_classes(if a.from_user { &["todo", "mine"] } else { &["todo"] });
                check.set_active(a.done);
                check.set_valign(gtk::Align::Start);
                let text = para(&a.text, &["action-text"]);
                text.set_hexpand(true);
                if !a.from_user {
                    text.add_css_class("ai");
                }
                if a.done {
                    text.add_css_class("done");
                }
                let owner = label(a.owner.as_deref().unwrap_or(""), &["owner"]);
                owner.set_size_request(72, -1);
                owner.set_xalign(1.0);
                owner.set_valign(gtk::Align::Start);
                check.connect_toggled(glib::clone!(#[strong(rename_to = w)] self, #[weak] text, move |c| {
                    match c.is_active() {
                        true => text.add_css_class("done"),
                        false => text.remove_css_class("done"),
                    }
                    w.set_action_done(i, c.is_active());
                }));
                let slot = self.chip_slot(&a.sources, segs, audio);
                sec.append(&with(hbox(12, &[]), &[check.upcast_ref(), text.upcast_ref(), owner.upcast_ref(), slot.upcast_ref()]));
            });
            root.append(&sec);
        }
        root
    }

    fn set_action_done(&self, i: usize, done: bool) {
        let mut st = self.st.borrow_mut();
        let Some(notes) = st.notes.as_mut() else { return };
        let Some(enh) = notes.enhanced.as_mut() else { return };
        if let Some(a) = enh.action_items.get_mut(i) {
            a.done = done;
            let _ = store::set_enhanced(&self.db, &notes.meeting_id, Some(enh));
        }
    }

    fn render_transcript(self: &Rc<Self>, segs: &[Segment], m: &Meeting) {
        dbg_log!("render_transcript");
        // A player belongs to one meeting.
        if self.player.borrow().as_ref().is_some_and(|(id, _)| *id != m.id) {
            self.stop_player();
        }
        self.player_bar.set_visible(has_audio(m));
        clear(&self.drawer_body);
        self.rows.borrow_mut().clear();
        self.playing_row.set(None);
        let rows = vm::transcript_rows(segs, &m.speaker_names);
        if rows.is_empty() {
            let text = match m.state {
                MeetingState::Recording => "The transcript appears after the call.",
                MeetingState::Processing => "Transcribing…",
                _ => "No transcript for this meeting.",
            };
            self.drawer_body.append(&para(text, &["drawer-empty"]));
            return;
        }
        rows.iter().for_each(|r| {
            let who: gtk::Widget = match r.me {
                true => label(&r.speaker, &["speaker", "me"]).upcast(),
                false => self.speaker_button(&m.id, &r.speaker).upcast(),
            };
            let ts = gtk::Button::with_label(&r.time);
            ts.set_css_classes(&["ts-btn"]);
            ts.set_tooltip_text(Some("Play from here"));
            ts.set_visible(has_audio(m));
            let start = r.start;
            ts.connect_clicked(glib::clone!(#[strong(rename_to = w)] self, move |_| w.play_from(start)));
            let ts_label = label(&r.time, &["ts"]);
            ts_label.set_visible(!has_audio(m));
            let head = with(hbox(8, &[]), &[ts.upcast_ref(), ts_label.upcast_ref(), &who]);
            let text = para(&r.text, &["utterance"]);
            text.set_selectable(true);
            let row = with(vbox(4, &["t-row"]), &[head.upcast_ref(), text.upcast_ref()]);
            self.drawer_body.append(&row);
            self.rows.borrow_mut().push((r.clone(), row));
        });
        if let Some(p) = self.current_player() {
            self.highlight_playing(p.position());
        }
    }

    /// Speaker name that opens a rename popover. Renaming applies to every line from this
    /// person; reusing another speaker's name merges the two.
    fn speaker_button(self: &Rc<Self>, id: &str, speaker: &str) -> gtk::MenuButton {
        let entry = gtk::Entry::new();
        entry.set_text(speaker);
        entry.set_width_chars(22);
        entry.set_placeholder_text(Some("Name"));
        let save = gtk::Button::with_label("Rename");
        save.set_css_classes(&["primary", "small"]);
        let hint = para(
            &format!("Updates every line from {speaker}. Use another speaker's name to merge them."),
            &["rename-hint"],
        );
        hint.set_max_width_chars(30);
        let body = with(vbox(10, &["rename-pop"]), &[entry.upcast_ref(), hint.upcast_ref(), save.upcast_ref()]);
        let pop = gtk::Popover::new();
        pop.set_child(Some(&body));
        pop.connect_show(glib::clone!(#[weak] entry, move |_| {
            entry.grab_focus();
            entry.select_region(0, -1);
        }));
        let btn = gtk::MenuButton::new();
        btn.set_child(Some(&label(speaker, &[])));
        btn.set_css_classes(&["speaker-btn"]);
        btn.set_tooltip_text(Some("Rename speaker"));
        btn.set_popover(Some(&pop));
        if dbg_on() {
            let who = speaker.to_string();
            btn.connect_active_notify(move |b| dbg_log!("{who}: active={}", b.is_active()));
            pop.connect_show(|_| dbg_log!("popover show"));
            pop.connect_closed(|_| dbg_log!("popover closed"));
            if let Some(inner) = btn.first_child() {
                let click = gtk::GestureClick::new();
                click.set_propagation_phase(gtk::PropagationPhase::Capture);
                click.connect_pressed(|_, n, x, y| dbg_log!("press n={n} at {x:.0},{y:.0}"));
                click.connect_released(|_, n, x, y| dbg_log!("release n={n} at {x:.0},{y:.0}"));
                inner.add_controller(click);
            }
            entry.connect_activate(|_| dbg_log!("entry activate"));
            save.connect_clicked(|_| dbg_log!("save clicked"));
        }
        let commit = {
            let (w, id, from) = (self.clone(), id.to_string(), speaker.to_string());
            glib::clone!(#[weak] entry, #[weak] pop, move || {
                let to = entry.text().trim().to_string();
                pop.popdown();
                if to != from {
                    w.send(Request::RenameSpeaker { id: id.clone(), from: from.clone(), to }, Some("Speaker renamed"));
                }
            })
        };
        let c2 = commit.clone();
        entry.connect_activate(move |_| c2());
        save.connect_clicked(move |_| commit());
        btn
    }

    // ------------------------------------------------------------------ playback

    fn current_player(&self) -> Option<Rc<Player>> {
        self.player.borrow().as_ref().map(|(_, p)| p.clone())
    }

    fn stop_player(&self) {
        drop(self.player.take());
        self.player_icon.set_icon(&icons::PLAY);
        self.player_time.set_text("00:00 / 00:00");
        self.player_scale.set_range(0.0, 1.0);
        self.player_scale.set_value(0.0);
        self.mark_row(None, "playing");
    }

    /// Run `then` with a player for the current meeting, building its playback copy first if
    /// needed (the daemon does that from the raw recording; a few seconds per hour).
    fn with_player(self: &Rc<Self>, then: impl FnOnce(&Rc<Self>, &Rc<Player>) + 'static) {
        let Some(id) = self.st.borrow().current.as_ref().map(|m| m.id.clone()) else { return };
        if let Some(p) = self.current_player() {
            return then(self, &p);
        }
        self.player_time.set_text("Preparing audio…");
        let w = self.clone();
        glib::spawn_future_local(async move {
            let opened = daemon::request(Request::PreparePlayback { id: id.clone() })
                .await
                .and_then(|v| v["dir"].as_str().map(std::path::PathBuf::from).ok_or_else(|| "no playback".to_string()))
                .and_then(|dir| Player::open(&dir).map_err(|e| e.to_string()));
            // The user may have moved to another meeting meanwhile.
            if w.st.borrow().current.as_ref().map(|m| &m.id) != Some(&id) {
                return;
            }
            match opened {
                Ok(p) => {
                    let p = Rc::new(p);
                    w.player_scale.set_range(0.0, p.duration_s.max(0.1));
                    w.player_time.set_text(&vm::play_time(0.0, p.duration_s));
                    *w.player.borrow_mut() = Some((id, p.clone()));
                    then(&w, &p);
                }
                Err(e) => {
                    w.player_time.set_text("00:00 / 00:00");
                    w.toast(&format!("Can't play this meeting: {e}"));
                }
            }
        });
    }

    fn toggle_play(self: &Rc<Self>) {
        self.with_player(|w, p| match p.is_playing() {
            true => {
                p.pause();
                w.player_icon.set_icon(&icons::PLAY);
            }
            false => {
                p.play();
                w.start_ticking();
            }
        });
    }

    /// Open the transcript and play from meeting time `t`.
    fn play_from(self: &Rc<Self>, t: f64) {
        self.split.set_show_sidebar(true);
        self.with_player(move |w, p| {
            p.seek(t);
            p.play();
            w.player_scale.set_value(t);
            w.highlight_playing(t);
            w.start_ticking();
        });
    }

    fn seek_to(self: &Rc<Self>, t: f64, _from_code: bool) {
        self.with_player(move |w, p| {
            p.seek(t);
            w.player_scale.set_value(t);
            w.player_time.set_text(&vm::play_time(t, p.duration_s));
            w.highlight_playing(t);
        });
    }

    fn start_ticking(self: &Rc<Self>) {
        self.player_icon.set_icon(&icons::PAUSE);
        if self.ticking.replace(true) {
            return;
        }
        let w = self.clone();
        glib::timeout_add_local(Duration::from_millis(120), move || {
            let Some(p) = w.current_player() else {
                w.ticking.set(false);
                return glib::ControlFlow::Break;
            };
            let t = p.position();
            w.player_scale.set_value(t);
            w.player_time.set_text(&vm::play_time(t, p.duration_s));
            w.highlight_playing(t);
            match p.is_playing() {
                true => glib::ControlFlow::Continue,
                false => {
                    w.player_icon.set_icon(&icons::PLAY);
                    w.ticking.set(false);
                    glib::ControlFlow::Break
                }
            }
        });
    }

    /// Mark the row being played and keep it in view.
    fn highlight_playing(self: &Rc<Self>, t: f64) {
        let i = vm::row_at(&self.rows.borrow().iter().map(|(r, _)| r.clone()).collect::<Vec<_>>(), t);
        if i != self.playing_row.get() {
            self.playing_row.set(i);
            self.mark_row(i.map(|i| vec![i]), "playing");
            if let Some(i) = i {
                self.scroll_to_row(i);
            }
        }
    }

    /// Put `class` on exactly the given rows.
    fn mark_row(&self, rows: Option<Vec<usize>>, class: &str) {
        let keep = rows.unwrap_or_default();
        self.rows.borrow().iter().enumerate().for_each(|(i, (_, w))| match keep.contains(&i) {
            true => w.add_css_class(class),
            false => w.remove_css_class(class),
        });
    }

    /// Bring row `i` into view, and keep it there for a moment: a drawer that was just opened
    /// is still laying out, which moves rows after the first scroll.
    fn scroll_to_row(self: &Rc<Self>, i: usize) {
        let visible = self.row_box(i).is_some_and(|(y, h)| {
            let adj = self.drawer_scroll.vadjustment();
            y >= adj.value() && y + h <= adj.value() + adj.page_size()
        });
        if visible && self.scroll_target.get().is_none() {
            return;
        }
        let g = self.scroll_gen.get() + 1;
        self.scroll_gen.set(g);
        self.scroll_target.set(Some(i));
        self.apply_scroll(i);
        let w = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(1500), move || {
            if w.scroll_gen.get() == g {
                w.scroll_target.set(None);
            }
        });
    }

    /// Row `i`'s top and height inside the transcript, once laid out.
    fn row_box(&self, i: usize) -> Option<(f64, f64)> {
        let rows = self.rows.borrow();
        let (_, w) = rows.get(i)?;
        let p = w.compute_point(&self.drawer_body, &gtk::graphene::Point::new(0.0, 0.0))?;
        (w.height() > 0).then(|| (p.y() as f64, w.height() as f64))
    }

    fn apply_scroll(&self, i: usize) {
        if let Some((y, _)) = self.row_box(i) {
            self.drawer_scroll.vadjustment().set_value((y - 24.0).max(0.0));
        }
    }

    fn copy_transcript(&self) {
        let st = self.st.borrow();
        let Some(m) = st.current.as_ref() else { return };
        let segs = store::segments(&self.db, &m.id).unwrap_or_default();
        let rows = vm::transcript_rows(&segs, &m.speaker_names);
        let msg = match rows.is_empty() {
            true => "No transcript yet",
            false => {
                self.window.clipboard().set_text(&vm::transcript_text(&m.display_title(), &rows));
                "Transcript copied"
            }
        };
        drop(st);
        self.toast(msg);
    }

    // ------------------------------------------------------------------ actions

    fn commit_title(self: &Rc<Self>) {
        dbg_log!("commit_title");
        if self.st.borrow().loading {
            return;
        }
        let Some(m) = self.st.borrow().current.clone() else { return };
        self.st.borrow_mut().title_dirty = false;
        let typed = text_of(&self.title).replace('\n', " ");
        let naming = self.naming.is_visible();
        let req = match vm::title_action(&m, &typed) {
            TitleAction::Nothing => {
                self.unfocus();
                return;
            }
            TitleAction::Confirm => Request::ConfirmTitle { id: m.id.clone() },
            TitleAction::Rename(t) => Request::Rename { id: m.id.clone(), title: t, confirm: true },
        };
        self.unfocus();
        let w = self.clone();
        glib::spawn_future_local(async move {
            let result = daemon::request(req.clone()).await;
            if result.is_err() {
                // Daemon down: write the name ourselves so the user isn't stuck.
                let title = match &req {
                    Request::Rename { title, .. } => title.clone(),
                    _ => m.display_title(),
                };
                let _ = store::set_title(&w.db, &m.id, &title, true);
            }
            if naming {
                w.toast("Saved");
            }
            w.reload_list();
            w.render_meeting(false);
        });
    }

    fn skip_naming(self: &Rc<Self>) {
        let id = self.st.borrow().selected.clone();
        if let Some(id) = id {
            self.st.borrow_mut().skipped.insert(id);
            self.unfocus();
            self.render_meeting(true);
        }
    }

    fn enhance(self: &Rc<Self>) {
        self.flush_notes();
        let Some(id) = self.st.borrow().selected.clone() else { return };
        self.st.borrow_mut().awaiting_enhance = Some((id.clone(), store::now_ms()));
        self.send(Request::Enhance { id }, Some("Enhancing your notes…"));
    }

    fn confirm_delete(self: &Rc<Self>) {
        let Some(m) = self.st.borrow().current.clone() else { return };
        let dlg = adw::AlertDialog::new(
            Some("Delete this meeting?"),
            Some(&format!("“{}” — its notes, transcript and audio are removed from this computer.", m.display_title())),
        );
        dlg.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
        dlg.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dlg.set_close_response("cancel");
        let w = self.clone();
        dlg.connect_response(None, move |_, resp| {
            if resp != "delete" {
                return;
            }
            let w = w.clone();
            let id = m.id.clone();
            glib::spawn_future_local(async move {
                match daemon::request(Request::Delete { id }).await {
                    Ok(_) => {
                        w.st.borrow_mut().selected = None;
                        w.reload_list();
                    }
                    Err(e) => w.toast(&e),
                }
            });
        });
        dlg.present(Some(&self.window));
    }

    fn copy(&self) {
        let st = self.st.borrow();
        let Some(m) = st.current.as_ref() else { return };
        let enhanced = st.notes.as_ref().and_then(|n| n.enhanced.as_ref());
        let text = vm::copy_text(&m.display_title(), enhanced, &text_of(&self.notes_view));
        self.window.clipboard().set_text(&text);
        drop(st);
        self.toast("Copied to clipboard");
    }

    // ------------------------------------------------------------------ user notes

    fn on_notes_changed(self: &Rc<Self>, empty: bool) {
        self.notes_placeholder.set_visible(empty);
        let g = {
            let mut st = self.st.borrow_mut();
            if st.loading {
                return;
            }
            st.notes_dirty = true;
            st.notes_gen += 1;
            st.notes_gen
        };
        let w = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(600), move || {
            if w.st.borrow().notes_gen == g {
                w.flush_notes();
            }
        });
    }

    fn flush_notes(&self) {
        let (dirty, id) = {
            let st = self.st.borrow();
            (st.notes_dirty, st.notes_for.clone())
        };
        if let (true, Some(id)) = (dirty, id) {
            let md = text_of(&self.notes_view);
            match store::set_user_notes(&self.db, &id, &md) {
                Ok(()) => {
                    let mut st = self.st.borrow_mut();
                    st.notes_dirty = false;
                    if let Some(n) = st.notes.as_mut().filter(|n| n.meeting_id == id) {
                        n.user_md = md;
                    }
                }
                Err(e) => self.toast(&format!("Couldn't save notes: {e}")),
            }
        }
    }

    // ------------------------------------------------------------------ ask

    fn render_ask_model(self: &Rc<Self>) {
        let current = config::Config::load().openrouter.chat_model;
        self.ask_model.set_text(&vm::model_name(&current));
        clear(&self.ask_models);
        let listed = vm::ASK_MODELS.iter().any(|(id, _, _)| *id == current);
        let custom_name = vm::model_name(&current);
        let extra = (!listed).then_some((current.as_str(), custom_name.as_str(), "Custom"));
        vm::ASK_MODELS.iter().copied().chain(extra).for_each(|(id, name, hint)| {
            let check = IconView::new(&icons::CHECK);
            check.set_opacity(if id == current { 1.0 } else { 0.0 });
            let name = label(name, &["model-name"]);
            name.set_hexpand(true);
            let row = gtk::Button::new();
            row.set_child(Some(&with(hbox(10, &[]), &[check.upcast_ref(), name.upcast_ref(), label(hint, &["model-hint"]).upcast_ref()])));
            row.set_css_classes(&["model-row"]);
            row.set_tooltip_text(Some(id));
            let (w, id) = (self.clone(), id.to_string());
            row.connect_clicked(move |b| {
                if let Some(p) = b.ancestor(gtk::Popover::static_type()) {
                    p.downcast::<gtk::Popover>().map(|p| p.popdown()).ok();
                }
                w.set_ask_model(&id);
            });
            self.ask_models.append(&row);
        });
    }

    /// Persist the questions/titles model and tell the daemon (it names meetings with it).
    fn set_ask_model(self: &Rc<Self>, id: &str) {
        let id = id.trim();
        let mut cfg = config::Config::load();
        if id.is_empty() || cfg.openrouter.chat_model == id {
            return;
        }
        cfg.openrouter.chat_model = id.to_string();
        match cfg.save() {
            Ok(()) => {
                settings::reload_daemon();
                self.render_ask_model();
            }
            Err(e) => self.toast(&format!("Couldn't save: {e}")),
        }
    }

    fn render_thread(&self, id: &str) {
        clear(&self.thread);
        store::chats(&self.db, id).unwrap_or_default().iter().for_each(|c| {
            self.thread.append(&chat_label(&c.role, &c.content));
        });
    }

    fn ask(self: &Rc<Self>) {
        let q = self.ask_entry.text().trim().to_string();
        let Some(m) = self.st.borrow().current.clone() else { return };
        if q.is_empty() {
            return;
        }
        if config::api_key().is_none() {
            self.toast("Add your OpenRouter API key in Settings first");
            return;
        }
        let cfg = config::Config::load();
        let segs = store::segments(&self.db, &m.id).unwrap_or_default();
        let notes = store::get_notes(&self.db, &m.id).ok();
        let notes_md = notes
            .as_ref()
            .and_then(|n| n.enhanced.as_ref().map(EnhancedNotes::to_markdown))
            .filter(|s| !s.trim().is_empty())
            .or_else(|| notes.as_ref().map(|n| n.user_md.clone()))
            .unwrap_or_default();
        let history: Vec<Message> = store::chats(&self.db, &m.id)
            .unwrap_or_default()
            .into_iter()
            .map(|c| match c.role.as_str() {
                "assistant" => Message::assistant(c.content),
                _ => Message::user(c.content),
            })
            .collect();
        let msgs = prompts::ask_messages(&q, &transcript::render_for_prompt(&segs), &notes_md, &history);
        let _ = store::add_chat(&self.db, &m.id, "user", &q);
        self.ask_entry.set_text("");
        self.thread.append(&chat_label("user", &q));
        let pending = para("Thinking…", &["a", "pending"]);
        self.thread.append(&pending);
        self.thread_rev.set_reveal_child(true);
        self.scroll_thread();
        let w = self.clone();
        glib::spawn_future_local(async move {
            let reply = daemon::chat(cfg.openrouter.chat_model.clone(), msgs).await;
            pending.remove_css_class("pending");
            pending.set_selectable(true);
            match reply {
                Ok(a) => {
                    let _ = store::add_chat(&w.db, &m.id, "assistant", a.trim());
                    pending.set_text(a.trim());
                }
                Err(e) => pending.set_text(&format!("Couldn't get an answer: {e}")),
            }
            w.scroll_thread();
        });
    }

    fn scroll_thread(&self) {
        let adj = self.thread_scroll.vadjustment();
        glib::idle_add_local_once(move || adj.set_value(adj.upper()));
    }
}

fn chat_label(role: &str, content: &str) -> gtk::Label {
    let l = para(content, if role == "user" { &["q"] } else { &["a"] });
    l.set_selectable(role != "user");
    l
}

fn clear_wrap(b: &adw::WrapBox) {
    while let Some(c) = b.first_child() {
        b.remove(&c);
    }
}

fn set_button_text(b: &gtk::Button, text: &str) {
    let label = b.child().and_then(|c| c.last_child()).and_then(|l| l.downcast::<gtk::Label>().ok());
    if let Some(l) = label {
        l.set_text(text);
    }
}


/// Can this meeting be played back? (Not while recording, and only while its audio exists.)
fn has_audio(m: &Meeting) -> bool {
    let dir = muesli_core::paths::meeting_dir(&m.id);
    m.state != MeetingState::Recording && (dir.join("raw").is_dir() || dir.join("playback").join("index.json").exists())
}
