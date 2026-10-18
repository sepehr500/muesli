//! Small widget helpers and a left-aligned max-width container.

use crate::icons::{Icon, IconView};
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{glib, graphene};

pub fn label(text: &str, classes: &[&str]) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_xalign(0.0);
    l.set_css_classes(classes);
    l
}

/// Wrapping, left-aligned label.
pub fn para(text: &str, classes: &[&str]) -> gtk::Label {
    let l = label(text, classes);
    l.set_wrap(true);
    l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    l.set_natural_wrap_mode(gtk::NaturalWrapMode::Word);
    l
}

pub fn ellipsized(text: &str, classes: &[&str]) -> gtk::Label {
    let l = label(text, classes);
    l.set_ellipsize(gtk::pango::EllipsizeMode::End);
    l
}

pub fn hbox(spacing: i32, classes: &[&str]) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, spacing);
    b.set_css_classes(classes);
    b
}

pub fn vbox(spacing: i32, classes: &[&str]) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, spacing);
    b.set_css_classes(classes);
    b
}

pub fn with(b: gtk::Box, children: &[&gtk::Widget]) -> gtk::Box {
    children.iter().for_each(|c| b.append(*c));
    b
}

pub fn clear(b: &gtk::Box) {
    while let Some(c) = b.first_child() {
        b.remove(&c);
    }
}

/// Button with a 16px icon and optional text, e.g. "Transcript", "Copy".
pub fn icon_button(icon: &'static Icon, text: Option<&str>, classes: &[&str]) -> gtk::Button {
    let content = hbox(8, &[]);
    content.append(&IconView::new(icon));
    if let Some(t) = text {
        content.append(&label(t, &[]));
    }
    let b = gtk::Button::new();
    b.set_child(Some(&content));
    b.set_css_classes(classes);
    if let Some(t) = text {
        b.set_tooltip_text(Some(t));
    }
    b
}

pub fn spacer() -> gtk::Box {
    let b = hbox(0, &[]);
    b.set_hexpand(true);
    b.set_vexpand(true);
    b
}

mod imp {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Default)]
    pub struct LeftClamp {
        pub max: Cell<i32>,
        pub child: RefCell<Option<gtk::Widget>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for LeftClamp {
        const NAME: &'static str = "MuesliLeftClamp";
        type Type = super::LeftClamp;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for LeftClamp {
        fn dispose(&self) {
            if let Some(c) = self.child.take() {
                c.unparent();
            }
        }
    }

    impl WidgetImpl for LeftClamp {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::HeightForWidth
        }

        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let Some(child) = self.child.borrow().clone() else { return (0, 0, -1, -1) };
            let max = self.max.get();
            match orientation {
                gtk::Orientation::Horizontal => {
                    let (min, _, _, _) = child.measure(orientation, for_size);
                    (min, max.max(min), -1, -1)
                }
                _ => {
                    let w = if for_size < 0 { max } else { for_size.min(max) };
                    child.measure(orientation, w)
                }
            }
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            if let Some(child) = self.child.borrow().as_ref() {
                child.allocate(width.min(self.max.get()), height, baseline, None);
            }
        }

        /// Only the child is clickable, so an overlaid clamp doesn't swallow input beside it.
        fn contains(&self, x: f64, y: f64) -> bool {
            self.child.borrow().as_ref().is_some_and(|c| {
                c.compute_bounds(&*self.obj())
                    .is_some_and(|b: graphene::Rect| b.contains_point(&graphene::Point::new(x as f32, y as f32)))
            })
        }
    }
}

glib::wrapper! {
    /// Like `adw::Clamp`, but the child is aligned left instead of centred.
    pub struct LeftClamp(ObjectSubclass<imp::LeftClamp>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl LeftClamp {
    pub fn new(max: i32, child: &impl IsA<gtk::Widget>) -> Self {
        let c: Self = glib::Object::new();
        c.imp().max.set(max);
        child.set_parent(&c);
        c.imp().child.replace(Some(child.clone().upcast()));
        c.set_hexpand(true);
        c
    }
}
