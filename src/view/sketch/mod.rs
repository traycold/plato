use std::fs::{self, File};
use std::path::PathBuf;
use rand::{Rng, SeedableRng};
use rand_xorshift::XorShiftRng;
use fnv::FnvHashMap;
use chrono::Local;
use glob::glob;
use failure::Error;
use crate::device::{CURRENT_DEVICE, BAR_SIZES};
use crate::geom::{Point, Rectangle, CornerSpec};
use crate::input::{DeviceEvent, FingerStatus};
use crate::view::icon::Icon;
use crate::view::notification::Notification;
use crate::view::menu::{Menu, MenuKind};
use crate::view::common::{locate_by_id};
use crate::view::{View, Event, Hub, Bus, EntryKind, EntryId, ViewId};
use crate::framebuffer::{Framebuffer, UpdateMode, Pixmap};
use crate::metadata::import;
use crate::settings::{ImportSettings, Pen};
use crate::font::Fonts;
use crate::color::{BLACK, WHITE};
use crate::app::Context;

const FILENAME_PATTERN: &str = "sketch-%Y%m%d_%H%M%S.png";
// https://oeis.org/A000041
const PEN_SIZES: [i32; 12] = [1, 2, 3, 5, 7, 11, 15, 22, 30, 42, 56, 77];

struct TouchState {
    pt: Point,
    time: f64,
    radius: f32,
}

impl TouchState {
    fn new(pt: Point, time: f64, radius: f32) -> TouchState {
        TouchState { pt, time, radius }
    }
}

pub struct Sketch {
    rect: Rectangle,
    children: Vec<Box<dyn View>>,
    pixmap: Pixmap,
    random: Pixmap,
    fingers: FnvHashMap<i32, TouchState>,
    pen: Pen,
    save_path: PathBuf,
    filename: String,
}

impl Sketch {
    pub fn new(rect: Rectangle, hub: &Hub, context: &mut Context) -> Sketch {
        let mut children = Vec::new();
        let dpi = CURRENT_DEVICE.dpi;
        let (_, height) = context.display.dims;
        let (small_height, _) = *BAR_SIZES.get(&(height, dpi)).unwrap();
        let side = small_height as i32;
        let icon_rect = rect![rect.min.x, rect.max.y - side,
                              rect.min.x + side, rect.max.y];
        let icon = Icon::new("ellipsis",
                             icon_rect,
                             Event::ToggleNear(ViewId::TitleMenu, icon_rect))
                        .corners(Some(CornerSpec::Uniform(side / 2)));
        children.push(Box::new(icon) as Box<dyn View>);
        let mut random = Pixmap::new(rect.width(), rect.height());
        let mut rng = XorShiftRng::seed_from_u64(Local::now().timestamp_millis() as u64);
        rng.fill(random.data_mut());
        let save_path = context.settings.library_path.join(&context.settings.sketch.save_path);
        hub.send(Event::Render(rect, UpdateMode::Full)).ok();
        Sketch {
            rect,
            children,
            pixmap: Pixmap::new(rect.width(), rect.height()),
            random,
            fingers: FnvHashMap::default(),
            pen: context.settings.sketch.pen.clone(),
            save_path,
            filename: Local::now().format(FILENAME_PATTERN).to_string(),
        }
    }

    fn toggle_title_menu(&mut self, rect: Rectangle, enable: Option<bool>, hub: &Hub, context: &mut Context) {
        if let Some(index) = locate_by_id(self, ViewId::SketchMenu) {
            if let Some(true) = enable {
                return;
            }

            hub.send(Event::Expose(*self.child(index).rect(), UpdateMode::Gui)).ok();
            self.children.remove(index);
        } else {
            if let Some(false) = enable {
                return;
            }

            let mut loadables: Vec<PathBuf> = self.save_path.join("*.png").to_str().and_then(|s| {
                glob(s).ok().map(|paths| {
                    paths.filter_map(|x| x.ok().and_then(|p| p.file_name().map(PathBuf::from))).collect()
                })
            }).unwrap_or_default();

            loadables.sort_by(|a, b| b.cmp(a));

            let mut sizes = vec![
                EntryKind::CheckBox("Dynamic".to_string(),
                                    EntryId::TogglePenDynamism,
                                    self.pen.dynamic),
                EntryKind::Separator,
            ];

            for s in PEN_SIZES.iter() {
                sizes.push(EntryKind::RadioButton(s.to_string(),
                                                  EntryId::SetPenSize(*s),
                                                  self.pen.size == *s));
            }

            let mut colors = vec![
                EntryKind::RadioButton("White".to_string(),
                                       EntryId::SetPenColor(WHITE),
                                       self.pen.color == WHITE),
                EntryKind::RadioButton("Black".to_string(),
                                       EntryId::SetPenColor(BLACK),
                                       self.pen.color == BLACK),
            ];

            for i in 1..=14 {
                let c = i * 17;
                if i % 7 == 1 {
                    colors.push(EntryKind::Separator);
                }
                colors.push(EntryKind::RadioButton(format!("Gray {:02}", i),
                                                   EntryId::SetPenColor(c),
                                                   self.pen.color == c));
            }

            let mut entries = vec![
                EntryKind::SubMenu("Size".to_string(), sizes),
                EntryKind::SubMenu("Color".to_string(), colors),
                EntryKind::Separator,
                EntryKind::Command("Save".to_string(), EntryId::Save),
                EntryKind::Command("Refresh".to_string(), EntryId::Refresh),
                EntryKind::Command("New".to_string(), EntryId::New),
                EntryKind::Command("Quit".to_string(), EntryId::Quit),
            ];

            if !loadables.is_empty() {
                entries.insert(entries.len() - 1, EntryKind::SubMenu("Load".to_string(),
                    loadables.into_iter().map(|e|
                        EntryKind::Command(e.to_string_lossy().into_owned(),
                                           EntryId::Load(e))).collect()));
            }

            let sketch_menu = Menu::new(rect, ViewId::SketchMenu, MenuKind::Contextual, entries, context);
            hub.send(Event::Render(*sketch_menu.rect(), UpdateMode::Gui)).ok();
            self.children.push(Box::new(sketch_menu) as Box<dyn View>);
        }
    }

    fn load(&mut self, filename: &PathBuf) -> Result<(), Error> {
        let path = self.save_path.join(filename);
        let decoder = png::Decoder::new(File::open(path)?);
        let (_, mut reader) = decoder.read_info()?;
        reader.next_frame(self.pixmap.data_mut())?;
        self.filename = filename.to_string_lossy().into_owned();
        Ok(())
    }

    fn save(&self) -> Result<(), Error> {
        if !self.save_path.exists() {
            fs::create_dir_all(&self.save_path)?;
        }
        let path = self.save_path.join(&self.filename);
        self.pixmap.save(&path.to_string_lossy().into_owned())?;
        Ok(())
    }

    fn quit(&self, context: &mut Context) {
        if let Ok(suffix) = self.save_path.strip_prefix(&context.settings.library_path) {
            let import_settings = ImportSettings {
                allowed_kinds: ["png".to_string()].iter().cloned().collect(),
                .. Default::default()
            };
            let imported_metadata = import(&context.settings.library_path,
                                           &context.metadata,
                                           &import_settings);
            if let Ok(mut imported_metadata) = imported_metadata {
                imported_metadata.retain(|info| info.file.path.starts_with(&suffix));
                context.metadata.append(&mut imported_metadata);
            }
        }
    }
}

#[inline]
fn draw_segment(pixmap: &mut Pixmap, ts: &mut TouchState, position: Point, time: f64, pen: &Pen, fb_rect: &Rectangle, hub: &Hub) {
    let (start_radius, end_radius) = if pen.dynamic {
        if time > ts.time {
            let d = vec2!((position.x - ts.pt.x) as f32,
                          (position.y - ts.pt.y) as f32).length();
            let speed = d / (time - ts.time) as f32;
            let base_radius = pen.size as f32 / 2.0;
            let radius = base_radius + (1.0 + base_radius.sqrt()) * speed.max(pen.min_speed).min(pen.max_speed) / (pen.max_speed - pen.min_speed);
            (ts.radius, radius)
        } else {
            (ts.radius, ts.radius)
        }
    } else {
        let radius = pen.size as f32 / 2.0;
        (radius, radius)
    };

    let rect = Rectangle::from_segment(ts.pt, position,
                                       start_radius.ceil() as i32,
                                       end_radius.ceil() as i32);

    pixmap.draw_segment(ts.pt, position, start_radius, end_radius, pen.color);

    if let Some(render_rect) = rect.intersection(fb_rect) {
        hub.send(Event::RenderNoWaitRegion(render_rect, UpdateMode::FastMono)).ok();
    }

    ts.pt = position;
    ts.time = time;
    ts.radius = end_radius;
}

impl View for Sketch {
    fn handle_event(&mut self, evt: &Event, hub: &Hub, _bus: &mut Bus, context: &mut Context) -> bool {
        match *evt {
            Event::Device(DeviceEvent::Finger { status: FingerStatus::Motion, id, position, time }) => {
                if let Some(ts) = self.fingers.get_mut(&id) {
                    draw_segment(&mut self.pixmap, ts, position, time, &self.pen, &self.rect, hub);
                }
                true
            },
            Event::Device(DeviceEvent::Finger { status: FingerStatus::Down, id, position, time }) => {
                let radius = self.pen.size as f32 / 2.0;
                self.fingers.insert(id, TouchState::new(position, time, radius));
                true
            },
            Event::Device(DeviceEvent::Finger { status: FingerStatus::Up, id, position, time }) => {
                if let Some(ts) = self.fingers.get_mut(&id) {
                    draw_segment(&mut self.pixmap, ts, position, time, &self.pen, &self.rect, hub);
                }
                self.fingers.remove(&id);
                true
            },
            Event::ToggleNear(ViewId::TitleMenu, rect) => {
                self.toggle_title_menu(rect, None, hub, context);
                true
            },
            Event::Select(EntryId::SetPenSize(size)) => {
                self.pen.size = size;
                true
            },
            Event::Select(EntryId::SetPenColor(color)) => {
                self.pen.color = color;
                true
            },
            Event::Select(EntryId::TogglePenDynamism) => {
                self.pen.dynamic = !self.pen.dynamic;
                true
            },
            Event::Select(EntryId::Load(ref name)) => {
                if let Err(e) = self.load(name) {
                    let msg = format!("Couldn't load sketch: {}).", e);
                    let notif = Notification::new(ViewId::LoadSketchNotif, msg, hub, context);
                    self.children.push(Box::new(notif) as Box<dyn View>);
                } else {
                    hub.send(Event::Render(self.rect, UpdateMode::Gui)).ok();
                }
                true
            },
            Event::Select(EntryId::Refresh) => {
                hub.send(Event::Render(self.rect, UpdateMode::Full)).ok();
                true
            },
            Event::Select(EntryId::New) => {
                self.pixmap.clear(WHITE);
                self.filename = Local::now().format(FILENAME_PATTERN).to_string();
                hub.send(Event::Render(self.rect, UpdateMode::Gui)).ok();
                true
            },
            Event::Select(EntryId::Save) => {
                let mut msg = match self.save() {
                    Err(e) => Some(format!("Can't save sketch: {}.", e)),
                    Ok(..) => {
                        if context.settings.sketch.notify_success {
                            Some(format!("Saved {}.", self.filename))
                        } else {
                            None
                        }
                    },
                };
                if let Some(msg) = msg.take() {
                    let notif = Notification::new(ViewId::SaveSketchNotif,
                                                  msg, hub, context);
                    self.children.push(Box::new(notif) as Box<dyn View>);
                }
                true
            },
            Event::Select(EntryId::Quit) => {
                self.quit(context);
                hub.send(Event::Back).ok();
                true
            },
            _ => false,
        }
    }

    fn render(&self, fb: &mut dyn Framebuffer, rect: Rectangle, _fonts: &mut Fonts) {
        fb.draw_framed_pixmap_halftone(&self.pixmap, &self.random, &rect, rect.min);
    }

    fn render_rect(&self, rect: &Rectangle) -> Rectangle {
        rect.intersection(&self.rect)
            .unwrap_or(self.rect)
    }

    fn might_rotate(&self) -> bool {
        false
    }

    fn is_background(&self) -> bool {
        true
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
