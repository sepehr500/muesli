//! Vector icons drawn with GSK paths (the SVG geometry from the Paper designs),
//! so they stay crisp at any scale and take their colour from CSS `color`.

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk};

#[derive(Debug, Clone, Copy)]
pub enum Shape {
    Path(&'static str),
    Circle(f32, f32, f32),
    /// x, y, w, h, radius
    Rect(f32, f32, f32, f32, f32),
}

#[derive(Debug, Clone, Copy)]
pub enum Paint {
    Stroke(f32),
    Fill,
}

/// Where a part's colour comes from.
#[derive(Debug, Clone, Copy)]
pub enum Ink {
    /// CSS `color` of the icon widget.
    Current,
    /// 0xRRGGBBAA in light mode, in dark mode.
    Fixed(u32, u32),
}

#[derive(Debug, Clone, Copy)]
pub struct Part(pub Shape, pub Paint, pub Ink);

#[derive(Debug)]
pub struct Icon {
    pub w: f32,
    pub h: f32,
    pub parts: &'static [Part],
}

use Ink::Current as C;
use Paint::{Fill, Stroke};
use Shape::{Circle, Path, Rect};

const fn s(shape: Shape, w: f32) -> Part {
    Part(shape, Stroke(w), C)
}

pub static LOGO: Icon = Icon {
    w: 64.0,
    h: 22.0,
    parts: &[
        s(Path("M1 21 L14 9 L19 13 L30 2 L38 10 L43 6 L63 21"), 1.5),
        Part(Path("M26 6 L30 2 L34 6 L31.5 5 L30 7 L28.5 5 Z"), Fill, C),
    ],
};
pub static PLUS: Icon = Icon { w: 16.0, h: 16.0, parts: &[s(Path("M8 3 V13 M3 8 H13"), 1.5)] };
pub static SEARCH: Icon = Icon {
    w: 14.0,
    h: 14.0,
    parts: &[s(Circle(6.0, 6.0, 4.5), 1.4), s(Path("M9.5 9.5 L12.5 12.5"), 1.4)],
};
pub static VIDEO: Icon = Icon {
    w: 16.0,
    h: 16.0,
    parts: &[s(Rect(1.5, 4.0, 9.0, 8.0, 2.0), 1.4), s(Path("M10.5 7 L14.5 4.8 V11.2 L10.5 9"), 1.4)],
};
pub static HEADSET: Icon = Icon {
    w: 16.0,
    h: 16.0,
    parts: &[
        s(Path("M2.5 10 V8 a5.5 5.5 0 0 1 11 0 V10"), 1.4),
        s(Rect(1.5, 9.0, 3.0, 4.5, 1.2), 1.4),
        s(Rect(11.5, 9.0, 3.0, 4.5, 1.2), 1.4),
    ],
};
pub static MIC: Icon = Icon {
    w: 16.0,
    h: 16.0,
    parts: &[s(Rect(5.5, 1.5, 5.0, 8.0, 2.5), 1.4), s(Path("M3 8 a5 5 0 0 0 10 0 M8 13 V14.5"), 1.4)],
};
const MEADOW_DARK: u32 = 0x5E7A3CFF;
const MEADOW: u32 = 0x93AA6EFF;
pub static LISTENING: Icon = Icon {
    w: 16.0,
    h: 16.0,
    parts: &[
        Part(Circle(8.0, 8.0, 1.8), Fill, Ink::Fixed(MEADOW_DARK, 0x9DBA74FF)),
        Part(Path("M5 5 a4.2 4.2 0 0 0 0 6 M11 5 a4.2 4.2 0 0 1 0 6"), Stroke(1.4), Ink::Fixed(MEADOW_DARK, 0x9DBA74FF)),
        Part(
            Path("M2.9 2.9 a7.2 7.2 0 0 0 0 10.2 M13.1 2.9 a7.2 7.2 0 0 1 0 10.2"),
            Stroke(1.4),
            Ink::Fixed(MEADOW, 0x6F8752FF),
        ),
    ],
};
pub static RECORDING: Icon = Icon {
    w: 16.0,
    h: 16.0,
    parts: &[
        Part(Circle(8.0, 8.0, 6.2), Stroke(1.4), Ink::Fixed(0xE2231A55, 0xFF5A4E66)),
        Part(Circle(8.0, 8.0, 3.6), Fill, Ink::Fixed(0xE2231AFF, 0xFF5A4EFF)),
    ],
};
pub static OFFLINE: Icon = Icon {
    w: 16.0,
    h: 16.0,
    parts: &[s(Circle(8.0, 8.0, 5.8), 1.4), s(Path("M4 12 L12 4"), 1.4)],
};
pub static SLIDERS: Icon = Icon {
    w: 16.0,
    h: 16.0,
    parts: &[
        s(Path("M2 4.5 H9 M12.5 4.5 H14 M2 11.5 H3.5 M7 11.5 H14"), 1.4),
        s(Circle(10.75, 4.5, 1.75), 1.4),
        s(Circle(5.25, 11.5, 1.75), 1.4),
    ],
};
pub static WAVEFORM: Icon =
    Icon { w: 16.0, h: 16.0, parts: &[s(Path("M2 6.5 V9.5 M5 4 V12 M8 2 V14 M11 5 V11 M14 7 V9"), 1.4)] };
pub static COPY: Icon = Icon {
    w: 16.0,
    h: 16.0,
    parts: &[s(Rect(5.0, 5.0, 9.0, 9.0, 2.0), 1.4), s(Path("M11 2.5 H4 a1.5 1.5 0 0 0 -1.5 1.5 V11"), 1.4)],
};
pub static ELLIPSIS: Icon = Icon {
    w: 16.0,
    h: 16.0,
    parts: &[
        Part(Circle(3.5, 8.0, 1.2), Fill, C),
        Part(Circle(8.0, 8.0, 1.2), Fill, C),
        Part(Circle(12.5, 8.0, 1.2), Fill, C),
    ],
};
pub static CLOCK: Icon = Icon {
    w: 16.0,
    h: 16.0,
    parts: &[s(Circle(8.0, 8.0, 6.0), 1.4), s(Path("M8 4.8 V8 L10.2 9.4"), 1.4)],
};
pub static PEOPLE: Icon = Icon {
    w: 16.0,
    h: 16.0,
    parts: &[
        s(Circle(6.0, 5.5, 2.5), 1.4),
        s(Path("M1.5 13.5 a4.5 4.5 0 0 1 9 0"), 1.4),
        s(Path("M11 3.2 a2.5 2.5 0 0 1 0 4.6 M12.5 9.6 a4.5 4.5 0 0 1 2 3.9"), 1.4),
    ],
};
pub static ASK: Icon = Icon {
    w: 18.0,
    h: 18.0,
    parts: &[
        s(Path("M9 2 L10.4 7.6 L16 9 L10.4 10.4 L9 16 L7.6 10.4 L2 9 L7.6 7.6 Z"), 1.4),
        s(Path("M14.5 1.5 V4.5 M13 3 H16"), 1.3),
    ],
};
pub static SPARKLE: Icon = Icon {
    w: 16.0,
    h: 16.0,
    parts: &[s(Path("M8 1.5 L9.3 6.7 L14.5 8 L9.3 9.3 L8 14.5 L6.7 9.3 L1.5 8 L6.7 6.7 Z"), 1.4)],
};
pub static SEND: Icon = Icon { w: 16.0, h: 16.0, parts: &[s(Path("M8 13 V3 M3.5 7.5 L8 3 L12.5 7.5"), 1.6)] };
pub static CLEANED: Icon = Icon {
    w: 16.0,
    h: 16.0,
    parts: &[
        s(Path("M1.5 8 H3 L4.5 4.5 L6.5 11.5 L8 8 H9.5"), 1.4),
        s(Path("M11.5 8 H14.5"), 1.4),
        Part(Path("M12.5 3 L13 4.5 L14.5 5 L13 5.5 L12.5 7 L12 5.5 L10.5 5 L12 4.5 Z"), Fill, C),
    ],
};
pub static BUBBLE: Icon = Icon {
    w: 16.0,
    h: 16.0,
    parts: &[
        s(
            Path("M2.5 3 H13.5 a1 1 0 0 1 1 1 V10.5 a1 1 0 0 1 -1 1 H7 L4 14 V11.5 H2.5 a1 1 0 0 1 -1 -1 V4 a1 1 0 0 1 1 -1 Z"),
            1.4,
        ),
        s(Path("M4.5 6 H11.5 M4.5 8.5 H9"), 1.4),
    ],
};
pub static CHECK: Icon = Icon { w: 16.0, h: 16.0, parts: &[s(Path("M3.5 8.5 L6.5 11.5 L12.5 4.5"), 1.6)] };
pub static SPINNER: Icon = Icon {
    w: 16.0,
    h: 16.0,
    parts: &[
        Part(Circle(8.0, 8.0, 5.5), Stroke(1.6), Ink::Fixed(0xD5DDE3FF, 0x36405AFF)),
        Part(Path("M8 2.5 A5.5 5.5 0 0 1 13.5 8"), Stroke(1.6), Ink::Fixed(0x2147A6FF, 0x8FA8F5FF)),
    ],
};
pub static ALERT: Icon = Icon {
    w: 16.0,
    h: 16.0,
    parts: &[s(Circle(8.0, 8.0, 6.0), 1.4), s(Path("M8 4.8 V8.6"), 1.6), Part(Circle(8.0, 11.0, 0.9), Fill, C)],
};
pub static CHEVRON: Icon = Icon { w: 16.0, h: 16.0, parts: &[s(Path("M5 6.5 L8 9.5 L11 6.5"), 1.5)] };
pub static CLOSE: Icon = Icon { w: 16.0, h: 16.0, parts: &[s(Path("M4.5 4.5 L11.5 11.5 M11.5 4.5 L4.5 11.5"), 1.5)] };

const ROCK: Ink = Ink::Fixed(0xC9D5DEFF, 0x2F3A52FF);
const SNOWCAP: Ink = Ink::Fixed(0xFFFFFFFF, 0xF2C9B8FF);
const MEADOW_LIGHT_INK: Ink = Ink::Fixed(0xA9BE84FF, 0x3E4E3AFF);
const MEADOW_INK: Ink = Ink::Fixed(0x93AA6EFF, 0x34432FFF);
const GENTIAN_INK: Ink = Ink::Fixed(0x2147A6FF, 0x8FA8F5FF);
const FLOWER_INK: Ink = Ink::Fixed(0xFFFFFFFF, 0xE9EEF5FF);

/// The alpine vignette (rock, snowcaps, meadow, a few gentians and an edelweiss).
pub static VIGNETTE: Icon = Icon {
    w: 272.0,
    h: 132.0,
    parts: &[
        Part(Path("M0 96 L44 56 L62 68 L100 22 L120 44 L134 34 L176 76 L198 60 L272 104 L272 132 L0 132 Z"), Fill, ROCK),
        Part(Path("M100 22 L87.6 37 L93 34.5 L97.5 38.5 L103 33.5 L107 36.5 L113.6 37 Z"), Fill, SNOWCAP),
        Part(Path("M134 34 L122.8 42 L127.5 40.5 L131.5 43.5 L136 40 L142 42 Z"), Fill, SNOWCAP),
        Part(Path("M44 56 L36.3 63 L40 62 L44 64.5 L47.5 61.5 L52.5 61.4 Z"), Fill, SNOWCAP),
        Part(Path("M0 110 C46 92 92 90 136 101 C178 111 222 104 272 90 L272 132 L0 132 Z"), Fill, MEADOW_LIGHT_INK),
        Part(Path("M0 121 C60 110 140 112 272 118 L272 132 L0 132 Z"), Fill, MEADOW_INK),
        Part(Circle(70.0, 110.0, 2.2), Fill, GENTIAN_INK),
        Part(Circle(79.0, 114.0, 1.7), Fill, GENTIAN_INK),
        Part(Circle(204.0, 108.0, 2.0), Fill, FLOWER_INK),
    ],
};

pub fn for_app(app: Option<muesli_core::detect::App>) -> &'static Icon {
    use muesli_core::detect::App;
    match app {
        Some(App::Zoom) => &VIDEO,
        Some(App::Discord) => &HEADSET,
        None => &MIC,
    }
}

pub fn for_step(kind: muesli_core::model::StepKind) -> &'static Icon {
    use muesli_core::model::StepKind;
    match kind {
        StepKind::Saved => &MIC,
        StepKind::Cleaned => &CLEANED,
        StepKind::Transcribed => &BUBBLE,
        StepKind::Notes => &SPARKLE,
    }
}

fn rgba(hex: u32) -> gdk::RGBA {
    let c = |shift: u32| ((hex >> shift) & 0xFF) as f32 / 255.0;
    gdk::RGBA::new(c(24), c(16), c(8), c(0))
}

fn path_of(shape: Shape) -> Option<gsk::Path> {
    match shape {
        Path(d) => gsk::Path::parse(d).ok(),
        Circle(x, y, r) => {
            let b = gsk::PathBuilder::new();
            b.add_circle(&graphene::Point::new(x, y), r);
            Some(b.to_path())
        }
        Rect(x, y, w, h, r) => {
            let b = gsk::PathBuilder::new();
            let rect = graphene::Rect::new(x, y, w, h);
            b.add_rounded_rect(&gsk::RoundedRect::from_rect(rect, r));
            Some(b.to_path())
        }
    }
}

mod imp {
    use super::*;
    use std::cell::Cell;

    #[derive(Default)]
    pub struct IconView {
        pub icon: Cell<Option<&'static Icon>>,
        pub size: Cell<(i32, i32)>,
        pub spin: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for IconView {
        const NAME: &'static str = "MuesliIcon";
        type Type = super::IconView;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("muesli-icon");
        }
    }

    impl ObjectImpl for IconView {
        fn constructed(&self) {
            self.parent_constructed();
            let weak = self.obj().downgrade();
            adw::StyleManager::default().connect_dark_notify(move |_| {
                if let Some(w) = weak.upgrade() {
                    w.queue_draw();
                }
            });
        }
    }

    impl WidgetImpl for IconView {
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let (w, h) = self.size.get();
            let v = if orientation == gtk::Orientation::Horizontal { w } else { h };
            (v, v, -1, -1)
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let Some(icon) = self.icon.get() else { return };
            let obj = self.obj();
            let (w, h) = (obj.width() as f32, obj.height() as f32);
            let scale = (w / icon.w).min(h / icon.h);
            let dark = adw::StyleManager::default().is_dark();
            let current = obj.color();
            snapshot.save();
            snapshot.translate(&graphene::Point::new((w - icon.w * scale) / 2.0, (h - icon.h * scale) / 2.0));
            snapshot.scale(scale, scale);
            if self.spin.get() {
                let t = obj.frame_clock().map(|c| c.frame_time()).unwrap_or(0) as f64 / 1e6;
                let center = graphene::Point::new(icon.w / 2.0, icon.h / 2.0);
                snapshot.translate(&center);
                snapshot.rotate(((t * 400.0) % 360.0) as f32);
                snapshot.translate(&graphene::Point::new(-center.x(), -center.y()));
            }
            icon.parts.iter().for_each(|Part(shape, paint, ink)| {
                let color = match ink {
                    Ink::Current => current,
                    Ink::Fixed(l, d) => rgba(if dark { *d } else { *l }),
                };
                let Some(path) = path_of(*shape) else { return };
                match paint {
                    Paint::Fill => snapshot.append_fill(&path, gsk::FillRule::Winding, &color),
                    Paint::Stroke(width) => {
                        let stroke = gsk::Stroke::new(*width);
                        stroke.set_line_cap(gsk::LineCap::Round);
                        stroke.set_line_join(gsk::LineJoin::Round);
                        snapshot.append_stroke(&path, &stroke, &color);
                    }
                }
            });
            snapshot.restore();
        }
    }
}

glib::wrapper! {
    pub struct IconView(ObjectSubclass<imp::IconView>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl IconView {
    /// Icon at its natural size.
    pub fn new(icon: &'static Icon) -> Self {
        Self::sized(icon, icon.w.round() as i32, icon.h.round() as i32)
    }

    pub fn sized(icon: &'static Icon, w: i32, h: i32) -> Self {
        let v: Self = glib::Object::new();
        v.imp().icon.set(Some(icon));
        v.imp().size.set((w, h));
        v.set_valign(gtk::Align::Center);
        v
    }

    pub fn set_icon(&self, icon: &'static Icon) {
        self.imp().icon.set(Some(icon));
        self.queue_draw();
    }

    pub fn spinning(self) -> Self {
        self.imp().spin.set(true);
        self.add_tick_callback(|w, _| {
            w.queue_draw();
            glib::ControlFlow::Continue
        });
        self
    }

    pub fn class(self, c: &str) -> Self {
        self.add_css_class(c);
        self
    }
}

impl Default for IconView {
    fn default() -> Self {
        Self::new(&PLUS)
    }
}
