use std::ptr;
use std::path::Path;
use std::io;
use std::fs::{OpenOptions, File};
use std::slice;
use std::os::unix::io::AsRawFd;
use std::ops::Drop;
use failure::{Error, ResultExt};
use crate::geom::Rectangle;
use crate::device::{CURRENT_DEVICE, Model};
use super::{UpdateMode, Framebuffer};
use super::mxcfb_sys::*;

impl Into<MxcfbRect> for Rectangle {
    fn into(self) -> MxcfbRect {
        MxcfbRect {
            top: self.min.y as u32,
            left: self.min.x as u32,
            width: self.width(),
            height: self.height(),
        }
    }
}

type SetPixelRgb = fn(&mut KoboFramebuffer, u32, u32, [u8; 3]);
type GetPixelRgb = fn(&KoboFramebuffer, u32, u32) -> [u8; 3];
type AsRgb = fn(&KoboFramebuffer) -> Vec<u8>;

pub struct KoboFramebuffer {
    file: File,
    frame: *mut libc::c_void,
    frame_size: libc::size_t, 
    token: u32,
    flags: u32,
    monochrome: bool,
    set_pixel_rgb: SetPixelRgb,
    get_pixel_rgb: GetPixelRgb,
    as_rgb: AsRgb,
    bytes_per_pixel: u8,
    var_info: VarScreenInfo,
    fix_info: FixScreenInfo,
}

impl KoboFramebuffer {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<KoboFramebuffer, Error> {
        let file = OpenOptions::new().read(true)
                                     .write(true)
                                     .open(path)
                                     .context("Can't open framebuffer device.")?;

        let var_info = var_screen_info(&file)?;
        let fix_info = fix_screen_info(&file)?;

        assert_eq!(var_info.bits_per_pixel % 8, 0);

        let bytes_per_pixel = var_info.bits_per_pixel / 8;
        let frame_size = (var_info.yres * fix_info.line_length) as libc::size_t;

        let frame = unsafe {
            libc::mmap(ptr::null_mut(), fix_info.smem_len as usize,
                       libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED,
                       file.as_raw_fd(), 0)
        };

        if frame == libc::MAP_FAILED {
            Err(Error::from(io::Error::last_os_error()).context("Can't map memory.").into())
        } else {
            let (set_pixel_rgb, get_pixel_rgb, as_rgb): (SetPixelRgb, GetPixelRgb, AsRgb) = if var_info.bits_per_pixel > 16 {
                (set_pixel_rgb_32, get_pixel_rgb_32, as_rgb_32)
            } else {
                (set_pixel_rgb_16, get_pixel_rgb_16, as_rgb_16)
            };
            Ok(KoboFramebuffer {
                   file,
                   frame,
                   frame_size,
                   token: 1,
                   flags: 0,
                   monochrome: false,
                   set_pixel_rgb,
                   get_pixel_rgb,
                   as_rgb,
                   bytes_per_pixel: bytes_per_pixel as u8,
                   var_info,
                   fix_info,
               })
        }
    }

    fn as_bytes(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.frame as *const u8, self.frame_size) }
    }
}

impl Framebuffer for KoboFramebuffer {
    fn set_pixel(&mut self, x: u32, y: u32, color: u8) {
        (self.set_pixel_rgb)(self, x, y, [color, color, color]);
    }

    fn set_blended_pixel(&mut self, x: u32, y: u32, color: u8, alpha: f32) {
        if alpha >= 1.0 {
            self.set_pixel(x, y, color);
            return;
        }
        let rgb = (self.get_pixel_rgb)(self, x, y);
        let color_alpha = color as f32 * alpha;
        let red = color_alpha + (1.0 - alpha) * rgb[0] as f32;
        let green = color_alpha + (1.0 - alpha) * rgb[1] as f32;
        let blue = color_alpha + (1.0 - alpha) * rgb[2] as f32;
        (self.set_pixel_rgb)(self, x, y, [red as u8, green as u8, blue as u8]);
    }

    fn invert_region(&mut self, rect: &Rectangle) {
        for y in rect.min.y..rect.max.y {
            for x in rect.min.x..rect.max.x {
                let rgb = (self.get_pixel_rgb)(self, x as u32, y as u32);
                let red = 255 - rgb[0];
                let green = 255 - rgb[1];
                let blue = 255 - rgb[2];
                (self.set_pixel_rgb)(self, x as u32, y as u32, [red, green, blue]);
            }
        }
    }

    fn shift_region(&mut self, rect: &Rectangle, drift: u8) {
        for y in rect.min.y..rect.max.y {
            for x in rect.min.x..rect.max.x {
                let rgb = (self.get_pixel_rgb)(self, x as u32, y as u32);
                let red = rgb[0].saturating_sub(drift);
                let green = rgb[1].saturating_sub(drift);
                let blue = rgb[2].saturating_sub(drift);
                (self.set_pixel_rgb)(self, x as u32, y as u32, [red, green, blue]);
            }
        }
    }

    // Tell the driver that the screen needs to be redrawn.
    fn update(&mut self, rect: &Rectangle, mode: UpdateMode) -> Result<u32, Error> {
        let update_marker = self.token;
        let mut flags = self.flags;
        let mark = CURRENT_DEVICE.mark();

        let (update_mode, mut waveform_mode) = match mode {
            UpdateMode::Gui => (UPDATE_MODE_PARTIAL, WAVEFORM_MODE_AUTO),
            UpdateMode::Partial  => {
                if mark >= 7 {
                    (UPDATE_MODE_PARTIAL, NTX_WFM_MODE_GLR16)
                } else if CURRENT_DEVICE.model == Model::Aura {
                    flags |= EPDC_FLAG_USE_AAD;
                    (UPDATE_MODE_FULL, NTX_WFM_MODE_GLD16)
                } else {
                    (UPDATE_MODE_PARTIAL, WAVEFORM_MODE_AUTO)
                }
            },
            UpdateMode::Full     => (UPDATE_MODE_FULL, NTX_WFM_MODE_GC16),
            UpdateMode::Fast     => (UPDATE_MODE_PARTIAL, NTX_WFM_MODE_A2),
            UpdateMode::FastMono => {
                flags |= EPDC_FLAG_FORCE_MONOCHROME;
                (UPDATE_MODE_PARTIAL, NTX_WFM_MODE_A2)
            },
        };

        if self.monochrome {
            flags |= EPDC_FLAG_FORCE_MONOCHROME;
            waveform_mode = NTX_WFM_MODE_A2;
        }

        let result = if mark >= 7 {
            let update_data = MxcfbUpdateDataV2 {
                update_region: (*rect).into(),
                waveform_mode,
                update_mode,
                update_marker,
                temp: TEMP_USE_AMBIENT,
                flags,
                dither_mode: 0,
                quant_bit: 0,
                alt_buffer_data: MxcfbAltBufferDataV2::default(),
            };
            unsafe {
                send_update_v2(self.file.as_raw_fd(), &update_data)
            }
        } else {
            let update_data = MxcfbUpdateDataV1 {
                update_region: (*rect).into(),
                waveform_mode,
                update_mode,
                update_marker,
                temp: TEMP_USE_AMBIENT,
                flags,
                alt_buffer_data: MxcfbAltBufferDataV1::default(),
            };
            unsafe {
                send_update_v1(self.file.as_raw_fd(), &update_data)
            }
        };

        match result {
            Err(e) => Err(Error::from(e).context("Can't send framebuffer update.").into()),
            _ => {
                self.token = self.token.wrapping_add(1);
                Ok(update_marker)
            }
        }
    }

    // Wait for a specific update to complete.
    fn wait(&self, token: u32) -> Result<i32, Error> {
        let result = if CURRENT_DEVICE.mark() >= 7 {
            let mut marker_data = MxcfbUpdateMarkerData {
                update_marker: token,
                collision_test: 0,
            };
            unsafe {
                wait_for_update_v2(self.file.as_raw_fd(), &mut marker_data)
            }
        } else {
            unsafe {
                wait_for_update_v1(self.file.as_raw_fd(), &token)
            }
        };
        result.map_err(|e| Error::from(e).context("Can't wait for framebuffer update.").into())
    }

    fn save(&self, path: &str) -> Result<(), Error> {
        let (width, height) = self.dims();
        let file = File::create(path).context("Can't create output file.")?;
        let mut encoder = png::Encoder::new(file, width, height);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_color(png::ColorType::RGB);
        let mut writer = encoder.write_header().context("Can't write header.")?;
        writer.write_image_data(&(self.as_rgb)(self)).context("Can't write data to file.")?;
        Ok(())
    }

    #[inline]
    fn rotation(&self) -> i8 {
        self.var_info.rotate as i8
    }

    fn set_rotation(&mut self, n: i8) -> Result<(u32, u32), Error> {
        let read_rotation = self.rotation();

        // On the Aura H₂O, the first ioctl call will succeed but have no effect,
        // if (n - m).abs() % 2 == 1, where m is the previously written value.
        // In order for the call to have an effect, we need to write an intermediate
        // value: (n+1)%4.
        for (i, v) in [n, (n+1)%4, n].iter().enumerate() {
            self.var_info.rotate = *v as u32;

            let result = unsafe {
                write_variable_screen_info(self.file.as_raw_fd(), &mut self.var_info)
            };

            if let Err(e) = result {
                return Err(Error::from(e)
                                 .context("Can't set variable screen info.").into());
            }

            // If the first call changed the rotation value, we can exit the loop.
            if i == 0 && read_rotation != self.rotation() {
                break;
            }
        }

        self.fix_info = fix_screen_info(&self.file)?;
        self.frame_size = (self.var_info.yres * self.fix_info.line_length) as libc::size_t;

        println!("Framebuffer rotation: {} -> {}.", n, self.rotation());

        Ok((self.var_info.xres, self.var_info.yres))
    }

    fn set_inverted(&mut self, enable: bool) {
        if enable {
            self.flags |= EPDC_FLAG_ENABLE_INVERSION;
        } else {
            self.flags &= !EPDC_FLAG_ENABLE_INVERSION;
        }
    }

    fn inverted(&self) -> bool {
        self.flags & EPDC_FLAG_ENABLE_INVERSION != 0
    }

    fn set_monochrome(&mut self, enable: bool) {
        self.monochrome = enable;
    }

    fn monochrome(&self) -> bool {
        self.monochrome
    }

    fn width(&self) -> u32 {
        self.var_info.xres
    }

    fn height(&self) -> u32 {
        self.var_info.yres
    }
}

pub fn set_pixel_rgb_16(fb: &mut KoboFramebuffer, x: u32, y: u32, rgb: [u8; 3]) {
    let addr = (fb.var_info.xoffset as isize + x as isize) * (fb.bytes_per_pixel as isize) +
               (fb.var_info.yoffset as isize + y as isize) * (fb.fix_info.line_length as isize);

    debug_assert!(addr < fb.frame_size as isize);

    unsafe {
        let spot = fb.frame.offset(addr) as *mut u8;
        *spot.offset(0) = rgb[2] >> 3 | (rgb[1] & 0b0001_1100) << 3;
        *spot.offset(1) = (rgb[0] & 0b1111_1000) | rgb[1] >> 5;
    }
}

pub fn set_pixel_rgb_32(fb: &mut KoboFramebuffer, x: u32, y: u32, rgb: [u8; 3]) {
    let addr = (fb.var_info.xoffset as isize + x as isize) * (fb.bytes_per_pixel as isize) +
               (fb.var_info.yoffset as isize + y as isize) * (fb.fix_info.line_length as isize);

    debug_assert!(addr < fb.frame_size as isize);

    unsafe {
        let spot = fb.frame.offset(addr) as *mut u8;
        *spot.offset(0) = rgb[2];
        *spot.offset(1) = rgb[1];
        *spot.offset(2) = rgb[0];
        // *spot.offset(3) = 0x00;
    }
}

fn get_pixel_rgb_16(fb: &KoboFramebuffer, x: u32, y: u32) -> [u8; 3] {
    let addr = (fb.var_info.xoffset as isize + x as isize) * (fb.bytes_per_pixel as isize) +
               (fb.var_info.yoffset as isize + y as isize) * (fb.fix_info.line_length as isize);
    let pair = unsafe {
        let spot = fb.frame.offset(addr) as *mut u8;
        [*spot.offset(0), *spot.offset(1)]
    };
    let red = pair[1] & 0b1111_1000;
    let green = ((pair[1] & 0b0000_0111) << 5) | ((pair[0] & 0b1110_0000) >> 3);
    let blue = (pair[0] & 0b0001_1111) << 3;
    [red, green, blue]
}

fn get_pixel_rgb_32(fb: &KoboFramebuffer, x: u32, y: u32) -> [u8; 3] {
    let addr = (fb.var_info.xoffset as isize + x as isize) * (fb.bytes_per_pixel as isize) +
               (fb.var_info.yoffset as isize + y as isize) * (fb.fix_info.line_length as isize);
    unsafe {
        let spot = fb.frame.offset(addr) as *mut u8;
        [*spot.offset(2), *spot.offset(1), *spot.offset(0)]
    }
}

fn as_rgb_16(fb: &KoboFramebuffer) -> Vec<u8> {
    let (width, height) = fb.dims();
    let mut rgb888 = Vec::with_capacity((width * height * 3) as usize);
    let rgb565 = fb.as_bytes();
    let virtual_width = fb.var_info.xres_virtual as usize;
    for (_, pair) in rgb565.chunks(2).take(height as usize * virtual_width).enumerate()
                           .filter(|&(i, _)| i % virtual_width < width as usize) {
        let red = pair[1] & 0b1111_1000;
        let green = ((pair[1] & 0b0000_0111) << 5) | ((pair[0] & 0b1110_0000) >> 3);
        let blue = (pair[0] & 0b0001_1111) << 3;
        rgb888.extend_from_slice(&[red, green, blue]);
    }
    rgb888
}

fn as_rgb_32(fb: &KoboFramebuffer) -> Vec<u8> {
    let (width, height) = fb.dims();
    let mut rgb888 = Vec::with_capacity((width * height * 3) as usize);
    let bgra8888 = fb.as_bytes();
    let virtual_width = fb.var_info.xres_virtual as usize;
    for (_, bgra) in bgra8888.chunks(4).take(height as usize * virtual_width).enumerate()
                           .filter(|&(i, _)| i % virtual_width < width as usize) {
        let red = bgra[2];
        let green = bgra[1];
        let blue = bgra[0];
        rgb888.extend_from_slice(&[red, green, blue]);
    }
    rgb888
}

pub fn fix_screen_info(file: &File) -> Result<FixScreenInfo, Error> {
    let mut info: FixScreenInfo = Default::default();
    let result = unsafe {
        read_fixed_screen_info(file.as_raw_fd(), &mut info)
    };
    match result {
        Err(e) => Err(Error::from(e).context("Can't get fixed screen info.").into()),
        _ => Ok(info),
    }
}

pub fn var_screen_info(file: &File) -> Result<VarScreenInfo, Error> {
    let mut info: VarScreenInfo = Default::default();
    let result = unsafe {
        read_variable_screen_info(file.as_raw_fd(), &mut info)
    };
    match result {
        Err(e) => Err(Error::from(e).context("Can't get variable screen info.").into()),
        _ => Ok(info),
    }
}

impl Drop for KoboFramebuffer {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.frame, self.fix_info.smem_len as usize);
        }
    }
}
