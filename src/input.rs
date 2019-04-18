use std::mem;
use std::ptr;
use std::slice;
use std::thread;
use std::io::Read;
use std::fs::File;
use std::sync::mpsc::{self, Sender, Receiver};
use std::os::unix::io::AsRawFd;
use std::ffi::CString;
use fnv::{FnvHashMap, FnvHashSet};
use crate::framebuffer::Display;
use crate::device::CURRENT_DEVICE;
use crate::geom::Point;
use failure::{Error, ResultExt};

// Event types
pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_ABS: u16 = 0x03;
pub const EV_MSC: u16 = 0x04;

// Event codes
pub const ABS_MT_TRACKING_ID: u16 = 0x39;
pub const ABS_MT_POSITION_X: u16 = 0x35;
pub const ABS_MT_POSITION_Y: u16 = 0x36;
pub const ABS_MT_PRESSURE: u16 = 0x3a;
pub const ABS_MT_TOUCH_MAJOR: u16 = 0x30;
pub const SYN_MT_REPORT: u16 = 0x02;
pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const ABS_PRESSURE: u16 = 0x18;
pub const MSC_RAW: u16 = 0x03;
pub const SYN_REPORT: u16 = 0x00;

// Event values
pub const MSC_RAW_GSENSOR_PORTRAIT_DOWN: i32 = 0x17;
pub const MSC_RAW_GSENSOR_PORTRAIT_UP: i32 = 0x18;
pub const MSC_RAW_GSENSOR_LANDSCAPE_RIGHT: i32 = 0x19;
pub const MSC_RAW_GSENSOR_LANDSCAPE_LEFT: i32 = 0x1a;
// pub const MSC_RAW_GSENSOR_BACK: i32 = 0x1b;
// pub const MSC_RAW_GSENSOR_FRONT: i32 = 0x1c;
pub const VAL_RELEASE: i32 = 0;
pub const VAL_PRESS: i32 = 1;
pub const VAL_REPEAT: i32 = 2;

// Key codes
pub const KEY_POWER: u16 = 116;
pub const KEY_HOME: u16 = 102;
pub const KEY_LIGHT: u16 = 90;
pub const KEY_BACKWARD: u16 = 193;
pub const KEY_FORWARD: u16 = 194;
pub const KEY_ROTATE_DISPLAY: u16 = 153;
pub const SLEEP_COVER: u16 = 59;

pub const SINGLE_TOUCH_CODES: TouchCodes = TouchCodes {
    pressure: ABS_PRESSURE,
    x: ABS_X,
    y: ABS_Y,
};

pub const MULTI_TOUCH_CODES_A: TouchCodes = TouchCodes {
    pressure: ABS_MT_TOUCH_MAJOR,
    x: ABS_MT_POSITION_X,
    y: ABS_MT_POSITION_Y,
};

pub const MULTI_TOUCH_CODES_B: TouchCodes = TouchCodes {
    pressure: ABS_MT_PRESSURE,
    .. MULTI_TOUCH_CODES_A
};

#[repr(C)]
pub struct InputEvent {
    pub time: libc::timeval,
    pub kind: u16, // type
    pub code: u16,
    pub value: i32,
}

// Handle different touch protocols
#[derive(Debug)]
pub struct TouchCodes {
    pressure: u16,
    x: u16,
    y: u16,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum TouchProto {
    Single,
    MultiA,
    MultiB,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum FingerStatus {
    Down,
    Motion,
    Up,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ButtonStatus {
    Pressed,
    Released,
    Repeated,
}

impl ButtonStatus {
    pub fn try_from_raw(value: i32) -> Option<ButtonStatus> {
        match value {
            VAL_RELEASE => Some(ButtonStatus::Released),
            VAL_PRESS => Some(ButtonStatus::Pressed),
            VAL_REPEAT => Some(ButtonStatus::Repeated),
            _ => None,
        }
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum ButtonCode {
    Power,
    Home,
    Light,
    Backward,
    Forward,
    Raw(u16),
}

impl ButtonCode {
    fn from_raw(code: u16, rotation: i8) -> ButtonCode {
        if code == KEY_POWER {
            ButtonCode::Power
        } else if code == KEY_HOME {
            ButtonCode::Home
        } else if code == KEY_LIGHT {
            ButtonCode::Light
        } else if code == KEY_BACKWARD {
            if rotation < 2 {
                ButtonCode::Backward
            } else {
                ButtonCode::Forward
            }
        } else if code == KEY_FORWARD {
            if rotation < 2 {
                ButtonCode::Forward
            } else {
                ButtonCode::Backward
            }
        } else {
            ButtonCode::Raw(code)
        }
    }
}

pub fn display_rotate_event(n: i8) -> InputEvent {
    let mut tp = libc::timeval { tv_sec: 0, tv_usec: 0 };
    unsafe { libc::gettimeofday(&mut tp, ptr::null_mut()); }
    InputEvent {
        time: tp,
        kind: EV_KEY,
        code: KEY_ROTATE_DISPLAY,
        value: n as i32,
    }
}

#[derive(Debug, Copy, Clone)]
pub enum DeviceEvent {
    Finger {
        id: i32,
        time: f64,
        status: FingerStatus,
        position: Point,
    },
    Button {
        time: f64,
        code: ButtonCode,
        status: ButtonStatus,
    },
    Plug(PowerSource),
    Unplug(PowerSource),
    RotateScreen(i8),
    CoverOn,
    CoverOff,
    NetUp,
    UserActivity,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum PowerSource {
    Host,
    Wall,
}

pub fn seconds(time: libc::timeval) -> f64 {
    time.tv_sec as f64 + time.tv_usec as f64 / 1e6
}

pub fn raw_events(paths: Vec<String>) -> (Sender<InputEvent>, Receiver<InputEvent>) {
    let (tx, rx) = mpsc::channel();
    let tx2 = tx.clone();
    thread::spawn(move || parse_raw_events(&paths, &tx));
    (tx2, rx)
}

pub fn parse_raw_events(paths: &[String], tx: &Sender<InputEvent>) -> Result<(), Error> {
    let mut files = Vec::new();
    let mut pfds = Vec::new();

    for path in paths.iter() {
        let file = File::open(path).context("Can't open input file.")?;
        let fd = file.as_raw_fd();
        files.push(file);
        pfds.push(libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        });
    }

    loop {
        let ret = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, -1) };
        if ret < 0 {
            break;
        }
        for (pfd, mut file) in pfds.iter().zip(&files) {
            if pfd.revents & libc::POLLIN != 0 {
                let mut input_event: InputEvent = unsafe { mem::uninitialized() };
                unsafe {
                    let event_slice = slice::from_raw_parts_mut(&mut input_event as *mut InputEvent as *mut u8,
                                                                mem::size_of::<InputEvent>());
                    if file.read_exact(event_slice).is_err() {
                        break;
                    }
                }
                tx.send(input_event).unwrap();
            }
        }
    }

    Ok(())
}

pub fn usb_events() -> Receiver<DeviceEvent> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || parse_usb_events(&tx));
    rx
}

fn parse_usb_events(tx: &Sender<DeviceEvent>) {
    let path = CString::new("/tmp/nickel-hardware-status").unwrap();
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_NONBLOCK | libc::O_RDWR) };

    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };

    const BUF_LEN: usize = 256;

    loop {
        let ret = unsafe { libc::poll(&mut pfd as *mut libc::pollfd, 1, -1) };

        if ret < 0 {
            break;
        }

        let buf = CString::new(vec![1; BUF_LEN]).unwrap();
        let c_buf = buf.into_raw();

        if pfd.revents & libc::POLLIN != 0 {
            let n = unsafe { libc::read(fd, c_buf as *mut libc::c_void, BUF_LEN as libc::size_t) };
            let buf = unsafe { CString::from_raw(c_buf) };
            if n > 0 {
                if let Ok(s) = buf.to_str() {
                    for msg in s[..n as usize].lines() {
                        if msg == "usb plug add" {
                            tx.send(DeviceEvent::Plug(PowerSource::Host)).unwrap();
                        } else if msg == "usb plug remove" {
                            tx.send(DeviceEvent::Unplug(PowerSource::Host)).unwrap();
                        } else if msg == "usb ac add" {
                            tx.send(DeviceEvent::Plug(PowerSource::Wall)).unwrap();
                        } else if msg == "usb ac remove" {
                            tx.send(DeviceEvent::Unplug(PowerSource::Wall)).unwrap();
                        } else if msg.starts_with("network bound") {
                            tx.send(DeviceEvent::NetUp).unwrap();
                        }
                    }
                }
            } else {
                break;
            }
        }
    }
}

pub fn device_events(rx: Receiver<InputEvent>, display: Display) -> Receiver<DeviceEvent> {
    let (ty, ry) = mpsc::channel();
    thread::spawn(move || parse_device_events(&rx, &ty, display));
    ry
}

pub fn parse_device_events(rx: &Receiver<InputEvent>, ty: &Sender<DeviceEvent>, display: Display) {
    let mut id = 0;
    let mut position = Point::default();
    let mut pressure = 0;
    let mut last_activity = -60;
    let Display { mut dims, mut rotation } = display;
    let mut fingers: FnvHashMap<i32, Point> = FnvHashMap::default();
    let mut packet_ids: FnvHashSet<i32> = FnvHashSet::default();
    let proto = CURRENT_DEVICE.proto;

    let mut tc = match proto {
        TouchProto::Single => SINGLE_TOUCH_CODES,
        TouchProto::MultiA => MULTI_TOUCH_CODES_A,
        TouchProto::MultiB => MULTI_TOUCH_CODES_B,
    };

    let (mut mirror_x, mut mirror_y) = CURRENT_DEVICE.should_mirror_axes(rotation);
    if rotation % 2 == 1 {
        mem::swap(&mut tc.x, &mut tc.y);
    }

    while let Ok(evt) = rx.recv() {
        if evt.kind == EV_ABS {
            if evt.code == ABS_MT_TRACKING_ID {
                id = evt.value;
                if proto == TouchProto::MultiB {
                    packet_ids.insert(id);
                }
            } else if evt.code == tc.x {
                position.x = if mirror_x {
                    dims.0 as i32 - 1 - evt.value
                } else {
                    evt.value
                };
            } else if evt.code == tc.y {
                position.y = if mirror_y {
                    dims.1 as i32 - 1 - evt.value
                } else {
                    evt.value
                };
            } else if evt.code == tc.pressure {
                pressure = evt.value;
            }
        } else if evt.kind == EV_SYN {
            // The absolute value accounts for the wrapping around that might occur,
            // since `tv_sec` can't grow forever.
            if (evt.time.tv_sec - last_activity).abs() >= 60 {
                last_activity = evt.time.tv_sec;
                ty.send(DeviceEvent::UserActivity).unwrap();
            }
            if evt.code == SYN_MT_REPORT || (proto == TouchProto::Single && evt.code == SYN_REPORT) {
                if let Some(&p) = fingers.get(&id) {
                    if pressure > 0 {
                        if p != position {
                            ty.send(DeviceEvent::Finger {
                                id,
                                time: seconds(evt.time),
                                status: FingerStatus::Motion,
                                position,
                            }).unwrap();
                            fingers.insert(id, position);
                        }
                    } else {
                        ty.send(DeviceEvent::Finger {
                            id,
                            time: seconds(evt.time),
                            status: FingerStatus::Up,
                            position,
                        }).unwrap();
                        fingers.remove(&id);
                    }
                } else {
                    ty.send(DeviceEvent::Finger {
                        id,
                        time: seconds(evt.time),
                        status: FingerStatus::Down,
                        position,
                    }).unwrap();
                    fingers.insert(id, position);
                }
            } else if proto == TouchProto::MultiB && evt.code == SYN_REPORT {
                fingers.retain(|other_id, other_position| {
                    packet_ids.contains(other_id) ||
                    ty.send(DeviceEvent::Finger {
                        id: *other_id,
                        time: seconds(evt.time),
                        status: FingerStatus::Up,
                        position: *other_position,
                    }).is_err()
                });
                packet_ids.clear();
            }
        } else if evt.kind == EV_KEY {
            if evt.code == SLEEP_COVER {
                if evt.value == VAL_PRESS {
                    ty.send(DeviceEvent::CoverOn).unwrap();
                } else if evt.value == VAL_RELEASE {
                    ty.send(DeviceEvent::CoverOff).unwrap();
                }
            } else if evt.code == KEY_ROTATE_DISPLAY {
                let next_rotation = evt.value as i8;
                if next_rotation != rotation {
                    let delta = (rotation - next_rotation).abs();
                    if delta % 2 == 1 {
                        mem::swap(&mut tc.x, &mut tc.y);
                        mem::swap(&mut dims.0, &mut dims.1);
                    }
                    rotation = next_rotation;
                    let should_mirror = CURRENT_DEVICE.should_mirror_axes(rotation);
                    mirror_x = should_mirror.0;
                    mirror_y = should_mirror.1;
                }
            } else {
                if let Some(button_status) = ButtonStatus::try_from_raw(evt.value) {
                    ty.send(DeviceEvent::Button {
                        time: seconds(evt.time),
                        code: ButtonCode::from_raw(evt.code, rotation),
                        status: button_status,
                    }).unwrap();
                }
            }
        } else if evt.kind == EV_MSC && evt.code == MSC_RAW {
            if evt.value >= MSC_RAW_GSENSOR_PORTRAIT_DOWN && evt.value <= MSC_RAW_GSENSOR_LANDSCAPE_LEFT {
                let next_rotation = match evt.value {
                    MSC_RAW_GSENSOR_PORTRAIT_DOWN => 3,
                    MSC_RAW_GSENSOR_PORTRAIT_UP => 1,
                    MSC_RAW_GSENSOR_LANDSCAPE_RIGHT => 2,
                    MSC_RAW_GSENSOR_LANDSCAPE_LEFT => 0,
                    _ => rotation,
                };
                ty.send(DeviceEvent::RotateScreen(next_rotation)).unwrap();
            }
        }
    }
}
