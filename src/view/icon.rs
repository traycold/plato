use std::path::Path;
use fnv::FnvHashMap;
use lazy_static::lazy_static;
use crate::device::CURRENT_DEVICE;
use crate::framebuffer::{Framebuffer, Pixmap, UpdateMode};
use super::{View, Event, Hub, Bus, ViewId, Align};
use super::BORDER_RADIUS_SMALL;
use crate::gesture::GestureEvent;
use crate::input::{DeviceEvent, FingerStatus};
use crate::document::pdf::PdfOpener;
use crate::color::{TEXT_NORMAL, TEXT_INVERTED_HARD};
use crate::unit::{scale_by_dpi, scale_by_dpi_raw};
use crate::geom::{Rectangle, CornerSpec};
use crate::font::Fonts;
use crate::app::Context;

const ICON_SCALE: f32 = 1.0 / 32.0;

lazy_static! {
    pub static ref ICONS_PIXMAPS: FnvHashMap<&'static str, Pixmap> = {
        let mut m = FnvHashMap::default();
        let scale = scale_by_dpi_raw(ICON_SCALE, CURRENT_DEVICE.dpi);
        let dir = Path::new("icons");
        for name in ["home", "search", "back", "frontlight", "frontlight-disabled", "menu",
                     "angle-left", "angle-right", "angle-left-small", "angle-right-small",
                     "delete-backward", "delete-forward", "move-backward", "move-forward",
                     "close",  "check_mark-small", "check_mark","check_mark-large", "bullet",
                     "arrow-left", "arrow-right", "double_angle-left", "double_angle-right",
                     "angle-down", "angle-up", "plus", "minus", "crop", "toc", "font_family",
                     "font_size", "line_height", "align-justify", "align-left", "align-right",
                     "align-center", "margin", "plug", "ellipsis", "contrast", "gray"].iter().cloned() {
            let path = dir.join(&format!("{}.svg", name));
            let doc = PdfOpener::new().and_then(|o| o.open(path)).unwrap();
            let pixmap = doc.page(0).and_then(|p| p.pixmap(scale)).unwrap();
            m.insert(name, pixmap);
        }
        m
    };
}

pub struct Icon {
    pub rect: Rectangle,
    children: Vec<Box<dyn View>>,
    pub name: String,
    background: u8,
    align: Align,
    corners: Option<CornerSpec>,
    event: Event,
    active: bool,
}

impl Icon {
    pub fn new(name: &str, rect: Rectangle, event: Event) -> Icon {
        Icon {
            rect,
            children: vec![],
            name: name.to_string(),
            background: TEXT_NORMAL[0],
            align: Align::Center,
            corners: None,
            event,
            active: false,
        }
    }

    pub fn background(mut self, background: u8) -> Icon {
        self.background = background;
        self
    }

    pub fn align(mut self, align: Align) -> Icon {
        self.align = align;
        self
    }

    pub fn corners(mut self, corners: Option<CornerSpec>) -> Icon {
        self.corners = corners;
        self
    }
}

impl View for Icon {
    fn handle_event(&mut self, evt: &Event, hub: &Hub, bus: &mut Bus, _context: &mut Context) -> bool {
        match *evt {
            Event::Device(DeviceEvent::Finger { status, position, .. }) => {
                match status {
                    FingerStatus::Down if self.rect.includes(position) => {
                        self.active = true;
                        hub.send(Event::Render(self.rect, UpdateMode::Fast)).unwrap();
                        true
                    },
                    FingerStatus::Up if self.active => {
                        self.active = false;
                        hub.send(Event::Render(self.rect, UpdateMode::Gui)).unwrap();
                        true
                    },
                    _ => false,
                }
            },
            Event::Gesture(GestureEvent::Tap(center)) if self.rect.includes(center) => {
                bus.push_back(self.event.clone());
                true
            },
            Event::Gesture(GestureEvent::HoldFinger(center)) if self.rect.includes(center) => {
                match self.event {
                    Event::Page(dir) => bus.push_back(Event::Chapter(dir)),
                    Event::Show(ViewId::Frontlight) => {
                        hub.send(Event::ToggleFrontlight).unwrap();
                    },
                    Event::Show(ViewId::MarginCropper) => {
                        bus.push_back(Event::ToggleNear(ViewId::MarginCropperMenu, self.rect));
                    },
                    Event::Show(ViewId::SearchBar) | Event::Focus(Some(ViewId::SearchInput)) => {
                        bus.push_back(Event::ToggleNear(ViewId::SearchMenu, self.rect));
                    },
                    Event::History(dir, false) => {
                        bus.push_back(Event::History(dir, true));
                    },
                    _ => (),
                }
                true
            },
            _ => false,
        }
    }

    fn render(&self, fb: &mut Framebuffer, _rect: Rectangle, _fonts: &mut Fonts) -> Rectangle {
        let dpi = CURRENT_DEVICE.dpi;

        let scheme = if self.active {
            TEXT_INVERTED_HARD
        } else {
            TEXT_NORMAL
        };

        let pixmap = ICONS_PIXMAPS.get(&self.name[..]).unwrap();
        let dx = self.align.offset(pixmap.width as i32, self.rect.width() as i32);
        let dy = (self.rect.height() as i32 - pixmap.height as i32) / 2;
        let pt = self.rect.min + pt!(dx, dy);

        if let Some(ref cs) = self.corners {
            fb.draw_rounded_rectangle(&self.rect, cs, self.background);
        } else {
            fb.draw_rectangle(&self.rect, self.background);
        }

        if self.active {
            let padding = ((self.rect.width() as i32 - pixmap.width as i32).min(self.rect.height() as i32 - pixmap.height as i32) / 3).max(1);
            let bg_rect = rect![pt - padding, pt + pt!(pixmap.width as i32, pixmap.height as i32) + padding];
            let border_radius = scale_by_dpi(BORDER_RADIUS_SMALL, dpi) as i32;
            fb.draw_rounded_rectangle(&bg_rect, &CornerSpec::Uniform(border_radius), scheme[0]);
        }

        fb.draw_blended_pixmap(pixmap, pt, scheme[1]);
        self.rect
    }

    fn resize(&mut self, rect: Rectangle, _hub: &Hub, _context: &mut Context) {
        if let Event::ToggleNear(_, ref mut event_rect) = self.event {
            *event_rect = rect;
        }
        self.rect = rect;
    }

    fn rect(&self) -> &Rectangle {
        &self.rect
    }

    fn rect_mut(&mut self) -> &mut Rectangle {
        &mut self.rect
    }

    fn children(&self) -> &Vec<Box<dyn View>> {
        &self.children
    }

    fn children_mut(&mut self) -> &mut Vec<Box<dyn View>> {
        &mut self.children
    }
}
